//! Macroblock layer of progressive P pictures (SMPTE 421M 7.1.3 and 8.3),
//! following FFmpeg's vc1_decode_p_mb. Motion vectors are predicted as in
//! ff_vc1_pred_mv because the HYBRIDPRED bit is present only when the
//! predictors disagree.

use super::bitplane::{Bitplane, PlaneValues};
use super::bits::{BitReader, BitWriter, OutOfBits};
use super::coeff::{
    AcCoeff, CodingSet, CoeffCoder, Fault, IntraBlock, PictureCodingSets, Span, read_coeffs,
    read_intra_block, write_coeffs, write_intra_block,
};
use super::picture::{BHeader, MvMode, PHeader, PictureHeader, Pquant, TransformSignal};
use super::tables::{CBPCY_P, MV_DIFF, SUBBLKPAT, TTBLK, TTMB};
use super::{
    BlockError, BlockFault, Bounded, FrameContext, FrameError, MbDims, MbFault, OpaqueBits,
};

/// MVDATA in coded form: the MVDIFF symbol and the raw bits after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MvData {
    pub symbol: Bounded<72>,
    /// Horizontal extra bits; their width follows from the symbol.
    pub x_bits: u16,
    pub y_bits: u16,
}

/// Motion vector differential in quarter or half pel units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Mv {
    pub x: i32,
    pub y: i32,
}

/// What MVDATA says about a macroblock or block.
#[derive(Clone, Copy)]
struct MvMeaning {
    diff: Mv,
    intra: bool,
    has_coeffs: bool,
}

const MV_ESCAPE: u32 = 35;
const MV_INTRA: u32 = 36;
const MV_OFFSET: [i32; 6] = [0, 1, 3, 7, 15, 31];
const MV_SIZE: [u32; 6] = [0, 2, 3, 4, 5, 8];

/// Motion vector ranges selected by MVRANGE.
#[derive(Clone, Copy)]
struct MvRange {
    k_x: u32,
    k_y: u32,
}

impl MvRange {
    fn new(mvrange: Option<Bounded<3>>) -> Self {
        let r = u32::from(mvrange.map_or(0, |v| v.get()));
        MvRange {
            k_x: r + 9 + (r >> 1),
            k_y: r + 8,
        }
    }

    fn range_x(self) -> i32 {
        1 << (self.k_x - 1)
    }

    fn range_y(self) -> i32 {
        1 << (self.k_y - 1)
    }
}

/// Per-picture motion vector coding parameters.
#[derive(Clone, Copy)]
struct MvCoding {
    table: usize,
    quarter_sample: bool,
    range: MvRange,
}

/// An MVDIFF symbol split into its motion index and coefficient flag.
struct MvIndex {
    index: u32,
    has_coeffs: bool,
}

fn split_index(symbol: Bounded<72>) -> MvIndex {
    let index = u32::from(symbol.get()) + 1;
    if index > 36 {
        MvIndex {
            index: index - 37,
            has_coeffs: true,
        }
    } else {
        MvIndex {
            index,
            has_coeffs: false,
        }
    }
}

/// Widths of the extra bits after an MVDIFF symbol.
struct MvWidths {
    x: u32,
    y: u32,
}

impl MvCoding {
    /// Widths of the horizontal and vertical extra bits.
    fn widths(self, symbol: Bounded<72>) -> MvWidths {
        let index = split_index(symbol).index;
        let q = u32::from(self.quarter_sample);
        match index {
            0 | MV_INTRA => MvWidths { x: 0, y: 0 },
            MV_ESCAPE => MvWidths {
                x: self.range.k_x - 1 + q,
                y: self.range.k_y - 1 + q,
            },
            _ => MvWidths {
                x: self.component_width(index % 6),
                y: self.component_width(index / 6),
            },
        }
    }

    fn component_width(self, i: u32) -> u32 {
        MV_SIZE[i as usize] - u32::from(!self.quarter_sample && i == 5)
    }

    fn meaning(self, d: &MvData) -> MvMeaning {
        let MvIndex { index, has_coeffs } = split_index(d.symbol);
        let MvWidths { x: wx, y: wy } = self.widths(d.symbol);
        let component = |i: u32, width: u32, bits: u16| -> i32 {
            let offset = MV_OFFSET[i as usize];
            if width == 0 {
                return offset;
            }
            let v = i32::from(bits);
            let m = (v >> 1) + offset;
            if v & 1 == 1 { -m } else { m }
        };
        let diff = match index {
            0 | MV_INTRA => Mv::default(),
            MV_ESCAPE => Mv {
                x: d.x_bits.into(),
                y: d.y_bits.into(),
            },
            _ => Mv {
                x: component(index % 6, wx, d.x_bits),
                y: component(index / 6, wy, d.y_bits),
            },
        };
        MvMeaning {
            diff,
            intra: index == MV_INTRA,
            has_coeffs,
        }
    }

    /// MVDATA for a differential in coded units (half samples without
    /// quarter-sample motion), the inverse of `meaning`. `None` for a zero
    /// differential without coefficients, which has no code: such a
    /// macroblock is skipped.
    fn code(self, diff: Mv, has_coeffs: bool) -> Option<MvData> {
        /// A component's size class and the bits that follow it.
        #[derive(Clone, Copy)]
        struct Class {
            index: u32,
            bits: u16,
        }
        let class = |d: i32| -> Option<Class> {
            if d == 0 {
                return Some(Class { index: 0, bits: 0 });
            }
            let m = d.unsigned_abs();
            (1..6u32).find_map(|index| {
                let offset = MV_OFFSET[index as usize] as u32;
                let width = self.component_width(index);
                let v = (m.checked_sub(offset)? << 1) | u32::from(d < 0);
                (v >> width == 0).then_some(Class {
                    index,
                    bits: v as u16,
                })
            })
        };
        let symbol = |index: u32| Bounded::from_field(index + 37 * u32::from(has_coeffs) - 1);
        match (class(diff.x), class(diff.y)) {
            (Some(x), Some(y)) if x.index == 0 && y.index == 0 => has_coeffs.then(|| MvData {
                symbol: symbol(0),
                x_bits: 0,
                y_bits: 0,
            }),
            (Some(x), Some(y)) if x.index + 6 * y.index < MV_ESCAPE => Some(MvData {
                symbol: symbol(x.index + 6 * y.index),
                x_bits: x.bits,
                y_bits: y.bits,
            }),
            _ => {
                let q = u32::from(self.quarter_sample);
                let wrap = |d: i32, w: u32| (d.rem_euclid(1 << w)) as u16;
                Some(MvData {
                    symbol: symbol(MV_ESCAPE),
                    x_bits: wrap(diff.x, self.range.k_x - 1 + q),
                    y_bits: wrap(diff.y, self.range.k_y - 1 + q),
                })
            }
        }
    }

    fn read(self, r: &mut BitReader) -> Result<MvData, FrameError> {
        let symbol = Bounded::from_field(MV_DIFF[self.table].read(r)?.into());
        let MvWidths { x: wx, y: wy } = self.widths(symbol);
        Ok(MvData {
            symbol,
            x_bits: r.read(wx)? as u16,
            y_bits: r.read(wy)? as u16,
        })
    }

    fn write(self, w: &mut BitWriter, d: &MvData) -> Result<(), Fault> {
        let MvWidths { x: wx, y: wy } = self.widths(d.symbol);
        if u32::from(d.x_bits) >> wx != 0 || u32::from(d.y_bits) >> wy != 0 {
            return Err(Fault::Mb(MbFault::MvBits));
        }
        MV_DIFF[self.table].write(w, d.symbol.get().into())?;
        w.write(wx, d.x_bits.into());
        w.write(wy, d.y_bits.into());
        Ok(())
    }
}

/// Motion vectors and intra flags of luma blocks, laid out like FFmpeg's
/// b8 arrays (stride `2 * mb_width + 1`, one border row and column) so that
/// neighbour offsets match exactly.
struct MvField {
    dims: MbDims,
    wrap: isize,
    mv: Vec<Mv>,
    intra: Vec<bool>,
    /// Chroma intra flag per macroblock.
    chroma_intra: Vec<bool>,
}

/// Outcome of motion vector prediction before HYBRIDPRED is applied.
struct Prediction {
    p: Mv,
    a: Mv,
    c: Mv,
    hybrid: bool,
}

impl MvField {
    fn new(dims: MbDims) -> Self {
        let wrap = 2 * dims.width as isize + 1;
        let len = (wrap * (2 * dims.height as isize + 2) + 2) as usize;
        MvField {
            dims,
            wrap,
            mv: vec![Mv::default(); len],
            intra: vec![false; len],
            chroma_intra: vec![false; dims.count()],
        }
    }

    /// Index of luma block n of macroblock (mx, my).
    fn xy(&self, mx: usize, my: usize, n: usize) -> isize {
        self.wrap
            + 1
            + self.wrap * (2 * my as isize + (n >> 1) as isize)
            + 2 * mx as isize
            + (n & 1) as isize
    }

    fn mv_at(&self, i: isize) -> Mv {
        self.mv[i as usize]
    }

    fn intra_at(&self, i: isize) -> bool {
        self.intra[i as usize]
    }

