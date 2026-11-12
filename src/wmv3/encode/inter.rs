//! P picture encoding with 1MV quarter-sample bicubic motion: motion
//! search on the decoder's own prediction, inter residuals with the 8x8
//! transform, and a skip decision weighing distortion against an estimate
//! of the bits.

use super::intra::{max_dc_diff, quantize_ac};
use super::transform::forward;
use super::{PlaneKind, Yuv420};
use crate::wmv3::coeff::{CodingSet, CoeffValue, DcDiff, Esc3Plan, IntraBlock, PictureCodingSets};
use crate::wmv3::decode::BLOCK_MAP;
use crate::wmv3::decode::Picture;
use crate::wmv3::decode::Rnd;
use crate::wmv3::decode::dsp::put_block;
use crate::wmv3::decode::dsp::{Block, inv_trans_8x8};
use crate::wmv3::decode::intra::Quant;
use crate::wmv3::decode::intra::scan as intra_scan;
use crate::wmv3::decode::p::{FastUv, MbAt, MbSamples, Mc, scan};
use crate::wmv3::decode::p::{PIntra, block_place, overlap_filter};
use crate::wmv3::inter::{Hybrid, InterBlock, InterShape, Mv, MvBounds, PLayerBuilder};
use crate::wmv3::picture::Pquant;
use crate::wmv3::{
    Bitplane, BitplaneCoding, Bounded, Frame, FrameBody, FrameContext, PBlock, PHeader, PMbBody,
    PictureHeader, PictureKind, PqIndex, QuantizerMode, TransformSignal,
};

/// Integer motion search reaches this many luma samples from its centre.
const SEARCH: i32 = 12;

/// Rough bits per coded coefficient and per motion vector component step,
/// for the rate side of decisions.
const BITS_PER_COEFF: f64 = 6.0;
const BITS_PER_SKIP: f64 = 1.0;
const BITS_PER_MV: f64 = 8.0;
const BITS_PER_DC: f64 = 4.0;

fn header(ctx: &FrameContext, pqindex: PqIndex, framecnt: Bounded<3>) -> PictureHeader {
    let seq = ctx.seq;
    let pquant = crate::wmv3::pquant(pqindex, seq.quantizer);
    let zero = || Bounded::new(0).expect("zero is in range");
    PictureHeader {
        interpfrm: seq.finterpflag.then_some(false),
        framecnt,
        rangeredfrm: seq.rangered.then_some(false),
        kind: PictureKind::P(PHeader {
            // 1MV with quarter-sample bicubic motion: index 0 of the
            // low-quantizer row of the MVMODE table, index 1 of the other.
            mvmode: Bounded::new(u8::from(pquant.get() > 12)).expect("0 or 1"),
            intensity: None,
            mvtypemb: None,
            skipmb: Bitplane {
                invert: false,
                coding: BitplaneCoding::Raw,
            },
            mvtab: zero(),
            cbptab: zero(),
            transform: if seq.vstransform {
                TransformSignal::Frame(zero())
            } else {
                TransformSignal::Absent
            },
        }),
        pqindex,
        halfqp: (pqindex.get() <= 8).then_some(false),
        pquantizer: (seq.quantizer == QuantizerMode::Explicit).then_some(true),
        mvrange: seq.extended_mv.then(zero),
        respic: seq.multires.then(zero),
        x8_type: None,
        transacfrm: Bounded::new(0).expect("zero is in range"),
        transacfrm2: None,
        transdctab: false,
    }
}

/// Source samples of one macroblock, edge replicated.
fn source_mb(src: &Yuv420, at: MbAt) -> MbSamples {
    MbSamples {
        y: std::array::from_fn(|i| {
            src.sample(PlaneKind::Luma, at.x * 16 + i % 16, at.y * 16 + i / 16)
        }),
        cb: std::array::from_fn(|i| src.sample(PlaneKind::Cb, at.x * 8 + i % 8, at.y * 8 + i / 8)),
        cr: std::array::from_fn(|i| src.sample(PlaneKind::Cr, at.x * 8 + i % 8, at.y * 8 + i / 8)),
    }
}

fn sad(a: &[u8], b: &[u8]) -> u32 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| u32::from(x.abs_diff(y)))
        .sum()
}

