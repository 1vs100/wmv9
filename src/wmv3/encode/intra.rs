//! I picture encoding: forward transform, quantization matching the
//! decoder's dequantization, DC prediction from the decoder's predictor and
//! the shortest coded form of every coefficient. AC prediction is not used.

use super::transform::forward;
use super::{EncodeError, PlaneKind, Yuv420};
use crate::wmv3::coeff::{CodingSet, CoeffValue, Esc3Plan};
use crate::wmv3::decode::intra::{DcPredictor, Quant, scan};
use crate::wmv3::intra::IntraMbContent;
use crate::wmv3::picture::Pquant;
use crate::wmv3::{
    Bounded, DcDiff, Frame, FrameBody, FrameContext, IntraBlock, IntraLayer, PictureHeader,
    PictureKind, PqIndex, QuantizerMode,
};

/// Picture header of an I picture with the given quantizer, every optional
/// field present exactly when the sequence calls for it.
fn header(ctx: &FrameContext, pqindex: PqIndex, framecnt: Bounded<3>) -> PictureHeader {
    let seq = ctx.seq;
    PictureHeader {
        interpfrm: seq.finterpflag.then_some(false),
        framecnt,
        rangeredfrm: seq.rangered.then_some(false),
        // BUFFER_FULLNESS is informational; decoders do not use it.
        kind: PictureKind::I {
            buffer_fullness: Bounded::new(0).expect("zero is in range"),
        },
        pqindex,
        halfqp: (pqindex.get() <= 8).then_some(false),
        pquantizer: (seq.quantizer == QuantizerMode::Explicit).then_some(true),
        mvrange: seq
            .extended_mv
            .then(|| Bounded::new(0).expect("zero is in range")),
        respic: seq
            .multires
            .then(|| Bounded::new(0).expect("zero is in range")),
        x8_type: seq.res_x8.then_some(false),
        transacfrm: Bounded::new(0).expect("zero is in range"),
        transacfrm2: Some(Bounded::new(0).expect("zero is in range")),
        transdctab: false,
    }
}

/// Largest DC differential the escape field carries at this PQUANT.
pub(super) fn max_dc_diff(pquant: Pquant) -> i32 {
    let extra_bits = match pquant.get() {
        1 => 2,
        2 => 1,
        _ => 0,
    };
    (1 << (8 + extra_bits)) - 1
}

/// Largest AC level escape mode 3 carries at this PQUANT.
fn max_level(pquant: Pquant) -> i32 {
    if pquant.get() < 8 { 2047 } else { 255 }
}

/// Quantized level of an AC coefficient, inverting `requant_coded`:
/// `level * scale`, plus `pq` away from zero with the non-uniform
/// quantizer.
pub(super) fn quantize_ac(c: f64, q: &Quant) -> i32 {
    let scale = f64::from(q.pq * 2 + q.halfpq);
    let magnitude = if q.uniform {
        c.abs() / scale
    } else {
        (c.abs() - f64::from(q.pq)) / scale
    };
    let level = (magnitude + 0.5).floor().max(0.0) as i32;
    level.min(max_level(q.pquant)) * if c < 0.0 { -1 } else { 1 }
}

/// One plane of the source at the macroblock-aligned size, as values the
/// decoder's blocks should reproduce.
struct SourcePlane {
    width: usize,
    data: Vec<f64>,
}

impl SourcePlane {
    fn new(src: &Yuv420, plane: PlaneKind, width: usize, height: usize, offset: f64) -> Self {
        SourcePlane {
            width,
            data: (0..width * height)
                .map(|i| f64::from(src.sample(plane, i % width, i / width)) - offset)
                .collect(),
        }
    }

    fn height(&self) -> usize {
        self.data.len() / self.width
    }

    /// Applies `m` to the four samples across every internal 8x8 block
    /// edge; `vertical` selects edges between vertically adjacent blocks.
    fn filter_edges(&mut self, m: &[[f64; 4]; 4], vertical: bool) {
        let (along, across) = if vertical {
            (self.width, self.height())
        } else {
            (self.height(), self.width)
        };
        for edge in (8..across).step_by(8) {
            for line in 0..along {
                let idx = |k: usize| {
                    let pos = edge + k - 2;
                    if vertical {
                        pos * self.width + line
                    } else {
                        line * self.width + pos
                    }
                };
                let v: [f64; 4] = std::array::from_fn(|k| self.data[idx(k)]);
                for (k, row) in m.iter().enumerate() {
                    self.data[idx(k)] = row.iter().zip(v).map(|(a, b)| a * b).sum();
                }
            }
        }
    }
}

/// Inverse of the overlap smoothing of v_s_overlap and h_s_overlap on the
/// four samples a, b | c, d across an edge, rounding left out.
fn overlap_prefilter() -> [[f64; 4]; 4] {
    let smoothing = [
        [7.0, 0.0, 0.0, 1.0],
        [-1.0, 7.0, 1.0, 1.0],
        [1.0, 1.0, 7.0, -1.0],
        [1.0, 0.0, 0.0, 7.0],
    ]
    .map(|r: [f64; 4]| r.map(|v| v / 8.0));
    invert4(smoothing)
}

