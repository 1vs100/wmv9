//! WMA Pro encoding in the layout of the SDK 11 samples: frames of one
//! block, or of shorter blocks around attacks, mid/side stereo, packets of
//! eight frames' worth of bytes.
//!
//! Frame `k` codes the MDCT of frames `k` and `k + 1` of the input
//! preceded by one frame of silence. Decoders drop the output of frame 0
//! (FFmpeg's skip_frame; Microsoft's streams trim it), so the output starts
//! with the input, and the last frame trims what lies past its end.

use std::f64::consts::PI;

use super::config::{Config, ConfigError, DecodeFlags, Header};
use super::packets::{LayoutError, Sequence, join, layout};
use super::syntax::{
    Bits, ChannelIndex, Codec, CoefTable, Coefficients, Frame, Group, Position, PostProc, Quant,
    ScaleChange, ScaleFactors, Subframe, SubframeChannel, Tiling, Transform, Trim, WriteError,
    pack, unpack,
};
use crate::asf::mux::AudioFormat as AsfAudioFormat;
use crate::asf::{Hns, WaveFormatEx};
use crate::wma::mdct::Mdct;
use crate::wma::{ByteRate, Channels, Format, FormatError};

/// WAVE_FORMAT_WMAUDIO3.
pub const FORMAT_TAG: u16 = 0x0162;

/// Length prefix and DRC gain, as in the samples' flags (0xe0).
const DECODE_FLAGS_BASE: u16 = 0xc0;
/// Subframes per frame at most, as a power of two: the samples' 16, fewer
/// where frames are short, as a subframe has at least 64 samples.
const LOG2_MAX_SUBFRAMES: u32 = 4;
const MIN_SUBFRAME_LEN: usize = 64;
const BITS_PER_SAMPLE: u16 = 16;
/// Frames a packet's size is reckoned on: 5945 bytes at 128 kbit/s and
/// 2973 at 64 kbit/s, 44.1 kHz, as in the samples.
const FRAMES_PER_PACKET: u64 = 8;
/// Decibels per scale factor unit.
const SCALE_STEP: u8 = 2;
/// How far the quantizer step follows band energy on a logarithmic
/// scale, as in the WMA 9 Standard encoder.
const ENERGY_WEIGHT: f64 = 0.25;
/// Largest scale factor difference between neighbouring bands the DPCM
/// code carries.
const MAX_SCALE_DELTA: i32 = 60;
/// Decoders start DPCM scale factors at 45 / step.
const DPCM_START: i32 = 45 / SCALE_STEP as i32;
/// Attack detection over segments of the shortest block: a segment whose
/// high-passed energy exceeds `ATTACK_RATIO` times the mean of the
/// `ATTACK_HISTORY` segments before it (10 dB) and `ATTACK_FLOOR` per
/// sample (an amplitude of about 100) holds an attack.
const ATTACK_RATIO: f64 = 10.0;
const ATTACK_HISTORY: usize = 4;
const ATTACK_FLOOR: f64 = 1e4;
/// Bit target of a frame of several blocks, in frame budgets; the balance
/// takes the rest from the frames after it. On the hvideo15_1 soundtrack
/// at 128 kbit/s 1.5 gives 0.3 dB more than 1, and 2 gives 1.2 dB less.
const SWITCHED_SHARE: f64 = 1.5;
/// Quantizer steps the search covers, in decibels. Microsoft's decoder
/// outputs nothing for frames of steps of -4 dB and below, which FFmpeg
/// decodes; the search stops at 0.
const STEPS: std::ops::RangeInclusive<i32> = 0..=200;
/// Bits the running balance may run ahead of or behind the budget, in
/// packets: after silence the encoder may spend at most this much more.
const BALANCE_PACKETS: f64 = 2.0;
/// Sample rates of the encoder: those of the WMA 9 Standard rate tables.
const SAMPLE_RATES: std::ops::RangeInclusive<u32> = 8000..=48000;
/// Smallest packet: a length prefix of 10 bits holds the coarsest frame.
const MIN_BLOCK_ALIGN: u64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error("{samples} samples do not make whole sample frames of {channels} channels")]
    PartialSampleFrame { samples: usize, channels: usize },
}

/// A frame by its place in the stream, from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FrameIndex(pub u64);

/// A packet and the frame its time comes from.
pub struct Packet {
    pub data: Vec<u8>,
    /// The first frame that starts in the packet, or, when none does, the
    /// next frame to start.
    pub frame: FrameIndex,
}

/// A coded frame, its bits packed.
struct PackedFrame {
    bytes: Vec<u8>,
    bits: usize,
}

