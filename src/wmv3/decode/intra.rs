//! I and BI picture reconstruction, following FFmpeg's vc1_decode_i_blocks:
//! blocks are transformed into a ring of macroblock buffers, overlap
//! smoothed there, written to the picture one row and column late, and loop
//! filtered one further row and column late. The order matters because the
//! filters round and overlap one another.

use vc1_tables as t;

use super::dsp::{
    Block, h_loop_filter, h_s_overlap, h_s_overlap_within, inv_trans_8x8, put_block, v_loop_filter,
    v_s_overlap, v_s_overlap_within,
};
use super::{BLOCK_MAP, Picture, chroma_plane};
use crate::wmv3::coeff::{CodingSet, IntraBlock};
use crate::wmv3::intra::IntraLayer;
use crate::wmv3::picture::{PictureHeader, Pquant};
use crate::wmv3::{FrameContext, MbDims, QuantizerMode};

/// Values with a one-entry zero border above and to the left, so that
/// neighbour reads outside the picture see zero as FFmpeg's never-written
/// border entries do.
pub(super) struct Grid<T> {
    width: usize,
    data: Vec<T>,
}

impl<T: Copy + Default> Grid<T> {
    fn new(width: usize, height: usize) -> Self {
        Grid {
            width: width + 1,
            data: vec![T::default(); (width + 1) * (height + 1)],
        }
    }

    fn at(&self, x: isize, y: isize) -> T {
        self.data[((y + 1) as usize) * self.width + (x + 1) as usize]
    }

    fn set(&mut self, x: usize, y: usize, v: T) {
        self.data[(y + 1) * self.width + x + 1] = v;
    }
}

/// DC and AC prediction state of one plane class (luma blocks or the
/// macroblocks of one chroma plane).
pub(super) struct PredState {
    dc: Grid<i32>,
    /// Per block: first column (0..8) and first row (8..16) of the
    /// unscaled coefficients.
    ac: Grid<[i16; 16]>,
}

impl PredState {
    pub(super) fn new(width: usize, height: usize) -> Self {
        PredState {
            dc: Grid::new(width, height),
            ac: Grid::new(width, height),
        }
    }

    pub(super) fn clear(&mut self, x: usize, y: usize) {
        self.dc.set(x, y, 0);
        self.ac.set(x, y, [0; 16]);
    }
}

/// FFmpeg's dcpred table: the DC value of mid grey at each DC scale.
const DC_PRED: [i32; 32] = [
    -1, 1024, 512, 341, 256, 205, 171, 146, 128, 114, 102, 93, 85, 79, 73, 68, 64, 60, 57, 54, 51,
    49, 47, 45, 43, 41, 39, 38, 37, 35, 34, 33,
];

/// Prediction direction for DC and AC.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Left,
    Top,
}

/// Scan tables in FFmpeg's transposed layout (ff_vc1_init_transposed_scantables).
/// Table 1 is the intra scan without AC prediction.
pub(crate) fn scan(table: usize) -> [usize; 64] {
    std::array::from_fn(|i| {
        let x = usize::from(t::WMV1_SCANTABLE[table][i]);
        (x >> 3) | ((x & 7) << 3)
    })
}

/// Parameters that apply to every block of the picture.
#[derive(Clone, Copy)]
pub(crate) struct Quant {
    pub(crate) pquant: Pquant,
    pub(crate) pq: i32,
    pub(crate) halfpq: i32,
    /// PQUANTIZER: uniform quantizer when set.
    pub(crate) uniform: bool,
    pub(crate) dc_scale: i32,
    /// Overlap smoothing applies, and intra blocks are stored centred on
    /// zero (put with +128).
    pub(crate) overlap: bool,
}

