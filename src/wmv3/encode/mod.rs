//! WMV3 encoding. The encoder chooses syntax and fills the bitstream model;
//! the model's writer produces the bytes and the decoder in `decode`
//! produces the reference pictures, so the encoder predicts from exactly
//! what this crate's decoder reconstructs.

mod inter;
mod intra;
mod rate;
mod transform;

pub use intra::encode_intra_picture;
pub use rate::{FrameRate, RateError, RateTarget};

use std::num::NonZeroU32;

use super::decode::{DecodeError, Decoder, Picture};
use super::{Bounded, Frame, FrameContext, FrameError, PqIndex, SequenceHeader};
use rate::{Attempt, Bits, Kind, RateControl};

/// A picture in planar YUV 4:2:0 at its display size. Chroma planes are
/// `width.div_ceil(2)` by `height.div_ceil(2)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Yuv420 {
    pub width: u16,
    pub height: u16,
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
}

/// Which plane of a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneKind {
    Luma,
    Cb,
    Cr,
}

/// Sequence settings the encoder does not produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeFeature {
    /// DQUANT other than 0: per-macroblock quantizers.
    Dquant,
    /// RES_FASTTX clear: the forward transform inverts only the fast
    /// transform.
    SlowTransform,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error("{0:?} is not supported by the encoder")]
    Unsupported(EncodeFeature),
    #[error("source is {source_width}x{source_height}, the sequence is {width}x{height}")]
    SizeMismatch {
        source_width: u16,
        source_height: u16,
        width: u16,
        height: u16,
    },
    #[error("{plane:?} plane holds {found} samples, {expected} expected")]
    PlaneLength {
        plane: PlaneKind,
        expected: usize,
        found: usize,
    },
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// Reconstructing an encoded picture failed, which means the encoder
    /// produced syntax its own decoder rejects.
    #[error("reconstructing the encoded picture failed: {0}")]
    Reconstruct(#[from] DecodeError),
    /// An earlier picture failed to reconstruct.
    #[error("the encoder stopped at an earlier error")]
    Failed,
}

/// How the encoder chooses the quantizer of each picture.
#[derive(Debug, Clone, Copy)]
pub enum Quantizer {
    Fixed(PqIndex),
    Rate(RateTarget),
}

/// Fixed encoder settings.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub quantizer: Quantizer,
    /// Pictures from one I picture to the next.
    pub key_interval: NonZeroU32,
}

/// A picture coded at one quantizer, not yet the reference.
struct Coded {
    frame: Frame,
    kind: Kind,
    /// Encoder's reconstruction of a P picture before the loop filter.
    model: Option<Picture>,
}

/// Encodes pictures in order as I and P pictures, keeping the decoder's
/// reconstruction of each as the reference for the next.
/// The quantizer, or the rate control that chooses it.
enum Quantizing {
    Fixed(PqIndex),
    Rate(RateControl),
}

pub struct Encoder<'a> {
    ctx: FrameContext<'a>,
    settings: Settings,
    quantizing: Quantizing,
    decoder: Decoder<'a>,
    count: u32,
    /// Pictures since the last I picture.
    since_key: u32,
    /// Set when reconstructing a picture failed: the decoder state no
    /// longer follows the stream, so no further picture can be encoded.
    failed: bool,
}

impl<'a> Encoder<'a> {
    pub fn new(ctx: FrameContext<'a>, settings: Settings) -> Result<Self, EncodeError> {
        check_sequence(&ctx)?;
        ctx.check_size()?;
        Ok(Encoder {
            ctx,
            settings,
            quantizing: match settings.quantizer {
                Quantizer::Fixed(pq) => Quantizing::Fixed(pq),
                Quantizer::Rate(target) => {
                    Quantizing::Rate(RateControl::new(&target, ctx.seq.quantizer))
                }
            },
            decoder: Decoder::new(ctx),
            count: 0,
            since_key: 0,
            failed: false,
        })
    }

    pub fn encode(&mut self, src: &Yuv420) -> Result<Frame, EncodeError> {
        if self.failed {
            return Err(EncodeError::Failed);
        }
        if (src.width, src.height) != (self.ctx.width, self.ctx.height) {
            return Err(EncodeError::SizeMismatch {
                source_width: src.width,
                source_height: src.height,
                width: self.ctx.width,
                height: self.ctx.height,
            });
        }
        src.check()?;
        let key = self.since_key >= self.settings.key_interval.get();
        let source = Source {
            ctx: &self.ctx,
            decoder: &self.decoder,
            count: self.count,
            src,
        };
        let coded = match &mut self.quantizing {
            Quantizing::Fixed(pq) => source.code(*pq, key)?,
            Quantizing::Rate(rate) => source.code_to_rate(rate, key)?,
        };
        self.since_key = match coded.kind {
            Kind::Intra => 1,
            Kind::Inter => self.since_key + 1,
        };
        let decoded = self
            .decoder
            .decode(&coded.frame)
            .inspect_err(|_| self.failed = true)?;
        // The loop filter acts after the encoder's decisions; without it
        // the decoder must arrive at the encoder's reconstruction.
        if let (Some(model), Some(decoded)) = (coded.model, decoded)
            && !self.ctx.seq.loop_filter
        {
            assert!(
                model == decoded,
                "picture {} decodes differently from what the encoder reconstructed",
                self.count
            );
        }
        self.count += 1;
        Ok(coded.frame)
    }

