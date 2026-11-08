//! Reconstruction of decoded pictures, bit-exact with FFmpeg's VC-1 decoder.

mod b;
pub(crate) mod dsp;
pub(crate) mod intra;
mod itrans;
mod mc;
pub(crate) mod p;

pub use dsp::Plane;
pub use mc::Rnd;

use super::frame::{Frame, FrameBody};
use super::picture::PictureKind;
use super::{FrameContext, FrameError, MbDims};
use b::{BPicture, BReferences, CarriedMvs, DirectVectors};
use p::IntensityLuts;

/// FFmpeg stores the luma blocks of a macroblock as Y0, Y2, Y1, Y3
/// (block_map); the overlap filter's block indices assume that order.
pub(crate) const BLOCK_MAP: [usize; 6] = [0, 2, 1, 3, 4, 5];

/// A decoded picture at macroblock-aligned size. Samples beyond the
/// display size are real reference data for motion compensation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub y: Plane,
    pub cb: Plane,
    pub cr: Plane,
}

impl Picture {
    pub(crate) fn new(dims: MbDims) -> Self {
        let (w, h) = (usize::from(dims.width) * 16, usize::from(dims.height) * 16);
        Picture {
            y: Plane::new(w, h),
            cb: Plane::new(w / 2, h / 2),
            cr: Plane::new(w / 2, h / 2),
        }
    }

    /// Width in macroblocks.
    fn mb_width(&self) -> usize {
        self.y.width / 16
    }

    /// Planar YUV 4:2:0 cropped to the display size, as FFmpeg outputs it.
    pub fn to_yuv420p(&self, width: usize, height: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * 3 / 2);
        let crop = |out: &mut Vec<u8>, p: &Plane, w: usize, h: usize| {
            for row in p.data.chunks(p.width).take(h) {
                out.extend_from_slice(&row[..w]);
            }
        };
        crop(&mut out, &self.y, width, height);
        crop(&mut out, &self.cb, width.div_ceil(2), height.div_ceil(2));
        crop(&mut out, &self.cr, width.div_ceil(2), height.div_ceil(2));
        out
    }
}

/// Chroma plane of block 4 (Cb) or 5 (Cr).
fn chroma_plane(pic: &mut Picture, block: usize) -> &mut Plane {
    if block == 4 { &mut pic.cb } else { &mut pic.cr }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeFeature {
    /// RES_FASTTX clear selects a different inverse transform.
    SlowTransform,
    /// Range reduced pictures (RANGEREDFRM).
    RangeReduction,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("{0:?} is not supported")]
    Unsupported(DecodeFeature),
    #[error("picture has no decoded reference picture")]
    MissingReference,
    #[error(transparent)]
    Frame(#[from] FrameError),
}

/// The most recent reference picture with what B pictures take from it.
struct Anchor {
    picture: Picture,
    direct: DirectVectors,
    luts: Option<IntensityLuts>,
}

/// Stateful reconstruction of pictures in coded order. P pictures predict
/// from the most recent I or P picture, B pictures from the two most
/// recent.
pub struct Decoder<'a> {
    ctx: FrameContext<'a>,
    /// The reference before `newer`, B pictures' forward reference.
    older: Option<Picture>,
    newer: Option<Anchor>,
    /// Rounding of the most recent I, BI or P picture header.
    rnd: Rnd,
    carried: CarriedMvs,
}

impl<'a> Decoder<'a> {
    pub fn new(ctx: FrameContext<'a>) -> Self {
        Decoder {
            ctx,
            older: None,
            newer: None,
            rnd: Rnd::One,
            carried: CarriedMvs::default(),
        }
    }

    /// The picture the next P picture predicts from.
    pub(crate) fn reference(&self) -> Option<&Picture> {
        self.newer.as_ref().map(|a| &a.picture)
    }

    /// The rounding the next P picture uses.
    pub(crate) fn next_p_rnd(&self) -> Rnd {
        self.rnd.toggled()
    }

    fn push_reference(&mut self, anchor: Anchor) {
        self.older = self.newer.take().map(|a| a.picture);
        self.newer = Some(anchor);
    }

