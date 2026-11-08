//! P picture reconstruction for progressive Simple and Main profile,
//! transcribed from FFmpeg's vc1_decode_p_mb, vc1_mc.c,
//! ff_vc1_p_overlap_filter and ff_vc1_p_loop_filter.

use crate::wmv3::FrameContext;
use crate::wmv3::coeff::{AcCoeff, CodingSet, IntraBlock, PictureCodingSets};
pub(crate) use crate::wmv3::inter::MbAt;
use crate::wmv3::inter::{DerivedPMb, InterLayer, InterLayout, InterShape, Mv, PBlock, PMbBody};
use crate::wmv3::picture::{MvMode, PHeader, PictureHeader};
use vc1_tables as t;

use super::dsp::{
    Block, Plane, h_loop_filter, h_s_overlap, h_s_overlap_within, put_block, v_loop_filter,
    v_s_overlap, v_s_overlap_within,
};
use super::intra::{BlockAt, IntraIn, Neighbours, PredState, Quant, intra_block, pred_dc_level_p};
use super::itrans;
use super::mc::{self, Rnd, Source};
use super::{BLOCK_MAP, DecodeError, DecodeFeature, Picture, chroma_plane};

/// Intensity compensation tables (INIT_LUT in vc1.c).
#[derive(Debug, Clone)]
pub(crate) struct IntensityLuts {
    luma: [u8; 256],
    chroma: [u8; 256],
}

impl IntensityLuts {
    pub(super) fn new(p: &PHeader) -> Option<Self> {
        let c = p.intensity.as_ref()?;
        let lumscale = i32::from(c.lumscale.get());
        let lumshift = i32::from(c.lumshift.get());
        let (scale, shift) = if lumscale == 0 {
            let extra = if lumshift > 31 { 128 << 6 } else { 0 };
            (-64, (255 - lumshift * 2) * 64 + extra)
        } else {
            let signed = if lumshift > 31 {
                lumshift - 64
            } else {
                lumshift
            };
            (lumscale + 32, signed << 6)
        };
        Some(IntensityLuts {
            luma: std::array::from_fn(|v| {
                ((scale * v as i32 + shift + 32) >> 6).clamp(0, 255) as u8
            }),
            chroma: std::array::from_fn(|v| {
                ((scale * (v as i32 - 128) + 128 * 64 + 32) >> 6).clamp(0, 255) as u8
            }),
        })
    }
}

/// Integer source position, clamped as FFmpeg's ff_vc1_mc_* do for
/// Simple and Main profile: at most `margin` samples before the plane and
/// at most the plane size. The fractional phase is kept.
fn clamp_source(pos: usize, mv: i32, size: usize, margin: isize) -> isize {
    (pos as isize + (mv >> 2) as isize).clamp(-margin, size as isize)
}

/// A chroma motion vector in quarter chroma samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ChromaMv {
    x: i32,
    y: i32,
}

/// FASTUVMC rounding of chroma vectors to half samples.
/// ff_vc1_interp_mc rounds away from zero where ff_vc1_mc_1mv rounds
/// toward zero.
#[derive(Clone, Copy)]
pub(crate) enum FastUv {
    Off,
    TowardZero,
    AwayFromZero,
}

impl FastUv {
    /// The rounding of ff_vc1_mc_1mv and the 4MV chroma path.
    pub(crate) fn of(fastuvmc: bool) -> Self {
        if fastuvmc {
            FastUv::TowardZero
        } else {
            FastUv::Off
        }
    }

    /// The rounding of ff_vc1_interp_mc.
    pub(super) fn interpolated(fastuvmc: bool) -> Self {
        if fastuvmc {
            FastUv::AwayFromZero
        } else {
            FastUv::Off
        }
    }
}

/// Chroma vector from a luma vector. With `FastUv::Off` this is FFmpeg's
/// luma_mv, which the loop filter compares.
pub(super) fn chroma_vector(mv: Mv, fast: FastUv) -> ChromaMv {
    let f = |v: i32| {
        let half = (v + i32::from(v & 3 == 3)) >> 1;
        let odd = half & 1;
        match (fast, half < 0) {
            (FastUv::Off, _) => half,
            (FastUv::TowardZero, true) | (FastUv::AwayFromZero, false) => half + odd,
            (FastUv::TowardZero, false) | (FastUv::AwayFromZero, true) => half - odd,
        }
    };
    ChromaMv {
        x: f(mv.x),
        y: f(mv.y),
    }
}