    fn predict(&self, mx: usize, my: usize, n: usize, one_mv: bool, rtm: bool) -> Prediction {
        let mbw = usize::from(self.dims.width);
        let mbh = usize::from(self.dims.height);
        let xy = self.xy(mx, my, n);
        let wrap = self.wrap;
        let a_valid = my != 0 || n == 2 || n == 3;
        let mut b_valid = a_valid;
        let c_valid = mx != 0 || n == 1 || n == 3;
        let off: isize = if one_mv {
            b_valid = b_valid && mbw > 1;
            if mx == mbw - 1 { -1 } else { 2 }
        } else {
            match n {
                0 if rtm => {
                    if mx != 0 {
                        -1
                    } else {
                        1
                    }
                }
                0 => {
                    if mx != 0 {
                        -1
                    } else {
                        2 * mbw as isize - wrap - 1
                    }
                }
                1 => {
                    if mx == mbw - 1 {
                        -1
                    } else {
                        1
                    }
                }
                2 => 1,
                _ => -1,
            }
        };
        let a = if a_valid {
            self.mv_at(xy - wrap)
        } else {
            Mv::default()
        };
        let b = if b_valid {
            self.mv_at(xy - wrap + off)
        } else {
            Mv::default()
        };
        let c = if c_valid {
            self.mv_at(xy - 1)
        } else {
            Mv::default()
        };
        let valid = usize::from(a_valid) + usize::from(b_valid) + usize::from(c_valid);
        let mut p = if a_valid {
            a
        } else if c_valid {
            c
        } else if b_valid {
            b
        } else {
            Mv::default()
        };
        if valid > 1 {
            p = Mv {
                x: median(a.x, b.x, c.x),
                y: median(a.y, b.y, c.y),
            };
        }
        // Pullback (8.3.5.3.4).
        let limit = if one_mv { -60 } else { -28 };
        let qx = ((mx as i32) << 6) + if n == 1 || n == 3 { 32 } else { 0 };
        let qy = ((my as i32) << 6) + if n == 2 || n == 3 { 32 } else { 0 };
        let max_x = ((mbw as i32) << 6) - 4;
        let max_y = ((mbh as i32) << 6) - 4;
        if qx + p.x < limit {
            p.x = limit - qx;
        }
        if qy + p.y < limit {
            p.y = limit - qy;
        }
        if qx + p.x > max_x {
            p.x = max_x - qx;
        }
        if qy + p.y > max_y {
            p.y = max_y - qy;
        }
        // Hybrid prediction (8.3.5.3.5).
        let far = |intra: bool, q: Mv| {
            let sum = if intra {
                p.x.abs() + p.y.abs()
            } else {
                (p.x - q.x).abs() + (p.y - q.y).abs()
            };
            sum > 32
        };
        let hybrid = a_valid
            && c_valid
            && (far(self.intra_at(xy - wrap), a) || far(self.intra_at(xy - 1), c));
        Prediction { p, a, c, hybrid }
    }

    /// Stores the reconstructed vector of block n, or of all four luma
    /// blocks for a 1MV macroblock.
    fn store(&mut self, mx: usize, my: usize, n: usize, one_mv: bool, mv: Mv) {
        let xy = self.xy(mx, my, n);
        let targets: &[isize] = if one_mv {
            &[0, 1, self.wrap, self.wrap + 1]
        } else {
            &[0]
        };
        for &d in targets {
            self.mv[(xy + d) as usize] = mv;
        }
    }

    fn set_intra(&mut self, mx: usize, my: usize, n: usize, v: bool) {
        if n < 4 {
            let i = self.xy(mx, my, n) as usize;
            self.intra[i] = v;
        } else {
            self.chroma_intra[my * usize::from(self.dims.width) + mx] = v;
        }
    }

    /// Whether block n has an intra block above or to its left, which
    /// makes ACPRED present in a 4MV macroblock.
    fn intra_neighbour(&self, mx: usize, my: usize, n: usize) -> bool {
        let mbw = usize::from(self.dims.width);
        if n >= 4 {
            let above = my != 0 && self.chroma_intra[(my - 1) * mbw + mx];
            let left = mx != 0 && self.chroma_intra[my * mbw + mx - 1];
            return above || left;
        }
        let xy = self.xy(mx, my, n);
        let above = (my != 0 || n == 2 || n == 3) && self.intra_at(xy - self.wrap);
        let left = (mx != 0 || n == 1 || n == 3) && self.intra_at(xy - 1);
        above || left
    }
}