/// Per segment of the shortest block length in a frame: whether it holds
/// an attack.
struct Attacks(Vec<bool>);

/// Per segment of the shortest block length in a frame: whether it is
/// coded in a block of that length.
struct ShortSegments(Vec<bool>);

/// A block of a frame: its length, the coded channels' spectra (mid and
/// side for stereo) and the scale factors they share.
struct Block {
    len: usize,
    spectra: Vec<Vec<f64>>,
    scale: Vec<i32>,
}

pub struct Encoder {
    format: Format,
    codec: Codec,
    /// Per block size index (frame_len >> index).
    mdcts: Vec<Mdct>,
    /// Length of the last block coded.
    prev_block: usize,
    /// Attack segments of the next two frames. A frame's last block
    /// overlaps the next frame's first, and an attack at the start of a
    /// frame makes the end of the frame before it short, so a frame's
    /// blocks are final with the attacks of the frame after it known.
    ahead: std::collections::VecDeque<Attacks>,
    /// Scaled input not yet coded, per channel, from one frame of silence.
    pending: Vec<Vec<f64>>,
    frames: Vec<PackedFrame>,
    /// Input sample frames taken.
    taken: u64,
    /// Bits coded beyond the frame budget so far, within
    /// `BALANCE_PACKETS` packets.
    balance: f64,
}

impl Encoder {
    pub fn new(format: Format) -> Result<Self, EncodeError> {
        let channel_mask = match format.channels {
            // Front centre.
            Channels::Mono => 0x4,
            Channels::Stereo => 0x3,
        };
        if !SAMPLE_RATES.contains(&format.sample_rate.0) {
            return Err(FormatError::SampleRate {
                rate: format.sample_rate,
            }
            .into());
        }
        let mut header = Header {
            bits_per_sample: BITS_PER_SAMPLE,
            channel_mask,
            reserved: [0; 8],
            decode_flags: DecodeFlags(DECODE_FLAGS_BASE),
            advanced: 0,
        };
        let rate = u64::from(format.sample_rate.0);
        // Frame length first, from a provisional packet size and one
        // subframe.
        let n = Config::new(header, 1 << 12, format.sample_rate.0)?.frame_len;
        let log2_max = LOG2_MAX_SUBFRAMES.min((n / MIN_SUBFRAME_LEN).ilog2());
        header.decode_flags = DecodeFlags(DECODE_FLAGS_BASE | (log2_max as u16) << 3);
        // block_align = byte_rate * 8 frames * n / rate, within
        // MIN_BLOCK_ALIGN and what a WAVEFORMATEX holds.
        let per_byte = FRAMES_PER_PACKET * n as u64;
        let min = (MIN_BLOCK_ALIGN * rate).div_ceil(per_byte);
        let max = u64::from(u16::MAX) * rate / per_byte;
        let byte_rate = u64::from(format.byte_rate.0);
        if !(min..=max).contains(&byte_rate) {
            return Err(FormatError::ByteRate {
                rate: format.byte_rate,
                min: ByteRate(min as u32),
                max: ByteRate(max as u32),
            }
            .into());
        }
        let block_align = (byte_rate * per_byte).div_ceil(rate) as usize;
        let cfg = Config::new(header, block_align, format.sample_rate.0)?;
        let channels = format.channels.count();
        Ok(Encoder {
            format,
            mdcts: (0..cfg.bands.len()).map(|i| Mdct::new(n >> i)).collect(),
            codec: Codec::new(&cfg),
            prev_block: n,
            ahead: std::collections::VecDeque::new(),
            pending: vec![vec![0.0; n]; channels],
            frames: Vec::new(),
            taken: 0,
            balance: 0.0,
        })
    }

    pub fn config(&self) -> &Config {
        self.codec.config()
    }

    /// Samples per channel each frame codes.
    pub fn frame_len(&self) -> usize {
        self.config().frame_len
    }