fn median3(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

fn median4(mut v: [i32; 4]) -> i32 {
    v.sort_unstable();
    (v[1] + v[2]) / 2
}

/// get_chroma_mv: the luma vector a 4MV macroblock's chroma uses, from its
/// inter blocks. `None` when fewer than two are inter.
fn four_mv_chroma_source(d: &DerivedPMb) -> Option<Mv> {
    let inter: Vec<Mv> = (0..4)
        .filter(|&i| !d.intra[i])
        .map(|i| d.vectors[i])
        .collect();
    let axis = |f: fn(&Mv) -> i32| -> i32 {
        let v: Vec<i32> = inter.iter().map(f).collect();
        match v.len() {
            4 => median4([v[0], v[1], v[2], v[3]]),
            3 => median3(v[0], v[1], v[2]),
            _ => (v[0] + v[1]) / 2,
        }
    };
    (inter.len() >= 2).then(|| Mv {
        x: axis(|m| m.x),
        y: axis(|m| m.y),
    })
}

fn is_four_mv(body: &PMbBody) -> bool {
    matches!(body, PMbBody::FourMv(_) | PMbBody::SkippedFourMv { .. })
}

/// The luma vector chroma is predicted from, or `None` when chroma is
/// intra.
fn chroma_source(body: &PMbBody, d: &DerivedPMb) -> Option<Mv> {
    if is_four_mv(body) {
        four_mv_chroma_source(d)
    } else {
        (!d.intra[0]).then_some(d.vectors[0])
    }
}

/// How a prediction enters the picture.
#[derive(Clone, Copy)]
pub(super) enum Write {
    Put,
    /// Rounding average with the prediction already there, for the second
    /// direction of an interpolated B macroblock.
    Average,
}

fn write(plane: &mut Plane, x: usize, y: usize, n: usize, pred: &[u8], how: Write) {
    match how {
        Write::Put => mc::store(plane, x, y, n, pred),
        Write::Average => mc::average(plane, x, y, n, pred),
    }
}

/// Motion compensation from one reference picture.
pub(crate) struct Mc<'a> {
    pub(crate) reference: &'a Picture,
    pub(crate) mspel: bool,
    pub(crate) rnd: Rnd,
    pub(crate) luts: Option<&'a IntensityLuts>,
}

impl Mc<'_> {
    /// The `size` square luma prediction at sample (`x`, `y`).
    fn luma_pixels(&self, x: usize, y: usize, size: usize, mv: Mv) -> Vec<u8> {
        let src = &self.reference.y;
        let source = Source::fetch(
            src,
            clamp_source(x, mv.x, src.width, 16),
            clamp_source(y, mv.y, src.height, 16),
            size,
            self.luts.map(|l| &l.luma),
        );
        if self.mspel {
            mc::mspel(&source, (mv.x & 3) as usize, (mv.y & 3) as usize, self.rnd)
        } else {
            mc::hpel(&source, ((mv.x & 2) >> 1 | (mv.y & 2)) as usize, self.rnd)
        }
    }

    /// The 8x8 prediction of chroma block `block` (4 or 5) of a macroblock.
    fn chroma_pixels(&self, block: usize, at: MbAt, mv: ChromaMv) -> Vec<u8> {
        let src = if block == 4 {
            &self.reference.cb
        } else {
            &self.reference.cr
        };
        let source = Source::fetch(
            src,
            clamp_source(at.x * 8, mv.x, src.width, 8),
            clamp_source(at.y * 8, mv.y, src.height, 8),
            8,
            self.luts.map(|l| &l.chroma),
        );
        mc::chroma(&source, (mv.x & 3) * 2, (mv.y & 3) * 2, self.rnd)
    }

    fn luma(&self, dst: &mut Picture, x: usize, y: usize, size: usize, mv: Mv, how: Write) {
        let pixels = self.luma_pixels(x, y, size, mv);
        write(&mut dst.y, x, y, size, &pixels, how);
    }

    fn chroma(&self, dst: &mut Picture, at: MbAt, mv: ChromaMv, how: Write) {
        for block in [4, 5] {
            let pixels = self.chroma_pixels(block, at, mv);
            write(
                chroma_plane(dst, block),
                at.x * 8,
                at.y * 8,
                8,
                &pixels,
                how,
            );
        }
    }

    /// ff_vc1_mc_1mv, or with `Write::Average` ff_vc1_interp_mc.
    pub(super) fn one_mv(&self, dst: &mut Picture, at: MbAt, mv: Mv, fast: FastUv, how: Write) {
        self.luma(dst, at.x * 16, at.y * 16, 16, mv, how);
        self.chroma(dst, at, chroma_vector(mv, fast), how);
    }

    fn macroblock(
        &self,
        dst: &mut Picture,
        body: &PMbBody,
        d: &DerivedPMb,
        at: MbAt,
        fast: FastUv,
    ) {
        let (x, y) = (at.x * 16, at.y * 16);
        if is_four_mv(body) {
            for block in (0..4).filter(|&b| !d.intra[b]) {
                let (bx, by) = (x + (block & 1) * 8, y + (block >> 1) * 8);
                self.luma(dst, bx, by, 8, d.vectors[block], Write::Put);
            }
        } else if !d.intra[0] {
            self.luma(dst, x, y, 16, d.vectors[0], Write::Put);
        }
        if let Some(mv) = chroma_source(body, d) {
            self.chroma(dst, at, chroma_vector(mv, fast), Write::Put);
        }
    }
}