impl Quant {
    pub(crate) fn new(h: &PictureHeader, ctx: &FrameContext) -> Self {
        let pq = h.pquant(ctx.seq.quantizer);
        let uniform = match ctx.seq.quantizer {
            QuantizerMode::Implicit => h.pqindex.get() < 9,
            QuantizerMode::Explicit => h.pquantizer == Some(true),
            QuantizerMode::NonUniform => false,
            QuantizerMode::Uniform => true,
        };
        Quant {
            pquant: pq,
            pq: pq.get().into(),
            halfpq: i32::from(h.halfqp == Some(true)),
            uniform,
            dc_scale: t::WMV3_DC_SCALE_TABLE[usize::from(pq.get())].into(),
            overlap: ctx.seq.overlap && pq.get() >= 9,
        }
    }
}

/// Block position for prediction purposes.
pub(super) struct BlockAt {
    pub x: usize,
    pub y: usize,
    /// Block number within its macroblock (0..6).
    pub n: usize,
    pub mb_x: usize,
    pub first_row: bool,
}

/// vc1_i_pred_dc: prediction and direction.
fn pred_dc(s: &PredState, b: &BlockAt, q: &Quant) -> (i32, Dir) {
    let (x, y) = (b.x as isize, b.y as isize);
    let mut a = s.dc.at(x, y - 1);
    let mut bb = s.dc.at(x - 1, y - 1);
    let mut c = s.dc.at(x - 1, y);
    let outer = if q.overlap {
        0
    } else {
        DC_PRED[q.dc_scale as usize]
    };
    if b.first_row && b.n != 2 && b.n != 3 {
        bb = outer;
        a = outer;
    }
    if b.mb_x == 0 && b.n != 1 && b.n != 3 {
        bb = outer;
        c = outer;
    }
    if (a - bb).abs() <= (bb - c).abs() {
        (c, Dir::Left)
    } else {
        (a, Dir::Top)
    }
}

/// Intra neighbours of a block in a P picture (FFmpeg's a_avail and
/// c_avail).
#[derive(Clone, Copy)]
pub(super) struct Neighbours {
    pub top: bool,
    pub left: bool,
}

/// Picture an intra block belongs to. Intra blocks of P pictures predict
/// only from intra neighbours and always use the progressive scan.
#[derive(Clone, Copy)]
pub(super) enum IntraIn {
    IPicture,
    PPicture(Neighbours),
}

/// ff_vc1_pred_dc without DQUANT, so no predictor rescaling.
fn pred_dc_p(s: &PredState, b: &BlockAt, n: Neighbours) -> (i32, Dir) {
    let (x, y) = (b.x as isize, b.y as isize);
    let a = s.dc.at(x, y - 1);
    let bb = s.dc.at(x - 1, y - 1);
    let c = s.dc.at(x - 1, y);
    if n.left && (!n.top || (a - bb).abs() <= (bb - c).abs()) {
        (c, Dir::Left)
    } else if n.top {
        (a, Dir::Top)
    } else {
        (0, Dir::Left)
    }
}

/// The DC level an intra block of a P or B picture is predicted from.
pub(super) fn pred_dc_level_p(s: &PredState, b: &BlockAt, n: Neighbours) -> i32 {
    pred_dc_p(s, b, n).0
}

/// DC prediction of an I or BI picture as vc1_decode_i_block computes it,
/// for an encoder choosing DC differentials. Blocks must be visited in
/// decoding order, each `predict` followed by `store`.
pub(crate) struct DcPredictor {
    luma: PredState,
    chroma: [PredState; 2],
    q: Quant,
}

impl DcPredictor {
    pub(crate) fn new(h: &PictureHeader, ctx: &FrameContext) -> Self {
        let dims = ctx.mb_dims();
        let (mbw, mbh) = (usize::from(dims.width), usize::from(dims.height));
        DcPredictor {
            luma: PredState::new(mbw * 2, mbh * 2),
            chroma: [PredState::new(mbw, mbh), PredState::new(mbw, mbh)],
            q: Quant::new(h, ctx),
        }
    }

    fn at(mb_x: usize, mb_y: usize, n: usize) -> BlockAt {
        let (x, y) = if n < 4 {
            (mb_x * 2 + (n & 1), mb_y * 2 + (n >> 1))
        } else {
            (mb_x, mb_y)
        };
        BlockAt {
            x,
            y,
            n,
            mb_x,
            first_row: mb_y == 0,
        }
    }

