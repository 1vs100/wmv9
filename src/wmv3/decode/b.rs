//! B picture reconstruction for progressive Simple and Main profile,
//! transcribed from FFmpeg's vc1_decode_b_mb, ff_vc1_pred_b_mv, vc1_b_mc
//! and ff_vc1_interp_mc.

use crate::wmv3::FrameContext;
use crate::wmv3::coeff::CodingSet;
use crate::wmv3::inter::{BLayer, BMbBody, BMotion, BVectorRange, DerivedBMb, Mv};
use crate::wmv3::picture::{BFractionIndex, BHeader, PictureHeader};
use vc1_tables as t;

use super::intra::{Quant, loop_filter_picture};
use super::mc::Rnd;
use super::p::{
    CodedMb, FastUv, IntensityLuts, MbAt, Mc, Write, add_residuals, intra_blocks, put_intra_blocks,
};
use super::{DecodeError, Picture};

/// What direct mode scales: FFmpeg's motion_val[1] of block 0 of each
/// macroblock of the later reference.
pub(super) enum DirectVectors {
    /// An I picture, whose vectors FFmpeg allocates zeroed.
    Zero,
    /// A P picture's vectors in raster order.
    PerMacroblock(Vec<Mv>),
}

impl DirectVectors {
    fn at(&self, i: usize) -> Mv {
        match self {
            DirectVectors::Zero => Mv::default(),
            DirectVectors::PerMacroblock(v) => v[i],
        }
    }
}

/// BFRACTION as a numerator over 256 (B_FRACTION_DEN 256).
#[derive(Clone, Copy)]
struct BFraction(i32);

/// Direction of a direct mode vector.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

/// The two references of a B picture.
pub(super) struct BReferences<'a> {
    /// The earlier reference in display order (FFmpeg's last_pic).
    pub(super) forward: &'a Picture,
    /// The later reference (next_pic).
    pub(super) backward: &'a Picture,
    pub(super) direct: &'a DirectVectors,
    /// Intensity compensation of the later reference when it is a P
    /// picture that signals it. FFmpeg applies it to the forward
    /// prediction (last_luty); backward prediction never uses it.
    pub(super) forward_luts: Option<&'a IntensityLuts>,
}

/// FFmpeg's s->mv[0][0] and s->mv[1][0], which persist from macroblock to
/// macroblock and from picture to picture.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CarriedMvs {
    pub(super) forward: Mv,
    pub(super) backward: Mv,
}

/// scale_mv with B_FRACTION_DEN 256.
fn scale(v: i32, bfraction: BFraction, dir: Direction, quarter_sample: bool) -> i32 {
    let n = match dir {
        Direction::Forward => bfraction.0,
        Direction::Backward => bfraction.0 - 256,
    };
    if quarter_sample {
        (v * n + 128) >> 8
    } else {
        2 * ((v * n + 255) >> 9)
    }
}

fn median3(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

/// Macroblock grid size.
#[derive(Clone, Copy)]
struct Grid {
    width: usize,
    height: usize,
}

/// Direct mode vectors: the later reference's vector scaled by BFRACTION,
/// pulled back to within the picture (8.4.5.4).
fn direct(anchor: Mv, at: MbAt, g: Grid, bfraction: BFraction, qs: bool) -> CarriedMvs {
    let clip = |v: i32, pos: usize, size: usize| {
        let pos = pos as i32;
        v.clamp(-60 - (pos << 6), ((size as i32) << 6) - 4 - (pos << 6))
    };
    let scaled = |dir: Direction| Mv {
        x: clip(scale(anchor.x, bfraction, dir, qs), at.x, g.width),
        y: clip(scale(anchor.y, bfraction, dir, qs), at.y, g.height),
    };
    CarriedMvs {
        forward: scaled(Direction::Forward),
        backward: scaled(Direction::Backward),
    }
}

/// Predicted vector of one direction from the stored vectors of earlier
/// macroblocks, with the Simple/Main pullback (8.3.5.3.4).
fn predict(field: &[Mv], at: MbAt, g: Grid) -> Mv {
    let get = |x: usize, y: usize| field[y * g.width + x];
    let c = if at.x == 0 {
        Mv::default()
    } else {
        get(at.x - 1, at.y)
    };
    let p = if at.y != 0 {
        let a = get(at.x, at.y - 1);
        if g.width == 1 {
            a
        } else {
            let bx = if at.x == g.width - 1 {
                at.x - 1
            } else {
                at.x + 1
            };
            let b = get(bx, at.y - 1);
            Mv {
                x: median3(a.x, b.x, c.x),
                y: median3(a.y, b.y, c.y),
            }
        }
    } else if at.x != 0 {
        c
    } else {
        Mv::default()
    };
    let limit = |v: i32, pos: usize, size: usize| {
        let q = (pos as i32) << 5;
        let max = ((size as i32) << 5) - 4;
        v.clamp(-28 - q, max - q)
    };
    Mv {
        x: limit(p.x, at.x, g.width),
        y: limit(p.y, at.y, g.height),
    }
}

/// Prediction plus differential, wrapped into the vector range.
fn reconstruct(pred: Mv, diff: Mv, range: BVectorRange) -> Mv {
    let wrap = |v: i32, r: i32| ((v + r) & (2 * r - 1)) - r;
    Mv {
        x: wrap(pred.x + diff.x, range.x),
        y: wrap(pred.y + diff.y, range.y),
    }
}

/// Which references a macroblock is predicted from.
enum Prediction {
    None,
    Forward,
    Backward,
    Both,
}

/// The parsed parts of one B picture.
#[derive(Clone, Copy)]
pub(super) struct BPicture<'a> {
    pub(super) header: &'a PictureHeader,
    pub(super) bfraction: BFractionIndex,
    pub(super) b: &'a BHeader,
    pub(super) layer: &'a BLayer,
}