/// Samples of one macroblock: 16x16 luma and two 8x8 chroma blocks,
/// row-major.
#[derive(Clone)]
pub(crate) struct MbSamples {
    pub(crate) y: [u8; 256],
    pub(crate) cb: [u8; 64],
    pub(crate) cr: [u8; 64],
}

impl Mc<'_> {
    /// The 1MV prediction of the macroblock at `at`, as `one_mv` writes
    /// it, without a picture to write into.
    pub(crate) fn predict_one_mv(&self, at: MbAt, mv: Mv, fast: FastUv) -> MbSamples {
        MbSamples {
            y: self.predict_luma(at, mv),
            cb: self
                .chroma_pixels(4, at, chroma_vector(mv, fast))
                .try_into()
                .expect("8x8 prediction"),
            cr: self
                .chroma_pixels(5, at, chroma_vector(mv, fast))
                .try_into()
                .expect("8x8 prediction"),
        }
    }

    /// The luma part of `predict_one_mv`.
    pub(crate) fn predict_luma(&self, at: MbAt, mv: Mv) -> [u8; 256] {
        self.luma_pixels(at.x * 16, at.y * 16, 16, mv)
            .try_into()
            .expect("16x16 prediction")
    }
}

/// Coefficient positions by transform shape, in FFmpeg's layout.
pub(crate) fn scan(shape: InterShape) -> Vec<usize> {
    let widen = |t: &[u8]| t.iter().map(|&x| usize::from(x)).collect();
    match shape {
        InterShape::EightByEight => (0..64)
            .map(|i| {
                let x = usize::from(t::WMV1_SCANTABLE[0][i]);
                (x >> 3) | ((x & 7) << 3)
            })
            .collect(),
        InterShape::EightByFour => widen(&t::WMV2_SCANTABLEA[..32]),
        InterShape::FourByEight => widen(&t::WMV2_SCANTABLEB[..32]),
        InterShape::FourByFour => widen(&t::VC1_SIMPLE_PROGRESSIVE_4X4_ZZ),
    }
}

/// vc1_decode_p_block dequantization. A coded level of zero (escape mode
/// 3) still takes the non-uniform offset, as in FFmpeg.
fn dequantize(level: i32, q: &Quant) -> i16 {
    let c = (level * (q.pq * 2 + q.halfpq)) as i16;
    if q.uniform {
        c
    } else {
        (i32::from(c) + if c < 0 { -q.pq } else { q.pq }) as i16
    }
}

/// Position of a subblock within its 8x8 block, in samples.
struct SubblockAt {
    dx: usize,
    dy: usize,
}

impl SubblockAt {
    fn new(shape: InterShape, index: usize) -> Self {
        match shape {
            InterShape::EightByEight => SubblockAt { dx: 0, dy: 0 },
            InterShape::EightByFour => SubblockAt {
                dx: 0,
                dy: index * 4,
            },
            InterShape::FourByEight => SubblockAt {
                dx: index * 4,
                dy: 0,
            },
            InterShape::FourByFour => SubblockAt {
                dx: (index & 1) * 4,
                dy: (index >> 1) * 4,
            },
        }
    }

    /// Index of the subblock's first coefficient in the 8x8 block array.
    fn offset(&self) -> usize {
        self.dy * 8 + self.dx
    }
}

/// An inter block's coded residual with how to decode it.
struct Residual<'a> {
    layout: &'a InterLayout,
    subblocks: &'a [Vec<AcCoeff>],
    cs: CodingSet,
}

/// Dequantizes, inverse transforms and adds a residual to the prediction
/// in `plane` at (x, y).
fn add_residual(plane: &mut Plane, x: usize, y: usize, r: &Residual, q: &Quant) {
    let shape = r.layout.shape;
    let positions = scan(shape);
    let mut block: Block = [0; 64];
    let coded = (0..r.layout.coded.len()).filter(|&i| r.layout.coded[i]);
    for (index, coefficients) in coded.zip(r.subblocks) {
        let at = SubblockAt::new(shape, index);
        let off = at.offset();
        let mut position = 0;
        for c in coefficients {
            let v = r.cs.value(c);
            position += v.run as usize;
            if position >= positions.len() {
                break;
            }
            block[off + positions[position]] = dequantize(v.level, q);
            position += 1;
        }
        let dc_only = position == 1;
        let (sx, sy) = (x + at.dx, y + at.dy);
        match (shape, dc_only) {
            (InterShape::EightByEight, true) => itrans::add_8x8_dc(plane, sx, sy, &block),
            (InterShape::EightByEight, false) => itrans::add_8x8(plane, sx, sy, &mut block),
            (InterShape::EightByFour, true) => itrans::add_8x4_dc(plane, sx, sy, &block, off),
            (InterShape::EightByFour, false) => itrans::add_8x4(plane, sx, sy, &mut block, off),
            (InterShape::FourByEight, true) => itrans::add_4x8_dc(plane, sx, sy, &block, off),
            (InterShape::FourByEight, false) => itrans::add_4x8(plane, sx, sy, &mut block, off),
            (InterShape::FourByFour, true) => itrans::add_4x4_dc(plane, sx, sy, &block, off),
            (InterShape::FourByFour, false) => itrans::add_4x4(plane, sx, sy, &mut block, off),
        }
    }
}