    /// Pictures under rate control that the bucket did not hold even at
    /// PQINDEX 31: the bit rate is too low for the content there. `None`
    /// with a fixed quantizer.
    pub fn buffer_overflows(&self) -> Option<u64> {
        match &self.quantizing {
            Quantizing::Fixed(_) => None,
            Quantizing::Rate(rate) => Some(rate.overflows()),
        }
    }
}

/// A source picture with what coding it reads from the encoder.
struct Source<'s, 'a> {
    ctx: &'s FrameContext<'a>,
    decoder: &'s Decoder<'a>,
    /// Pictures coded before this one.
    count: u32,
    src: &'s Yuv420,
}

impl Source<'_, '_> {
    /// Codes the picture at `pq`, as a P picture unless `intra`, there is
    /// no reference, or it is a scene cut.
    fn code(&self, pq: PqIndex, intra: bool) -> Result<Coded, EncodeError> {
        let framecnt = Bounded::new((self.count % 4) as u8).expect("below 4");
        if let Some(reference) = self.decoder.reference()
            && !intra
        {
            let p = inter::encode_p_picture(
                self.src,
                reference,
                self.decoder.next_p_rnd(),
                self.ctx,
                pq,
                framecnt,
            )?;
            // A scene cut: most of the picture has nothing to predict
            // from, and an I picture codes it with fewer bits.
            if p.intra_macroblocks * 2 <= self.ctx.mb_dims().count() {
                return Ok(Coded {
                    frame: p.frame,
                    kind: Kind::Inter,
                    model: Some(p.model),
                });
            }
        }
        Ok(Coded {
            frame: encode_intra_picture(self.src, self.ctx, pq, framecnt)?,
            kind: Kind::Intra,
            model: None,
        })
    }

    /// Codes the picture at the quantizer rate control chooses, and again
    /// when its size shows the choice was wrong. Once an attempt is a
    /// scene cut, the later ones are I pictures, since whether a picture
    /// is a cut depends on the quantizer.
    fn code_to_rate(&self, rate: &mut RateControl, key: bool) -> Result<Coded, EncodeError> {
        let expected = if key || self.decoder.reference().is_none() {
            Kind::Intra
        } else {
            Kind::Inter
        };
        let mut pq = rate.choose(expected);
        let mut intra = key;
        let mut first_attempt = true;
        loop {
            let coded = self.code(pq, intra)?;
            let attempt = Attempt {
                expected,
                kind: coded.kind,
                pq,
                bits: Bits(coded.frame.to_bytes(self.ctx)?.len() * 8),
            };
            match rate.retry(&attempt, first_attempt) {
                Some(again) => pq = again,
                None => {
                    rate.commit(&attempt);
                    return Ok(coded);
                }
            }
            intra |= coded.kind == Kind::Intra;
            first_attempt = false;
        }
    }
}

fn check_sequence(ctx: &FrameContext) -> Result<(), EncodeError> {
    if ctx.seq.dquant.get() != 0 {
        return Err(EncodeError::Unsupported(EncodeFeature::Dquant));
    }
    if !ctx.seq.res_fasttx {
        return Err(EncodeError::Unsupported(EncodeFeature::SlowTransform));
    }
    Ok(())
}

impl Yuv420 {
    fn check(&self) -> Result<(), EncodeError> {
        let (w, h) = (usize::from(self.width), usize::from(self.height));
        let chroma = w.div_ceil(2) * h.div_ceil(2);
        for (plane, data, expected) in [
            (PlaneKind::Luma, &self.y, w * h),
            (PlaneKind::Cb, &self.cb, chroma),
            (PlaneKind::Cr, &self.cr, chroma),
        ] {
            if data.len() != expected {
                return Err(EncodeError::PlaneLength {
                    plane,
                    expected,
                    found: data.len(),
                });
            }
        }
        Ok(())
    }

    /// Sample of a plane with the edge replicated beyond the display size,
    /// which fills the macroblock-aligned coded area.
    fn sample(&self, plane: PlaneKind, x: usize, y: usize) -> u8 {
        let (w, h) = (usize::from(self.width), usize::from(self.height));
        let data = match plane {
            PlaneKind::Luma => &self.y,
            PlaneKind::Cb => &self.cb,
            PlaneKind::Cr => &self.cr,
        };
        let (pw, ph) = match plane {
            PlaneKind::Luma => (w, h),
            PlaneKind::Cb | PlaneKind::Cr => (w.div_ceil(2), h.div_ceil(2)),
        };
        data[y.min(ph - 1) * pw + x.min(pw - 1)]
    }
}

/// The sequence header of the SDK 11 samples (STRUCT_C `4e594a11`): Main
/// profile with loop filter, overlap, fast transform, variable-size
/// transform, extended motion vectors, one B picture between references
/// and the implicit quantizer.
pub fn sample_sequence() -> SequenceHeader {
    SequenceHeader::parse(&[0x4e, 0x59, 0x4a, 0x11, 0x00]).expect("the samples' STRUCT_C parses")
}