    /// The stream's description for an ASF file. The codec name is the
    /// samples'; their description reads "128 kbps, 44 kHz, 2 channel 24
    /// bit 2-pass VBR" and their conformance template is M1.
    pub fn asf_format(&self) -> AsfAudioFormat {
        let f = &self.format;
        let cfg = self.config();
        let kbps = u64::from(f.byte_rate.0) * 8 / 1000;
        // A packet's duration at the stream's rate.
        let packet = cfg.block_align as u64 * 10_000_000 / u64::from(f.byte_rate.0);
        AsfAudioFormat {
            wave: WaveFormatEx {
                format_tag: FORMAT_TAG,
                channels: f.channels.count() as u16,
                samples_per_sec: f.sample_rate.0,
                avg_bytes_per_sec: f.byte_rate.0,
                block_align: u16::try_from(cfg.block_align).expect("new checks the byte rate"),
                bits_per_sample: BITS_PER_SAMPLE,
                extra: cfg.header.to_bytes().to_vec(),
            },
            average_time_per_object: Hns(packet),
            codec_name: "Windows Media Audio 10 Professional".into(),
            codec_description: format!(
                "{kbps} kbps, {} kHz, {} channel {BITS_PER_SAMPLE} bit 1-pass CBR",
                f.sample_rate.0 / 1000,
                f.channels.count()
            ),
            conformance: "M1".into(),
        }
    }

    /// Takes interleaved samples.
    pub fn push(&mut self, pcm: &[i16]) -> Result<(), EncodeError> {
        let channels = self.format.channels.count();
        if !pcm.len().is_multiple_of(channels) {
            return Err(EncodeError::PartialSampleFrame {
                samples: pcm.len(),
                channels,
            });
        }
        for frame in pcm.chunks(channels) {
            for (ch, &s) in frame.iter().enumerate() {
                self.pending[ch].push(f64::from(s));
            }
        }
        self.taken += (pcm.len() / channels) as u64;
        let n = self.config().frame_len;
        let mut start = 0;
        // A frame takes 2n samples from `start`; finding the attacks of
        // the two frames after it takes 3n / 2 more.
        while self.pending[0].len() - start >= 7 * n / 2 {
            self.next_frame(start, None)?;
            start += n;
        }
        for p in &mut self.pending {
            p.drain(..start);
        }
        Ok(())
    }

    /// Codes the rest, completed with silence, and returns the packets.
    pub fn finish(mut self) -> Result<Vec<Packet>, EncodeError> {
        let n = self.config().frame_len;
        // Output of frame k is input frame k - 1; the last frame needed
        // ends the input.
        let needed = (self.taken as usize).div_ceil(n) + 1;
        let past = (needed - 1) * n - self.taken as usize;
        let mut start = 0;
        while self.frames.len() < needed {
            for p in &mut self.pending {
                p.resize(start + 7 * n / 2, 0.0);
            }
            let last = self.frames.len() + 1 == needed;
            self.next_frame(start, last.then_some(past as u32))?;
            start += n;
        }
        let cfg = self.codec.config().clone();
        let lens: Vec<usize> = self.frames.iter().map(|f| f.bits).collect();
        let lay = layout(&cfg, &lens)?;
        let frames: Vec<Bits> = self
            .frames
            .iter()
            .map(|f| unpack(&f.bytes, f.bits))
            .collect();
        let packets = join(&cfg, &frames, &lay, Sequence(0));
        Ok(packets
            .into_iter()
            .enumerate()
            .map(|(p, data)| Packet {
                data,
                frame: FrameIndex(lay.starts.partition_point(|&s| s < p) as u64),
            })
            .collect())
    }

    /// Codes the frame of pending samples from `start` in the blocks its
    /// attacks and those of the two frames after it give.
    fn next_frame(&mut self, start: usize, trim_end: Option<u32>) -> Result<(), EncodeError> {
        let n = self.config().frame_len;
        while self.ahead.len() < 3 {
            let k = self.ahead.len();
            self.ahead.push_back(self.attacks(start + k * n));
        }
        let current = Self::finish_attacks(&self.ahead[0], &self.ahead[1]);
        let next = Self::finish_attacks(&self.ahead[1], &self.ahead[2]);
        let tiling = self.tiles(&current);
        let next_first = self.tiles(&next)[0];
        self.frame(start, trim_end, &tiling, next_first)?;
        self.ahead.pop_front();
        Ok(())
    }

    /// The short segments of a frame: its attack segments, the segment
    /// before each, and its last segment when the next frame starts with
    /// an attack.
    fn finish_attacks(frame: &Attacks, next: &Attacks) -> ShortSegments {
        let mut short = frame.0.clone();
        for s in 1..frame.0.len() {
            if frame.0[s] {
                short[s - 1] = true;
            }
        }
        if next.0[0] {
            *short.last_mut().expect("a segment") = true;
        }
        ShortSegments(short)
    }