fn coded_blocks(body: &PMbBody) -> Option<&[PBlock; 6]> {
    match body {
        PMbBody::OneMv(mb) => Some(&mb.blocks),
        PMbBody::FourMv(mb) => Some(&mb.blocks),
        PMbBody::SkippedOneMv { .. } | PMbBody::SkippedFourMv { .. } => None,
    }
}

/// Block `i` (0..6) of a macroblock: its plane and top left sample.
pub(crate) struct BlockPlace<'a> {
    pub(crate) plane: &'a mut Plane,
    pub(crate) x: usize,
    pub(crate) y: usize,
}

pub(crate) fn block_place(picture: &mut Picture, mb: MbAt, i: usize) -> BlockPlace<'_> {
    match i {
        0..4 => BlockPlace {
            plane: &mut picture.y,
            x: mb.x * 16 + (i & 1) * 8,
            y: mb.y * 16 + (i >> 1) * 8,
        },
        _ => BlockPlace {
            plane: chroma_plane(picture, i),
            x: mb.x * 8,
            y: mb.y * 8,
        },
    }
}

fn layout_of(layouts: &[Option<InterLayout>; 6], i: usize) -> &InterLayout {
    layouts[i]
        .as_ref()
        .expect("derive gives every inter block a layout")
}

/// One macroblock's blocks as the intra and residual passes of P and B
/// pictures see them.
pub(super) struct CodedMb<'a> {
    /// Intra flag per block (FFmpeg's mb_type[0]).
    pub(super) intra: [bool; 6],
    /// The six blocks; `None` for a skipped macroblock.
    pub(super) blocks: Option<&'a [PBlock; 6]>,
    pub(super) ac_pred: bool,
    pub(super) layouts: &'a [Option<InterLayout>; 6],
}

fn p_coded_mbs<'a>(layer: &'a InterLayer, derived: &'a [DerivedPMb]) -> Vec<CodedMb<'a>> {
    layer
        .macroblocks
        .iter()
        .zip(derived)
        .map(|(mb, d)| CodedMb {
            intra: d.intra,
            blocks: coded_blocks(&mb.body),
            // ACPRED is absent in an inter macroblock and in a 4MV
            // macroblock whose intra blocks have no intra neighbour;
            // FFmpeg takes 0.
            ac_pred: match &mb.body {
                PMbBody::OneMv(m) => m.ac_pred.unwrap_or(false),
                PMbBody::FourMv(m) => m.ac_pred.unwrap_or(false),
                PMbBody::SkippedOneMv { .. } | PMbBody::SkippedFourMv { .. } => false,
            },
            layouts: &d.layouts,
        })
        .collect()
}

pub(super) fn add_residuals(picture: &mut Picture, mbs: &[CodedMb], cs: CodingSet, q: &Quant) {
    let mbw = picture.mb_width();
    for (index, mb) in mbs.iter().enumerate() {
        let Some(blocks) = mb.blocks else {
            continue;
        };
        for (i, coded) in blocks.iter().enumerate() {
            let PBlock::Inter(inter) = coded else {
                continue;
            };
            let r = Residual {
                layout: layout_of(mb.layouts, i),
                subblocks: &inter.subblocks,
                cs,
            };
            let place = block_place(picture, MbAt::of(index, mbw), i);
            add_residual(place.plane, place.x, place.y, &r, q);
        }
    }
}

/// Coded 4x4 quarters of a block, FFmpeg's per block cbp nibble: bit 3
/// top left, 2 top right, 1 bottom left, 0 bottom right.
#[derive(Clone, Copy)]
struct Quarters(u8);

impl Quarters {
    const ALL: Quarters = Quarters(15);
    const NONE: Quarters = Quarters(0);

    fn of(layout: &InterLayout) -> Self {
        let c = |i: usize| u8::from(layout.coded[i]);
        Quarters(match layout.shape {
            InterShape::EightByEight => c(0) * 15,
            InterShape::EightByFour => (c(0) * 12) | (c(1) * 3),
            InterShape::FourByEight => (c(0) * 10) | (c(1) * 5),
            InterShape::FourByFour => c(0) << 3 | c(1) << 2 | c(2) << 1 | c(3),
        })
    }