    fn state(&self, n: usize) -> &PredState {
        if n < 4 {
            &self.luma
        } else {
            &self.chroma[n - 4]
        }
    }

    /// Predicted DC level of block `n` (0..6) of a macroblock.
    pub(crate) fn predict(&self, mb_x: usize, mb_y: usize, n: usize) -> i32 {
        pred_dc(self.state(n), &Self::at(mb_x, mb_y, n), &self.q).0
    }

    /// Records the block's reconstructed DC level.
    pub(crate) fn store(&mut self, mb_x: usize, mb_y: usize, n: usize, level: i32) {
        let at = Self::at(mb_x, mb_y, n);
        let state = if n < 4 {
            &mut self.luma
        } else {
            &mut self.chroma[n - 4]
        };
        state.dc.set(at.x, at.y, level);
    }
}

/// Reconstructs one intra block's coefficients (vc1_decode_i_block).
pub(super) fn intra_block(
    s: &mut PredState,
    at: &BlockAt,
    q: &Quant,
    cs: CodingSet,
    ac_pred: bool,
    picture: IntraIn,
    coded: &IntraBlock,
) -> Block {
    let mut block: Block = [0; 64];
    let (pred, dir) = match picture {
        IntraIn::PPicture(n) => pred_dc_p(s, at, n),
        IntraIn::IPicture => pred_dc(s, at, q),
    };
    // Without an intra neighbour there is nothing to predict AC from.
    let ac_pred = ac_pred
        && match picture {
            IntraIn::PPicture(n) => n.top || n.left,
            IntraIn::IPicture => true,
        };
    let dc = coded.dc.value(q.pquant) + pred;
    s.dc.set(at.x, at.y, dc);
    block[0] = (dc * q.dc_scale) as i16;

    let src = match dir {
        Dir::Left => s.ac.at(at.x as isize - 1, at.y as isize),
        Dir::Top => s.ac.at(at.x as isize, at.y as isize - 1),
    };
    let (sh, half) = match dir {
        Dir::Left => (0, 0),
        Dir::Top => (3, 8),
    };
    let scale = q.pq * 2 + q.halfpq;
    // Coded coefficients take the non-uniform offset unconditionally once
    // known non-zero; predicted ones only when non-zero after scaling.
    let offset = |v: i32| if v < 0 { -q.pq } else { q.pq };
    let requant_coded = |v: i32| -> i16 {
        let v = i32::from((v * scale) as i16);
        if q.uniform {
            v as i16
        } else {
            (v + offset(v)) as i16
        }
    };
    let requant_predicted = |v: i32| -> i16 {
        let v = i32::from((v * scale) as i16);
        if q.uniform || v == 0 {
            v as i16
        } else {
            (v + offset(v)) as i16
        }
    };
    let mut saved = [0i16; 16];
    if !coded.ac.is_empty() {
        let zz = scan(match (picture, ac_pred, dir) {
            (IntraIn::PPicture(_), _, _) => 0,
            (IntraIn::IPicture, false, _) => 1,
            (IntraIn::IPicture, true, Dir::Top) => 2,
            (IntraIn::IPicture, true, Dir::Left) => 3,
        });
        let mut i = 1u32;
        for c in &coded.ac {
            let v = cs.value(c);
            i += v.run;
            if i > 63 {
                break;
            }
            block[zz[i as usize]] = v.level as i16;
            i += 1;
        }
        if ac_pred {
            for k in 1..8 {
                let p = &mut block[k << sh];
                *p = (i32::from(*p) + i32::from(src[half + k])) as i16;
            }
        }
        for k in 1..8 {
            saved[k] = block[k];
            saved[k + 8] = block[k << 3];
        }
        for v in block.iter_mut().skip(1) {
            if *v != 0 {
                *v = requant_coded(i32::from(*v));
            }
        }
    } else if ac_pred {
        saved[half..half + 8].copy_from_slice(&src[half..half + 8]);
        for k in 1..8 {
            block[k << sh] = requant_predicted(i32::from(src[half + k]));
        }
    }
    s.ac.set(at.x, at.y, saved);
    inv_trans_8x8(&mut block);
    block
}