    /// Decodes the next frame in coded order. Returns `None` for a B or BI
    /// picture FFmpeg drops because no earlier reference exists yet (see
    /// `display_order`). Rounding follows the picture header even when
    /// reconstruction fails, as in FFmpeg, where it is set while the header
    /// is parsed. References change only on success.
    pub fn decode(&mut self, frame: &Frame) -> Result<Option<Picture>, DecodeError> {
        let ctx = &self.ctx;
        if !ctx.seq.res_fasttx {
            return Err(DecodeError::Unsupported(DecodeFeature::SlowTransform));
        }
        if frame.header.rangeredfrm == Some(true) {
            return Err(DecodeError::Unsupported(DecodeFeature::RangeReduction));
        }
        match &frame.header.kind {
            PictureKind::I { .. } | PictureKind::Bi { .. } => self.rnd = Rnd::One,
            PictureKind::P(_) => self.rnd = self.rnd.toggled(),
            PictureKind::B { .. } => {}
        }
        let non_reference = PictureRole::of(&frame.header.kind) == PictureRole::NonReference;
        if non_reference && self.older.is_none() {
            return Ok(None);
        }
        match (&frame.body, &frame.header.kind) {
            (FrameBody::Intra(layer), PictureKind::I { .. } | PictureKind::Bi { .. }) => {
                let dims = ctx.mb_dims();
                if layer.macroblocks.len() != dims.count() {
                    return Err(DecodeError::Frame(FrameError::MacroblockCount {
                        expected: dims.count(),
                        found: layer.macroblocks.len(),
                    }));
                }
                let picture = intra::decode(&frame.header, layer, ctx);
                if matches!(frame.header.kind, PictureKind::I { .. }) {
                    self.push_reference(Anchor {
                        picture: picture.clone(),
                        direct: DirectVectors::Zero,
                        luts: None,
                    });
                }
                Ok(Some(picture))
            }
            (FrameBody::P(layer), PictureKind::P(header)) => {
                let reference = self.newer.as_ref().ok_or(DecodeError::MissingReference)?;
                let decoded = p::decode(
                    &frame.header,
                    header,
                    layer,
                    ctx,
                    &reference.picture,
                    self.rnd,
                )?;
                let picture = decoded.picture.clone();
                self.carried.forward = decoded.last_forward;
                self.push_reference(Anchor {
                    picture: decoded.picture,
                    direct: DirectVectors::PerMacroblock(decoded.direct),
                    luts: decoded.luts,
                });
                Ok(Some(picture))
            }
            (FrameBody::B(layer), PictureKind::B { bfraction, header }) => {
                let (Some(forward), Some(newer)) = (&self.older, &self.newer) else {
                    return Err(DecodeError::MissingReference);
                };
                let refs = BReferences {
                    forward,
                    backward: &newer.picture,
                    direct: &newer.direct,
                    forward_luts: newer.luts.as_ref(),
                };
                let pic = BPicture {
                    header: &frame.header,
                    bfraction: *bfraction,
                    b: header,
                    layer,
                };
                b::decode(&pic, ctx, &refs, self.rnd, &mut self.carried).map(Some)
            }
            _ => Err(DecodeError::Frame(FrameError::BodyMismatch)),
        }
    }
}

/// Whether a picture is a reference for later pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureRole {
    /// I and P pictures.
    Reference,
    /// B and BI pictures.
    NonReference,
}

impl PictureRole {
    pub fn of(kind: &PictureKind) -> Self {
        match kind {
            PictureKind::I { .. } | PictureKind::P(_) => PictureRole::Reference,
            PictureKind::B { .. } | PictureKind::Bi { .. } => PictureRole::NonReference,
        }
    }
}

/// Display position of each picture given in coded order, or `None` for a
/// picture FFmpeg drops. A reference picture is shown when the next one
/// arrives or the stream ends. B and BI pictures are shown at once, except
/// before the second reference picture, where FFmpeg has no backward
/// reference and drops them ("Skipping B frame without reference frames").
pub fn display_order(roles: impl IntoIterator<Item = PictureRole>) -> Vec<Option<usize>> {
    let mut shown = Vec::new();
    let mut next = 0;
    let mut pending: Option<usize> = None;
    let mut references = 0;
    for (i, role) in roles.into_iter().enumerate() {
        shown.push(None);
        match role {
            PictureRole::Reference => {
                references += 1;
                if let Some(p) = pending.replace(i) {
                    shown[p] = Some(next);
                    next += 1;
                }
            }
            PictureRole::NonReference if references >= 2 => {
                shown[i] = Some(next);
                next += 1;
            }
            PictureRole::NonReference => {}
        }
    }
    if let Some(p) = pending {
        shown[p] = Some(next);
    }
    shown
}