    fn any(self, mask: u8) -> bool {
        self.0 & mask != 0
    }
}

/// Vector a loop filter edge compares; luma and chroma blocks are never
/// compared with each other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockMv {
    Luma(Mv),
    Chroma(ChromaMv),
    /// Intra chroma: the intra flag decides the edge first.
    Intra,
}

/// Per block inputs of ff_vc1_p_loop_filter.
#[derive(Clone, Copy)]
struct FilterBlock {
    cbp: Quarters,
    intra: bool,
    mv: BlockMv,
    shape: Option<InterShape>,
}

fn filter_block(body: &PMbBody, d: &DerivedPMb, i: usize) -> FilterBlock {
    let (cbp, shape) = match coded_blocks(body).map(|b| &b[i]) {
        Some(PBlock::Intra(_)) => (Quarters::ALL, None),
        Some(PBlock::Inter(_)) => {
            let layout = layout_of(&d.layouts, i);
            (Quarters::of(layout), Some(layout.shape))
        }
        Some(PBlock::Empty) | None => (Quarters::NONE, None),
    };
    let mv = match i {
        0..4 => BlockMv::Luma(d.vectors[i]),
        _ => chroma_source(body, d).map_or(BlockMv::Intra, |mv| {
            BlockMv::Chroma(chroma_vector(mv, FastUv::Off))
        }),
    };
    FilterBlock {
        cbp,
        intra: d.intra[i],
        mv,
        shape,
    }
}

/// Top left sample of a block.
struct SampleAt {
    x: usize,
    y: usize,
}

/// An 8x8 block in a plane being loop filtered.
struct EdgeAt<'a> {
    plane: &'a mut Plane,
    x: usize,
    y: usize,
    pq: i32,
}

/// Edge below an 8x8 block and its internal horizontal edge.
fn filter_below(e: EdgeAt, top: FilterBlock, bottom: Option<FilterBlock>) {
    let EdgeAt { plane, x, y, pq } = e;
    if let Some(bottom) = bottom {
        if top.intra || bottom.intra || top.mv != bottom.mv {
            v_loop_filter(plane, x, y + 8, 8, pq);
        } else {
            let mask = (top.cbp.0 | (bottom.cbp.0 >> 2)) & 3;
            if mask & 1 != 0 {
                v_loop_filter(plane, x + 4, y + 8, 4, pq);
            }
            if mask & 2 != 0 {
                v_loop_filter(plane, x, y + 8, 4, pq);
            }
        }
    }
    if matches!(
        top.shape,
        Some(InterShape::FourByFour | InterShape::EightByFour)
    ) {
        if top.cbp.any(5) {
            v_loop_filter(plane, x + 4, y + 4, 4, pq);
        }
        if top.cbp.any(10) {
            v_loop_filter(plane, x, y + 4, 4, pq);
        }
    }
}

/// Edge right of an 8x8 block and its internal vertical edge.
fn filter_right(e: EdgeAt, left: FilterBlock, right: Option<FilterBlock>) {
    let EdgeAt { plane, x, y, pq } = e;
    if let Some(right) = right {
        if left.intra || right.intra || left.mv != right.mv {
            h_loop_filter(plane, x + 8, y, 8, pq);
        } else {
            let mask = (left.cbp.0 | (right.cbp.0 >> 1)) & 5;
            if mask & 1 != 0 {
                h_loop_filter(plane, x + 8, y + 4, 4, pq);
            }
            if mask & 4 != 0 {
                h_loop_filter(plane, x + 8, y, 4, pq);
            }
        }
    }
    if matches!(
        left.shape,
        Some(InterShape::FourByFour | InterShape::FourByEight)
    ) {
        if left.cbp.any(3) {
            h_loop_filter(plane, x + 4, y + 4, 4, pq);
        }
        if left.cbp.any(12) {
            h_loop_filter(plane, x + 4, y, 4, pq);
        }
    }
}

