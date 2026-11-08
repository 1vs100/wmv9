//! Intra macroblock layer of I and BI pictures (SMPTE 421M 7.1.4 and 8.1).

use super::bits::{BitReader, BitWriter, OutOfBits};
use super::coeff::{CodingSet, CoeffCoder, Fault, IntraBlock, read_intra_block, write_intra_block};
use super::picture::PictureHeader;
use super::tables::INTRA_CBPCY;
use super::{BlockError, Bounded, FrameContext, FrameError, MbDims, OpaqueBits};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntraMb {
    pub cbpcy: Bounded<63>,
    pub ac_pred: bool,
    /// Y0 to Y3, Cb, Cr.
    pub blocks: [IntraBlock; 6],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntraLayer {
    pub macroblocks: Vec<IntraMb>,
    /// Bits after the last macroblock, normally zero padding.
    pub trailing: OpaqueBits,
}

/// Coded flags of luma blocks on the 8x8 block grid, for CBPCY prediction.
struct CodedGrid {
    width: usize,
    flags: Vec<bool>,
}

impl CodedGrid {
    fn new(d: MbDims) -> Self {
        let width = usize::from(d.width) * 2;
        CodedGrid {
            width,
            flags: vec![false; width * usize::from(d.height) * 2],
        }
    }

    fn get(&self, x: isize, y: isize) -> bool {
        if x < 0 || y < 0 {
            return false;
        }
        self.flags[y as usize * self.width + x as usize]
    }

    /// Prediction from the left (A), top left (B) and top (C) blocks.
    fn predict(&self, x: usize, y: usize) -> bool {
        let (x, y) = (x as isize, y as isize);
        let a = self.get(x - 1, y);
        let b = self.get(x - 1, y - 1);
        let c = self.get(x, y - 1);
        if b == c { a } else { c }
    }

    fn set(&mut self, x: usize, y: usize, v: bool) {
        self.flags[y * self.width + x] = v;
    }
}

/// Position on the 8x8 block grid.
#[derive(Clone, Copy)]
struct BlockPos {
    x: usize,
    y: usize,
}

/// Luma block k of the macroblock at (mx, my).
fn luma_pos(mx: usize, my: usize, k: usize) -> BlockPos {
    BlockPos {
        x: mx * 2 + (k & 1),
        y: my * 2 + (k >> 1),
    }
}

/// Coded flag of block k: the CBPCY bit, predicted for luma blocks.
fn coded_flag(grid: &mut CodedGrid, cbpcy: Bounded<63>, mx: usize, my: usize, k: usize) -> bool {
    let bit = (cbpcy.get() >> (5 - k)) & 1 == 1;
    if k >= 4 {
        return bit;
    }
    let p = luma_pos(mx, my, k);
    let v = bit ^ grid.predict(p.x, p.y);
    grid.set(p.x, p.y, v);
    v
}

/// An intra macroblock before its CBPCY is coded.
pub(crate) struct IntraMbContent {
    pub ac_pred: bool,
    pub blocks: [IntraBlock; 6],
}

impl IntraLayer {
    /// Builds a layer from macroblocks in raster order, coding each CBPCY
    /// bit against the prediction `coded_flag` applies when reading. A block
    /// is coded when it has AC coefficients.
    pub(crate) fn from_content(mbs: Vec<IntraMbContent>, dims: MbDims) -> Self {
        let mut grid = CodedGrid::new(dims);
        let width = usize::from(dims.width);
        let macroblocks = mbs
            .into_iter()
            .enumerate()
            .map(|(i, mb)| {
                let (mx, my) = (i % width, i / width);
                let mut cbpcy = 0u8;
                for (k, b) in mb.blocks.iter().enumerate() {
                    let coded = !b.ac.is_empty();
                    let bit = if k < 4 {
                        let p = luma_pos(mx, my, k);
                        let predicted = grid.predict(p.x, p.y);
                        grid.set(p.x, p.y, coded);
                        coded ^ predicted
                    } else {
                        coded
                    };
                    cbpcy |= u8::from(bit) << (5 - k);
                }
                IntraMb {
                    cbpcy: Bounded::from_field(cbpcy.into()),
                    ac_pred: mb.ac_pred,
                    blocks: mb.blocks,
                }
            })
            .collect();
        IntraLayer {
            macroblocks,
            trailing: OpaqueBits::empty(),
        }
    }
}

/// Each macroblock takes at least one bit of CBPCY, ACPRED and six DC
/// symbols; checking this first bounds allocations by the input size.
const MIN_MB_BITS: usize = 8;

impl IntraLayer {
    pub fn read(
        r: &mut BitReader,
        h: &PictureHeader,
        ctx: &FrameContext,
    ) -> Result<Self, FrameError> {
        let dims = ctx.mb_dims();
        let needed = dims.count().saturating_mul(MIN_MB_BITS);
        if needed > r.remaining() {
            return Err(OutOfBits {
                bit: r.position(),
                needed: needed - r.remaining(),
            }
            .into());
        }
        let sets = CodingSet::for_picture(h);
        let mut c = CoeffCoder::new(h, h.pquant(ctx.seq.quantizer));
        let mut grid = CodedGrid::new(dims);
        let mut macroblocks = Vec::with_capacity(dims.count());
        for my in 0..usize::from(dims.height) {
            for mx in 0..usize::from(dims.width) {
                let cbpcy = Bounded::from_field(INTRA_CBPCY.read(r)?.into());
                let ac_pred = r.bit()?;
                let mut blocks = Vec::with_capacity(6);
                for k in 0..6 {
                    let coded = coded_flag(&mut grid, cbpcy, mx, my, k);
                    let cs = if k < 4 { sets.intra_luma } else { sets.inter };
                    blocks.push(read_intra_block(r, &mut c, cs, k >= 4, coded)?);
                }
                macroblocks.push(IntraMb {
                    cbpcy,
                    ac_pred,
                    blocks: blocks.try_into().expect("six blocks"),
                });
            }
        }
        Ok(IntraLayer {
            macroblocks,
            trailing: OpaqueBits::rest(r),
        })
    }

    pub fn write(
        &self,
        w: &mut BitWriter,
        h: &PictureHeader,
        ctx: &FrameContext,
    ) -> Result<(), FrameError> {
        let dims = ctx.mb_dims();
        if self.macroblocks.len() != dims.count() {
            return Err(FrameError::MacroblockCount {
                expected: dims.count(),
                found: self.macroblocks.len(),
            });
        }
        let sets = CodingSet::for_picture(h);
        let mut c = CoeffCoder::new(h, h.pquant(ctx.seq.quantizer));
        let mut grid = CodedGrid::new(dims);
        for (i, mb) in self.macroblocks.iter().enumerate() {
            let (mx, my) = (i % usize::from(dims.width), i / usize::from(dims.width));
            INTRA_CBPCY.write(w, mb.cbpcy.get().into())?;
            w.bit(mb.ac_pred);
            for (k, block) in mb.blocks.iter().enumerate() {
                let coded = coded_flag(&mut grid, mb.cbpcy, mx, my, k);
                let cs = if k < 4 { sets.intra_luma } else { sets.inter };
                write_intra_block(w, &mut c, cs, k >= 4, coded, block).map_err(|f| match f {
                    Fault::Frame(e) => e,
                    Fault::Block(fault) => FrameError::from(BlockError {
                        mb: i,
                        block: k,
                        fault,
                    }),
                    Fault::Mb(fault) => FrameError::Macroblock { mb: i, fault },
                })?;
            }
        }
        self.trailing.write(w);
        Ok(())
    }
}