fn ssd(a: &[u8], b: &[u8]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| f64::from(x.abs_diff(y)).powi(2))
        .sum()
}

/// Luma SAD at a vector with whole-sample components, sampling the
/// reference with its edge replicated.
fn integer_sad(reference: &Picture, src: &MbSamples, at: MbAt, mv: Mv) -> u32 {
    let p = &reference.y;
    let (x0, y0) = (at.x as i32 * 16 + mv.x / 4, at.y as i32 * 16 + mv.y / 4);
    let inside = x0 >= 0 && y0 >= 0 && x0 + 16 <= p.width as i32 && y0 + 16 <= p.height as i32;
    if inside {
        let (x0, y0) = (x0 as usize, y0 as usize);
        return (0..16)
            .map(|r| {
                let o = (y0 + r) * p.width + x0;
                sad(&src.y[r * 16..r * 16 + 16], &p.data[o..o + 16])
            })
            .sum();
    }
    let xs: [usize; 16] =
        std::array::from_fn(|c| (x0 + c as i32).clamp(0, p.width as i32 - 1) as usize);
    src.y
        .chunks_exact(16)
        .enumerate()
        .map(|(r, s)| {
            let y = (y0 + r as i32).clamp(0, p.height as i32 - 1) as usize;
            let row = &p.data[y * p.width..(y + 1) * p.width];
            let line: [u8; 16] = std::array::from_fn(|c| row[xs[c]]);
            sad(s, &line)
        })
        .sum()
}

/// Block `n` (0..6) of a macroblock's samples as 64 values.
fn block_of(m: &MbSamples, n: usize) -> [u8; 64] {
    match n {
        0..4 => std::array::from_fn(|i| m.y[((n >> 1) * 8 + i / 8) * 16 + (n & 1) * 8 + i % 8]),
        4 => m.cb,
        _ => m.cr,
    }
}

/// Inverse of vc1_decode_p_block's dequantization: `level * scale`, plus
/// `pq` away from zero with the non-uniform quantizer.
fn quantize(c: f64, q: &Quant, max: i32) -> i32 {
    let scale = f64::from(q.pq * 2 + q.halfpq);
    let magnitude = if q.uniform {
        c.abs() / scale
    } else {
        (c.abs() - f64::from(q.pq)) / scale
    };
    // Inter coefficients round toward zero more than intra ones: a level
    // costs bits that small gains do not repay.
    let level = (magnitude + 1.0 / 3.0).floor().max(0.0) as i32;
    level.min(max) * if c < 0.0 { -1 } else { 1 }
}

fn dequantize(level: i32, q: &Quant) -> i16 {
    let c = level * (q.pq * 2 + q.halfpq);
    let c = if q.uniform {
        c
    } else {
        c + if c < 0 { -q.pq } else { q.pq }
    };
    c as i16
}

fn max_level(pquant: Pquant) -> i32 {
    if pquant.get() < 8 { 2047 } else { 255 }
}

/// A residual block after quantization: its coded form and the samples
/// the decoder will add, for the decision.
struct CodedResidual {
    block: PBlock,
    coefficients: usize,
    added: [i16; 64],
}

/// Distances from the predicted vector, in quarter samples, whose rate
/// `PPicture` keeps: those the searches around the prediction and zero
/// meet most.
const MV_COSTS: i32 = 1024;

/// Rate term of a vector `d` quarter samples from the predicted one.
fn mv_cost(lambda: f64, d: i32) -> f64 {
    lambda.sqrt() * BITS_PER_MV * f64::from(d).ln_1p()
}

/// Encoder state of one P picture.
struct PPicture<'a> {
    src: &'a Yuv420,
    reference: &'a Picture,
    rnd: Rnd,
    fastuvmc: bool,
    q: Quant,
    cs: CodingSet,
    esc3: Esc3Plan,
    zigzag: Vec<usize>,
    /// Lagrange multiplier: squared error per bit.
    lambda: f64,
    /// `mv_cost` by distance, below `MV_COSTS`.
    mv_costs: Vec<f64>,
    /// What the decoder reconstructs before the loop filter.
    model: Picture,
    intra: PIntra,
    sets: PictureCodingSets,
    intra_zigzag: [usize; 64],
    intra_mbs: Vec<IntraMb>,
    mb_width: usize,
}