fn invert4(mut a: [[f64; 4]; 4]) -> [[f64; 4]; 4] {
    let mut inv: [[f64; 4]; 4] =
        std::array::from_fn(|r| std::array::from_fn(|c| f64::from(u8::from(r == c))));
    for col in 0..4 {
        let pivot = (col..4)
            .max_by(|&x, &y| a[x][col].abs().total_cmp(&a[y][col].abs()))
            .expect("non-empty range");
        a.swap(col, pivot);
        inv.swap(col, pivot);
        let d = a[col][col];
        for c in 0..4 {
            a[col][c] /= d;
            inv[col][c] /= d;
        }
        for r in 0..4 {
            if r != col {
                let f = a[r][col];
                for c in 0..4 {
                    a[r][c] -= f * a[col][c];
                    inv[r][c] -= f * inv[col][c];
                }
            }
        }
    }
    inv
}

/// Encoder state of one picture.
struct Picture {
    /// Luma, Cb and Cr targets.
    planes: [SourcePlane; 3],
    q: Quant,
    dc: DcPredictor,
    esc3: Esc3Plan,
    luma_set: CodingSet,
    chroma_set: CodingSet,
    zigzag: [usize; 64],
}

impl Picture {
    fn block(&mut self, mb_x: usize, mb_y: usize, n: usize) -> IntraBlock {
        let plane = match n {
            0..4 => 0,
            4 => 1,
            _ => 2,
        };
        let x0 = if n < 4 {
            mb_x * 16 + (n & 1) * 8
        } else {
            mb_x * 8
        };
        let y0 = if n < 4 {
            mb_y * 16 + (n >> 1) * 8
        } else {
            mb_y * 8
        };
        let p = &self.planes[plane];
        let pixels: [f64; 64] =
            std::array::from_fn(|i| p.data[(y0 + i / 8) * p.width + x0 + i % 8]);
        let coeffs = forward(&pixels);

        let pred = self.dc.predict(mb_x, mb_y, n);
        let wanted = (coeffs[0] / f64::from(self.q.dc_scale)).round() as i32;
        // A differential beyond the escape field is clamped to it; later
        // blocks predict from the level actually coded.
        let max = max_dc_diff(self.q.pquant);
        let dc = DcDiff::code((wanted - pred).clamp(-max, max), self.q.pquant)
            .expect("clamped to the escape field");
        self.dc.store(mb_x, mb_y, n, pred + dc.value(self.q.pquant));

        let levels: Vec<(usize, i32)> = (1..64)
            .map(|i| (i, quantize_ac(coeffs[self.zigzag[i]], &self.q)))
            .filter(|&(_, l)| l != 0)
            .collect();
        let cs = if n < 4 {
            self.luma_set
        } else {
            self.chroma_set
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
        IntraBlock { dc, ac }
    }
}

/// Encodes one picture as an I picture at a fixed quantizer.
pub fn encode_intra_picture(
    src: &Yuv420,
    ctx: &FrameContext,
    pqindex: PqIndex,
    framecnt: Bounded<3>,
) -> Result<Frame, EncodeError> {
    super::check_sequence(ctx)?;
    if (src.width, src.height) != (ctx.width, ctx.height) {
        return Err(EncodeError::SizeMismatch {
            source_width: src.width,
            source_height: src.height,
            width: ctx.width,
            height: ctx.height,
        });
    }
    src.check()?;
    ctx.check_size()?;
    let h = header(ctx, pqindex, framecnt);
    let q = Quant::new(&h, ctx);
    let sets = CodingSet::for_picture(&h);
    let dims = ctx.mb_dims();
    let (w, h_px) = (usize::from(dims.width) * 16, usize::from(dims.height) * 16);
    // With overlap smoothing the decoder adds 128 when it stores intra
    // blocks; otherwise the DC level carries the offset.
    let offset = if q.overlap { 128.0 } else { 0.0 };
    let mut planes = [
        SourcePlane::new(src, PlaneKind::Luma, w, h_px, offset),
        SourcePlane::new(src, PlaneKind::Cb, w / 2, h_px / 2, offset),
        SourcePlane::new(src, PlaneKind::Cr, w / 2, h_px / 2, offset),
    ];
    if q.overlap {
        // The decoder smooths edges between horizontally adjacent blocks
        // before those between vertically adjacent ones; undo them in the
        // reverse order. The two passes nearly commute.
        let m = overlap_prefilter();
        for p in &mut planes {
            p.filter_edges(&m, true);
            p.filter_edges(&m, false);
        }
    }
    let mut pic = Picture {
        planes,
        esc3: Esc3Plan::widest(q.pquant),
        dc: DcPredictor::new(&h, ctx),
        q,
        luma_set: sets.intra_luma,
        chroma_set: sets.inter,
        zigzag: scan(1),
    };
    let mut mbs = Vec::with_capacity(dims.count());
    for mb_y in 0..usize::from(dims.height) {
        for mb_x in 0..usize::from(dims.width) {
            mbs.push(IntraMbContent {
                ac_pred: false,
                blocks: [0, 1, 2, 3, 4, 5].map(|n| pic.block(mb_x, mb_y, n)),
            });
        }
    }
    let mut layer = IntraLayer::from_content(mbs, dims);
    pic.esc3.place(
        layer
            .macroblocks
            .iter_mut()
            .flat_map(|mb| mb.blocks.iter_mut())
            .flat_map(|b| b.ac.iter_mut()),
    );
    Ok(Frame {
        header: h,
        body: FrameBody::Intra(layer),
    })
}