/// Reconstructs a B picture. `carried` holds FFmpeg's vectors left by the
/// previous macroblock, which one degenerate syntax form uses.
pub(super) fn decode(
    pic: &BPicture,
    ctx: &FrameContext,
    refs: &BReferences,
    rnd: Rnd,
    carried: &mut CarriedMvs,
) -> Result<Picture, DecodeError> {
    let BPicture {
        header: h,
        bfraction,
        b,
        layer,
    } = *pic;
    let bfrac = BFraction(i32::from(
        t::VC1_BFRACTION_LUT[usize::from(bfraction.get())],
    ));
    let (derived, range) = layer.derive(h, b, ctx, bfrac.0 >= 128)?;
    let dims = ctx.mb_dims();
    let g = Grid {
        width: usize::from(dims.width),
        height: usize::from(dims.height),
    };
    let qs = b.mvmode_quarter_pel;
    let forward_mc = Mc {
        reference: refs.forward,
        mspel: qs,
        rnd,
        luts: refs.forward_luts,
    };
    let backward_mc = Mc {
        reference: refs.backward,
        mspel: qs,
        rnd,
        luts: None,
    };
    let fast = FastUv::of(ctx.seq.fastuvmc);
    let fast_interp = FastUv::interpolated(ctx.seq.fastuvmc);
    let mut forward_field = vec![Mv::default(); g.width * g.height];
    let mut backward_field = vec![Mv::default(); g.width * g.height];
    let mut picture = refs.backward.clone();
    for (i, d) in derived.iter().enumerate() {
        let at = MbAt::of(i, g.width);
        let anchor = refs.direct.at(i);
        let scaled = || direct(anchor, at, g, bfrac, qs);
        let (stored, prediction) = match d.motion {
            BMotion::Intra => (CarriedMvs::default(), Prediction::None),
            BMotion::StaleInterpolated => (CarriedMvs::default(), Prediction::Both),
            BMotion::Direct => {
                *carried = scaled();
                (*carried, Prediction::Both)
            }
            BMotion::Forward(diff) => {
                *carried = CarriedMvs {
                    forward: reconstruct(predict(&forward_field, at, g), diff, range),
                    ..scaled()
                };
                (*carried, Prediction::Forward)
            }
            BMotion::Backward(diff) => {
                *carried = CarriedMvs {
                    backward: reconstruct(predict(&backward_field, at, g), diff, range),
                    ..scaled()
                };
                (*carried, Prediction::Backward)
            }
            BMotion::Interpolated { forward, backward } => {
                *carried = CarriedMvs {
                    forward: reconstruct(predict(&forward_field, at, g), forward, range),
                    backward: reconstruct(predict(&backward_field, at, g), backward, range),
                };
                (*carried, Prediction::Both)
            }
        };
        forward_field[i] = stored.forward;
        backward_field[i] = stored.backward;
        let mv = *carried;
        match prediction {
            Prediction::None => {}
            Prediction::Forward => {
                forward_mc.one_mv(&mut picture, at, mv.forward, fast, Write::Put)
            }
            Prediction::Backward => {
                backward_mc.one_mv(&mut picture, at, mv.backward, fast, Write::Put)
            }
            Prediction::Both => {
                forward_mc.one_mv(&mut picture, at, mv.forward, fast, Write::Put);
                backward_mc.one_mv(&mut picture, at, mv.backward, fast_interp, Write::Average);
            }
        }
    }
    let mbs = b_coded_mbs(layer, &derived);
    let q = Quant::new(h, ctx);
    let intra = intra_blocks(h, ctx, &mbs, g.width);
    put_intra_blocks(&mut picture, &mbs, &intra);
    add_residuals(&mut picture, &mbs, CodingSet::for_picture(h).inter, &q);
    if ctx.seq.loop_filter {
        loop_filter_picture(&mut picture, dims, q.pq);
    }
    Ok(picture)
}

fn b_coded_mbs<'a>(layer: &'a BLayer, derived: &'a [DerivedBMb]) -> Vec<CodedMb<'a>> {
    layer
        .macroblocks
        .iter()
        .zip(derived)
        .map(|(mb, d)| {
            let intra = matches!(d.motion, BMotion::Intra);
            let (blocks, ac_pred) = match &mb.body {
                BMbBody::SkippedDirect | BMbBody::Skipped { .. } => (None, false),
                BMbBody::Direct(m) => (Some(&m.blocks), false),
                // An intra B macroblock always sends ACPRED.
                BMbBody::Coded(m) => (Some(&m.blocks), m.ac_pred.unwrap_or(false)),
            };
            CodedMb {
                intra: [intra; 6],
                blocks,
                ac_pred,
                layouts: &d.layouts,
            }
        })
        .collect()
}