/// Writes a macroblock's samples into a picture.
fn put(pic: &mut Picture, at: MbAt, m: &MbSamples) {
    for (r, row) in m.y.chunks(16).enumerate() {
        let o = (at.y * 16 + r) * pic.y.width + at.x * 16;
        pic.y.data[o..o + 16].copy_from_slice(row);
    }
    for (plane, data) in [(&mut pic.cb, &m.cb), (&mut pic.cr, &m.cr)] {
        for (r, row) in data.chunks(8).enumerate() {
            let o = (at.y * 8 + r) * plane.width + at.x * 8;
            plane.data[o..o + 8].copy_from_slice(row);
        }
    }
}

/// Block `n` (0..6) of a macroblock's samples replaced by 64 values.
fn set_block(m: &mut MbSamples, n: usize, v: &[u8; 64]) {
    match n {
        0..4 => {
            for (i, &x) in v.iter().enumerate() {
                m.y[((n >> 1) * 8 + i / 8) * 16 + (n & 1) * 8 + i % 8] = x;
            }
        }
        4 => m.cb = *v,
        _ => m.cr = *v,
    }
}

/// The best candidate of a search so far.
#[derive(Clone, Copy)]
struct Candidate {
    cost: f64,
    mv: Mv,
}

const WINDOW: usize = 2 * SEARCH as usize + 1;

/// Whole-sample vectors the integer search tried within `SEARCH` of a
/// centre. Clamped candidates of a window stay within it.
struct Window {
    centre: Mv,
    tried: [bool; WINDOW * WINDOW],
}

impl Window {
    fn new(centre: Mv) -> Self {
        Window {
            centre,
            tried: [false; WINDOW * WINDOW],
        }
    }

    fn index(&self, mv: Mv) -> Option<usize> {
        let x = usize::try_from((mv.x - self.centre.x) / 4 + SEARCH).ok()?;
        let y = usize::try_from((mv.y - self.centre.y) / 4 + SEARCH).ok()?;
        (x < WINDOW && y < WINDOW).then_some(y * WINDOW + x)
    }

    fn tried(&self, mv: Mv) -> bool {
        self.index(mv).is_some_and(|i| self.tried[i])
    }

    fn mark(&mut self, mv: Mv) {
        let i = self.index(mv).expect("a candidate lies within its window");
        self.tried[i] = true;
    }
}

