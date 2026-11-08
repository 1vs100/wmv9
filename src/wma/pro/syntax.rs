//! WMA Pro frames as the bitstream codes them, read and written. The model
//! holds coded values (scale factor differences, quantized coefficients,
//! flags). Where the syntax allows several codings of the same values the
//! writer takes the one Microsoft's encoder writes in the samples
//! (tests/wmapro_samples.rs): vectors of four before two before one, table
//! codes before escapes, the shortest escape run, an end code only before
//! trailing zeros.

use super::config::Config;
use super::vlc::{Tables, VlcError};
use crate::wmv3::bits::{BitReader, BitWriter, OutOfBits};

/// An error in a frame's bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SyntaxError {
    #[error(transparent)]
    OutOfBits(#[from] OutOfBits),
    #[error("no code matches the bits at {position}")]
    NoCode { position: usize },
    #[error("subframe length below the smallest at bit {position}")]
    SubframeLength { position: usize },
    #[error("a channel's subframes run past the frame at bit {position}")]
    ChannelPastFrame { position: usize },
    #[error("reserved subframe bit set at bit {position}")]
    ReservedBit { position: usize },
    #[error("channel transform bit set at bit {position}")]
    TransformBit { position: usize },
    #[error("unknown two-channel transform at bit {position}")]
    TransformType { position: usize },
    #[error("vector count {count} past a subframe of {len} at bit {position}")]
    VectorCount {
        count: usize,
        len: usize,
        position: usize,
    },
    #[error("scale factor change past the bands at bit {position}")]
    ScaleBand { position: usize },
    #[error("escape run code 111 at bit {position}")]
    EscapeRun { position: usize },
    #[error("run past the subframe at bit {position}")]
    RunPastSubframe { position: usize },
    #[error(
        "length prefix {prefix} with {used} bits of frame; one bit and the trailer must follow"
    )]
    FrameEnd { prefix: usize, used: usize },
}

impl From<VlcError> for SyntaxError {
    fn from(e: VlcError) -> Self {
        match e {
            VlcError::OutOfBits(e) => SyntaxError::OutOfBits(e),
            VlcError::NoCode { position } => SyntaxError::NoCode { position },
        }
    }
}

/// A model the bitstream cannot carry, or the reader would not give back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    #[error("channel {channel}: subframes of {total} samples, not the frame length")]
    TilingLength { channel: usize, total: usize },
    #[error("channel {channel}: a subframe of {len} samples")]
    SubframeSize { channel: usize, len: usize },
    #[error("one tiling for all channels, but the channels' differ")]
    TilingNotShared,
    #[error("subframe {subframe} does not follow the tiling")]
    SubframeOrder { subframe: usize },
    #[error("subframe {subframe}: {what:?}")]
    Subframe {
        subframe: usize,
        what: SubframeError,
    },
    #[error("post-processing matrix of {len} values, or a value over 15")]
    PostProc { len: usize },
    #[error("DRC gain present {present} against the decode flag")]
    DrcGain { present: bool },
    #[error("trim of {value} samples does not fit {width} bits")]
    Trim { value: u32, width: u32 },
    #[error("frame of {bits} bits; the length prefix holds at most {max}")]
    FrameTooLong { bits: usize, max: usize },
}

/// What is wrong in one subframe of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubframeError {
    ChannelCount,
    Groups,
    Transform,
    TransformBands,
    /// Fill of no bits, a short fill of more than 3, or a long fill whose
    /// width does not hold its count.
    Fill,
    /// A quantizer without a channel to code, or a channel coded without a
    /// quantizer.
    Quant,
    VectorCount,
    Modifier,
    /// The scale factor coding does not follow the frame: DPCM until a
    /// channel's factors are coded, after that kept or changed.
    ScaleOrder,
    ScaleDelta,
    ScaleChange,
    Coefficients,
}

/// A channel of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChannelIndex(pub usize);

/// A place in a subframe's list of channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position(pub usize);

/// Subframe lengths per channel, in order; each channel's add up to the
/// frame length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tiling(pub Vec<Vec<usize>>);

/// The optional post-processing matrix of the frame header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostProc {
    Absent,
    /// The flag set but no matrix.
    Flag,
    /// channels * channels values of 4 bits.
    Matrix(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trim {
    pub start: Option<u32>,
    pub end: Option<u32>,
}

/// A frame. One bit, 1 in Microsoft's streams, and the trailer bit follow
/// the last subframe; the writer writes the 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub tiling: Tiling,
    /// The tiling header's flag that all channels share one tiling; read
    /// only when a frame can have more than one subframe.
    pub same_tiling: bool,
    pub postproc: PostProc,
    /// With the DRC decode flag.
    pub drc_gain: Option<u8>,
    pub trim: Option<Trim>,
    pub subframes: Vec<Subframe>,
}

/// The bits of an extended subframe header that decoders skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fill {
    /// 1 to 3 bits, counted in 2 bits.
    Short(Vec<bool>),
    /// Counted by a value of `width` bits, itself in 4 bits, plus one.
    Long { width: u8, bits: Vec<bool> },
}

/// How the channels of a group are mixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transform {
    /// Two channels without a transform (bits 1, 0).
    PairNone,
    /// Two channels, sum and difference (bit 0).
    PairMidSide,
    /// More than two channels without a transform (bit 0).
    None,
    /// More than two channels, the default matrix (bits 1, 0).
    Default,
    /// More than two channels, a matrix from rotations and signs.
    Matrix {
        rotations: Vec<u8>,
        signs: Vec<bool>,
    },
    /// One channel, or none; nothing coded.
    Single,
}

