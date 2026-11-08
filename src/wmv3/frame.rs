//! One coded WMV3 frame: picture header followed by the macroblock layer.

use super::bits::{BitReader, BitWriter};
use super::inter::{BLayer, InterLayer};
use super::intra::IntraLayer;
use super::picture::{PictureHeader, PictureKind};
use super::{Feature, FrameContext, FrameError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: PictureHeader,
    pub body: FrameBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameBody {
    /// I and BI pictures.
    Intra(IntraLayer),
    /// P pictures.
    P(InterLayer),
    /// B pictures.
    B(BLayer),
}

/// Bits kept verbatim. Equality compares the bits, not their alignment.
#[derive(Debug, Clone)]
pub struct OpaqueBits {
    bytes: Vec<u8>,
    start_bit: u8,
}

impl PartialEq for OpaqueBits {
    fn eq(&self, other: &Self) -> bool {
        self.bit_len() == other.bit_len() && self.bits().eq(other.bits())
    }
}

impl Eq for OpaqueBits {}

impl OpaqueBits {
    fn bits(&self) -> impl Iterator<Item = bool> + '_ {
        let mut r = BitReader::new(&self.bytes);
        r.skip(self.start_bit.into())
            .expect("start bit within first byte");
        std::iter::from_fn(move || r.bit().ok())
    }

    /// Takes everything left in `r`.
    pub(crate) fn rest(r: &mut BitReader) -> Self {
        let (bytes, start_bit) = r.rest_bits();
        OpaqueBits {
            bytes: bytes.to_vec(),
            start_bit,
        }
    }

    /// No bits; a frame written with it ends at its last syntax element,
    /// padded with zero bits to a byte boundary.
    pub fn empty() -> Self {
        OpaqueBits {
            bytes: Vec::new(),
            start_bit: 0,
        }
    }

    pub fn bit_len(&self) -> usize {
        self.bytes.len() * 8 - usize::from(self.start_bit)
    }

    /// True when every bit is zero.
    pub fn is_zero(&self) -> bool {
        self.bits().all(|b| !b)
    }

    pub(crate) fn write(&self, w: &mut BitWriter) {
        for b in self.bits() {
            w.bit(b);
        }
    }
}

/// Coded at reduced resolution, which changes the macroblock count.
fn check_resolution(h: &PictureHeader) -> Result<(), FrameError> {
    match h.respic {
        Some(r) if r.get() != 0 => Err(FrameError::Unsupported(Feature::ReducedResolution)),
        _ => Ok(()),
    }
}

impl Frame {
    pub fn parse(data: &[u8], ctx: &FrameContext) -> Result<Self, FrameError> {
        ctx.check_size()?;
        let mut r = BitReader::new(data);
        let header = PictureHeader::read(&mut r, ctx)?;
        check_resolution(&header)?;
        let body = match header.kind {
            PictureKind::I { .. } | PictureKind::Bi { .. } => {
                FrameBody::Intra(IntraLayer::read(&mut r, &header, ctx)?)
            }
            PictureKind::P(ref p) => FrameBody::P(InterLayer::read(&mut r, &header, p, ctx)?),
            PictureKind::B { header: ref b, .. } => {
                FrameBody::B(BLayer::read(&mut r, &header, b, ctx)?)
            }
        };
        Ok(Frame { header, body })
    }

    pub fn to_bytes(&self, ctx: &FrameContext) -> Result<Vec<u8>, FrameError> {
        ctx.check_size()?;
        let mut w = BitWriter::new();
        check_resolution(&self.header)?;
        self.header.write(&mut w, ctx)?;
        match (&self.body, &self.header.kind) {
            (FrameBody::Intra(layer), PictureKind::I { .. } | PictureKind::Bi { .. }) => {
                layer.write(&mut w, &self.header, ctx)?
            }
            (FrameBody::P(layer), PictureKind::P(p)) => {
                layer.write(&mut w, &self.header, p, ctx)?
            }
            (FrameBody::B(layer), PictureKind::B { header: b, .. }) => {
                layer.write(&mut w, &self.header, b, ctx)?
            }
            _ => return Err(FrameError::BodyMismatch),
        }
        Ok(w.finish())
    }
}