/// Filters all horizontal edges of a plane, then all vertical ones, which
/// is the order FFmpeg's lagged per macroblock filter reproduces.
fn loop_filter(picture: &mut Picture, layer: &InterLayer, derived: &[DerivedPMb], pq: i32) {
    let mbw = picture.mb_width();
    let mbh = derived.len() / mbw;
    let blocks: Vec<[FilterBlock; 6]> = layer
        .macroblocks
        .iter()
        .zip(derived)
        .map(|(mb, d)| std::array::from_fn(|i| filter_block(&mb.body, d, i)))
        .collect();
    let at = |mx: usize, my: usize| &blocks[my * mbw + mx];
    let luma = |mx: usize, my: usize, b: usize| SampleAt {
        x: mx * 16 + (b & 1) * 8,
        y: my * 16 + (b >> 1) * 8,
    };
    for my in 0..mbh {
        for mx in 0..mbw {
            for b in 0..4 {
                let below = match b {
                    0 | 1 => Some(at(mx, my)[b + 2]),
                    _ => (my + 1 < mbh).then(|| at(mx, my + 1)[b - 2]),
                };
                let SampleAt { x, y } = luma(mx, my, b);
                let plane = &mut picture.y;
                filter_below(EdgeAt { plane, x, y, pq }, at(mx, my)[b], below);
            }
        }
    }
    for my in 0..mbh {
        for mx in 0..mbw {
            for b in 0..4 {
                let right = match b {
                    0 | 2 => Some(at(mx, my)[b + 1]),
                    _ => (mx + 1 < mbw).then(|| at(mx + 1, my)[b - 1]),
                };
                let SampleAt { x, y } = luma(mx, my, b);
                let plane = &mut picture.y;
                filter_right(EdgeAt { plane, x, y, pq }, at(mx, my)[b], right);
            }
        }
    }
    for b in [4, 5] {
        for my in 0..mbh {
            for mx in 0..mbw {
                let below = (my + 1 < mbh).then(|| at(mx, my + 1)[b]);
                let (plane, x, y) = (chroma_plane(picture, b), mx * 8, my * 8);
                filter_below(EdgeAt { plane, x, y, pq }, at(mx, my)[b], below);
            }
        }
        for my in 0..mbh {
            for mx in 0..mbw {
                let right = (mx + 1 < mbw).then(|| at(mx + 1, my)[b]);
                let (plane, x, y) = (chroma_plane(picture, b), mx * 8, my * 8);
                filter_right(EdgeAt { plane, x, y, pq }, at(mx, my)[b], right);
            }
        }
    }
}

/// One block of the intra block buffers: macroblock index and coded
/// block number (0..6).
#[derive(Clone, Copy)]
struct BlockRef {
    mb: usize,
    n: usize,
}

/// Two blocks sharing an edge: `first` is left of or above `second`, and
/// `second` is in the same or a later macroblock.
#[derive(Clone, Copy)]
struct Edge {
    first: BlockRef,
    second: BlockRef,
}

fn overlap_edge(blocks: &mut [[Block; 6]], e: Edge, horizontal: bool) {
    let (a, b) = (BLOCK_MAP[e.first.n], BLOCK_MAP[e.second.n]);
    if e.first.mb == e.second.mb {
        let mb = &mut blocks[e.first.mb];
        if horizontal {
            h_s_overlap_within(mb, a, b);
        } else {
            v_s_overlap_within(mb, a, b);
        }
    } else {
        let (head, tail) = blocks.split_at_mut(e.second.mb);
        let (x, y) = (&mut head[e.first.mb][a], &mut tail[0][b]);
        if horizontal {
            h_s_overlap(x, y);
        } else {
            v_s_overlap(x, y);
        }
    }
}

/// ff_vc1_p_overlap_filter over the whole picture: smooths edges between
/// two intra blocks, in FFmpeg's case order, with the vertical pass one
/// macroblock behind the horizontal pass.
pub(crate) fn overlap_filter(blocks: &mut [[Block; 6]], flags: &[[bool; 6]], mbw: usize) {
    let intra = |r: BlockRef| flags[r.mb][r.n];
    let edge = |mb1: usize, n1: usize, mb2: usize, n2: usize| Edge {
        first: BlockRef { mb: mb1, n: n1 },
        second: BlockRef { mb: mb2, n: n2 },
    };
    let apply = |blocks: &mut [[Block; 6]], edges: [Option<Edge>; 6], h: bool| {
        for e in edges.into_iter().flatten() {
            if intra(e.first) && intra(e.second) {
                overlap_edge(blocks, e, h);
            }
        }
    };
    for i in 0..flags.len() {
        let mx = i % mbw;
        let left = mx > 0;
        apply(
            blocks,
            [
                left.then(|| edge(i - 1, 1, i, 0)),
                Some(edge(i, 0, i, 1)),
                left.then(|| edge(i - 1, 3, i, 2)),
                Some(edge(i, 2, i, 3)),
                left.then(|| edge(i - 1, 4, i, 4)),
                left.then(|| edge(i - 1, 5, i, 5)),
            ],
            true,
        );
        let targets = [left.then(|| i - 1), (mx + 1 == mbw).then_some(i)];
        for t in targets.into_iter().flatten() {
            let up = t >= mbw;
            apply(
                blocks,
                [
                    up.then(|| edge(t - mbw, 2, t, 0)),
                    up.then(|| edge(t - mbw, 3, t, 1)),
                    Some(edge(t, 0, t, 2)),
                    Some(edge(t, 1, t, 3)),
                    up.then(|| edge(t - mbw, 4, t, 4)),
                    up.then(|| edge(t - mbw, 5, t, 5)),
                ],
                false,
            );
        }
    }
}