impl PPicture<'_> {
    fn mc(&self) -> Mc<'_> {
        Mc {
            reference: self.reference,
            mspel: true,
            rnd: self.rnd,
            luts: None,
        }
    }

    fn predict(&self, at: MbAt, mv: Mv) -> MbSamples {
        self.mc().predict_one_mv(at, mv, FastUv::of(self.fastuvmc))
    }

    fn mv_cost(&self, mv: Mv, pred: Mv) -> f64 {
        let d = (mv.x - pred.x).abs() + (mv.y - pred.y).abs();
        usize::try_from(d)
            .ok()
            .and_then(|i| self.mv_costs.get(i).copied())
            .unwrap_or_else(|| mv_cost(self.lambda, d))
    }

    /// Integer search around the predicted and zero vectors, then half
    /// and quarter sample refinement on the exact luma prediction. Every
    /// candidate lies inside `bounds`.
    fn search(&self, at: MbAt, src: &MbSamples, pred: Mv, bounds: MvBounds) -> Mv {
        let whole = |mv: Mv| {
            let mv = bounds.clamp(mv);
            Mv {
                x: mv.x & !3,
                y: mv.y & !3,
            }
        };
        let centre = whole(pred);
        let mut centres = vec![centre];
        if centre != Mv::default() {
            centres.push(Mv::default());
        }
        let mut best: Option<Candidate> = None;
        let mut consider = |mv: Mv, distortion: u32| {
            let cost = f64::from(distortion) + self.mv_cost(mv, pred);
            if best.is_none_or(|b| cost < b.cost) {
                best = Some(Candidate { cost, mv });
            }
        };
        let mut windows: Vec<Window> = Vec::with_capacity(centres.len());
        for c in centres {
            windows.push(Window::new(c));
            for dy in -SEARCH..=SEARCH {
                for dx in -SEARCH..=SEARCH {
                    let mv = whole(Mv {
                        x: c.x + dx * 4,
                        y: c.y + dy * 4,
                    });
                    if !windows.iter().any(|w| w.tried(mv)) {
                        windows.last_mut().expect("pushed above").mark(mv);
                        consider(mv, integer_sad(self.reference, src, at, mv));
                    }
                }
            }
        }
        let mc = self.mc();
        let mut best = best.expect("the search tries at least one vector");
        for step in [2, 1] {
            let centre = best.mv;
            for dy in [-step, 0, step] {
                for dx in [-step, 0, step] {
                    let mv = bounds.clamp(Mv {
                        x: centre.x + dx,
                        y: centre.y + dy,
                    });
                    if mv == centre {
                        continue;
                    }
                    let cost =
                        f64::from(sad(&mc.predict_luma(at, mv), &src.y)) + self.mv_cost(mv, pred);
                    if cost < best.cost {
                        best = Candidate { cost, mv };
                    }
                }
            }
        }
        best.mv
    }

    fn residual(&mut self, src: &[u8; 64], pred: &[u8; 64]) -> CodedResidual {
        let diff: [f64; 64] = std::array::from_fn(|i| f64::from(src[i]) - f64::from(pred[i]));
        let coeffs = forward(&diff);
        let max = max_level(self.q.pquant);
        let levels: Vec<(usize, i32)> = (0..64)
            .map(|i| (i, quantize(coeffs[self.zigzag[i]], &self.q, max)))
            .filter(|&(_, l)| l != 0)
            .collect();
        let mut added: Block = [0; 64];
        if levels.is_empty() {
            return CodedResidual {
                block: PBlock::Empty,
                coefficients: 0,
                added,
            };
        }
        let mut ac = Vec::with_capacity(levels.len());
        let mut next = 0;
        for (k, &(i, level)) in levels.iter().enumerate() {
            added[self.zigzag[i]] = dequantize(level, &self.q);
            ac.push(
                self.cs
                    .code(
                        CoeffValue {
                            run: (i - next) as u32,
                            level,
                            last: k + 1 == levels.len(),
                        },
                        &self.esc3,
                    )
                    .expect("levels are clamped to what mode 3 carries"),
            );
            next = i + 1;
        }
        inv_trans_8x8(&mut added);
        CodedResidual {
            block: PBlock::Inter(InterBlock {
                ttblk: None,
                subblkpat: None,
                halves: None,
                subblocks: vec![ac],
            }),
            coefficients: levels.len(),
            added,
        }
    }

    fn macroblock(&mut self, builder: &mut PLayerBuilder, at: MbAt) {
        let src = source_mb(self.src, at);
        let next = builder.next();

        let choices: &[Hybrid] = if next.hybrid.is_some() {
            &[Hybrid::Above, Hybrid::Left]
        } else {
            &[Hybrid::Above]
        };
        let (skip_cost, skip_choice) = choices
            .iter()
            .map(|&c| {
                let p = self.predict(at, next.with(c));
                let d = ssd(&p.y, &src.y) + ssd(&p.cb, &src.cb) + ssd(&p.cr, &src.cr);
                (d + self.lambda * BITS_PER_SKIP, c)
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .expect("at least one choice");

        let mv = self.search(at, &src, next.with(skip_choice), builder.bounds());
        let pred = self.predict(at, mv);
        let residuals: [CodedResidual; 6] =
            std::array::from_fn(|n| self.residual(&block_of(&src, n), &block_of(&pred, n)));
        let mut distortion = 0.0;
        let mut rec = pred.clone();
        for (n, r) in residuals.iter().enumerate() {
            let s = block_of(&src, n);
            let p = block_of(&pred, n);
            let block: [u8; 64] = std::array::from_fn(|i| {
                (i32::from(p[i]) + i32::from(r.added[i])).clamp(0, 255) as u8
            });
            distortion += ssd(&block, &s);
            set_block(&mut rec, n, &block);
        }
        let coefficients: usize = residuals.iter().map(|r| r.coefficients).sum();
        let coded_cost =
            distortion + self.lambda * (BITS_PER_MV + BITS_PER_COEFF * coefficients as f64);

        let intra = self.intra_candidate(at, &src);
        if intra.cost < skip_cost.min(coded_cost) {
            builder.intra(false, intra.blocks);
            self.intra_mbs.push(IntraMb {
                at,
                samples: intra.samples,
            });
            put(&mut self.model, at, &intra.rec);
            return;
        }
        self.intra.mark(at, [false; 6]);
        for n in 0..6 {
            self.intra.clear(at, n);
        }
        if skip_cost <= coded_cost {
            builder.skip(skip_choice);
            let p = self.predict(at, next.with(skip_choice));
            put(&mut self.model, at, &p);
        } else {
            builder.one_mv(mv, skip_choice, residuals.map(|r| r.block));
            put(&mut self.model, at, &rec);
        }
    }

    /// Codes the macroblock at `at` as intra, leaving the intra prediction
    /// state as the decoder will have it if the macroblock is intra.
    fn intra_candidate(&mut self, at: MbAt, src: &MbSamples) -> IntraCandidate {
        self.intra.mark(at, [true; 6]);
        let mut coefficients = 0;
        let mut samples = [[0; 64]; 6];
        let mut rec = src.clone();
        let mut distortion = 0.0;
        let blocks: [IntraBlock; 6] = std::array::from_fn(|n| {
            let s = block_of(src, n);
            let pixels: [f64; 64] = std::array::from_fn(|i| f64::from(s[i]) - 128.0);
            let coeffs = forward(&pixels);
            let pred = self.intra.predict_dc(at, n);
            let wanted = (coeffs[0] / f64::from(self.q.dc_scale)).round() as i32;
            let max = max_dc_diff(self.q.pquant);
            let dc = DcDiff::code((wanted - pred).clamp(-max, max), self.q.pquant)
                .expect("clamped to the escape field");
            let levels: Vec<(usize, i32)> = (1..64)
                .map(|i| (i, quantize_ac(coeffs[self.intra_zigzag[i]], &self.q)))
                .filter(|&(_, l)| l != 0)
                .collect();
            let cs = if n < 4 {
                self.sets.intra_luma
            } else {
                self.sets.inter
            };
            let mut ac = Vec::with_capacity(levels.len());
            let mut next = 1;
            for (k, &(i, level)) in levels.iter().enumerate() {
                let v = CoeffValue {
                    run: (i - next) as u32,
                    level,
                    last: k + 1 == levels.len(),
                };
                ac.push(
                    cs.code(v, &self.esc3)
                        .expect("levels are clamped to what mode 3 carries"),
                );
                next = i + 1;
            }
            coefficients += levels.len();
            let block = IntraBlock { dc, ac };
            let out = self.intra.reconstruct(at, n, false, &block);
            let pixels: [u8; 64] =
                std::array::from_fn(|i| (i32::from(out[i]) + 128).clamp(0, 255) as u8);
            distortion += ssd(&pixels, &s);
            set_block(&mut rec, n, &pixels);
            samples[n] = out;
            block
        });
        IntraCandidate {
            cost: distortion
                + self.lambda
                    * (BITS_PER_MV + BITS_PER_DC * 6.0 + BITS_PER_COEFF * coefficients as f64),
            blocks,
            samples,
            rec,
        }
    }
}

/// An intra coding of one macroblock.
struct IntraCandidate {
    cost: f64,
    blocks: [IntraBlock; 6],
    /// Block samples before overlap smoothing, centred on zero, in block
    /// order.
    samples: [Block; 6],
    rec: MbSamples,
}

/// An intra macroblock of the picture, kept for overlap smoothing.
struct IntraMb {
    at: MbAt,
    samples: [Block; 6],
}

/// Applies the decoder's overlap smoothing to the intra macroblocks and
/// stores them in `model`.
fn smooth_intra(model: &mut Picture, mbs: &[IntraMb], count: usize, mb_width: usize) {
    let index = |at: MbAt| at.y * mb_width + at.x;
    let mut blocks = vec![[[0; 64]; 6]; count];
    let mut flags = vec![[false; 6]; count];
    for mb in mbs {
        for (n, b) in mb.samples.iter().enumerate() {
            blocks[index(mb.at)][BLOCK_MAP[n]] = *b;
        }
        flags[index(mb.at)] = [true; 6];
    }
    overlap_filter(&mut blocks, &flags, mb_width);
    for mb in mbs {
        for n in 0..6 {
            let place = block_place(model, mb.at, n);
            let b = &blocks[index(mb.at)][BLOCK_MAP[n]];
            put_block(place.plane, place.x, place.y, b, true);
        }
    }
}

/// An encoded P picture and the encoder's reconstruction of it.
pub(crate) struct EncodedP {
    pub(crate) frame: Frame,
    /// The picture the decoder reconstructs before the loop filter.
    pub(crate) model: Picture,
    /// How many macroblocks are intra.
    pub(crate) intra_macroblocks: usize,
}

/// Encodes one picture as a P picture predicted from `reference` with
/// rounding `rnd`, at a fixed quantizer.
pub(crate) fn encode_p_picture(
    src: &Yuv420,
    reference: &Picture,
    rnd: Rnd,
    ctx: &FrameContext,
    pqindex: PqIndex,
    framecnt: Bounded<3>,
) -> Result<EncodedP, super::EncodeError> {
    let h = header(ctx, pqindex, framecnt);
    let PictureKind::P(p) = &h.kind else {
        unreachable!("header builds a P picture")
    };
    let q = Quant::new(&h, ctx);
    let step = f64::from(q.pq * 2 + q.halfpq);
    let lambda = 0.1 * step * step;
    let mut pic = PPicture {
        src,
        reference,
        rnd,
        fastuvmc: ctx.seq.fastuvmc,
        esc3: Esc3Plan::widest(q.pquant),
        cs: CodingSet::for_picture(&h).inter,
        zigzag: scan(InterShape::EightByEight),
        lambda,
        mv_costs: (0..MV_COSTS).map(|d| mv_cost(lambda, d)).collect(),
        q,
        model: Picture::new(ctx.mb_dims()),
        intra: PIntra::new(&h, ctx),
        sets: CodingSet::for_picture(&h),
        intra_zigzag: intra_scan(0),
        intra_mbs: Vec::new(),
        mb_width: usize::from(ctx.mb_dims().width),
    };
    let mut builder = PLayerBuilder::new(&h, p, ctx)?;
    let dims = ctx.mb_dims();
    for i in 0..dims.count() {
        pic.macroblock(&mut builder, MbAt::of(i, usize::from(dims.width)));
    }
    let mut layer = builder.finish();
    pic.esc3.place(
        layer
            .macroblocks
            .iter_mut()
            .flat_map(|mb| match &mut mb.body {
                PMbBody::OneMv(m) => m.blocks.iter_mut().collect(),
                PMbBody::SkippedOneMv { .. } => Vec::new(),
                PMbBody::FourMv(_) | PMbBody::SkippedFourMv { .. } => {
                    unreachable!("the encoder codes 1MV macroblocks only")
                }
            })
            .flat_map(|b| match b {
                PBlock::Inter(i) => i.subblocks.iter_mut().flatten().collect(),
                PBlock::Intra(i) => i.ac.iter_mut().collect::<Vec<_>>(),
                PBlock::Empty => Vec::new(),
            }),
    );
    if pic.q.overlap {
        smooth_intra(&mut pic.model, &pic.intra_mbs, dims.count(), pic.mb_width);
    }
    let body = FrameBody::P(layer);
    Ok(EncodedP {
        frame: Frame { header: h, body },
        model: pic.model,
        intra_macroblocks: pic.intra_mbs.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wmv3::decode::Decoder;
    use crate::wmv3::encode::{encode_intra_picture, sample_sequence};
    use crate::wmv3::{QuantizerMode, SequenceHeader};

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Motion {
        /// The whole picture pans.
        Pan,
        /// Even frames are dark and odd frames bright.
        Cut,
        /// A 32x32 block moves over a still background, so that still
        /// macroblocks below it see disagreeing neighbours (HYBRIDPRED).
        Block,
    }

    /// A scene moving by (`dx`, `dy`) samples per frame.
    struct Scene {
        width: u16,
        height: u16,
        dx: f64,
        dy: f64,
        motion: Motion,
    }

    impl Scene {
        fn frame(&self, t: u32) -> Yuv420 {
            let (w, h) = (usize::from(self.width), usize::from(self.height));
            let (sx, sy) = (self.dx * f64::from(t), self.dy * f64::from(t));
            let cut = self.motion == Motion::Cut;
            let texture = |x: f64, y: f64| {
                let v = 128.0
                    + 90.0 * value_noise(x / 5.0, y / 5.0)
                    + 20.0 * (x * 0.37).sin() * (y * 0.29).cos();
                let v = match (cut, t % 2) {
                    (false, _) => v,
                    (true, 0) => v / 8.0,
                    (true, _) => 255.0 - v / 8.0,
                };
                v.clamp(0.0, 255.0) as u8
            };
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let chroma = |k: f64| -> Vec<u8> {
                (0..cw * ch)
                    .map(|i| {
                        let (x, y) = ((i % cw) as f64 * 2.0, (i / cw) as f64 * 2.0);
                        let (cx, cy) = match self.motion {
                            Motion::Block => (0.0, 0.0),
                            Motion::Pan | Motion::Cut => (sx, sy),
                        };
                        texture(x * k + cx, y + cy) / 2 + 64
                    })
                    .collect()
            };
            Yuv420 {
                width: self.width,
                height: self.height,
                y: (0..w * h)
                    .map(|i| {
                        let (x, y) = ((i % w) as f64, (i / w) as f64);
                        let left = 8.0 + sx;
                        let on_block =
                            (left..left + 32.0).contains(&x) && (16.0..48.0).contains(&y);
                        match self.motion {
                            Motion::Block if on_block => texture(x - left + 500.0, y),
                            Motion::Block => texture(x, y),
                            Motion::Pan | Motion::Cut => texture(x + sx, y + sy),
                        }
                    })
                    .collect(),
                cb: chroma(1.0),
                cr: chroma(0.5),
            }
        }
    }

    /// Smooth noise from -1 to 1 without repetition, so that motion
    /// search finds the true displacement and no alias of it.
    fn value_noise(x: f64, y: f64) -> f64 {
        let grid = |ix: i64, iy: i64| {
            let mut h = (ix as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
                ^ (iy as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
            h ^= h >> 29;
            h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
            h ^= h >> 32;
            (h % 2001) as f64 / 1000.0 - 1.0
        };
        let (fx, fy) = (x.floor(), y.floor());
        let (ax, ay) = (x - fx, y - fy);
        let (ix, iy) = (fx as i64, fy as i64);
        let top = grid(ix, iy) * (1.0 - ax) + grid(ix + 1, iy) * ax;
        let bottom = grid(ix, iy + 1) * (1.0 - ax) + grid(ix + 1, iy + 1) * ax;
        top * (1.0 - ay) + bottom * ay
    }

    const SCENES: [Scene; 7] = [
        Scene {
            width: 160,
            height: 80,
            dx: 12.0,
            dy: 0.0,
            motion: Motion::Block,
        },
        Scene {
            width: 88,
            height: 56,
            dx: 1.25,
            dy: -0.5,
            motion: Motion::Pan,
        },
        // Beyond each bound of the vector range of MVRANGE 0: -32 to
        // 31.75 samples down, -64 to 63.75 across.
        Scene {
            width: 48,
            height: 128,
            dx: 0.0,
            dy: 34.0,
            motion: Motion::Pan,
        },
        Scene {
            width: 48,
            height: 128,
            dx: 0.0,
            dy: -34.0,
            motion: Motion::Pan,
        },
        Scene {
            width: 176,
            height: 32,
            dx: -66.5,
            dy: 0.0,
            motion: Motion::Pan,
        },
        Scene {
            width: 176,
            height: 32,
            dx: 66.5,
            dy: 0.0,
            motion: Motion::Pan,
        },
        // Levels above 255, which only the finest quantizers carry.
        Scene {
            width: 48,
            height: 32,
            dx: 0.0,
            dy: 0.0,
            motion: Motion::Cut,
        },
    ];

    /// Lowest luma PSNR a P picture may have per PQINDEX, set somewhat
    /// below what the encoder reaches.
    fn min_psnr(pqindex: u8) -> f64 {
        const FLOORS: [(u8, f64); 11] = [
            (1, 49.5),
            (2, 41.5),
            (3, 41.5),
            (7, 35.5),
            (8, 34.5),
            (9, 31.0),
            (12, 31.0),
            (14, 29.5),
            (17, 28.0),
            (20, 26.0),
            (31, 23.0),
        ];
        FLOORS
            .iter()
            .rev()
            .find(|&&(q, _)| q <= pqindex)
            .map(|&(_, f)| f)
            .expect("PQINDEX is at least 1")
    }

    /// Encodes each scene and checks that every P picture decodes from
    /// its bytes to exactly the encoder's reconstruction, at a quality
    /// floor.
    fn reconstruction_matches(seq: &SequenceHeader, pqindex: u8) {
        for (s, scene) in SCENES.iter().enumerate() {
            let ctx = FrameContext {
                seq,
                width: scene.width,
                height: scene.height,
            };
            let pq = PqIndex::new(pqindex).unwrap();
            let mut decoder = Decoder::new(ctx);
            let mut intra = 0;
            for t in 0..4u32 {
                let src = scene.frame(t);
                let framecnt = Bounded::new((t % 4) as u8).unwrap();
                let (frame, model) = match decoder.reference() {
                    Some(r) if t > 0 => {
                        let e = encode_p_picture(&src, r, decoder.next_p_rnd(), &ctx, pq, framecnt)
                            .unwrap();
                        intra += e.intra_macroblocks;
                        (e.frame, Some(e.model))
                    }
                    _ => (
                        encode_intra_picture(&src, &ctx, pq, framecnt).unwrap(),
                        None,
                    ),
                };
                let bytes = frame.to_bytes(&ctx).unwrap();
                let parsed = Frame::parse(&bytes, &ctx).unwrap();
                let pic = decoder.decode(&parsed).unwrap().unwrap();
                if model.is_some() {
                    let yuv = pic.to_yuv420p(scene.width.into(), scene.height.into());
                    let luma = src.y.len();
                    let psnr =
                        10.0 * (255.0 * 255.0 * luma as f64 / ssd(&yuv[..luma], &src.y)).log10();
                    let floor = min_psnr(pqindex);
                    assert!(
                        psnr >= floor,
                        "scene {s} pqindex {pqindex} frame {t}: luma PSNR {psnr:.2} below {floor}"
                    );
                }
                if let Some(model) = model {
                    let differ = [
                        ("Y", pic.y == model.y),
                        ("Cb", pic.cb == model.cb),
                        ("Cr", pic.cr == model.cr),
                    ]
                    .iter()
                    .filter(|p| !p.1)
                    .map(|p| p.0)
                    .collect::<Vec<_>>();
                    assert!(
                        differ.is_empty(),
                        "scene {s} pqindex {pqindex} frame {t}: {differ:?} differ from the \
                         encoder's reconstruction"
                    );
                }
            }
            // A cut leaves nothing to predict from; neighbours of other
            // intra macroblocks exercise overlap smoothing.
            assert!(
                scene.motion != Motion::Cut || intra > 1,
                "scene {s} pqindex {pqindex}: {intra} intra macroblocks"
            );
        }
    }

    fn without_loop_filter() -> SequenceHeader {
        SequenceHeader {
            loop_filter: false,
            ..sample_sequence()
        }
    }

    #[test]
    fn decoder_reconstructs_what_the_encoder_predicted() {
        let seq = without_loop_filter();
        for pqindex in [1, 3, 7, 8, 9, 12, 14, 17, 20, 31] {
            reconstruction_matches(&seq, pqindex);
        }
    }

    #[test]
    fn reconstruction_matches_under_other_sequence_settings() {
        let variants: [fn(&mut SequenceHeader); 4] = [
            |s| {
                s.fastuvmc = false;
                s.extended_mv = false;
            },
            |s| s.quantizer = QuantizerMode::Explicit,
            |s| s.quantizer = QuantizerMode::NonUniform,
            |s| s.quantizer = QuantizerMode::Uniform,
        ];
        for f in variants {
            let mut seq = without_loop_filter();
            f(&mut seq);
            for pqindex in [2, 9, 20] {
                reconstruction_matches(&seq, pqindex);
            }
        }
    }
}