/// Blocks of one macroblock in FFmpeg's storage order (see BLOCK_MAP).
type MbBlocks = [Block; 6];

/// H overlap cases of vc1_h_overlap_filter for progressive pictures, as
/// (left storage index, right storage index); `None` uses the left
/// macroblock for the left block.
fn h_overlap(left: Option<&mut MbBlocks>, cur: &mut MbBlocks, case: usize) {
    match (case, left) {
        (0, Some(l)) => h_s_overlap(&mut l[2], &mut cur[0]),
        (1, _) => h_s_overlap_within(cur, 0, 2),
        (2, Some(l)) => h_s_overlap(&mut l[3], &mut cur[1]),
        (3, _) => h_s_overlap_within(cur, 1, 3),
        (4 | 5, Some(l)) => h_s_overlap(&mut l[case], &mut cur[case]),
        _ => unreachable!("left edge cases are skipped"),
    }
}

/// V overlap cases of vc1_v_overlap_filter; `top` is `None` when the top
/// and bottom blocks are in the same macroblock buffer.
fn v_overlap(top: Option<&mut MbBlocks>, bottom: &mut MbBlocks, case: usize) {
    match (case, top) {
        (0, Some(t)) => v_s_overlap(&mut t[1], &mut bottom[0]),
        (1, Some(t)) => v_s_overlap(&mut t[3], &mut bottom[2]),
        (2, _) => v_s_overlap_within(bottom, 0, 1),
        (3, _) => v_s_overlap_within(bottom, 2, 3),
        (4 | 5, Some(t)) => v_s_overlap(&mut t[case], &mut bottom[case]),
        _ => unreachable!("top edge cases are skipped on the first row"),
    }
}

/// Ring of macroblock block buffers, FFmpeg's v->block with its four
/// indices.
struct Ring {
    blocks: Vec<MbBlocks>,
    cur: usize,
}

impl Ring {
    fn new(mb_width: usize) -> Self {
        Ring {
            blocks: vec![[[0; 64]; 6]; mb_width + 2],
            cur: 0,
        }
    }

    fn len(&self) -> usize {
        self.blocks.len()
    }

    fn left(&self) -> usize {
        (self.cur + self.len() - 1) % self.len()
    }

    fn top(&self) -> usize {
        (self.cur + 2) % self.len()
    }

    fn topleft(&self) -> usize {
        (self.cur + 1) % self.len()
    }

    fn pair(&mut self, a: usize, b: usize) -> (&mut MbBlocks, &mut MbBlocks) {
        assert_ne!(a, b);
        if a < b {
            let (x, y) = self.blocks.split_at_mut(b);
            (&mut x[a], &mut y[0])
        } else {
            let (x, y) = self.blocks.split_at_mut(a);
            (&mut y[0], &mut x[b])
        }
    }
}

/// ff_vc1_i_overlap_filter for progressive Simple/Main pictures.
fn overlap_filter(ring: &mut Ring, mb_x: usize, mb_width: usize, first_row: bool) {
    let (cur, left, top, topleft) = (ring.cur, ring.left(), ring.top(), ring.topleft());
    for case in 0..6 {
        if mb_x == 0 && (case & 5) != 1 {
            continue;
        }
        if mb_x == 0 {
            h_overlap(None, &mut ring.blocks[cur], case);
        } else {
            let (l, c) = ring.pair(left, cur);
            h_overlap(Some(l), c, case);
        }
    }
    for case in 0..6 {
        if first_row && case & 2 == 0 {
            continue;
        }
        if mb_x != 0 {
            if first_row {
                v_overlap(None, &mut ring.blocks[left], case);
            } else {
                let (t, b) = ring.pair(topleft, left);
                v_overlap(Some(t), b, case);
            }
        }
        if mb_x == mb_width - 1 {
            if first_row {
                v_overlap(None, &mut ring.blocks[cur], case);
            } else {
                let (t, b) = ring.pair(top, cur);
                v_overlap(Some(t), b, case);
            }
        }
    }
}