/// Position of block `i` within its prediction grid (luma blocks, or the
/// macroblocks of one chroma plane).
struct GridAt {
    x: usize,
    y: usize,
}

impl GridAt {
    fn new(mb: MbAt, i: usize) -> Self {
        match i {
            0..4 => GridAt {
                x: mb.x * 2 + (i & 1),
                y: mb.y * 2 + (i >> 1),
            },
            _ => GridAt { x: mb.x, y: mb.y },
        }
    }
}

/// Intra flags of one prediction grid, for a_avail and c_avail.
struct IntraGrid {
    width: usize,
    flags: Vec<bool>,
}

impl IntraGrid {
    fn new(width: usize, height: usize) -> Self {
        IntraGrid {
            width,
            flags: vec![false; width * height],
        }
    }

    fn set(&mut self, at: &GridAt, v: bool) {
        self.flags[at.y * self.width + at.x] = v;
    }

    fn neighbours(&self, at: &GridAt) -> Neighbours {
        Neighbours {
            top: at.y != 0 && self.flags[(at.y - 1) * self.width + at.x],
            left: at.x != 0 && self.flags[at.y * self.width + at.x - 1],
        }
    }
}

/// The prediction state, intra flags and coding set of one block's plane
/// class.
struct BlockClass<'a> {
    state: &'a mut PredState,
    grid: &'a IntraGrid,
    cs: CodingSet,
}

/// DC and AC prediction of the intra blocks of a P or B picture, visited
/// in decoding order.
pub(crate) struct PIntra {
    luma: PredState,
    chroma: [PredState; 2],
    luma_intra: IntraGrid,
    chroma_intra: [IntraGrid; 2],
    sets: PictureCodingSets,
    q: Quant,
}

impl PIntra {
    pub(crate) fn new(h: &PictureHeader, ctx: &FrameContext) -> Self {
        let dims = ctx.mb_dims();
        let (mbw, mbh) = (usize::from(dims.width), usize::from(dims.height));
        PIntra {
            luma: PredState::new(mbw * 2, mbh * 2),
            chroma: [PredState::new(mbw, mbh), PredState::new(mbw, mbh)],
            luma_intra: IntraGrid::new(mbw * 2, mbh * 2),
            chroma_intra: [IntraGrid::new(mbw, mbh), IntraGrid::new(mbw, mbh)],
            sets: CodingSet::for_picture(h),
            q: Quant::new(h, ctx),
        }
    }

    fn parts(&mut self, i: usize) -> BlockClass<'_> {
        match i {
            0..4 => BlockClass {
                state: &mut self.luma,
                grid: &self.luma_intra,
                cs: self.sets.intra_luma,
            },
            _ => BlockClass {
                state: &mut self.chroma[i - 4],
                grid: &self.chroma_intra[i - 4],
                cs: self.sets.inter,
            },
        }
    }

    /// Records which blocks of the macroblock at `at` are intra, before
    /// any of them is predicted.
    pub(crate) fn mark(&mut self, at: MbAt, intra: [bool; 6]) {
        for (i, &v) in intra.iter().enumerate() {
            let g = GridAt::new(at, i);
            match i {
                0..4 => self.luma_intra.set(&g, v),
                _ => self.chroma_intra[i - 4].set(&g, v),
            }
        }
    }

    /// Clears the prediction values of block `i`, as FFmpeg does for a
    /// coded block that is not intra.
    pub(crate) fn clear(&mut self, at: MbAt, i: usize) {
        let g = GridAt::new(at, i);
        self.parts(i).state.clear(g.x, g.y);
    }

    fn block_at(at: MbAt, i: usize) -> BlockAt {
        let g = GridAt::new(at, i);
        BlockAt {
            x: g.x,
            y: g.y,
            n: i,
            mb_x: at.x,
            first_row: at.y == 0,
        }
    }

    /// The DC level block `i` of the macroblock at `at` is predicted from.
    pub(crate) fn predict_dc(&mut self, at: MbAt, i: usize) -> i32 {
        let g = GridAt::new(at, i);
        let BlockClass { state, grid, .. } = self.parts(i);
        let n = grid.neighbours(&g);
        pred_dc_level_p(state, &Self::block_at(at, i), n)
    }

    /// Reconstructs intra block `i`: its samples after the inverse
    /// transform, centred on zero, before overlap smoothing.
    pub(crate) fn reconstruct(
        &mut self,
        at: MbAt,
        i: usize,
        ac_pred: bool,
        block: &IntraBlock,
    ) -> Block {
        let g = GridAt::new(at, i);
        let q = self.q;
        let BlockClass { state, grid, cs } = self.parts(i);
        let picture = IntraIn::PPicture(grid.neighbours(&g));
        intra_block(
            state,
            &Self::block_at(at, i),
            &q,
            cs,
            ac_pred,
            picture,
            block,
        )
    }
}