fn median(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

/// Applies HYBRIDPRED and the differential, wrapping into the MV range.
fn reconstruct(pred: &Prediction, hybrid: Option<bool>, diff: Mv, c: MvCoding) -> Mv {
    let p = match hybrid {
        Some(true) => pred.a,
        Some(false) => pred.c,
        None => pred.p,
    };
    let scale = if c.quarter_sample { 1 } else { 2 };
    let wrap = |v: i32, r: i32| ((v + r) & (2 * r - 1)) - r;
    Mv {
        x: wrap(p.x + diff.x * scale, c.range.range_x()),
        y: wrap(p.y + diff.y * scale, c.range.range_y()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterLayer {
    pub macroblocks: Vec<PMb>,
    /// Bits after the last macroblock, normally zero padding.
    pub trailing: OpaqueBits,
}

/// What reconstruction needs from one parsed P macroblock beyond its
/// syntax: motion vectors after prediction, intra flags per block (chroma
/// is intra when three or more luma blocks are), and transform layouts of
/// its inter blocks.
pub(crate) struct DerivedPMb {
    pub vectors: [Mv; 4],
    pub intra: [bool; 6],
    pub layouts: [Option<InterLayout>; 6],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PMb {
    /// MVTYPEMB bit when that bitplane is sent raw.
    pub mvtype_bit: Option<bool>,
    /// SKIPMB bit when that bitplane is sent raw.
    pub skip_bit: Option<bool>,
    pub body: PMbBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PMbBody {
    /// Skipped 1MV macroblock, with HYBRIDPRED when its predictors disagree.
    SkippedOneMv {
        hybrid: Option<bool>,
    },
    /// Skipped 4MV macroblock, with HYBRIDPRED per luma block.
    SkippedFourMv {
        hybrid: [Option<bool>; 4],
    },
    OneMv(OneMvMb),
    FourMv(FourMvMb),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneMvMb {
    pub mv: MvData,
    pub hybrid: Option<bool>,
    pub ac_pred: Option<bool>,
    pub cbpcy: Option<Bounded<63>>,
    pub ttmb: Option<Bounded<15>>,
    pub blocks: [PBlock; 6],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FourMvMb {
    pub cbpcy: Bounded<63>,
    pub mvs: [Option<MvData>; 4],
    pub hybrid: [Option<bool>; 4],
    pub ac_pred: Option<bool>,
    pub ttmb: Option<Bounded<15>>,
    pub blocks: [PBlock; 6],
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PBlock {
    #[default]
    Empty,
    Intra(IntraBlock),
    Inter(InterBlock),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterBlock {
    /// TTBLK symbol when the block signals its own transform type.
    pub ttblk: Option<Bounded<7>>,
    /// SUBBLKPAT symbol of a 4x4 transform.
    pub subblkpat: Option<Bounded<14>>,
    /// DECODE012 subblock pattern of an 8x4 or 4x8 transform.
    pub halves: Option<Bounded<2>>,
    /// Coefficients of each coded subblock in order.
    pub subblocks: Vec<Vec<AcCoeff>>,
}

/// Transform types numbered as in FFmpeg's `enum TransformTypes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tt {
    T8x8,
    T8x4Bottom,
    T8x4Top,
    T8x4,
    T4x8Right,
    T4x8Left,
    T4x8,
    T4x4,
}

impl Tt {
    fn from_index(v: u8) -> Tt {
        [
            Tt::T8x8,
            Tt::T8x4Bottom,
            Tt::T8x4Top,
            Tt::T8x4,
            Tt::T4x8Right,
            Tt::T4x8Left,
            Tt::T4x8,
            Tt::T4x4,
        ][usize::from(v & 7)]
    }
}

/// ff_vc1_ttblk_to_tt.
const TTBLK_TO_TT: [[Tt; 8]; 3] = [
    [
        Tt::T8x4,
        Tt::T4x8,
        Tt::T8x8,
        Tt::T4x4,
        Tt::T8x4Top,
        Tt::T8x4Bottom,
        Tt::T4x8Right,
        Tt::T4x8Left,
    ],
    [
        Tt::T8x8,
        Tt::T4x8Right,
        Tt::T4x8Left,
        Tt::T4x4,
        Tt::T8x4,
        Tt::T4x8,
        Tt::T8x4Bottom,
        Tt::T8x4Top,
    ],
    [
        Tt::T8x8,
        Tt::T4x8,
        Tt::T4x4,
        Tt::T8x4Bottom,
        Tt::T4x8Right,
        Tt::T4x8Left,
        Tt::T8x4,
        Tt::T8x4Top,
    ],
];

/// ff_vc1_ttfrm_to_tt.
const TTFRM_TO_TT: [Tt; 4] = [Tt::T8x8, Tt::T8x4, Tt::T4x8, Tt::T4x4];

/// Macroblock level transform signalling state, FFmpeg's `ttmb`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MbTransform {
    /// A type that applies to blocks; `all` is bit 3 of TTMB (or frame
    /// level signalling), meaning later blocks keep it.
    Fixed { tt: Tt, all: bool },
    /// Each later block sends TTBLK.
    PerBlock,
}

/// Optional transform fields an inter block carries.
struct TransformFields {
    /// SUBBLKPAT of a 4x4 transform.
    subblkpat: bool,
    /// DECODE012 pattern of an 8x4 or 4x8 transform.
    halves: bool,
}

/// Subblocks of one inter block after its transform signalling.
struct Layout {
    shape: InterShape,
    span: Span,
    coded: Vec<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterShape {
    EightByEight,
    EightByFour,
    FourByEight,
    FourByFour,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterLayout {
    pub shape: InterShape,
    pub coded: Vec<bool>,
}

/// Picture header fields the P and B macroblock layers share.
struct InterParams<'a> {
    quarter_sample: bool,
    mvtab: Bounded<3>,
    cbptab: Bounded<3>,
    transform: TransformSignal,
    mvtypemb: Option<&'a Bitplane>,
    skipmb: &'a Bitplane,
}

/// Picture level state of the P and B macroblock layers. The motion
/// vector field is used by P pictures only.
struct PContext {
    dims: MbDims,
    sets: PictureCodingSets,
    coeffs: CoeffCoder,
    mv: MvCoding,
    cbptab: usize,
    tt_class: QuantClass,
    frame_tt: Option<Tt>,
    rtm: bool,
    mvtype: Plane,
    skip: Plane,
    field: MvField,
}

/// A bitplane's decoded values, or `Raw` when sent per macroblock.
enum Plane {
    Raw,
    Values(Vec<bool>),
    /// Not sent: every value is clear (MVTYPEMB outside mixed MV mode).
    AllClear,
}

impl Plane {
    fn of(bp: Option<&Bitplane>, dims: MbDims) -> Result<Plane, FrameError> {
        Ok(match bp.map(|bp| bp.decode(dims)).transpose()? {
            None => Plane::AllClear,
            Some(PlaneValues::Raw) => Plane::Raw,
            Some(PlaneValues::Values(v)) => Plane::Values(v),
        })
    }

    fn value(&self, i: usize) -> Option<bool> {
        match self {
            Plane::Raw => None,
            Plane::Values(v) => Some(v[i]),
            Plane::AllClear => Some(false),
        }
    }
}

/// Each macroblock takes at least one bit (a raw SKIPMB bit, or MVDATA),
/// except that coded skip planes can make every macroblock free; this bound
/// is only applied when the skip plane is raw.
const MIN_RAW_MB_BITS: usize = 1;

impl PContext {
    fn for_p(h: &PictureHeader, p: &PHeader, ctx: &FrameContext) -> Result<Self, FrameError> {
        let mode = p.effective_mv_mode(h.pquant(ctx.seq.quantizer));
        PContext::new(
            h,
            &InterParams {
                quarter_sample: !matches!(
                    mode,
                    MvMode::OneMvHalfPel | MvMode::OneMvHalfPelBilinear
                ),
                mvtab: p.mvtab,
                cbptab: p.cbptab,
                transform: p.transform,
                mvtypemb: p.mvtypemb.as_ref(),
                skipmb: &p.skipmb,
            },
            ctx,
        )
    }

    fn for_b(h: &PictureHeader, b: &BHeader, ctx: &FrameContext) -> Result<Self, FrameError> {
        PContext::new(
            h,
            &InterParams {
                quarter_sample: b.mvmode_quarter_pel,
                mvtab: b.mvtab,
                cbptab: b.cbptab,
                transform: b.transform,
                mvtypemb: None,
                skipmb: &b.skipmb,
            },
            ctx,
        )
    }

    fn new(h: &PictureHeader, p: &InterParams, ctx: &FrameContext) -> Result<Self, FrameError> {
        let dims = ctx.mb_dims();
        let pquant = h.pquant(ctx.seq.quantizer);
        Ok(PContext {
            dims,
            sets: CodingSet::for_picture(h),
            coeffs: CoeffCoder::new(h, pquant),
            mv: MvCoding {
                table: usize::from(p.mvtab.get()),
                quarter_sample: p.quarter_sample,
                range: MvRange::new(h.mvrange),
            },
            cbptab: usize::from(p.cbptab.get()),
            tt_class: QuantClass::of(pquant),
            frame_tt: match p.transform {
                TransformSignal::Absent => Some(Tt::T8x8),
                TransformSignal::PerMacroblock => None,
                TransformSignal::Frame(v) => Some(TTFRM_TO_TT[usize::from(v.get())]),
            },
            rtm: ctx.seq.res_rtm_flag,
            mvtype: Plane::of(p.mvtypemb, dims)?,
            skip: Plane::of(Some(p.skipmb), dims)?,
            field: MvField::new(dims),
        })
    }

    fn ttmbf(&self) -> bool {
        self.frame_tt.is_some()
    }

    /// Transform state for the first inter block, `None` when neither
    /// frame level signalling nor TTMB is present (no inter block follows).
    fn initial_transform(&self, ttmb: Option<Bounded<15>>) -> Option<MbTransform> {
        match (self.frame_tt, ttmb) {
            (Some(tt), _) => Some(MbTransform::Fixed { tt, all: false }),
            (None, Some(v)) => Some(MbTransform::Fixed {
                tt: Tt::from_index(v.get()),
                all: v.get() & 8 != 0,
            }),
            (None, None) => None,
        }
    }

    /// Resolves the transform of one inter block from its signalling.
    fn layout(
        &self,
        mut tt: Tt,
        subblkpat: Option<Bounded<14>>,
        halves: Option<Bounded<2>>,
    ) -> Layout {
        // Bits set mark subblocks that are not coded.
        let mut pattern: u8 = 0;
        if let Some(s) = subblkpat {
            pattern = !(s.get() + 1) & 0xf;
        }
        if let Some(v) = halves {
            pattern = match v.get() {
                0 => 0,
                n => n ^ 3,
            };
            tt = match tt {
                Tt::T8x4Top | Tt::T8x4Bottom => Tt::T8x4,
                Tt::T4x8Right | Tt::T4x8Left => Tt::T4x8,
                t => t,
            };
        }
        match tt {
            Tt::T8x4Top | Tt::T8x4Bottom => {
                pattern = 2 - u8::from(tt == Tt::T8x4Top);
                tt = Tt::T8x4;
            }
            Tt::T4x8Right | Tt::T4x8Left => {
                pattern = 2 - u8::from(tt == Tt::T4x8Left);
                tt = Tt::T4x8;
            }
            _ => {}
        }
        match tt {
            Tt::T8x8 => Layout {
                shape: InterShape::EightByEight,
                span: Span { first: 0, last: 63 },
                coded: vec![true],
            },
            Tt::T4x4 => Layout {
                shape: InterShape::FourByFour,
                span: Span { first: 0, last: 15 },
                coded: (0..4).map(|j| pattern & (1 << (3 - j)) == 0).collect(),
            },
            _ => Layout {
                shape: match tt {
                    Tt::T8x4 => InterShape::EightByFour,
                    Tt::T4x8 => InterShape::FourByEight,
                    _ => unreachable!("transform normalized above"),
                },
                span: Span { first: 0, last: 31 },
                coded: (0..2).map(|j| pattern & (1 << (1 - j)) == 0).collect(),
            },
        }
    }

    /// Which optional transform fields an inter block carries.
    fn block_fields(&self, state: MbTransform, first: bool, tt_after_ttblk: Tt) -> TransformFields {
        TransformFields {
            subblkpat: tt_after_ttblk == Tt::T4x4,
            halves: !matches!(tt_after_ttblk, Tt::T8x8 | Tt::T4x4)
                && (self.ttmbf()
                    || matches!(state, MbTransform::Fixed { all: true, .. }) && !first
                    || !self.rtm && !first),
        }
    }
}

/// Quantizer class selecting the TTMB, TTBLK and SUBBLKPAT tables:
/// PQUANT up to 4, up to 12, and above.
#[derive(Clone, Copy)]
struct QuantClass(usize);

impl QuantClass {
    fn of(pq: Pquant) -> Self {
        QuantClass(usize::from(pq.get() > 4) + usize::from(pq.get() > 12))
    }
}

/// The reader sends TTMB exactly when a coded inter block follows and
/// frame level signalling is off.
const TTMB_PRECEDES: &str = "TTMB or TTFRM precedes coded inter blocks";

/// After a coded inter block, a TTMB without bit 3 hands later blocks to
/// TTBLK.
fn next_transform(state: MbTransform, ttmbf: bool) -> MbTransform {
    match state {
        MbTransform::Fixed { all: false, .. } if !ttmbf => MbTransform::PerBlock,
        s => s,
    }
}

fn read_inter_block(
    r: &mut BitReader,
    c: &mut PContext,
    state: MbTransform,
    first: bool,
) -> Result<InterBlock, FrameError> {
    let (ttblk, tt) = match state {
        MbTransform::PerBlock => {
            let s: Bounded<7> = Bounded::from_field(TTBLK[c.tt_class.0].read(r)?.into());
            (Some(s), TTBLK_TO_TT[c.tt_class.0][usize::from(s.get())])
        }
        MbTransform::Fixed { tt, .. } => (None, tt),
    };
    let fields = c.block_fields(state, first, tt);
    let subblkpat = if fields.subblkpat {
        Some(Bounded::from_field(SUBBLKPAT[c.tt_class.0].read(r)?.into()))
    } else {
        None
    };
    let halves = if fields.halves {
        Some(read_012(r)?)
    } else {
        None
    };
    let layout = c.layout(tt, subblkpat, halves);
    let mut subblocks = Vec::new();
    for &coded in &layout.coded {
        if coded {
            subblocks.push(read_coeffs(r, &mut c.coeffs, c.sets.inter, layout.span)?);
        }
    }
    Ok(InterBlock {
        ttblk,
        subblkpat,
        halves,
        subblocks,
    })
}

fn write_inter_block(
    w: &mut BitWriter,
    c: &mut PContext,
    state: MbTransform,
    first: bool,
    b: &InterBlock,
) -> Result<(), Fault> {
    let shape = Fault::Mb(MbFault::TransformFields);
    let tt = match (state, b.ttblk) {
        (MbTransform::PerBlock, Some(s)) => {
            TTBLK[c.tt_class.0].write(w, s.get().into())?;
            TTBLK_TO_TT[c.tt_class.0][usize::from(s.get())]
        }
        (MbTransform::Fixed { tt, .. }, None) => tt,
        _ => return Err(shape),
    };
    let fields = c.block_fields(state, first, tt);
    match (fields.subblkpat, b.subblkpat) {
        (true, Some(s)) => SUBBLKPAT[c.tt_class.0].write(w, s.get().into())?,
        (false, None) => {}
        _ => return Err(shape),
    }
    match (fields.halves, b.halves) {
        (true, Some(v)) => write_012(w, v),
        (false, None) => {}
        _ => return Err(shape),
    }
    let layout = c.layout(tt, b.subblkpat, b.halves);
    let coded = layout.coded.iter().filter(|&&x| x).count();
    if coded != b.subblocks.len() {
        return Err(Fault::Block(BlockFault::CoefficientList));
    }
    for coeffs in &b.subblocks {
        write_coeffs(w, &mut c.coeffs, c.sets.inter, layout.span, coeffs)?;
    }
    Ok(())
}

fn read_012(r: &mut BitReader) -> Result<Bounded<2>, FrameError> {
    Ok(Bounded::from_field(if !r.bit()? {
        0
    } else {
        1 + u32::from(r.bit()?)
    }))
}

fn write_012(w: &mut BitWriter, v: Bounded<2>) {
    match v.get() {
        0 => w.bit(false),
        n => {
            w.bit(true);
            w.bit(n == 2);
        }
    }
}

fn cbp_bit(cbp: u8, i: usize) -> bool {
    (cbp >> (5 - i)) & 1 == 1
}

/// Coding set of an intra block in a P picture.
fn intra_set(c: &PContext, i: usize) -> CodingSet {
    if i < 4 {
        c.sets.intra_luma
    } else {
        c.sets.inter
    }
}

/// Transform layouts of the inter blocks of one P macroblock.
fn transform_layouts(
    c: &PContext,
    mb_index: usize,
    mb: &PMb,
) -> Result<[Option<InterLayout>; 6], FrameError> {
    match &mb.body {
        PMbBody::OneMv(mb) => block_layouts(c, mb_index, &mb.blocks, mb.ttmb),
        PMbBody::FourMv(mb) => block_layouts(c, mb_index, &mb.blocks, mb.ttmb),
        PMbBody::SkippedOneMv { .. } | PMbBody::SkippedFourMv { .. } => {
            Ok(std::array::from_fn(|_| None))
        }
    }
}

/// Transform layouts of the inter blocks among `blocks`, following the
/// TTMB/TTBLK state machine.
fn block_layouts(
    c: &PContext,
    mb_index: usize,
    blocks: &[PBlock; 6],
    ttmb: Option<Bounded<15>>,
) -> Result<[Option<InterLayout>; 6], FrameError> {
    let mut layouts = std::array::from_fn(|_| None);
    let mut state = c.initial_transform(ttmb);
    for (block_index, block) in blocks.iter().enumerate() {
        let PBlock::Inter(block) = block else {
            continue;
        };
        let Some(s) = state else {
            return Err(FrameError::Macroblock {
                mb: mb_index,
                fault: MbFault::Ttmb,
            });
        };
        let tt = match (s, block.ttblk) {
            (MbTransform::PerBlock, Some(v)) => TTBLK_TO_TT[c.tt_class.0][usize::from(v.get())],
            (MbTransform::Fixed { tt, .. }, None) => tt,
            _ => {
                return Err(FrameError::Macroblock {
                    mb: mb_index,
                    fault: MbFault::TransformFields,
                });
            }
        };
        let layout = c.layout(tt, block.subblkpat, block.halves);
        layouts[block_index] = Some(InterLayout {
            shape: layout.shape,
            coded: layout.coded,
        });
        state = Some(next_transform(s, c.ttmbf()));
    }
    Ok(layouts)
}

impl InterLayer {
    pub(crate) fn derive(
        &self,
        h: &PictureHeader,
        p: &PHeader,
        ctx: &FrameContext,
    ) -> Result<Vec<DerivedPMb>, FrameError> {
        let dims = ctx.mb_dims();
        if self.macroblocks.len() != dims.count() {
            return Err(FrameError::MacroblockCount {
                expected: dims.count(),
                found: self.macroblocks.len(),
            });
        }
        let mut c = PContext::for_p(h, p, ctx)?;
        let mut derived = Vec::with_capacity(self.macroblocks.len());
        for (i, mb) in self.macroblocks.iter().enumerate() {
            let (mx, my) = (i % usize::from(dims.width), i / usize::from(dims.width));
            let mut item = DerivedPMb {
                vectors: [Mv::default(); 4],
                intra: [false; 6],
                layouts: transform_layouts(&c, i, mb)?,
            };
            match &mb.body {
                PMbBody::SkippedOneMv { hybrid } => {
                    let pred = c.field.predict(mx, my, 0, true, c.rtm);
                    let mv = reconstruct(&pred, *hybrid, Mv::default(), c.mv);
                    c.field.store(mx, my, 0, true, mv);
                    item.vectors = [mv; 4];
                    for n in 0..6 {
                        c.field.set_intra(mx, my, n, false);
                    }
                }
                PMbBody::SkippedFourMv { hybrid } => {
                    for (n, &h) in hybrid.iter().enumerate() {
                        let pred = c.field.predict(mx, my, n, false, c.rtm);
                        let mv = reconstruct(&pred, h, Mv::default(), c.mv);
                        c.field.store(mx, my, n, false, mv);
                        item.vectors[n] = mv;
                        c.field.set_intra(mx, my, n, false);
                    }
                    c.field.set_intra(mx, my, 4, false);
                    c.field.set_intra(mx, my, 5, false);
                }
                PMbBody::OneMv(mb) => {
                    let meaning = c.mv.meaning(&mb.mv);
                    let mv = if meaning.intra {
                        Mv::default()
                    } else {
                        let pred = c.field.predict(mx, my, 0, true, c.rtm);
                        reconstruct(&pred, mb.hybrid, meaning.diff, c.mv)
                    };
                    c.field.store(mx, my, 0, true, mv);
                    item.vectors = [mv; 4];
                    item.intra = [meaning.intra; 6];
                    for n in 0..6 {
                        c.field.set_intra(mx, my, n, meaning.intra);
                    }
                }
                PMbBody::FourMv(mb) => {
                    for n in 0..4 {
                        // A block without BLKMVDATA has a zero differential
                        // and no coefficients (FFmpeg's dmv_x = dmv_y = 0).
                        let meaning = mb.mvs[n].map_or(
                            MvMeaning {
                                diff: Mv::default(),
                                intra: false,
                                has_coeffs: false,
                            },
                            |d| c.mv.meaning(&d),
                        );
                        let mv = if meaning.intra {
                            Mv::default()
                        } else {
                            let pred = c.field.predict(mx, my, n, false, c.rtm);
                            reconstruct(&pred, mb.hybrid[n], meaning.diff, c.mv)
                        };
                        c.field.store(mx, my, n, false, mv);
                        c.field.set_intra(mx, my, n, meaning.intra);
                        item.vectors[n] = mv;
                        item.intra[n] = meaning.intra;
                    }
                    let chroma_intra = item.intra[..4].iter().filter(|&&v| v).count() >= 3;
                    item.intra[4] = chroma_intra;
                    item.intra[5] = chroma_intra;
                    c.field.set_intra(mx, my, 4, chroma_intra);
                    c.field.set_intra(mx, my, 5, chroma_intra);
                }
            }
            derived.push(item);
        }
        Ok(derived)
    }

    pub fn read(
        r: &mut BitReader,
        h: &PictureHeader,
        p: &PHeader,
        ctx: &FrameContext,
    ) -> Result<Self, FrameError> {
        let mut c = PContext::for_p(h, p, ctx)?;
        let dims = c.dims;
        if matches!(c.skip, Plane::Raw) {
            let needed = dims.count().saturating_mul(MIN_RAW_MB_BITS);
            if needed > r.remaining() {
                return Err(OutOfBits {
                    bit: r.position(),
                    needed: needed - r.remaining(),
                }
                .into());
            }
        }
        let mut macroblocks = Vec::new();
        for my in 0..usize::from(dims.height) {
            for mx in 0..usize::from(dims.width) {
                macroblocks.push(read_mb(r, &mut c, mx, my)?);
            }
        }
        Ok(InterLayer {
            macroblocks,
            trailing: OpaqueBits::rest(r),
        })
    }

    pub fn write(
        &self,
        w: &mut BitWriter,
        h: &PictureHeader,
        p: &PHeader,
        ctx: &FrameContext,
    ) -> Result<(), FrameError> {
        let mut c = PContext::for_p(h, p, ctx)?;
        let dims = c.dims;
        if self.macroblocks.len() != dims.count() {
            return Err(FrameError::MacroblockCount {
                expected: dims.count(),
                found: self.macroblocks.len(),
            });
        }
        for (i, mb) in self.macroblocks.iter().enumerate() {
            let (mx, my) = (i % usize::from(dims.width), i / usize::from(dims.width));
            write_mb(w, &mut c, mx, my, mb).map_err(|f| locate(f, i))?;
        }
        self.trailing.write(w);
        Ok(())
    }
}

/// Turns a write fault into an error naming macroblock `mb`.
fn locate(f: Located, mb: usize) -> FrameError {
    match f.fault {
        Fault::Frame(e) => e,
        Fault::Block(fault) => BlockError {
            mb,
            block: f.block,
            fault,
        }
        .into(),
        Fault::Mb(fault) => FrameError::Macroblock { mb, fault },
    }
}

/// A macroblock's bitplane value, and the bit it took when sent raw.
struct FlagRead {
    value: bool,
    raw: Option<bool>,
}

fn read_flag(r: &mut BitReader, plane: &Plane, i: usize) -> Result<FlagRead, FrameError> {
    Ok(match plane.value(i) {
        Some(value) => FlagRead { value, raw: None },
        None => {
            let b = r.bit()?;
            FlagRead {
                value: b,
                raw: Some(b),
            }
        }
    })
}

fn read_mb(r: &mut BitReader, c: &mut PContext, mx: usize, my: usize) -> Result<PMb, FrameError> {
    let i = my * usize::from(c.dims.width) + mx;
    let fourmv = read_flag(r, &c.mvtype, i)?;
    let skipped = read_flag(r, &c.skip, i)?;
    let body = match (fourmv.value, skipped.value) {
        (false, true) => {
            for n in 0..6 {
                c.field.set_intra(mx, my, n, false);
            }
            let pred = c.field.predict(mx, my, 0, true, c.rtm);
            let hybrid = if pred.hybrid { Some(r.bit()?) } else { None };
            let mv = reconstruct(&pred, hybrid, Mv::default(), c.mv);
            c.field.store(mx, my, 0, true, mv);
            PMbBody::SkippedOneMv { hybrid }
        }
        (true, true) => {
            for n in 0..6 {
                c.field.set_intra(mx, my, n, false);
            }
            let mut hybrid = [None; 4];
            for (n, h) in hybrid.iter_mut().enumerate() {
                let pred = c.field.predict(mx, my, n, false, c.rtm);
                *h = if pred.hybrid { Some(r.bit()?) } else { None };
                let mv = reconstruct(&pred, *h, Mv::default(), c.mv);
                c.field.store(mx, my, n, false, mv);
            }
            PMbBody::SkippedFourMv { hybrid }
        }
        (false, false) => PMbBody::OneMv(read_one_mv(r, c, mx, my)?),
        (true, false) => PMbBody::FourMv(read_four_mv(r, c, mx, my)?),
    };
    Ok(PMb {
        mvtype_bit: fourmv.raw,
        skip_bit: skipped.raw,
        body,
    })
}

fn read_one_mv(
    r: &mut BitReader,
    c: &mut PContext,
    mx: usize,
    my: usize,
) -> Result<OneMvMb, FrameError> {
    let mv = c.mv.read(r)?;
    let m = c.mv.meaning(&mv);
    let hybrid = if m.intra {
        c.field.store(mx, my, 0, true, Mv::default());
        None
    } else {
        let pred = c.field.predict(mx, my, 0, true, c.rtm);
        let h = if pred.hybrid { Some(r.bit()?) } else { None };
        let v = reconstruct(&pred, h, m.diff, c.mv);
        c.field.store(mx, my, 0, true, v);
        h
    };
    let ac_pred = if m.intra { Some(r.bit()?) } else { None };
    let cbpcy = if m.has_coeffs {
        Some(Bounded::from_field(CBPCY_P[c.cbptab].read(r)?.into()))
    } else {
        None
    };
    let ttmb = if !c.ttmbf() && !m.intra && m.has_coeffs {
        Some(Bounded::from_field(TTMB[c.tt_class.0].read(r)?.into()))
    } else {
        None
    };
    let cbp = cbp_or_uncoded(cbpcy);
    let mut state = c.initial_transform(ttmb);
    let mut first = true;
    let mut blocks = Vec::with_capacity(6);
    for i in 0..6 {
        let coded = cbp_bit(cbp, i);
        blocks.push(if m.intra {
            let cs = intra_set(c, i);
            PBlock::Intra(read_intra_block(r, &mut c.coeffs, cs, i >= 4, coded)?)
        } else if coded {
            let s = state.expect(TTMB_PRECEDES);
            let b = read_inter_block(r, c, s, first)?;
            state = Some(next_transform(s, c.ttmbf()));
            first = false;
            PBlock::Inter(b)
        } else {
            PBlock::Empty
        });
    }
    for n in 0..6 {
        c.field.set_intra(mx, my, n, m.intra);
    }
    Ok(OneMvMb {
        mv,
        hybrid,
        ac_pred,
        cbpcy,
        ttmb,
        blocks: blocks.try_into().expect("six blocks"),
    })
}

/// Per-block flags of a 4MV macroblock once its vectors are known.
struct FourMvFlags {
    intra: [bool; 6],
    coded: [bool; 6],
}

impl FourMvFlags {
    fn any_intra(&self) -> bool {
        self.intra.iter().any(|&b| b)
    }

    fn coded_inter(&self) -> bool {
        (0..6).any(|i| !self.intra[i] && self.coded[i])
    }
}

fn read_four_mv(
    r: &mut BitReader,
    c: &mut PContext,
    mx: usize,
    my: usize,
) -> Result<FourMvMb, FrameError> {
    let cbpcy: Bounded<63> = Bounded::from_field(CBPCY_P[c.cbptab].read(r)?.into());
    let mut mvs = [None; 4];
    let mut hybrid = [None; 4];
    let mut flags = FourMvFlags {
        intra: [false; 6],
        coded: [false; 6],
    };
    for n in 0..4 {
        let m = if cbp_bit(cbpcy.get(), n) {
            let d = c.mv.read(r)?;
            mvs[n] = Some(d);
            c.mv.meaning(&d)
        } else {
            MvMeaning {
                diff: Mv::default(),
                intra: false,
                has_coeffs: false,
            }
        };
        if m.intra {
            c.field.store(mx, my, n, false, Mv::default());
        } else {
            let pred = c.field.predict(mx, my, n, false, c.rtm);
            hybrid[n] = if pred.hybrid { Some(r.bit()?) } else { None };
            let v = reconstruct(&pred, hybrid[n], m.diff, c.mv);
            c.field.store(mx, my, n, false, v);
        }
        flags.intra[n] = m.intra;
        flags.coded[n] = m.has_coeffs;
        c.field.set_intra(mx, my, n, m.intra);
    }
    let chroma_intra = flags.intra[..4].iter().filter(|&&b| b).count() >= 3;
    for n in 4..6 {
        flags.intra[n] = chroma_intra;
        flags.coded[n] = cbp_bit(cbpcy.get(), n);
    }
    c.field.set_intra(mx, my, 4, chroma_intra);
    let mut blocks: [PBlock; 6] = Default::default();
    if !flags.any_intra() && !flags.coded_inter() {
        return Ok(FourMvMb {
            cbpcy,
            mvs,
            hybrid,
            ac_pred: None,
            ttmb: None,
            blocks,
        });
    }
    let intrapred = (0..6).any(|n| flags.intra[n] && c.field.intra_neighbour(mx, my, n));
    let ac_pred = if intrapred { Some(r.bit()?) } else { None };
    let ttmb = if !c.ttmbf() && flags.coded_inter() {
        Some(Bounded::from_field(TTMB[c.tt_class.0].read(r)?.into()))
    } else {
        None
    };
    let mut state = c.initial_transform(ttmb);
    let mut first = true;
    for (n, block) in blocks.iter_mut().enumerate() {
        if flags.intra[n] {
            let cs = intra_set(c, n);
            *block = PBlock::Intra(read_intra_block(
                r,
                &mut c.coeffs,
                cs,
                n >= 4,
                flags.coded[n],
            )?);
        } else if flags.coded[n] {
            let s = state.expect(TTMB_PRECEDES);
            *block = PBlock::Inter(read_inter_block(r, c, s, first)?);
            state = Some(next_transform(s, c.ttmbf()));
            first = false;
        }
    }
    Ok(FourMvMb {
        cbpcy,
        mvs,
        hybrid,
        ac_pred,
        ttmb,
        blocks,
    })
}

/// A write fault and the block of the macroblock it arose in.
struct Located {
    fault: Fault,
    block: usize,
}

type MbResult = Result<(), Located>;

/// Tags a fault with the block it arose in.
fn at(block: usize) -> impl Fn(Fault) -> Located {
    move |fault| Located { fault, block }
}

fn write_flag(
    w: &mut BitWriter,
    plane: &Plane,
    i: usize,
    bit: Option<bool>,
) -> Result<bool, Fault> {
    match (plane.value(i), bit) {
        (Some(v), None) => Ok(v),
        (None, Some(b)) => {
            w.bit(b);
            Ok(b)
        }
        _ => Err(Fault::Mb(MbFault::RawPlaneBit)),
    }
}

fn write_hybrid(w: &mut BitWriter, needed: bool, h: Option<bool>) -> Result<(), Fault> {
    match (needed, h) {
        (true, Some(b)) => {
            w.bit(b);
            Ok(())
        }
        (false, None) => Ok(()),
        _ => Err(Fault::Mb(MbFault::Hybrid)),
    }
}

fn write_present<T: Copy>(
    present: bool,
    value: Option<T>,
    fault: MbFault,
    f: impl FnOnce(T) -> Result<(), Fault>,
) -> Result<(), Fault> {
    match (present, value) {
        (true, Some(v)) => f(v),
        (false, None) => Ok(()),
        _ => Err(Fault::Mb(fault)),
    }
}

fn write_mb(w: &mut BitWriter, c: &mut PContext, mx: usize, my: usize, mb: &PMb) -> MbResult {
    let i = my * usize::from(c.dims.width) + mx;
    let fourmv = write_flag(w, &c.mvtype, i, mb.mvtype_bit).map_err(at(0))?;
    let skipped = write_flag(w, &c.skip, i, mb.skip_bit).map_err(at(0))?;
    match (&mb.body, fourmv, skipped) {
        (PMbBody::SkippedOneMv { hybrid }, false, true) => {
            for n in 0..6 {
                c.field.set_intra(mx, my, n, false);
            }
            let pred = c.field.predict(mx, my, 0, true, c.rtm);
            write_hybrid(w, pred.hybrid, *hybrid).map_err(at(0))?;
            let mv = reconstruct(&pred, *hybrid, Mv::default(), c.mv);
            c.field.store(mx, my, 0, true, mv);
            Ok(())
        }
        (PMbBody::SkippedFourMv { hybrid }, true, true) => {
            for n in 0..6 {
                c.field.set_intra(mx, my, n, false);
            }
            for (n, &h) in hybrid.iter().enumerate() {
                let pred = c.field.predict(mx, my, n, false, c.rtm);
                write_hybrid(w, pred.hybrid, h).map_err(at(n))?;
                let mv = reconstruct(&pred, h, Mv::default(), c.mv);
                c.field.store(mx, my, n, false, mv);
            }
            Ok(())
        }
        (PMbBody::OneMv(m), false, false) => write_one_mv(w, c, mx, my, m),
        (PMbBody::FourMv(m), true, false) => write_four_mv(w, c, mx, my, m),
        _ => Err(Located {
            fault: Fault::Mb(MbFault::Kind),
            block: 0,
        }),
    }
}

fn write_one_mv(
    w: &mut BitWriter,
    c: &mut PContext,
    mx: usize,
    my: usize,
    mb: &OneMvMb,
) -> MbResult {
    c.mv.write(w, &mb.mv).map_err(at(0))?;
    let m = c.mv.meaning(&mb.mv);
    if m.intra {
        if mb.hybrid.is_some() {
            return Err(Located {
                fault: Fault::Mb(MbFault::Hybrid),
                block: 0,
            });
        }
        c.field.store(mx, my, 0, true, Mv::default());
    } else {
        let pred = c.field.predict(mx, my, 0, true, c.rtm);
        write_hybrid(w, pred.hybrid, mb.hybrid).map_err(at(0))?;
        let v = reconstruct(&pred, mb.hybrid, m.diff, c.mv);
        c.field.store(mx, my, 0, true, v);
    }
    write_present(m.intra, mb.ac_pred, MbFault::AcPred, |b| {
        w.bit(b);
        Ok(())
    })
    .map_err(at(0))?;
    write_present(m.has_coeffs, mb.cbpcy, MbFault::Cbpcy, |v| {
        Ok(CBPCY_P[c.cbptab].write(w, v.get().into())?)
    })
    .map_err(at(0))?;
    let ttmbf = c.ttmbf();
    let tt_class = c.tt_class;
    write_present(
        !ttmbf && !m.intra && m.has_coeffs,
        mb.ttmb,
        MbFault::Ttmb,
        |v| Ok(TTMB[tt_class.0].write(w, v.get().into())?),
    )
    .map_err(at(0))?;
    let cbp = cbp_or_uncoded(mb.cbpcy);
    let mut state = c.initial_transform(mb.ttmb);
    let mut first = true;
    for (i, block) in mb.blocks.iter().enumerate() {
        let coded = cbp_bit(cbp, i);
        match (block, m.intra, coded) {
            (PBlock::Intra(b), true, _) => {
                let cs = intra_set(c, i);
                write_intra_block(w, &mut c.coeffs, cs, i >= 4, coded, b).map_err(at(i))?;
            }
            (PBlock::Inter(b), false, true) => {
                let s = state.ok_or(Located {
                    fault: Fault::Mb(MbFault::Ttmb),
                    block: i,
                })?;
                write_inter_block(w, c, s, first, b).map_err(at(i))?;
                state = Some(next_transform(s, c.ttmbf()));
                first = false;
            }
            (PBlock::Empty, false, false) => {}
            _ => {
                return Err(Located {
                    fault: Fault::Block(BlockFault::CoefficientList),
                    block: i,
                });
            }
        }
    }
    for n in 0..6 {
        c.field.set_intra(mx, my, n, m.intra);
    }
    Ok(())
}

fn write_four_mv(
    w: &mut BitWriter,
    c: &mut PContext,
    mx: usize,
    my: usize,
    mb: &FourMvMb,
) -> MbResult {
    CBPCY_P[c.cbptab]
        .write(w, mb.cbpcy.get().into())
        .map_err(|e| Located {
            fault: Fault::Frame(e),
            block: 0,
        })?;
    let mut flags = FourMvFlags {
        intra: [false; 6],
        coded: [false; 6],
    };
    for n in 0..4 {
        let m = match (cbp_bit(mb.cbpcy.get(), n), &mb.mvs[n]) {
            (true, Some(d)) => {
                c.mv.write(w, d).map_err(at(n))?;
                c.mv.meaning(d)
            }
            (false, None) => MvMeaning {
                diff: Mv::default(),
                intra: false,
                has_coeffs: false,
            },
            _ => {
                return Err(Located {
                    fault: Fault::Mb(MbFault::FourMvData),
                    block: n,
                });
            }
        };
        if m.intra {
            if mb.hybrid[n].is_some() {
                return Err(Located {
                    fault: Fault::Mb(MbFault::Hybrid),
                    block: n,
                });
            }
            c.field.store(mx, my, n, false, Mv::default());
        } else {
            let pred = c.field.predict(mx, my, n, false, c.rtm);
            write_hybrid(w, pred.hybrid, mb.hybrid[n]).map_err(at(n))?;
            let v = reconstruct(&pred, mb.hybrid[n], m.diff, c.mv);
            c.field.store(mx, my, n, false, v);
        }
        flags.intra[n] = m.intra;
        flags.coded[n] = m.has_coeffs;
        c.field.set_intra(mx, my, n, m.intra);
    }
    let chroma_intra = flags.intra[..4].iter().filter(|&&b| b).count() >= 3;
    for n in 4..6 {
        flags.intra[n] = chroma_intra;
        flags.coded[n] = cbp_bit(mb.cbpcy.get(), n);
    }
    c.field.set_intra(mx, my, 4, chroma_intra);
    let active = flags.any_intra() || flags.coded_inter();
    let intrapred = active && (0..6).any(|n| flags.intra[n] && c.field.intra_neighbour(mx, my, n));
    write_present(intrapred, mb.ac_pred, MbFault::AcPred, |b| {
        w.bit(b);
        Ok(())
    })
    .map_err(at(0))?;
    let ttmbf = c.ttmbf();
    let tt_class = c.tt_class;
    write_present(
        active && !ttmbf && flags.coded_inter(),
        mb.ttmb,
        MbFault::Ttmb,
        |v| Ok(TTMB[tt_class.0].write(w, v.get().into())?),
    )
    .map_err(at(0))?;
    let mut state = c.initial_transform(mb.ttmb);
    let mut first = true;
    for (n, block) in mb.blocks.iter().enumerate() {
        let intra = active && flags.intra[n];
        let inter = active && !flags.intra[n] && flags.coded[n];
        match (block, intra, inter) {
            (PBlock::Intra(b), true, _) => {
                let cs = intra_set(c, n);
                write_intra_block(w, &mut c.coeffs, cs, n >= 4, flags.coded[n], b)
                    .map_err(at(n))?;
            }
            (PBlock::Inter(b), false, true) => {
                let s = state.ok_or(Located {
                    fault: Fault::Mb(MbFault::Ttmb),
                    block: n,
                })?;
                write_inter_block(w, c, s, first, b).map_err(at(n))?;
                state = Some(next_transform(s, c.ttmbf()));
                first = false;
            }
            (PBlock::Empty, false, false) => {}
            _ => {
                return Err(Located {
                    fault: Fault::Block(BlockFault::CoefficientList),
                    block: n,
                });
            }
        }
    }
    Ok(())
}

/// Macroblock layer of a B picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BLayer {
    pub macroblocks: Vec<BMb>,
    /// Bits after the last macroblock, normally zero padding.
    pub trailing: OpaqueBits,
}

/// One B macroblock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BMb {
    /// DIRECTMB bit when that bitplane is sent raw.
    pub direct_bit: Option<bool>,
    /// SKIPMB bit when that bitplane is sent raw.
    pub skip_bit: Option<bool>,
    pub body: BMbBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BMbBody {
    /// Direct and skipped: nothing is coded.
    SkippedDirect,
    /// Skipped with its prediction direction.
    Skipped {
        bmvtype: BmvType,
    },
    Direct(BDirectMb),
    Coded(BCodedMb),
}

/// BMVTYPE as coded (DECODE012). Which of the first two means forward
/// depends on BFRACTION, so they are kept as coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BmvType {
    First,
    Second,
    Interpolated,
}

impl BmvType {
    fn read(r: &mut BitReader) -> Result<Self, FrameError> {
        Ok(match read_012(r)?.get() {
            0 => BmvType::First,
            1 => BmvType::Second,
            _ => BmvType::Interpolated,
        })
    }

    fn write(self, w: &mut BitWriter) {
        let v = match self {
            BmvType::First => 0,
            BmvType::Second => 1,
            BmvType::Interpolated => 2,
        };
        write_012(w, Bounded::from_field(v));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BDirectMb {
    pub cbpcy: Bounded<63>,
    pub ttmb: Option<Bounded<15>>,
    pub blocks: [PBlock; 6],
}

/// A B macroblock that is neither direct nor skipped, following FFmpeg's
/// vc1_decode_b_mb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BCodedMb {
    pub mv: MvData,
    /// BMVTYPE, absent when MVDATA makes the macroblock intra.
    pub bmvtype: Option<BmvType>,
    /// Second MVDATA of an interpolated macroblock with coefficients.
    pub mv2: Option<MvData>,
    pub ac_pred: Option<bool>,
    pub cbpcy: Option<Bounded<63>>,
    pub ttmb: Option<Bounded<15>>,
    pub blocks: [PBlock; 6],
}

/// What follows the motion vector data of a coded B macroblock.
enum BShape {
    /// Nothing: no coefficients and not intra.
    End,
    /// Intra without coefficients: ACPRED, then six DC-only intra blocks.
    IntraNoCoeffs,
    /// Interpolated: a second MVDATA decides the rest.
    NeedSecondMv,
    /// CBPCY and blocks follow.
    Blocks { intra: bool },
}

/// Shape after MVDATA, and after the second MVDATA once it is known.
fn b_shape(c: &PContext, mv: &MvData, bmvtype: Option<BmvType>, mv2: Option<&MvData>) -> BShape {
    let m1 = c.mv.meaning(mv);
    if !m1.has_coeffs && !m1.intra {
        return BShape::End;
    }
    if m1.intra && !m1.has_coeffs {
        return BShape::IntraNoCoeffs;
    }
    if bmvtype != Some(BmvType::Interpolated) {
        return BShape::Blocks { intra: m1.intra };
    }
    match mv2.map(|d| c.mv.meaning(d)) {
        None => BShape::NeedSecondMv,
        Some(m2) if !m2.has_coeffs => BShape::End,
        Some(m2) => BShape::Blocks { intra: m2.intra },
    }
}

/// CBPCY of a macroblock that sends none: every block is uncoded, as
/// FFmpeg sets cbp to 0.
fn cbp_or_uncoded(cbpcy: Option<Bounded<63>>) -> u8 {
    cbpcy.map_or(0, |v| v.get())
}

/// Prediction of one B macroblock as its syntax states it, following
/// FFmpeg's vc1_decode_b_mb. Differentials are in quarter samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BMotion {
    Intra,
    Direct,
    Forward(Mv),
    Backward(Mv),
    Interpolated {
        forward: Mv,
        backward: Mv,
    },
    /// Interpolated, with a second MVDATA that is intra and has no
    /// coefficients. FFmpeg then stores zero vectors for the macroblock and
    /// compensates with the vectors left by the previous macroblock.
    StaleInterpolated,
}

/// What reconstruction needs from one parsed B macroblock beyond its
/// syntax.
pub(crate) struct DerivedBMb {
    pub motion: BMotion,
    pub layouts: [Option<InterLayout>; 6],
}

/// Motion vector range of a B picture (FFmpeg's range_x and range_y).
#[derive(Clone, Copy)]
pub(crate) struct BVectorRange {
    pub x: i32,
    pub y: i32,
}

impl BLayer {
    /// `backward_first` is BFRACTION >= 1/2, where BMVTYPE's first code
    /// means backward.
    pub(crate) fn derive(
        &self,
        h: &PictureHeader,
        b: &BHeader,
        ctx: &FrameContext,
        backward_first: bool,
    ) -> Result<(Vec<DerivedBMb>, BVectorRange), FrameError> {
        let dims = ctx.mb_dims();
        if self.macroblocks.len() != dims.count() {
            return Err(FrameError::MacroblockCount {
                expected: dims.count(),
                found: self.macroblocks.len(),
            });
        }
        let c = PContext::for_b(h, b, ctx)?;
        let scale = if c.mv.quarter_sample { 1 } else { 2 };
        let diff = |d: &MvData| {
            let m = c.mv.meaning(d).diff;
            Mv {
                x: m.x * scale,
                y: m.y * scale,
            }
        };
        let one_way = |t: BmvType, d: Mv| match (t, backward_first) {
            (BmvType::First, true) | (BmvType::Second, false) => BMotion::Backward(d),
            _ => BMotion::Forward(d),
        };
        let mut derived = Vec::with_capacity(self.macroblocks.len());
        for (i, mb) in self.macroblocks.iter().enumerate() {
            let (motion, layouts) = match &mb.body {
                BMbBody::SkippedDirect => (BMotion::Direct, std::array::from_fn(|_| None)),
                BMbBody::Skipped { bmvtype } => (
                    match bmvtype {
                        BmvType::Interpolated => BMotion::Interpolated {
                            forward: Mv::default(),
                            backward: Mv::default(),
                        },
                        t => one_way(*t, Mv::default()),
                    },
                    std::array::from_fn(|_| None),
                ),
                BMbBody::Direct(d) => (BMotion::Direct, block_layouts(&c, i, &d.blocks, d.ttmb)?),
                BMbBody::Coded(m) => {
                    let first = c.mv.meaning(&m.mv);
                    let second = m.mv2.map(|d| c.mv.meaning(&d));
                    let motion = match (m.bmvtype, second) {
                        (None, _) => BMotion::Intra,
                        (Some(BmvType::Interpolated), Some(s)) if s.intra && !s.has_coeffs => {
                            BMotion::StaleInterpolated
                        }
                        (Some(BmvType::Interpolated), Some(s)) if s.intra => BMotion::Intra,
                        // Without a second MVDATA the forward differential
                        // is zero (FFmpeg clears dmv_x[0] for BMVTYPE 2).
                        (Some(BmvType::Interpolated), _) => BMotion::Interpolated {
                            forward: m.mv2.as_ref().map_or(Mv::default(), diff),
                            backward: diff(&m.mv),
                        },
                        (Some(_), _) if first.intra => BMotion::Intra,
                        (Some(t), _) => one_way(t, diff(&m.mv)),
                    };
                    (motion, block_layouts(&c, i, &m.blocks, m.ttmb)?)
                }
            };
            derived.push(DerivedBMb { motion, layouts });
        }
        let range = BVectorRange {
            x: c.mv.range.range_x(),
            y: c.mv.range.range_y(),
        };
        Ok((derived, range))
    }

    pub fn read(
        r: &mut BitReader,
        h: &PictureHeader,
        b: &BHeader,
        ctx: &FrameContext,
    ) -> Result<Self, FrameError> {
        let mut c = PContext::for_b(h, b, ctx)?;
        let direct = Plane::of(Some(&b.directmb), c.dims)?;
        let mut macroblocks = Vec::new();
        for i in 0..c.dims.count() {
            macroblocks.push(read_b_mb(r, &mut c, &direct, i)?);
        }
        Ok(BLayer {
            macroblocks,
            trailing: OpaqueBits::rest(r),
        })
    }

    pub fn write(
        &self,
        w: &mut BitWriter,
        h: &PictureHeader,
        b: &BHeader,
        ctx: &FrameContext,
    ) -> Result<(), FrameError> {
        let mut c = PContext::for_b(h, b, ctx)?;
        let direct = Plane::of(Some(&b.directmb), c.dims)?;
        if self.macroblocks.len() != c.dims.count() {
            return Err(FrameError::MacroblockCount {
                expected: c.dims.count(),
                found: self.macroblocks.len(),
            });
        }
        for (i, mb) in self.macroblocks.iter().enumerate() {
            write_b_mb(w, &mut c, &direct, i, mb).map_err(|f| locate(f, i))?;
        }
        self.trailing.write(w);
        Ok(())
    }
}

/// Reads the six blocks of a B macroblock: intra blocks take the CBPCY bit
/// as their coded flag, inter blocks are present only when coded.
fn read_b_blocks(
    r: &mut BitReader,
    c: &mut PContext,
    intra: bool,
    cbp: u8,
    ttmb: Option<Bounded<15>>,
) -> Result<[PBlock; 6], FrameError> {
    let mut state = c.initial_transform(ttmb);
    let mut first = true;
    let mut blocks: [PBlock; 6] = Default::default();
    for (i, block) in blocks.iter_mut().enumerate() {
        let coded = cbp_bit(cbp, i);
        if intra {
            let cs = intra_set(c, i);
            *block = PBlock::Intra(read_intra_block(r, &mut c.coeffs, cs, i >= 4, coded)?);
        } else if coded {
            let s = state.expect(TTMB_PRECEDES);
            *block = PBlock::Inter(read_inter_block(r, c, s, first)?);
            state = Some(next_transform(s, c.ttmbf()));
            first = false;
        }
    }
    Ok(blocks)
}

fn write_b_blocks(
    w: &mut BitWriter,
    c: &mut PContext,
    intra: bool,
    cbp: u8,
    ttmb: Option<Bounded<15>>,
    blocks: &[PBlock; 6],
) -> MbResult {
    let mut state = c.initial_transform(ttmb);
    let mut first = true;
    for (i, block) in blocks.iter().enumerate() {
        let coded = cbp_bit(cbp, i);
        match (block, intra, coded) {
            (PBlock::Intra(b), true, _) => {
                let cs = intra_set(c, i);
                write_intra_block(w, &mut c.coeffs, cs, i >= 4, coded, b).map_err(at(i))?;
            }
            (PBlock::Inter(b), false, true) => {
                let s = state.ok_or(Located {
                    fault: Fault::Mb(MbFault::Ttmb),
                    block: i,
                })?;
                write_inter_block(w, c, s, first, b).map_err(at(i))?;
                state = Some(next_transform(s, c.ttmbf()));
                first = false;
            }
            (PBlock::Empty, false, false) => {}
            _ => {
                return Err(Located {
                    fault: Fault::Block(BlockFault::CoefficientList),
                    block: i,
                });
            }
        }
    }
    Ok(())
}

fn read_b_mb(
    r: &mut BitReader,
    c: &mut PContext,
    direct_plane: &Plane,
    i: usize,
) -> Result<BMb, FrameError> {
    let direct = read_flag(r, direct_plane, i)?;
    let skipped = read_flag(r, &c.skip, i)?;
    let body = match (direct.value, skipped.value) {
        (true, true) => BMbBody::SkippedDirect,
        (false, true) => BMbBody::Skipped {
            bmvtype: BmvType::read(r)?,
        },
        (true, false) => {
            let cbpcy: Bounded<63> = Bounded::from_field(CBPCY_P[c.cbptab].read(r)?.into());
            let ttmb = if c.ttmbf() {
                None
            } else {
                Some(Bounded::from_field(TTMB[c.tt_class.0].read(r)?.into()))
            };
            let blocks = read_b_blocks(r, c, false, cbpcy.get(), ttmb)?;
            BMbBody::Direct(BDirectMb {
                cbpcy,
                ttmb,
                blocks,
            })
        }
        (false, false) => BMbBody::Coded(read_b_coded(r, c)?),
    };
    Ok(BMb {
        direct_bit: direct.raw,
        skip_bit: skipped.raw,
        body,
    })
}

fn read_b_coded(r: &mut BitReader, c: &mut PContext) -> Result<BCodedMb, FrameError> {
    let mv = c.mv.read(r)?;
    let bmvtype = if c.mv.meaning(&mv).intra {
        None
    } else {
        Some(BmvType::read(r)?)
    };
    let mut mb = BCodedMb {
        mv,
        bmvtype,
        mv2: None,
        ac_pred: None,
        cbpcy: None,
        ttmb: None,
        blocks: Default::default(),
    };
    let mut shape = b_shape(c, &mv, bmvtype, None);
    if let BShape::NeedSecondMv = shape {
        let mv2 = c.mv.read(r)?;
        mb.mv2 = Some(mv2);
        shape = b_shape(c, &mv, bmvtype, Some(&mv2));
    }
    match shape {
        BShape::End | BShape::NeedSecondMv => {}
        BShape::IntraNoCoeffs => {
            mb.ac_pred = Some(r.bit()?);
            mb.blocks = read_b_blocks(r, c, true, cbp_or_uncoded(None), None)?;
        }
        BShape::Blocks { intra } => {
            if intra {
                mb.ac_pred = Some(r.bit()?);
            }
            let cbpcy: Bounded<63> = Bounded::from_field(CBPCY_P[c.cbptab].read(r)?.into());
            mb.cbpcy = Some(cbpcy);
            if !c.ttmbf() && !intra {
                mb.ttmb = Some(Bounded::from_field(TTMB[c.tt_class.0].read(r)?.into()));
            }
            mb.blocks = read_b_blocks(r, c, intra, cbpcy.get(), mb.ttmb)?;
        }
    }
    Ok(mb)
}

fn write_b_mb(
    w: &mut BitWriter,
    c: &mut PContext,
    direct_plane: &Plane,
    i: usize,
    mb: &BMb,
) -> MbResult {
    let direct = write_flag(w, direct_plane, i, mb.direct_bit).map_err(at(0))?;
    let skipped = write_flag(w, &c.skip, i, mb.skip_bit).map_err(at(0))?;
    match (&mb.body, direct, skipped) {
        (BMbBody::SkippedDirect, true, true) => Ok(()),
        (BMbBody::Skipped { bmvtype }, false, true) => {
            bmvtype.write(w);
            Ok(())
        }
        (BMbBody::Direct(d), true, false) => {
            CBPCY_P[c.cbptab]
                .write(w, d.cbpcy.get().into())
                .map_err(|e| at(0)(Fault::Frame(e)))?;
            let ttmbf = c.ttmbf();
            let tt_class = c.tt_class;
            write_present(!ttmbf, d.ttmb, MbFault::Ttmb, |v| {
                Ok(TTMB[tt_class.0].write(w, v.get().into())?)
            })
            .map_err(at(0))?;
            write_b_blocks(w, c, false, d.cbpcy.get(), d.ttmb, &d.blocks)
        }
        (BMbBody::Coded(m), false, false) => write_b_coded(w, c, m),
        _ => Err(Located {
            fault: Fault::Mb(MbFault::Kind),
            block: 0,
        }),
    }
}

fn mb_fault(fault: MbFault) -> Located {
    Located {
        fault: Fault::Mb(fault),
        block: 0,
    }
}

fn write_b_coded(w: &mut BitWriter, c: &mut PContext, mb: &BCodedMb) -> MbResult {
    c.mv.write(w, &mb.mv).map_err(at(0))?;
    write_present(
        !c.mv.meaning(&mb.mv).intra,
        mb.bmvtype,
        MbFault::Bmvtype,
        |v| {
            v.write(w);
            Ok(())
        },
    )
    .map_err(at(0))?;
    let mut shape = b_shape(c, &mb.mv, mb.bmvtype, None);
    match (&shape, mb.mv2) {
        (BShape::NeedSecondMv, Some(mv2)) => {
            c.mv.write(w, &mv2).map_err(at(0))?;
            shape = b_shape(c, &mb.mv, mb.bmvtype, Some(&mv2));
        }
        (BShape::NeedSecondMv, None) => return Err(mb_fault(MbFault::SecondMv)),
        (_, Some(_)) => return Err(mb_fault(MbFault::SecondMv)),
        (_, None) => {}
    }
    let empty = mb.blocks.iter().all(|b| *b == PBlock::Empty);
    match shape {
        BShape::End | BShape::NeedSecondMv => {
            if mb.ac_pred.is_some() || mb.cbpcy.is_some() || mb.ttmb.is_some() || !empty {
                return Err(mb_fault(MbFault::Kind));
            }
            Ok(())
        }
        BShape::IntraNoCoeffs => {
            let b = mb.ac_pred.ok_or(mb_fault(MbFault::AcPred))?;
            if mb.cbpcy.is_some() || mb.ttmb.is_some() {
                return Err(mb_fault(MbFault::Cbpcy));
            }
            w.bit(b);
            write_b_blocks(w, c, true, cbp_or_uncoded(None), None, &mb.blocks)
        }
        BShape::Blocks { intra } => {
            write_present(intra, mb.ac_pred, MbFault::AcPred, |b| {
                w.bit(b);
                Ok(())
            })
            .map_err(at(0))?;
            let cbpcy = mb.cbpcy.ok_or(mb_fault(MbFault::Cbpcy))?;
            CBPCY_P[c.cbptab]
                .write(w, cbpcy.get().into())
                .map_err(|e| at(0)(Fault::Frame(e)))?;
            let ttmbf = c.ttmbf();
            let tt_class = c.tt_class;
            write_present(!ttmbf && !intra, mb.ttmb, MbFault::Ttmb, |v| {
                Ok(TTMB[tt_class.0].write(w, v.get().into())?)
            })
            .map_err(at(0))?;
            write_b_blocks(w, c, intra, cbpcy.get(), mb.ttmb, &mb.blocks)
        }
    }
}

/// Which neighbour HYBRIDPRED selects when the prediction sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hybrid {
    /// The top neighbour (HYBRIDPRED 1).
    Above,
    /// The left neighbour (HYBRIDPRED 0).
    Left,
}

/// Vector prediction of the next macroblock as an encoder sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PNext {
    /// The predicted vector without HYBRIDPRED.
    pub predicted: Mv,
    /// When HYBRIDPRED is sent, the vectors its two values select.
    pub hybrid: Option<HybridChoices>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HybridChoices {
    pub above: Mv,
    pub left: Mv,
}

impl PNext {
    /// The vector a zero differential gives with this choice.
    pub(crate) fn with(&self, choice: Hybrid) -> Mv {
        match (self.hybrid, choice) {
            (None, _) => self.predicted,
            (Some(h), Hybrid::Above) => h.above,
            (Some(h), Hybrid::Left) => h.left,
        }
    }
}

/// Macroblock column and row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MbAt {
    pub(crate) x: usize,
    pub(crate) y: usize,
}

impl MbAt {
    pub(crate) fn of(index: usize, mbw: usize) -> Self {
        MbAt {
            x: index % mbw,
            y: index / mbw,
        }
    }
}

/// The vectors a P picture can carry, in quarter samples: each component
/// from `-range` to `range - step`, where `step` is the coded unit. A
/// vector outside wraps to another one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MvBounds {
    x: i32,
    y: i32,
    step: i32,
}

impl MvBounds {
    /// The nearest vector inside the bounds.
    pub(crate) fn clamp(self, mv: Mv) -> Mv {
        Mv {
            x: mv.x.clamp(-self.x, self.x - self.step),
            y: mv.y.clamp(-self.y, self.y - self.step),
        }
    }
}

/// Builds the macroblock layer of a 1MV P picture in raster order for an
/// encoder, applying the reader's prediction so the encoder knows each
/// macroblock's predicted vector before it decides. The header's SKIPMB
/// plane must be raw and MVTYPEMB absent.
pub(crate) struct PLayerBuilder {
    c: PContext,
    macroblocks: Vec<PMb>,
}

impl PLayerBuilder {
    pub(crate) fn new(
        h: &PictureHeader,
        p: &PHeader,
        ctx: &FrameContext,
    ) -> Result<Self, FrameError> {
        let c = PContext::for_p(h, p, ctx)?;
        Ok(PLayerBuilder {
            macroblocks: Vec::with_capacity(c.dims.count()),
            c,
        })
    }

    fn position(&self) -> MbAt {
        MbAt::of(self.macroblocks.len(), usize::from(self.c.dims.width))
    }

    fn prediction_raw(&self) -> Prediction {
        let at = self.position();
        self.c.field.predict(at.x, at.y, 0, true, self.c.rtm)
    }

    /// The vectors the picture can carry.
    pub(crate) fn bounds(&self) -> MvBounds {
        MvBounds {
            x: self.c.mv.range.range_x(),
            y: self.c.mv.range.range_y(),
            step: if self.c.mv.quarter_sample { 1 } else { 2 },
        }
    }

    pub(crate) fn next(&self) -> PNext {
        let p = self.prediction_raw();
        PNext {
            predicted: p.p,
            hybrid: p.hybrid.then_some(HybridChoices {
                above: p.a,
                left: p.c,
            }),
        }
    }

    fn hybrid_bit(pred: &Prediction, choice: Hybrid) -> Option<bool> {
        pred.hybrid.then_some(choice == Hybrid::Above)
    }

    /// Skips the next macroblock; returns its vector. `choice` matters
    /// only when the prediction sends HYBRIDPRED.
    pub(crate) fn skip(&mut self, choice: Hybrid) -> Mv {
        let at = self.position();
        let pred = self.prediction_raw();
        let hybrid = Self::hybrid_bit(&pred, choice);
        let mv = reconstruct(&pred, hybrid, Mv::default(), self.c.mv);
        self.c.field.store(at.x, at.y, 0, true, mv);
        for n in 0..6 {
            self.c.field.set_intra(at.x, at.y, n, false);
        }
        self.macroblocks.push(PMb {
            mvtype_bit: None,
            skip_bit: Some(true),
            body: PMbBody::SkippedOneMv { hybrid },
        });
        mv
    }

    /// Codes the next macroblock as 1MV inter with vector `mv` (quarter
    /// samples; even components in half-sample mode; inside `bounds()`)
    /// and the given blocks, each `PBlock::Inter` with one 8x8 subblock or
    /// `PBlock::Empty`. A vector equal to the prediction without
    /// coefficients is coded as a skip.
    pub(crate) fn one_mv(&mut self, mv: Mv, choice: Hybrid, blocks: [PBlock; 6]) {
        assert_eq!(self.bounds().clamp(mv), mv, "vector outside the MV range");
        let at = self.position();
        let pred = self.prediction_raw();
        let hybrid = Self::hybrid_bit(&pred, choice);
        let base = match hybrid {
            Some(true) => pred.a,
            Some(false) => pred.c,
            None => pred.p,
        };
        let scale = if self.c.mv.quarter_sample { 1 } else { 2 };
        let wrap = |v: i32, r: i32| ((v + r) & (2 * r - 1)) - r;
        let diff = Mv {
            x: wrap(mv.x - base.x, self.c.mv.range.range_x()).div_euclid(scale),
            y: wrap(mv.y - base.y, self.c.mv.range.range_y()).div_euclid(scale),
        };
        let coded: [bool; 6] = std::array::from_fn(|i| matches!(blocks[i], PBlock::Inter(_)));
        let has_coeffs = coded.iter().any(|&c| c);
        let Some(data) = self.c.mv.code(diff, has_coeffs) else {
            self.skip(choice);
            return;
        };
        let v = reconstruct(&pred, hybrid, self.c.mv.meaning(&data).diff, self.c.mv);
        assert_eq!(v, mv, "the decoder reconstructs the coded vector");
        self.c.field.store(at.x, at.y, 0, true, v);
        for n in 0..6 {
            self.c.field.set_intra(at.x, at.y, n, false);
        }
        let cbp = coded
            .iter()
            .enumerate()
            .fold(0u8, |acc, (i, &c)| acc | (u8::from(c) << (5 - i)));
        self.macroblocks.push(PMb {
            mvtype_bit: None,
            skip_bit: Some(false),
            body: PMbBody::OneMv(OneMvMb {
                mv: data,
                hybrid,
                ac_pred: None,
                cbpcy: has_coeffs.then(|| Bounded::from_field(cbp.into())),
                ttmb: None,
                blocks,
            }),
        });
    }

    /// Codes the next macroblock as intra. Its vector counts as zero for
    /// the prediction of later vectors.
    pub(crate) fn intra(&mut self, ac_pred: bool, blocks: [IntraBlock; 6]) {
        let at = self.position();
        self.c.field.store(at.x, at.y, 0, true, Mv::default());
        for n in 0..6 {
            self.c.field.set_intra(at.x, at.y, n, true);
        }
        let coded: [bool; 6] = std::array::from_fn(|i| !blocks[i].ac.is_empty());
        let has_coeffs = coded.iter().any(|&c| c);
        let cbp = coded
            .iter()
            .enumerate()
            .fold(0u8, |acc, (i, &c)| acc | (u8::from(c) << (5 - i)));
        self.macroblocks.push(PMb {
            mvtype_bit: None,
            skip_bit: Some(false),
            body: PMbBody::OneMv(OneMvMb {
                mv: MvData {
                    symbol: Bounded::from_field(MV_INTRA + 37 * u32::from(has_coeffs) - 1),
                    x_bits: 0,
                    y_bits: 0,
                },
                hybrid: None,
                ac_pred: Some(ac_pred),
                cbpcy: has_coeffs.then(|| Bounded::from_field(cbp.into())),
                ttmb: None,
                blocks: blocks.map(PBlock::Intra),
            }),
        });
    }

    pub(crate) fn finish(self) -> InterLayer {
        InterLayer {
            macroblocks: self.macroblocks,
            trailing: OpaqueBits::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coded_differentials_reconstruct_their_vector() {
        let mut escapes = 0;
        let mut classes = 0;
        for quarter_sample in [false, true] {
            for range in 0..4u8 {
                let c = MvCoding {
                    table: 0,
                    quarter_sample,
                    range: MvRange::new(Some(Bounded::new(range).unwrap())),
                };
                let pred = Prediction {
                    p: Mv { x: 12, y: -40 },
                    a: Mv::default(),
                    c: Mv::default(),
                    hybrid: false,
                };
                for has_coeffs in [false, true] {
                    for dy in (-300..=300).step_by(7) {
                        for dx in (-300..=300).step_by(5) {
                            let diff = Mv { x: dx, y: dy };
                            let Some(d) = c.code(diff, has_coeffs) else {
                                assert!(dx == 0 && dy == 0 && !has_coeffs);
                                continue;
                            };
                            let m = c.meaning(&d);
                            assert_eq!(m.has_coeffs, has_coeffs);
                            assert!(!m.intra);
                            if split_index(d.symbol).index == MV_ESCAPE {
                                escapes += 1;
                            } else {
                                classes += 1;
                            }
                            assert_eq!(
                                reconstruct(&pred, None, m.diff, c),
                                reconstruct(&pred, None, diff, c),
                                "qs {quarter_sample} range {range} diff {diff:?} -> {d:?}"
                            );
                        }
                    }
                }
            }
        }
        assert!(escapes > 0 && classes > 0);
    }
}