/// Writes all six blocks of a macroblock's buffer to the picture.
fn put_mb(pic: &mut Picture, blocks: &MbBlocks, mb_x: usize, mb_y: usize, signed: bool) {
    for (i, &storage) in BLOCK_MAP.iter().enumerate() {
        if i < 4 {
            let x = mb_x * 16 + (i & 1) * 8;
            let y = mb_y * 16 + (i & 2) * 4;
            put_block(&mut pic.y, x, y, &blocks[storage], signed);
        } else {
            put_block(
                chroma_plane(pic, i),
                mb_x * 8,
                mb_y * 8,
                &blocks[storage],
                signed,
            );
        }
    }
}

/// vc1_put_blocks_clamped for I pictures, where every block is intra.
fn put_lagged(pic: &mut Picture, ring: &Ring, mb: MbPos, dims: MbDims, signed: bool) {
    let last_x = usize::from(dims.width) - 1;
    let last_y = usize::from(dims.height) - 1;
    if mb.y != 0 {
        if mb.x != 0 {
            put_mb(
                pic,
                &ring.blocks[ring.topleft()],
                mb.x - 1,
                mb.y - 1,
                signed,
            );
        }
        if mb.x == last_x {
            put_mb(pic, &ring.blocks[ring.top()], mb.x, mb.y - 1, signed);
        }
    }
    if mb.y == last_y {
        if mb.x != 0 {
            put_mb(pic, &ring.blocks[ring.left()], mb.x - 1, mb.y, signed);
        }
        if mb.x == last_x {
            put_mb(pic, &ring.blocks[ring.cur], mb.x, mb.y, signed);
        }
    }
}

#[derive(Clone, Copy)]
struct MbPos {
    x: usize,
    y: usize,
}

/// Picture edge flags of the I loop filter.
#[derive(Clone, Copy, Default)]
struct Edges {
    left: bool,
    top: bool,
}

/// vc1_i_v_loop_filter: horizontal edges of one block of a macroblock.
fn i_v_loop(pic: &mut Picture, mb: MbPos, edges: Edges, block: usize, pq: i32) {
    if (block & 5) == 1 {
        return;
    }
    if edges.top && block & 2 == 0 {
        return;
    }
    if block > 3 {
        v_loop_filter(chroma_plane(pic, block), mb.x * 8, mb.y * 8, 8, pq);
    } else {
        let x = mb.x * 16 + (block & 1) * 8;
        let y = mb.y * 16 + (block & 2) * 4;
        v_loop_filter(&mut pic.y, x, y, 16, pq);
    }
}

/// vc1_i_h_loop_filter: vertical edges of one block of a macroblock.
fn i_h_loop(pic: &mut Picture, mb: MbPos, edges: Edges, block: usize, pq: i32) {
    if block & 2 != 0 {
        return;
    }
    if edges.left && (block & 5) != 1 {
        return;
    }
    if block > 3 {
        h_loop_filter(chroma_plane(pic, block), mb.x * 8, mb.y * 8, 8, pq);
    } else {
        let x = mb.x * 16 + (block & 1) * 8;
        let y = mb.y * 16 + (block & 2) * 4;
        h_loop_filter(&mut pic.y, x, y, 16, pq);
    }
}

/// ff_vc1_i_loop_filter over a whole picture whose macroblocks are all
/// final, as in B pictures, where FFmpeg runs it after each macroblock.
/// Each lagged step reads and writes only macroblocks at or before its
/// position, so running the steps after reconstruction gives the same
/// result.
pub(super) fn loop_filter_picture(pic: &mut Picture, dims: MbDims, pq: i32) {
    let mbw = usize::from(dims.width);
    for index in 0..dims.count() {
        let mb = MbPos {
            x: index % mbw,
            y: index / mbw,
        };
        loop_filter_lagged(pic, mb, dims, pq);
    }
}