impl Transform {
    fn active(&self) -> bool {
        matches!(
            self,
            Transform::PairMidSide | Transform::Default | Transform::Matrix { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub members: Vec<Position>,
    pub transform: Transform,
    /// With an active transform: per scale factor band, or `None` for all
    /// bands.
    pub bands: Option<Vec<bool>>,
}

/// A change of a resampled scale factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleChange {
    pub band: usize,
    pub delta: i32,
}

/// Scale factors of one channel in a subframe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScaleFactors {
    /// First in the frame: step (1 to 4) and differences from 45 / step.
    Dpcm { step: u8, deltas: Vec<i32> },
    /// The previous factors, resampled.
    Kept,
    /// The previous factors, resampled, with changes at bands.
    Changed(Vec<ScaleChange>),
}

/// The pair of run-level tables coefficients are coded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoefTable {
    /// COEF0 (bit 0).
    First,
    /// COEF1 (bit 1).
    Second,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coefficients {
    pub table: CoefTable,
    /// Quantized values of the whole subframe.
    pub values: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubframeChannel {
    pub transmit: bool,
    /// With `Quant::vector_counts`: coefficients coded as vectors.
    pub vector_count: Option<usize>,
    /// The raw modifier of the quantizer step when one is coded.
    pub modifier: Option<u32>,
    pub scale: Option<ScaleFactors>,
    pub coefficients: Option<Coefficients>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quant {
    pub vector_counts: bool,
    /// The quantizer step, quant_base plus the coded difference.
    pub step: i32,
    /// Width of the channel modifiers, coded when more than one channel.
    pub modifier_len: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subframe {
    pub channel_indexes: Vec<ChannelIndex>,
    pub len: usize,
    pub fill: Option<Fill>,
    pub groups: Vec<Group>,
    pub channels: Vec<SubframeChannel>,
    pub quant: Option<Quant>,
}

/// Per-channel state through a frame that decides how fields are coded.
#[derive(Clone)]
struct ChannelState {
    decoded: usize,
    next: usize,
    /// Scale factors were coded earlier in the frame.
    coded: bool,
}

fn new_state(channels: usize) -> Vec<ChannelState> {
    vec![
        ChannelState {
            decoded: 0,
            next: 0,
            coded: false,
        };
        channels
    ]
}

/// A subframe's channels and length.
struct SubframeShape {
    channels: Vec<ChannelIndex>,
    len: usize,
}

/// The order of subframes: the channel with the fewest samples so far
/// next, with every channel that is at the same point and has a subframe
/// of the same length.
fn next_subframe(tiling: &Tiling, state: &mut [ChannelState]) -> Option<SubframeShape> {
    let mut offset = usize::MAX;
    let mut len = 0;
    for (c, s) in state.iter().enumerate() {
        if s.next < tiling.0[c].len() && s.decoded < offset {
            offset = s.decoded;
            len = tiling.0[c][s.next];
        }
    }
    if offset == usize::MAX {
        return None;
    }
    let channels: Vec<ChannelIndex> = (0..state.len())
        .filter(|&c| {
            state[c].next < tiling.0[c].len()
                && state[c].decoded == offset
                && tiling.0[c][state[c].next] == len
        })
        .map(ChannelIndex)
        .collect();
    for c in &channels {
        state[c.0].decoded += len;
    }
    Some(SubframeShape { channels, len })
}

/// Bit sequences of frames and packets.
pub type Bits = Vec<bool>;

/// Bits as bytes, the last one padded with zeros.
pub fn pack(bits: &[bool]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |a, (i, &b)| a | u8::from(b) << (7 - i))
        })
        .collect()
}

pub fn unpack(bytes: &[u8], len: usize) -> Bits {
    (0..len)
        .map(|i| bytes[i / 8] >> (7 - i % 8) & 1 == 1)
        .collect()
}

fn writer_bits(w: BitWriter) -> Bits {
    let len = w.bit_len();
    unpack(&w.finish(), len)
}

/// Largest value of a large value code.
const MAX_LARGE: u32 = (1 << 31) - 1;

/// Reads a large value: 8, 16, 24 or 31 bits after a prefix of 0, 10,
/// 110 or 111.
fn get_large(r: &mut BitReader) -> Result<u32, SyntaxError> {
    let mut n = 8;
    if r.bit()? {
        n += 8;
        if r.bit()? {
            n += 8;
            if r.bit()? {
                n += 7;
            }
        }
    }
    Ok(r.read(n)?)
}

fn put_large(w: &mut BitWriter, v: u32) {
    match v {
        ..0x100 => {
            w.bit(false);
            w.write(8, v);
        }
        0x100..0x1_0000 => {
            w.write(2, 0b10);
            w.write(16, v);
        }
        0x1_0000..0x100_0000 => {
            w.write(3, 0b110);
            w.write(24, v);
        }
        _ => {
            w.write(3, 0b111);
            w.write(31, v);
        }
    }
}

/// Width of the escape run of run-level coefficients.
fn esc_len(len: usize) -> u32 {
    (len - 1).ilog2() + 1
}

/// A scale factor change as the run-level code carries it.
struct RunLevel {
    skip: usize,
    magnitude: i32,
    positive: bool,
}

/// The table code of a scale factor change, if there is one.
fn scale_rl_code(skip: usize, magnitude: u32) -> Option<usize> {
    use wma_tables::pro as t;
    (2..t::SCALE_RL_RUN.len()).find(|&k| {
        usize::from(t::SCALE_RL_RUN[k]) == skip && u32::from(t::SCALE_RL_LEVEL[k]) == magnitude
    })
}

/// Escaped scale factor changes: 8 bits of magnitude, 5 of skip.
const MAX_ESCAPED_SCALE: u32 = 255;
const MAX_ESCAPED_SKIP: usize = 31;

pub struct Codec {
    cfg: Config,
    t: Tables,
}

impl Codec {
    pub fn new(cfg: &Config) -> Self {
        Codec {
            cfg: cfg.clone(),
            t: Tables::new(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    fn num_bands(&self, len: usize) -> usize {
        self.cfg.bands[self.cfg.size_index(len)].len() - 1
    }

    /// Reads a frame from its first bit, the length prefix, to its trailer
    /// bit, which it returns too.
    pub fn read(&self, bits: &[bool]) -> Result<(Frame, bool), SyntaxError> {
        let bytes = pack(bits);
        let mut r = BitReader::new(&bytes);
        let cfg = &self.cfg;
        let len = if cfg.header.decode_flags.len_prefix() {
            r.read(cfg.log2_frame_size)? as usize
        } else {
            bits.len()
        };
        let (tiling, same_tiling) = self.read_tiling(&mut r)?;
        let postproc = if cfg.channels > 1 && r.bit()? {
            if r.bit()? {
                let n = cfg.channels * cfg.channels;
                PostProc::Matrix(
                    (0..n)
                        .map(|_| r.read(4).map(|v| v as u8))
                        .collect::<Result<_, _>>()?,
                )
            } else {
                PostProc::Flag
            }
        } else {
            PostProc::Absent
        };
        let drc_gain = if cfg.header.decode_flags.drc() {
            Some(r.read(8)? as u8)
        } else {
            None
        };
        let trim = if r.bit()? {
            let w = (cfg.frame_len * 2).ilog2();
            let start = if r.bit()? { Some(r.read(w)?) } else { None };
            let end = if r.bit()? { Some(r.read(w)?) } else { None };
            Some(Trim { start, end })
        } else {
            None
        };
        let mut state = new_state(cfg.channels);
        let mut subframes = Vec::new();
        while let Some(shape) = next_subframe(&tiling, &mut state) {
            subframes.push(self.read_subframe(&mut r, shape, &mut state)?);
        }
        let used = r.position();
        // FFmpeg takes a frame only when one bit follows the subframes
        // before the trailer.
        if len != used + 2 || len > bits.len() {
            return Err(SyntaxError::FrameEnd { prefix: len, used });
        }
        let trailer = bits[len - 1];
        Ok((
            Frame {
                tiling,
                same_tiling,
                postproc,
                drc_gain,
                trim,
                subframes,
            },
            trailer,
        ))
    }

    fn read_subframe_len(&self, r: &mut BitReader, offset: usize) -> Result<usize, SyntaxError> {
        let cfg = &self.cfg;
        if offset == cfg.frame_len - cfg.min_subframe_len {
            return Ok(cfg.min_subframe_len);
        }
        let shift = if cfg.max_subframe_len_bit {
            if r.bit()? {
                1 + r.read(cfg.subframe_len_bits - 1)?
            } else {
                0
            }
        } else {
            r.read(cfg.subframe_len_bits)?
        };
        let len = cfg.frame_len >> shift;
        if len < cfg.min_subframe_len {
            return Err(SyntaxError::SubframeLength {
                position: r.position(),
            });
        }
        Ok(len)
    }

    fn write_subframe_len(&self, w: &mut BitWriter, offset: usize, len: usize) {
        let cfg = &self.cfg;
        if offset == cfg.frame_len - cfg.min_subframe_len {
            return;
        }
        let shift = (cfg.frame_len / len).ilog2();
        if cfg.max_subframe_len_bit {
            if shift == 0 {
                w.bit(false);
            } else {
                w.bit(true);
                w.write(cfg.subframe_len_bits - 1, shift - 1);
            }
        } else {
            w.write(cfg.subframe_len_bits, shift);
        }
    }

    /// decode_tilehdr.
    fn read_tiling(&self, r: &mut BitReader) -> Result<(Tiling, bool), SyntaxError> {
        let cfg = &self.cfg;
        let n = cfg.channels;
        let same = cfg.max_subframes == 1 || r.bit()?;
        let mut tiles = vec![Vec::new(); n];
        let mut samples = vec![0usize; n];
        let mut min_len = 0;
        let mut channels_for_cur = n;
        while min_len < cfg.frame_len {
            let mut contains = vec![false; n];
            for c in 0..n {
                if samples[c] == min_len {
                    contains[c] = same
                        || channels_for_cur == 1
                        || min_len == cfg.frame_len - cfg.min_subframe_len
                        || r.bit()?;
                }
            }
            let len = self.read_subframe_len(r, min_len)?;
            min_len += len;
            for c in 0..n {
                if contains[c] {
                    tiles[c].push(len);
                    samples[c] += len;
                    if samples[c] > cfg.frame_len {
                        return Err(SyntaxError::ChannelPastFrame {
                            position: r.position(),
                        });
                    }
                } else if samples[c] <= min_len {
                    if samples[c] < min_len {
                        channels_for_cur = 0;
                        min_len = samples[c];
                    }
                    channels_for_cur += 1;
                }
            }
        }
        Ok((Tiling(tiles), same))
    }

    /// The bits of decode_tilehdr for a tiling `check` passed.
    fn write_tiling(&self, w: &mut BitWriter, tiling: &Tiling, same: bool) {
        let cfg = &self.cfg;
        let n = cfg.channels;
        if cfg.max_subframes > 1 {
            w.bit(same);
        }
        let mut next = vec![0usize; n];
        let mut samples = vec![0usize; n];
        let mut min_len = 0;
        let mut channels_for_cur = n;
        while min_len < cfg.frame_len {
            // The channels at the earliest point; the first of them sets
            // the length, and those with a subframe of that length next
            // contain it.
            let len = (0..n)
                .find(|&c| samples[c] == min_len)
                .map(|c| tiling.0[c][next[c]])
                .expect("a channel continues the frame");
            let mut contains = vec![false; n];
            for c in 0..n {
                if samples[c] == min_len {
                    let implied = same
                        || channels_for_cur == 1
                        || min_len == cfg.frame_len - cfg.min_subframe_len;
                    contains[c] = tiling.0[c][next[c]] == len;
                    if !implied {
                        w.bit(contains[c]);
                    }
                }
            }
            self.write_subframe_len(w, min_len, len);
            min_len += len;
            for c in 0..n {
                if contains[c] {
                    next[c] += 1;
                    samples[c] += len;
                } else if samples[c] <= min_len {
                    if samples[c] < min_len {
                        channels_for_cur = 0;
                        min_len = samples[c];
                    }
                    channels_for_cur += 1;
                }
            }
        }
    }

    fn read_subframe(
        &self,
        r: &mut BitReader,
        shape: SubframeShape,
        state: &mut [ChannelState],
    ) -> Result<Subframe, SyntaxError> {
        let cfg = &self.cfg;
        let SubframeShape {
            channels: channel_indexes,
            len,
        } = shape;
        let num_bands = self.num_bands(len);
        let fill = if r.bit()? {
            let short = r.read(2)?;
            let (count, width) = if short == 0 {
                let w = r.read(4)?;
                ((if w == 0 { 0 } else { r.read(w)? }) + 1, Some(w as u8))
            } else {
                (short, None)
            };
            let bits = (0..count).map(|_| r.bit()).collect::<Result<_, _>>()?;
            Some(match width {
                None => Fill::Short(bits),
                Some(width) => Fill::Long { width, bits },
            })
        } else {
            None
        };
        if r.bit()? {
            return Err(SyntaxError::ReservedBit {
                position: r.position(),
            });
        }
        let groups = self.read_groups(r, channel_indexes.len(), num_bands)?;
        let mut channels: Vec<SubframeChannel> = channel_indexes
            .iter()
            .map(|_| {
                Ok(SubframeChannel {
                    transmit: r.bit()?,
                    vector_count: None,
                    modifier: None,
                    scale: None,
                    coefficients: None,
                })
            })
            .collect::<Result<_, SyntaxError>>()?;
        let quant = if channels.iter().any(|c| c.transmit) {
            let vector_counts = r.bit()?;
            if vector_counts {
                let w = len.div_ceil(4).ilog2() + 1;
                for ch in &mut channels {
                    let count = (r.read(w)? as usize) << 2;
                    if count > len {
                        return Err(SyntaxError::VectorCount {
                            count,
                            len,
                            position: r.position(),
                        });
                    }
                    ch.vector_count = Some(count);
                }
            }
            let mut step = cfg.quant_base();
            let raw = r.read(6)? as i32;
            let delta = if raw >= 32 { raw - 64 } else { raw };
            step += delta;
            if delta == -32 || delta == 31 {
                let mut sum = 0;
                let mut last = r.read(5)? as i32;
                while last == 31 {
                    sum += 31;
                    last = r.read(5)? as i32;
                }
                step += if delta == 31 {
                    sum + last
                } else {
                    -(sum + last)
                };
            }
            let modifier_len = if channel_indexes.len() > 1 {
                let ml = r.read(3)? as u8;
                for ch in &mut channels {
                    if r.bit()? {
                        ch.modifier = Some(if ml > 0 { r.read(u32::from(ml))? } else { 0 });
                    }
                }
                Some(ml)
            } else {
                None
            };
            for (ch, c) in channels.iter_mut().zip(&channel_indexes) {
                ch.scale = Some(self.read_scale(r, num_bands, &mut state[c.0])?);
            }
            Some(Quant {
                vector_counts,
                step,
                modifier_len,
            })
        } else {
            None
        };
        for ch in &mut channels {
            if ch.transmit {
                ch.coefficients = Some(self.read_coefficients(r, len, ch.vector_count)?);
            }
        }
        for c in &channel_indexes {
            state[c.0].next += 1;
        }
        Ok(Subframe {
            channel_indexes,
            len,
            fill,
            groups,
            channels,
            quant,
        })
    }

    fn read_groups(
        &self,
        r: &mut BitReader,
        count: usize,
        num_bands: usize,
    ) -> Result<Vec<Group>, SyntaxError> {
        if self.cfg.channels == 1 {
            return Ok(Vec::new());
        }
        if r.bit()? {
            return Err(SyntaxError::TransformBit {
                position: r.position(),
            });
        }
        let mut grouped = vec![false; count];
        let mut remaining = count;
        let mut groups = Vec::new();
        while remaining > 0 && groups.len() < count {
            let mut members = Vec::new();
            if remaining > 2 {
                for (i, g) in grouped.iter_mut().enumerate() {
                    if !*g && r.bit()? {
                        *g = true;
                        members.push(Position(i));
                    }
                }
            } else {
                for (i, g) in grouped.iter_mut().enumerate() {
                    if !*g {
                        *g = true;
                        members.push(Position(i));
                    }
                }
            }
            let transform = match members.len() {
                2 => {
                    if r.bit()? {
                        if r.bit()? {
                            return Err(SyntaxError::TransformType {
                                position: r.position(),
                            });
                        }
                        Transform::PairNone
                    } else {
                        Transform::PairMidSide
                    }
                }
                n if n > 2 => {
                    if r.bit()? {
                        if r.bit()? {
                            let rotations = (0..n * (n - 1) / 2)
                                .map(|_| r.read(6).map(|v| v as u8))
                                .collect::<Result<_, _>>()?;
                            let signs = (0..n).map(|_| r.bit()).collect::<Result<_, _>>()?;
                            Transform::Matrix { rotations, signs }
                        } else {
                            Transform::Default
                        }
                    } else {
                        Transform::None
                    }
                }
                _ => Transform::Single,
            };
            let bands = if transform.active() && !r.bit()? {
                Some((0..num_bands).map(|_| r.bit()).collect::<Result<_, _>>()?)
            } else {
                None
            };
            remaining -= members.len();
            groups.push(Group {
                members,
                transform,
                bands,
            });
        }
        Ok(groups)
    }

    fn write_groups(&self, w: &mut BitWriter, count: usize, groups: &[Group]) {
        if self.cfg.channels == 1 {
            return;
        }
        w.bit(false);
        let mut grouped = vec![false; count];
        let mut remaining = count;
        for g in groups {
            if remaining > 2 {
                for (i, done) in grouped.iter_mut().enumerate() {
                    if !*done {
                        let has = g.members.contains(&Position(i));
                        w.bit(has);
                        *done = has;
                    }
                }
            } else {
                for p in &g.members {
                    grouped[p.0] = true;
                }
            }
            match &g.transform {
                Transform::PairNone => w.write(2, 0b10),
                Transform::PairMidSide => w.bit(false),
                Transform::None => w.bit(false),
                Transform::Default => w.write(2, 0b10),
                Transform::Matrix { rotations, signs } => {
                    w.write(2, 0b11);
                    for &v in rotations {
                        w.write(6, u32::from(v));
                    }
                    for &s in signs {
                        w.bit(s);
                    }
                }
                Transform::Single => {}
            }
            if g.transform.active() {
                match &g.bands {
                    None => w.bit(true),
                    Some(b) => {
                        w.bit(false);
                        for &x in b {
                            w.bit(x);
                        }
                    }
                }
            }
            remaining -= g.members.len();
        }
    }

    fn read_scale(
        &self,
        r: &mut BitReader,
        num_bands: usize,
        state: &mut ChannelState,
    ) -> Result<ScaleFactors, SyntaxError> {
        use wma_tables::pro as t;
        if state.next > 0 && !r.bit()? {
            return Ok(ScaleFactors::Kept);
        }
        let s = if !state.coded {
            let step = r.read(2)? as u8 + 1;
            let deltas = (0..num_bands)
                .map(|_| self.t.scale.get(r))
                .collect::<Result<_, _>>()?;
            ScaleFactors::Dpcm { step, deltas }
        } else {
            let mut changes = Vec::new();
            let mut i = 0;
            while i < num_bands {
                let idx = self.t.scale_rl.get(r)?;
                let rl = match idx {
                    0 => {
                        let code = r.read(14)?;
                        RunLevel {
                            skip: (code & 0x3f) as usize >> 1,
                            magnitude: (code >> 6) as i32,
                            positive: code & 1 == 1,
                        }
                    }
                    1 => break,
                    _ => RunLevel {
                        skip: usize::from(t::SCALE_RL_RUN[idx as usize]),
                        magnitude: i32::from(t::SCALE_RL_LEVEL[idx as usize]),
                        positive: r.bit()?,
                    },
                };
                i += rl.skip;
                if i >= num_bands {
                    return Err(SyntaxError::ScaleBand {
                        position: r.position(),
                    });
                }
                changes.push(ScaleChange {
                    band: i,
                    delta: if rl.positive {
                        rl.magnitude
                    } else {
                        -rl.magnitude
                    },
                });
                i += 1;
            }
            ScaleFactors::Changed(changes)
        };
        state.coded = true;
        Ok(s)
    }

    fn write_scale(&self, w: &mut BitWriter, num_bands: usize, first: bool, s: &ScaleFactors) {
        match s {
            ScaleFactors::Kept => w.bit(false),
            ScaleFactors::Dpcm { step, deltas } => {
                if !first {
                    w.bit(true);
                }
                w.write(2, u32::from(*step - 1));
                for &d in deltas {
                    self.t.scale.put(w, d);
                }
            }
            ScaleFactors::Changed(changes) => {
                if !first {
                    w.bit(true);
                }
                let mut i = 0;
                for c in changes {
                    let skip = c.band - i;
                    let magnitude = c.delta.unsigned_abs();
                    match scale_rl_code(skip, magnitude) {
                        Some(k) => {
                            self.t.scale_rl.put(w, k as i32);
                            w.bit(c.delta > 0);
                        }
                        None => {
                            self.t.scale_rl.put(w, 0);
                            w.write(
                                14,
                                magnitude << 6 | (skip as u32) << 1 | u32::from(c.delta > 0),
                            );
                        }
                    }
                    i = c.band + 1;
                }
                if i < num_bands {
                    self.t.scale_rl.put(w, 1);
                }
            }
        }
    }

    /// decode_coeffs: vectors of four, two and one magnitudes with signs,
    /// then run-level codes.
    fn read_coefficients(
        &self,
        r: &mut BitReader,
        len: usize,
        vector_count: Option<usize>,
    ) -> Result<Coefficients, SyntaxError> {
        let table = if r.bit()? {
            CoefTable::Second
        } else {
            CoefTable::First
        };
        let mut values = vec![0i32; len];
        let num_vec = vector_count.unwrap_or(len);
        let mut cur = 0;
        let mut zeros = 0;
        let mut rl_mode = false;
        while (vector_count.is_some() || !rl_mode) && cur + 3 < num_vec {
            let mut vals = [0u32; 4];
            let idx = self.t.vec4.get(r)?;
            if idx < 0 {
                for i in [0, 2] {
                    let idx = self.t.vec2.get(r)?;
                    if idx < 0 {
                        for v in &mut vals[i..i + 2] {
                            *v = self.t.vec1.get(r)? as u32;
                            if *v == 100 {
                                *v += get_large(r)?;
                            }
                        }
                    } else {
                        vals[i] = (idx >> 4) as u32;
                        vals[i + 1] = (idx & 0xf) as u32;
                    }
                }
            } else {
                for (i, v) in vals.iter_mut().enumerate() {
                    *v = (idx >> (12 - 4 * i) & 0xf) as u32;
                }
            }
            for v in vals {
                if v != 0 {
                    let positive = r.bit()?;
                    values[cur] = if positive { v as i32 } else { -(v as i32) };
                    zeros = 0;
                } else {
                    zeros += 1;
                    rl_mode |= zeros > len >> 8;
                }
                cur += 1;
            }
        }
        if cur < len {
            let (runs, levels) = run_level(table);
            let esc = esc_len(len);
            let coef = self.coef_vlc(table);
            let mut offset = cur;
            while offset < len {
                let code = coef.get(r)?;
                let (level, run) = match code {
                    1 => break,
                    0 => {
                        let level = get_large(r)?;
                        let run = if r.bit()? {
                            if r.bit()? {
                                if r.bit()? {
                                    return Err(SyntaxError::EscapeRun {
                                        position: r.position(),
                                    });
                                }
                                r.read(esc)? as usize + 4
                            } else {
                                r.read(2)? as usize + 1
                            }
                        } else {
                            0
                        };
                        (level, run)
                    }
                    _ => (
                        levels[code as usize] as u32,
                        usize::from(runs[code as usize]),
                    ),
                };
                offset += run;
                if offset >= len {
                    return Err(SyntaxError::RunPastSubframe {
                        position: r.position(),
                    });
                }
                let positive = r.bit()?;
                values[offset] = if positive {
                    level as i32
                } else {
                    -(level as i32)
                };
                offset += 1;
            }
        }
        Ok(Coefficients { table, values })
    }

    fn coef_vlc(&self, table: CoefTable) -> &super::vlc::Vlc {
        match table {
            CoefTable::First => &self.t.coef[0],
            CoefTable::Second => &self.t.coef[1],
        }
    }

    fn write_coefficients(&self, w: &mut BitWriter, c: &Coefficients, vector_count: Option<usize>) {
        let len = c.values.len();
        w.bit(c.table == CoefTable::Second);
        let num_vec = vector_count.unwrap_or(len);
        let mut cur = 0;
        let mut zeros = 0;
        let mut rl_mode = false;
        let mag = |v: i32| v.unsigned_abs();
        while (vector_count.is_some() || !rl_mode) && cur + 3 < num_vec {
            let q = [0, 1, 2, 3].map(|i| mag(c.values[cur + i]));
            let sym4 = q
                .iter()
                .all(|&v| v < 16)
                .then(|| (q[0] << 12 | q[1] << 8 | q[2] << 4 | q[3]) as i32);
            match sym4.filter(|&s| self.t.vec4.has(s)) {
                Some(s) => self.t.vec4.put(w, s),
                None => {
                    self.t.vec4.put(w, -1);
                    for pair in [&q[0..2], &q[2..4]] {
                        let sym2 = pair
                            .iter()
                            .all(|&v| v < 16)
                            .then(|| (pair[0] << 4 | pair[1]) as i32);
                        match sym2.filter(|&s| self.t.vec2.has(s)) {
                            Some(s) => self.t.vec2.put(w, s),
                            None => {
                                self.t.vec2.put(w, -1);
                                for &v in pair {
                                    if v < 100 {
                                        self.t.vec1.put(w, v as i32);
                                    } else {
                                        self.t.vec1.put(w, 100);
                                        put_large(w, v - 100);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            for i in 0..4 {
                let v = c.values[cur + i];
                if v != 0 {
                    w.bit(v > 0);
                    zeros = 0;
                } else {
                    zeros += 1;
                    rl_mode |= zeros > len >> 8;
                }
            }
            cur += 4;
        }
        if cur < len {
            let (runs, levels) = run_level(c.table);
            let coef = self.coef_vlc(c.table);
            let esc = esc_len(len);
            let mut run = 0usize;
            let last = c.values.iter().rposition(|&v| v != 0);
            for (i, &v) in c.values.iter().enumerate().skip(cur) {
                if v == 0 {
                    run += 1;
                    continue;
                }
                let level = mag(v);
                let code = (2..runs.len())
                    .find(|&k| usize::from(runs[k]) == run && levels[k] as u32 == level);
                match code {
                    Some(k) => coef.put(w, k as i32),
                    None => {
                        coef.put(w, 0);
                        put_large(w, level);
                        match run {
                            0 => w.bit(false),
                            1..=4 => {
                                w.write(2, 0b10);
                                w.write(2, run as u32 - 1);
                            }
                            _ => {
                                w.write(3, 0b110);
                                w.write(esc, run as u32 - 4);
                            }
                        }
                    }
                }
                w.bit(v > 0);
                run = 0;
                if Some(i) == last {
                    break;
                }
            }
            if last.is_none_or(|l| l < cur || l + 1 < len) {
                coef.put(w, 1);
            }
        }
    }

    /// Whether the bitstream carries `f` and the reader gives it back.
    pub fn check(&self, f: &Frame) -> Result<(), WriteError> {
        let cfg = &self.cfg;
        let n = cfg.channels;
        if f.tiling.0.len() != n {
            return Err(WriteError::TilingLength {
                channel: f.tiling.0.len().min(n),
                total: 0,
            });
        }
        for (c, t) in f.tiling.0.iter().enumerate() {
            let total: usize = t.iter().sum();
            if total != cfg.frame_len {
                return Err(WriteError::TilingLength { channel: c, total });
            }
            if let Some(&len) = t
                .iter()
                .find(|&&l| l < cfg.min_subframe_len || !l.is_power_of_two() || l > cfg.frame_len)
            {
                return Err(WriteError::SubframeSize { channel: c, len });
            }
        }
        if f.same_tiling && f.tiling.0.iter().any(|t| *t != f.tiling.0[0])
            || cfg.max_subframes == 1 && !f.same_tiling
        {
            return Err(WriteError::TilingNotShared);
        }
        match &f.postproc {
            PostProc::Matrix(m) if m.len() != n * n || m.iter().any(|&v| v > 15) => {
                return Err(WriteError::PostProc { len: m.len() });
            }
            PostProc::Flag | PostProc::Matrix(_) if n == 1 => {
                return Err(WriteError::PostProc { len: 0 });
            }
            _ => {}
        }
        if f.drc_gain.is_some() != cfg.header.decode_flags.drc() {
            return Err(WriteError::DrcGain {
                present: f.drc_gain.is_some(),
            });
        }
        if let Some(t) = f.trim {
            let width = (cfg.frame_len * 2).ilog2();
            for value in [t.start, t.end].into_iter().flatten() {
                if value >> width != 0 {
                    return Err(WriteError::Trim { value, width });
                }
            }
        }
        let mut state = new_state(n);
        let mut subframes = f.subframes.iter();
        let mut index = 0;
        while let Some(shape) = next_subframe(&f.tiling, &mut state) {
            let s = subframes
                .next()
                .filter(|s| s.channel_indexes == shape.channels && s.len == shape.len)
                .ok_or(WriteError::SubframeOrder { subframe: index })?;
            self.check_subframe(s, &mut state)
                .map_err(|what| WriteError::Subframe {
                    subframe: index,
                    what,
                })?;
            index += 1;
        }
        if subframes.next().is_some() {
            return Err(WriteError::SubframeOrder { subframe: index });
        }
        Ok(())
    }

    fn check_subframe(
        &self,
        s: &Subframe,
        state: &mut [ChannelState],
    ) -> Result<(), SubframeError> {
        use SubframeError as E;
        let count = s.channel_indexes.len();
        let num_bands = self.num_bands(s.len);
        if s.channels.len() != count {
            return Err(E::ChannelCount);
        }
        match &s.fill {
            Some(Fill::Short(b)) if b.is_empty() || b.len() > 3 => return Err(E::Fill),
            Some(Fill::Long { width, bits }) => {
                let c = bits.len().checked_sub(1).ok_or(E::Fill)? as u64;
                if *width > 15 || c >> *width != 0 {
                    return Err(E::Fill);
                }
            }
            _ => {}
        }
        if self.cfg.channels == 1 {
            if !s.groups.is_empty() {
                return Err(E::Groups);
            }
        } else {
            // The groups take the channels as the reader assigns them.
            let mut grouped = vec![false; count];
            let mut remaining = count;
            for (i, g) in s.groups.iter().enumerate() {
                if remaining == 0 || i >= count {
                    return Err(E::Groups);
                }
                let expected: Vec<Position> = if remaining > 2 {
                    let mut m = g.members.clone();
                    m.sort();
                    m.dedup();
                    if m != g.members || m.iter().any(|p| p.0 >= count || grouped[p.0]) {
                        return Err(E::Groups);
                    }
                    m
                } else {
                    (0..count).filter(|&p| !grouped[p]).map(Position).collect()
                };
                if expected != g.members {
                    return Err(E::Groups);
                }
                for p in &g.members {
                    grouped[p.0] = true;
                }
                let k = g.members.len();
                let fits = match &g.transform {
                    Transform::PairNone | Transform::PairMidSide => k == 2,
                    Transform::None | Transform::Default => k > 2,
                    Transform::Matrix { rotations, signs } => {
                        k > 2
                            && rotations.len() == k * (k - 1) / 2
                            && rotations.iter().all(|&r| r < 64)
                            && signs.len() == k
                    }
                    Transform::Single => k < 2,
                };
                if !fits {
                    return Err(E::Transform);
                }
                match &g.bands {
                    Some(b) if !g.transform.active() || b.len() != num_bands => {
                        return Err(E::TransformBands);
                    }
                    _ => {}
                }
                remaining -= k;
            }
            if remaining > 0 {
                return Err(E::Groups);
            }
        }
        let transmit = s.channels.iter().any(|c| c.transmit);
        if transmit != s.quant.is_some() {
            return Err(E::Quant);
        }
        if let Some(q) = &s.quant
            && (q.modifier_len.is_some() != (count > 1) || q.modifier_len.is_some_and(|m| m > 7))
        {
            return Err(E::Modifier);
        }
        for (ch, c) in s.channels.iter().zip(&s.channel_indexes) {
            let st = &mut state[c.0];
            match &s.quant {
                None => {
                    if ch.vector_count.is_some() || ch.modifier.is_some() || ch.scale.is_some() {
                        return Err(E::Quant);
                    }
                }
                Some(q) => {
                    match ch.vector_count {
                        Some(v) if !q.vector_counts || v > s.len || v % 4 != 0 => {
                            return Err(E::VectorCount);
                        }
                        None if q.vector_counts => return Err(E::VectorCount),
                        _ => {}
                    }
                    match (ch.modifier, q.modifier_len) {
                        (Some(_), None) => return Err(E::Modifier),
                        (Some(m), Some(0)) if m != 0 => return Err(E::Modifier),
                        (Some(m), Some(l)) if l > 0 && m >> l != 0 => return Err(E::Modifier),
                        _ => {}
                    }
                    match &ch.scale {
                        None => return Err(E::ScaleOrder),
                        Some(ScaleFactors::Dpcm { step, deltas }) => {
                            if st.coded {
                                return Err(E::ScaleOrder);
                            }
                            if !(1..=4).contains(step)
                                || deltas.len() != num_bands
                                || deltas.iter().any(|&d| !self.t.scale.has(d))
                            {
                                return Err(E::ScaleDelta);
                            }
                        }
                        Some(ScaleFactors::Kept) => {
                            if !st.coded || st.next == 0 {
                                return Err(E::ScaleOrder);
                            }
                        }
                        Some(ScaleFactors::Changed(changes)) => {
                            if !st.coded {
                                return Err(E::ScaleOrder);
                            }
                            let mut i = 0;
                            for c in changes {
                                if c.band < i || c.band >= num_bands {
                                    return Err(E::ScaleChange);
                                }
                                let skip = c.band - i;
                                let m = c.delta.unsigned_abs();
                                let escaped = skip <= MAX_ESCAPED_SKIP && m <= MAX_ESCAPED_SCALE;
                                if scale_rl_code(skip, m).is_none() && !escaped {
                                    return Err(E::ScaleChange);
                                }
                                i = c.band + 1;
                            }
                        }
                    }
                    st.coded = true;
                }
            }
            match &ch.coefficients {
                Some(co) => {
                    if !ch.transmit
                        || co.values.len() != s.len
                        || co.values.iter().any(|v| v.unsigned_abs() > MAX_LARGE)
                    {
                        return Err(E::Coefficients);
                    }
                }
                None if ch.transmit => return Err(E::Coefficients),
                None => {}
            }
        }
        for c in &s.channel_indexes {
            state[c.0].next += 1;
        }
        Ok(())
    }

    /// The bits of a frame from its length prefix to its trailer bit.
    pub fn write(&self, f: &Frame, trailer: bool) -> Result<Bits, WriteError> {
        self.check(f)?;
        let cfg = &self.cfg;
        let mut w = BitWriter::new();
        self.write_tiling(&mut w, &f.tiling, f.same_tiling);
        if cfg.channels > 1 {
            match &f.postproc {
                PostProc::Absent => w.bit(false),
                PostProc::Flag => w.write(2, 0b10),
                PostProc::Matrix(m) => {
                    w.write(2, 0b11);
                    for &v in m {
                        w.write(4, u32::from(v));
                    }
                }
            }
        }
        if let Some(g) = f.drc_gain {
            w.write(8, u32::from(g));
        }
        match f.trim {
            None => w.bit(false),
            Some(t) => {
                w.bit(true);
                let width = (cfg.frame_len * 2).ilog2();
                for v in [t.start, t.end] {
                    match v {
                        Some(v) => {
                            w.bit(true);
                            w.write(width, v);
                        }
                        None => w.bit(false),
                    }
                }
            }
        }
        let mut next = vec![0usize; cfg.channels];
        for s in &f.subframes {
            self.write_subframe(&mut w, s, &next);
            for c in &s.channel_indexes {
                next[c.0] += 1;
            }
        }
        let mut body = writer_bits(w);
        // The bit FFmpeg skips before the trailer.
        body.push(true);
        let mut out = Vec::with_capacity(body.len() + 32);
        if cfg.header.decode_flags.len_prefix() {
            let total = cfg.log2_frame_size as usize + body.len() + 1;
            let max = (1usize << cfg.log2_frame_size) - 1;
            if total > max {
                return Err(WriteError::FrameTooLong { bits: total, max });
            }
            for i in (0..cfg.log2_frame_size).rev() {
                out.push(total >> i & 1 == 1);
            }
        }
        out.extend(body);
        out.push(trailer);
        Ok(out)
    }

    fn write_subframe(&self, w: &mut BitWriter, s: &Subframe, next: &[usize]) {
        let cfg = &self.cfg;
        let num_bands = self.num_bands(s.len);
        match &s.fill {
            None => w.bit(false),
            Some(Fill::Short(bits)) => {
                w.bit(true);
                w.write(2, bits.len() as u32);
                for &b in bits {
                    w.bit(b);
                }
            }
            Some(Fill::Long { width, bits }) => {
                w.bit(true);
                w.write(2, 0);
                w.write(4, u32::from(*width));
                if *width > 0 {
                    w.write(u32::from(*width), bits.len() as u32 - 1);
                }
                for &b in bits {
                    w.bit(b);
                }
            }
        }
        w.bit(false);
        self.write_groups(w, s.channel_indexes.len(), &s.groups);
        for ch in &s.channels {
            w.bit(ch.transmit);
        }
        if let Some(q) = &s.quant {
            w.bit(q.vector_counts);
            if q.vector_counts {
                let width = s.len.div_ceil(4).ilog2() + 1;
                for ch in &s.channels {
                    w.write(
                        width,
                        (ch.vector_count.expect("check requires counts") >> 2) as u32,
                    );
                }
            }
            let delta = q.step - cfg.quant_base();
            match delta {
                -31..=30 => w.write(6, (delta & 0x3f) as u32),
                _ => {
                    let rest = if delta > 0 {
                        w.write(6, 31);
                        delta - 31
                    } else {
                        w.write(6, 0b10_0000);
                        -32 - delta
                    };
                    for _ in 0..rest / 31 {
                        w.write(5, 31);
                    }
                    w.write(5, (rest % 31) as u32);
                }
            }
            if let Some(ml) = q.modifier_len {
                w.write(3, u32::from(ml));
                for ch in &s.channels {
                    match ch.modifier {
                        None => w.bit(false),
                        Some(m) => {
                            w.bit(true);
                            if ml > 0 {
                                w.write(u32::from(ml), m);
                            }
                        }
                    }
                }
            }
            for (ch, c) in s.channels.iter().zip(&s.channel_indexes) {
                self.write_scale(
                    w,
                    num_bands,
                    next[c.0] == 0,
                    ch.scale.as_ref().expect("check requires factors"),
                );
            }
        }
        for ch in &s.channels {
            if let Some(c) = &ch.coefficients {
                self.write_coefficients(w, c, ch.vector_count);
            }
        }
    }
}

fn run_level(table: CoefTable) -> (&'static [u16], &'static [f32]) {
    use wma_tables::pro as t;
    match table {
        CoefTable::First => (&t::COEF0_RUN, &t::COEF0_LEVEL),
        CoefTable::Second => (&t::COEF1_RUN, &t::COEF1_LEVEL),
    }
}

#[cfg(test)]
mod tests;