    /// Segments of the shortest block length in the frame whose samples
    /// start at `start` that hold an attack: high-passed energy, summed
    /// over the channels, `ATTACK_RATIO` times the mean of the
    /// `ATTACK_HISTORY` segments before it and above `ATTACK_FLOOR`.
    fn attacks(&self, start: usize) -> Attacks {
        let cfg = self.config();
        let n = cfg.frame_len;
        let seg = cfg.min_subframe_len;
        let core = start + n / 2;
        // Mean energy per sample of the channels' first differences over
        // segment `s` of the frame's core, negative before it. The input
        // starts with a frame of silence, so history before the input's
        // first sample is silence: an empty or shortened range.
        let energy = |s: isize| -> f64 {
            let a = core as isize + s * seg as isize;
            (a.max(1) as usize..(a + seg as isize).max(1) as usize)
                .map(|t| {
                    self.pending
                        .iter()
                        .map(|p| (p[t] - p[t - 1]).powi(2))
                        .sum::<f64>()
                })
                .sum::<f64>()
                / seg as f64
        };
        let mut history: std::collections::VecDeque<f64> =
            (-(ATTACK_HISTORY as isize)..0).map(energy).collect();
        Attacks(
            (0..n / seg)
                .map(|s| {
                    let e = energy(s as isize);
                    let mean = history.iter().sum::<f64>() / ATTACK_HISTORY as f64;
                    history.pop_front();
                    history.push_back(e);
                    e > ATTACK_FLOOR && e > ATTACK_RATIO * mean
                })
                .collect(),
        )
    }

    /// Block lengths for a frame's short segments: the shortest block on
    /// each, and between them the powers of two that fill the gap, growing
    /// away from the short segments before them.
    fn tiles(&self, short: &ShortSegments) -> Vec<usize> {
        let seg = self.config().min_subframe_len;
        let short = &short.0;
        let mut out = Vec::new();
        let mut s = 0;
        while s < short.len() {
            if short[s] {
                out.push(seg);
                s += 1;
                continue;
            }
            let end = (s..short.len()).find(|&i| short[i]).unwrap_or(short.len());
            let run = end - s;
            let mut sizes: Vec<usize> = (0..usize::BITS)
                .rev()
                .map(|b| 1usize << b)
                .filter(|&b| run & b != 0)
                .map(|b| b * seg)
                .collect();
            if s > 0 {
                sizes.reverse();
            }
            out.extend(sizes);
            s = end;
        }
        out
    }

    /// The window of a block of `len` samples over its 2 * `len` input
    /// samples, with `left` and `right` samples of overlap with its
    /// neighbours: a sine slope over each overlap centred on the block's
    /// edge, 1 between, 0 outside, as FFmpeg's decoder applies it.
    fn window(len: usize, left: usize, right: usize) -> Vec<f64> {
        let slope =
            |overlap: usize, i: usize| ((i as f64 + 0.5) * PI / (2.0 * overlap as f64)).sin();
        (0..2 * len)
            .map(|i| {
                let rise = len / 2 - left / 2;
                let fall = len / 2 + len - right / 2;
                if i < rise {
                    0.0
                } else if i < rise + left {
                    slope(left, i - rise)
                } else if i < fall {
                    1.0
                } else if i < fall + right {
                    slope(right, fall + right - 1 - i)
                } else {
                    0.0
                }
            })
            .collect()
    }