/// ff_vc1_i_loop_filter for progressive Simple/Main pictures.
fn loop_filter_lagged(pic: &mut Picture, mb: MbPos, dims: MbDims, pq: i32) {
    let last_x = usize::from(dims.width) - 1;
    let last_y = usize::from(dims.height) - 1;
    let v_mb = |pic: &mut Picture, x: usize, y: usize, top: bool| {
        for i in 0..6 {
            i_v_loop(pic, MbPos { x, y }, Edges { left: false, top }, i, pq);
        }
    };
    let h_mb = |pic: &mut Picture, x: usize, y: usize, left: bool| {
        for i in 0..6 {
            i_h_loop(pic, MbPos { x, y }, Edges { left, top: false }, i, pq);
        }
    };
    if mb.y != 0 {
        let top = mb.y == 1;
        if mb.x != 0 {
            v_mb(pic, mb.x - 1, mb.y - 1, top);
        }
        if mb.x == last_x {
            v_mb(pic, mb.x, mb.y - 1, top);
        }
    }
    if mb.y == last_y {
        let top = mb.y == 0;
        if mb.x != 0 {
            v_mb(pic, mb.x - 1, mb.y, top);
        }
        if mb.x == last_x {
            v_mb(pic, mb.x, mb.y, top);
        }
    }
    if mb.y >= 2 {
        if mb.x != 0 {
            h_mb(pic, mb.x - 1, mb.y - 2, mb.x == 1);
        }
        if mb.x == last_x {
            h_mb(pic, mb.x, mb.y - 2, mb.x == 0);
        }
    }
    if mb.y == last_y {
        if mb.y >= 1 {
            if mb.x != 0 {
                h_mb(pic, mb.x - 1, mb.y - 1, mb.x == 1);
            }
            if mb.x == last_x {
                h_mb(pic, mb.x, mb.y - 1, mb.x == 0);
            }
        }
        if mb.x != 0 {
            h_mb(pic, mb.x - 1, mb.y, mb.x == 1);
        }
        if mb.x == last_x {
            h_mb(pic, mb.x, mb.y, mb.x == 0);
        }
    }
}

/// Reconstructs an I or BI picture.
pub(crate) fn decode(h: &PictureHeader, layer: &IntraLayer, ctx: &FrameContext) -> Picture {
    let dims = ctx.mb_dims();
    let (mbw, mbh) = (usize::from(dims.width), usize::from(dims.height));
    let q = Quant::new(h, ctx);
    let sets = CodingSet::for_picture(h);
    let mut pic = Picture::new(dims);
    let mut luma = PredState::new(mbw * 2, mbh * 2);
    let mut chroma = [PredState::new(mbw, mbh), PredState::new(mbw, mbh)];
    let mut ring = Ring::new(mbw);
    for (index, mb) in layer.macroblocks.iter().enumerate() {
        let pos = MbPos {
            x: index % mbw,
            y: index / mbw,
        };
        let cur = ring.cur;
        for (i, coded) in mb.blocks.iter().enumerate() {
            let (state, at) = if i < 4 {
                (
                    &mut luma,
                    BlockAt {
                        x: pos.x * 2 + (i & 1),
                        y: pos.y * 2 + (i >> 1),
                        n: i,
                        mb_x: pos.x,
                        first_row: pos.y == 0,
                    },
                )
            } else {
                (
                    &mut chroma[i - 4],
                    BlockAt {
                        x: pos.x,
                        y: pos.y,
                        n: i,
                        mb_x: pos.x,
                        first_row: pos.y == 0,
                    },
                )
            };
            let cs = if i < 4 { sets.intra_luma } else { sets.inter };
            ring.blocks[cur][BLOCK_MAP[i]] =
                intra_block(state, &at, &q, cs, mb.ac_pred, IntraIn::IPicture, coded);
        }
        if q.overlap {
            overlap_filter(&mut ring, pos.x, mbw, pos.y == 0);
        }
        put_lagged(&mut pic, &ring, pos, dims, q.overlap);
        if ctx.seq.loop_filter {
            loop_filter_lagged(&mut pic, pos, dims, q.pq);
        }
        ring.cur = (ring.cur + 1) % ring.len();
    }
    pic
}