/// Coefficients of every intra block after the inverse transform, before
/// overlap smoothing, in FFmpeg's storage order.
pub(super) fn intra_blocks(
    h: &PictureHeader,
    ctx: &FrameContext,
    mbs: &[CodedMb],
    mbw: usize,
) -> Vec<[Block; 6]> {
    let mut state = PIntra::new(h, ctx);
    let mut out = vec![[[0; 64]; 6]; mbs.len()];
    for (index, mb) in mbs.iter().enumerate() {
        let at = MbAt::of(index, mbw);
        state.mark(at, mb.intra);
        let Some(blocks) = mb.blocks else {
            continue;
        };
        for (i, coded) in blocks.iter().enumerate() {
            match coded {
                PBlock::Intra(block) => {
                    out[index][BLOCK_MAP[i]] = state.reconstruct(at, i, mb.ac_pred, block);
                }
                // FFmpeg zeroes dc_val of every non-intra block.
                PBlock::Inter(_) | PBlock::Empty => state.clear(at, i),
            }
        }
    }
    out
}

pub(super) fn put_intra_blocks(picture: &mut Picture, mbs: &[CodedMb], blocks: &[[Block; 6]]) {
    let mbw = picture.mb_width();
    for (index, mb) in mbs.iter().enumerate() {
        let Some(coded) = mb.blocks else {
            continue;
        };
        for (i, block) in coded.iter().enumerate() {
            if matches!(block, PBlock::Intra(_)) {
                let place = block_place(picture, MbAt::of(index, mbw), i);
                put_block(
                    place.plane,
                    place.x,
                    place.y,
                    &blocks[index][BLOCK_MAP[i]],
                    true,
                );
            }
        }
    }
}

/// A reconstructed P picture with what later B pictures need from it.
pub(super) struct DecodedP {
    pub(super) picture: Picture,
    /// Per macroblock, the vector B direct mode scales (FFmpeg's
    /// motion_val[1] of block 0): the chroma source vector, zero when
    /// chroma is intra.
    pub(super) direct: Vec<Mv>,
    /// FFmpeg's s->mv[0][0] after the last macroblock.
    pub(super) last_forward: Mv,
    pub(super) luts: Option<IntensityLuts>,
}

/// Reconstructs a P picture from its reference.
pub(super) fn decode(
    h: &PictureHeader,
    p: &PHeader,
    layer: &InterLayer,
    ctx: &FrameContext,
    reference: &Picture,
    rnd: Rnd,
) -> Result<DecodedP, DecodeError> {
    if !ctx.seq.res_fasttx {
        return Err(DecodeError::Unsupported(DecodeFeature::SlowTransform));
    }
    if h.rangeredfrm == Some(true) {
        return Err(DecodeError::Unsupported(DecodeFeature::RangeReduction));
    }
    let derived = layer.derive(h, p, ctx)?;
    let q = Quant::new(h, ctx);
    let mbw = usize::from(ctx.mb_dims().width);
    let luts = IntensityLuts::new(p);
    let mc = Mc {
        reference,
        mspel: !matches!(p.effective_mv_mode(q.pquant), MvMode::OneMvHalfPelBilinear),
        rnd,
        luts: luts.as_ref(),
    };
    let fast = FastUv::of(ctx.seq.fastuvmc);
    let mut picture = reference.clone();
    for (i, (mb, d)) in layer.macroblocks.iter().zip(&derived).enumerate() {
        mc.macroblock(&mut picture, &mb.body, d, MbAt::of(i, mbw), fast);
    }
    let mbs = p_coded_mbs(layer, &derived);
    let mut intra = intra_blocks(h, ctx, &mbs, mbw);
    if q.overlap {
        let flags: Vec<[bool; 6]> = derived.iter().map(|d| d.intra).collect();
        overlap_filter(&mut intra, &flags, mbw);
    }
    put_intra_blocks(&mut picture, &mbs, &intra);
    add_residuals(&mut picture, &mbs, CodingSet::for_picture(h).inter, &q);
    if ctx.seq.loop_filter {
        loop_filter(&mut picture, layer, &derived, q.pq);
    }
    let direct = layer
        .macroblocks
        .iter()
        .zip(&derived)
        .map(|(mb, d)| chroma_source(&mb.body, d).unwrap_or_default())
        .collect();
    let last_forward = derived.last().map_or(Mv::default(), |d| d.vectors[0]);
    Ok(DecodedP {
        picture,
        direct,
        last_forward,
        luts,
    })
}