    /// Codes the frame of pending samples from `start`, in blocks of
    /// `tiling`, the next frame starting with a block of `next_first`.
    fn frame(
        &mut self,
        start: usize,
        trim_end: Option<u32>,
        tiling: &[usize],
        next_first: usize,
    ) -> Result<(), EncodeError> {
        let cfg = self.codec.config().clone();
        let n = cfg.frame_len;
        let index = self.frames.len();
        let mut blocks = Vec::with_capacity(tiling.len());
        let mut offset = 0;
        let mut prev = self.prev_block;
        for (j, &len) in tiling.iter().enumerate() {
            let next = tiling.get(j + 1).copied().unwrap_or(next_first);
            let window = Self::window(len, prev.min(len), len.min(next));
            // The block's middle len samples follow the previous block's.
            let base = start + n / 2 + offset - len / 2;
            let size = cfg.size_index(len);
            let mut spectra: Vec<Vec<f64>> = self
                .pending
                .iter()
                .map(|p| {
                    let input: Vec<f64> = window
                        .iter()
                        .enumerate()
                        .map(|(i, w)| p[base + i] * w)
                        .collect();
                    self.mdcts[size]
                        .forward(&input)
                        .into_iter()
                        .map(|c| -c)
                        .collect()
                })
                .collect();
            if spectra.len() == 2 {
                let (l, r) = spectra.split_at_mut(1);
                for (a, b) in l[0].iter_mut().zip(r[0].iter_mut()) {
                    let (m, s) = ((*a + *b) * 0.5, (*b - *a) * 0.5);
                    *a = m;
                    *b = s;
                }
            }
            let scale = Self::scale_factors(&spectra, &cfg.bands[size]);
            blocks.push(Block {
                len,
                spectra,
                scale,
            });
            prev = len;
            offset += len;
        }
        self.prev_block = prev;
        let packet_bits = cfg.block_align as f64 * 8.0;
        let budget = packet_bits / FRAMES_PER_PACKET as f64;
        let share = if tiling.len() > 1 {
            SWITCHED_SHARE
        } else {
            1.0
        };
        let target =
            (share * budget - self.balance / 4.0).clamp(budget / 4.0, 2.0 * share * budget);
        let max = packet_bits - 6.0 - f64::from(cfg.log2_frame_size);
        let trim = match (index, trim_end) {
            (0, end) => Some(Trim {
                start: Some(n as u32),
                end,
            }),
            (_, Some(end)) => Some(Trim {
                start: None,
                end: Some(end),
            }),
            _ => None,
        };
        // The frame at `step`, or `None` when it is longer than its
        // length prefix holds.
        let code = |step: i32| -> Result<Option<Bits>, EncodeError> {
            let f = self.model(&blocks, step, trim);
            match self.codec.write(&f, false) {
                Ok(bits) => Ok(Some(bits)),
                Err(WriteError::FrameTooLong { .. }) => Ok(None),
                Err(e) => Err(e.into()),
            }
        };
        // The finest step whose frame fits the target: the size falls as
        // the step rises.
        let (mut lo, mut hi) = (*STEPS.start(), *STEPS.end());
        let mut best = code(hi)?.expect("the coarsest step codes no coefficients");
        while lo < hi {
            let mid = (lo + hi).div_euclid(2);
            match code(mid)? {
                Some(bits) if bits.len() as f64 <= target.min(max) => {
                    best = bits;
                    hi = mid;
                }
                _ => lo = mid + 1,
            }
        }
        let limit = BALANCE_PACKETS * packet_bits;
        self.balance = (self.balance + best.len() as f64 - budget).clamp(-limit, limit);
        self.frames.push(PackedFrame {
            bytes: pack(&best),
            bits: best.len(),
        });
        Ok(())
    }

    /// Scale factors shared by the coded channels: band energy with weight
    /// `ENERGY_WEIGHT`, in units of `SCALE_STEP` decibels, differences
    /// between bands, and of the first from the DPCM start, limited to
    /// what DPCM carries.
    fn scale_factors(spectra: &[Vec<f64>], bands: &[usize]) -> Vec<i32> {
        let mut out = Vec::with_capacity(bands.len() - 1);
        let mut last = DPCM_START;
        for w in bands.windows(2) {
            let energy: f64 = spectra
                .iter()
                .map(|s| s[w[0]..w[1]].iter().map(|c| c * c).sum::<f64>())
                .sum::<f64>()
                / ((w[1] - w[0]) * spectra.len()) as f64;
            // A band without energy codes no levels; its factor only has
            // to stay within DPCM's reach.
            let db = if energy > 0.0 {
                10.0 * energy.log10() * ENERGY_WEIGHT
            } else {
                0.0
            };
            let sf = ((db / f64::from(SCALE_STEP)).round() as i32)
                .clamp(last - MAX_SCALE_DELTA, last + MAX_SCALE_DELTA);
            out.push(sf);
            last = sf;
        }
        out
    }

    /// The frame at quantizer `step` (decibels). A channel's first scale
    /// factors in the frame are DPCM coded; later ones as changes to the
    /// last coded factors, resampled to the block's bands as decoders
    /// resample them.
    fn model(&self, blocks: &[Block], step: i32, trim: Option<Trim>) -> Frame {
        let cfg = self.codec.config();
        let channels = blocks[0].spectra.len();
        // The last coded factors and their block size index.
        let mut saved: Option<(Vec<i32>, usize)> = None;
        let subframes = blocks
            .iter()
            .map(|b| {
                let size = cfg.size_index(b.len);
                let bands = &cfg.bands[size];
                let max_sf = *b.scale.iter().max().expect("a band");
                // Decoders scale a block's output by 2 / len, so a step
                // adds noise in inverse proportion to the block's length:
                // shorter blocks take finer steps, by 3 dB per halving
                // (Microsoft's streams: 512, 256 and 128 blocks 4.5, 7.6
                // and 11.1 dB below the longest block of their frame).
                let finer = (10.0 * (cfg.frame_len as f64 / b.len as f64).log10()).round() as i32;
                let step = (step - finer).max(*STEPS.start());
                let values: Vec<Vec<i32>> = b
                    .spectra
                    .iter()
                    .map(|s| {
                        let mut v = vec![0; b.len];
                        for (band, w) in bands.windows(2).enumerate() {
                            let exp = step - (max_sf - b.scale[band]) * i32::from(SCALE_STEP);
                            let q = 10f64.powf(f64::from(exp) / 20.0);
                            for k in w[0]..w[1] {
                                v[k] = (s[k] / q).round() as i32;
                            }
                        }
                        v
                    })
                    .collect();
                let transmit: Vec<bool> =
                    values.iter().map(|v| v.iter().any(|&x| x != 0)).collect();
                let coded = transmit.iter().any(|&t| t);
                let scale = coded.then(|| match &saved {
                    None => ScaleFactors::Dpcm {
                        step: SCALE_STEP,
                        deltas: std::iter::once(b.scale[0] - DPCM_START)
                            .chain(b.scale.windows(2).map(|w| w[1] - w[0]))
                            .collect(),
                    },
                    Some((prev, prev_size)) => {
                        let changes: Vec<ScaleChange> = (0..bands.len() - 1)
                            .filter_map(|band| {
                                let resampled = prev[cfg.resample[size][*prev_size][band]];
                                let delta = b.scale[band] - resampled;
                                (delta != 0).then_some(ScaleChange { band, delta })
                            })
                            .collect();
                        if changes.is_empty() {
                            ScaleFactors::Kept
                        } else {
                            ScaleFactors::Changed(changes)
                        }
                    }
                });
                // Decoders keep the factors they last received; kept
                // factors are those, resampled.
                if matches!(
                    scale,
                    Some(ScaleFactors::Dpcm { .. } | ScaleFactors::Changed(_))
                ) {
                    saved = Some((b.scale.clone(), size));
                }
                let sub_channels = values
                    .into_iter()
                    .zip(&transmit)
                    .map(|(v, &t)| SubframeChannel {
                        transmit: t,
                        vector_count: None,
                        modifier: None,
                        scale: scale.clone(),
                        coefficients: t.then_some(Coefficients {
                            table: CoefTable::First,
                            values: v,
                        }),
                    })
                    .collect();
                Subframe {
                    channel_indexes: (0..channels).map(ChannelIndex).collect(),
                    len: b.len,
                    fill: None,
                    groups: if channels == 2 {
                        vec![Group {
                            members: vec![Position(0), Position(1)],
                            transform: Transform::PairMidSide,
                            bands: None,
                        }]
                    } else {
                        Vec::new()
                    },
                    channels: sub_channels,
                    quant: coded.then(|| Quant {
                        vector_counts: false,
                        step,
                        modifier_len: (channels > 1).then_some(0),
                    }),
                }
            })
            .collect();
        let tiling = blocks.iter().map(|b| b.len).collect::<Vec<_>>();
        Frame {
            tiling: Tiling(vec![tiling; channels]),
            same_tiling: true,
            postproc: PostProc::Absent,
            drc_gain: Some(0),
            trim,
            subframes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wma::SampleRate;
    use crate::wma::pro::packets::split;

    fn stereo_128k() -> Format {
        Format {
            sample_rate: SampleRate(44100),
            channels: Channels::Stereo,
            byte_rate: ByteRate(16000),
        }
    }

    /// Interleaved stereo noise of `frames` sample frames at `amplitude`.
    fn noise(frames: usize, amplitude: i32, seed: &mut u32) -> Vec<i16> {
        (0..2 * frames)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 16) as i32 % (2 * amplitude + 1) - amplitude) as i16
            })
            .collect()
    }

    /// The frames of an encoded stream, read back, with the packet each
    /// starts in.
    fn read_back(cfg: &Config, packets: &[Packet]) -> Vec<(Frame, usize)> {
        let refs: Vec<&[u8]> = packets.iter().map(|p| p.data.as_slice()).collect();
        let codec = Codec::new(cfg);
        split(cfg, &refs)
            .unwrap()
            .frames
            .iter()
            .map(|f| (codec.read(&f.bits).unwrap().0, f.packet))
            .collect()
    }

    fn encode(format: Format, pcm: &[i16]) -> (Config, Vec<Packet>) {
        let mut e = Encoder::new(format).unwrap();
        e.push(pcm).unwrap();
        let cfg = e.config().clone();
        (cfg, e.finish().unwrap())
    }

    /// Frame 0 trims the frame decoders drop, the last frame what lies
    /// past the input, and packets carry the first frame that starts in
    /// them, or the next one.
    #[test]
    fn trims_and_packet_times() {
        let mut seed = 1;
        // 3 s and 1000 samples: the last frame ends 2048 - 1000 % 2048
        // samples past the input.
        let len = 3 * 44100 + 1000;
        let (cfg, packets) = encode(stereo_128k(), &noise(len, 3000, &mut seed));
        let frames = read_back(&cfg, &packets);
        let n = cfg.frame_len;
        assert_eq!(frames.len(), len.div_ceil(n) + 1);
        let past = (frames.len() - 1) * n - len;
        for (i, (f, _)) in frames.iter().enumerate() {
            let expected = match i {
                0 => Some(Trim {
                    start: Some(n as u32),
                    end: None,
                }),
                _ if i + 1 == frames.len() => Some(Trim {
                    start: None,
                    end: Some(past as u32),
                }),
                _ => None,
            };
            assert_eq!(f.trim, expected, "frame {i}");
        }
        for (p, packet) in packets.iter().enumerate() {
            let next = frames
                .iter()
                .position(|(_, start)| *start >= p)
                .unwrap_or(frames.len());
            assert_eq!(packet.frame, FrameIndex(next as u64), "packet {p}");
        }
        assert!(packets.windows(2).all(|w| w[0].frame < w[1].frame));
    }

    /// Microsoft's decoder outputs nothing for frames of steps of -4 dB
    /// and below; quiet input keeps to steps it decodes.
    #[test]
    fn quiet_input_keeps_steps_the_decoder_takes() {
        let mut seed = 2;
        let (cfg, packets) = encode(stereo_128k(), &noise(44100, 1, &mut seed));
        for (f, _) in read_back(&cfg, &packets) {
            for s in &f.subframes {
                if let Some(q) = &s.quant {
                    assert!(q.step >= 0, "step {}", q.step);
                }
            }
        }
    }

    /// After 30 s of silence, 10 s of noise take at most two packets more
    /// than their share, and the packet their end shares with the padding.
    #[test]
    fn silence_does_not_bank_bits() {
        let mut seed = 3;
        let mut pcm = vec![0i16; 2 * 30 * 44100];
        pcm.extend(noise(10 * 44100, 20000, &mut seed));
        let (cfg, packets) = encode(stereo_128k(), &pcm);
        let frames = read_back(&cfg, &packets);
        let first_noise = 30 * 44100 / cfg.frame_len + 2;
        let start = frames[first_noise].1;
        let used = packets.len() - start;
        let share = (frames.len() - first_noise).div_ceil(FRAMES_PER_PACKET as usize);
        assert!(used <= share + 3, "{used} packets for {share}");
    }

    #[test]
    fn formats_outside_the_tables_are_refused() {
        let at = |rate: u32, bytes: u32| {
            Encoder::new(Format {
                sample_rate: SampleRate(rate),
                channels: Channels::Stereo,
                byte_rate: ByteRate(bytes),
            })
            .err()
        };
        assert_eq!(
            at(96000, 16000),
            Some(EncodeError::Format(FormatError::SampleRate {
                rate: SampleRate(96000)
            }))
        );
        // block_align from 64 to 65535 bytes: 8 frames of 2048 samples
        // at 44.1 kHz take 0.37 s.
        assert!(matches!(
            at(44100, 1),
            Some(EncodeError::Format(FormatError::ByteRate { .. }))
        ));
        assert!(matches!(
            at(44100, 200_000),
            Some(EncodeError::Format(FormatError::ByteRate { .. }))
        ));
        // 16 kHz frames of 512 samples take at most 8 subframes.
        assert!(at(16000, 4000).is_none());
    }

    /// Interleaved stereo of a 440 Hz tone at `amplitude`, with an
    /// impulse of 20000 at sample frame `click`, if any.
    fn tone(frames: usize, amplitude: f64, click: Option<usize>) -> Vec<i16> {
        (0..frames)
            .flat_map(|i| {
                let t = amplitude * (2.0 * PI * 440.0 * i as f64 / 44100.0).sin();
                let v = if Some(i) == click { 20000.0 } else { t };
                let v = v.round() as i16;
                [v, v]
            })
            .collect()
    }

    /// A steady tone keeps one block per frame; a click takes the shortest
    /// block where it is, and the blocks of every frame fill the frame.
    #[test]
    fn blocks_follow_attacks() {
        let n = 2048;
        let (cfg, packets) = encode(stereo_128k(), &tone(20 * n, 3000.0, None));
        let frames = read_back(&cfg, &packets);
        // The tone starts from silence in frame 1; past that, one block.
        for (i, (f, _)) in frames.iter().enumerate().skip(3) {
            assert_eq!(f.tiling.0[0], [n], "frame {i}");
        }

        // In segment 14 of its frame, where the blocks before an unmarked
        // segment 13 would end in 256.
        let click = 10 * n + 818;
        let (cfg, packets) = encode(stereo_128k(), &tone(20 * n, 300.0, Some(click)));
        let frames = read_back(&cfg, &packets);
        for (f, _) in &frames {
            assert_eq!(f.tiling.0[0].iter().sum::<usize>(), n);
        }
        // Input sample frame t is sample t + n after the frame of silence,
        // in the block cores of frame (t + n / 2) / n, at (t + n / 2) % n.
        let frame = (click + n / 2) / n;
        let at = (click + n / 2) % n;
        let tiles = &frames[frame].0.tiling.0[0];
        let mut start = 0;
        let at_click = tiles
            .iter()
            .position(|&l| {
                start += l;
                start > at
            })
            .unwrap();
        assert_eq!(tiles[at_click], cfg.min_subframe_len, "tiling {tiles:?}");
        // The block before the click's is short too, and shorter blocks
        // take finer steps, 3 dB per halving.
        assert!(
            at_click > 0 && tiles[at_click - 1] == cfg.min_subframe_len,
            "tiling {tiles:?}"
        );
        let steps: Vec<(usize, i32)> = frames[frame]
            .0
            .subframes
            .iter()
            .filter_map(|s| s.quant.as_ref().map(|q| (s.len, q.step)))
            .collect();
        // Decibels finer than a block of the frame's length.
        let finer = |l: usize| match n / l {
            1 => 0,
            2 => 3,
            4 => 6,
            8 => 9,
            16 => 12,
            r => panic!("block of 1/{r} frame"),
        };
        let (long, long_step) = *steps.iter().max_by_key(|s| s.0).unwrap();
        for &(l, step) in &steps {
            assert_eq!(
                step,
                (long_step + finer(long) - finer(l)).max(0),
                "steps {steps:?}"
            );
        }
    }

    /// Tilings for short segments: the shortest block on each, between
    /// them powers of two descending at the frame's start and ascending
    /// after a short block.
    #[test]
    fn tiles_grow_away_from_short_blocks() {
        let encoder = Encoder::new(stereo_128k()).unwrap();
        let tiles = |short: &[usize]| {
            let mut mask = vec![false; 16];
            for &s in short {
                mask[s] = true;
            }
            encoder.tiles(&ShortSegments(mask))
        };
        assert_eq!(tiles(&[]), [2048]);
        assert_eq!(tiles(&[3]), [256, 128, 128, 512, 1024]);
        assert_eq!(tiles(&[0]), [128, 128, 256, 512, 1024]);
        assert_eq!(tiles(&[15]), [1024, 512, 256, 128, 128]);
        assert_eq!(tiles(&[6, 7, 10]), [512, 256, 128, 128, 256, 128, 128, 512]);
    }

    /// Inputs without attacks keep one block per frame: steady noise,
    /// noise whose level steps by 6 dB every 1024 samples (below the
    /// 10 dB of an attack), and clicks of 30 in silence (below the floor).
    #[test]
    fn inputs_without_attacks_keep_long_blocks() {
        let n = 2048;
        let mut seed = 11u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (f64::from(seed >> 8) / f64::from(1u32 << 24) - 0.5) * 2.0
        };
        // A sample from its index and a uniform random value in [-1, 1).
        type Signal = fn(usize, f64) -> f64;
        let inputs: [(&str, Signal); 4] = [
            ("noise 300", |_, r| 300.0 * r),
            ("noise 3000", |_, r| 3000.0 * r),
            (
                "stepped noise",
                |i, r| if i / 1024 % 2 == 0 { 300.0 } else { 600.0 } * r,
            ),
            ("quiet clicks", |i, _| if i % 700 == 0 { 30.0 } else { 0.0 }),
        ];
        for (name, signal) in inputs {
            let pcm: Vec<i16> = (0..20 * n)
                .flat_map(|i| {
                    let v = signal(i, noise()).round() as i16;
                    [v, v]
                })
                .collect();
            let (cfg, packets) = encode(stereo_128k(), &pcm);
            // Frames 0 and 1 hold the start from silence.
            for (i, (f, _)) in read_back(&cfg, &packets).iter().enumerate().skip(2) {
                assert_eq!(f.tiling.0[0], [n], "{name}, frame {i}");
            }
        }
    }
}
