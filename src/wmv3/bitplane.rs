//! Bitplane coding (SMPTE 421M 8.7) kept in coded form, since the same
//! plane can be coded more than one way within one IMODE.

use super::bits::{BitReader, BitWriter};
use super::tables::{IMODE, NORM2, NORM6};
use super::{BitplanePart, FrameError, MbDims};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitplane {
    pub invert: bool,
    pub coding: BitplaneCoding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BitplaneCoding {
    /// One bit per macroblock, sent in the macroblock layer.
    Raw,
    Norm2 {
        differential: bool,
        /// First bit when the macroblock count is odd.
        leading: Option<bool>,
        /// NORM-2 symbols, two macroblocks each.
        pairs: Vec<u8>,
    },
    Norm6 {
        differential: bool,
        /// NORM-6 symbols for 2x3 or 3x2 tiles.
        tiles: Vec<u8>,
        /// Columns left of the tiled area, coded column by column.
        residual_columns: Vec<SkipLine>,
        /// Row above the tiled area in 3x2 tiling, coded row by row.
        residual_rows: Vec<SkipLine>,
    },
    RowSkip(Vec<SkipLine>),
    ColSkip(Vec<SkipLine>),
}

/// Decoded bitplane contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaneValues {
    /// One bit per macroblock, sent in the macroblock layer.
    Raw,
    /// Values in raster order.
    Values(Vec<bool>),
}

/// A row or column that is either all zero or sent bit by bit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipLine {
    Zero,
    Coded(Vec<bool>),
}

/// Layout of NORM-6 tiles for a plane size.
struct Norm6Layout {
    tiles: usize,
    residual_columns: usize,
    residual_column_len: usize,
    residual_rows: usize,
    residual_row_len: usize,
}

fn norm6_layout(d: MbDims) -> Norm6Layout {
    let (w, h) = (usize::from(d.width), usize::from(d.height));
    if h % 3 == 0 && w % 3 != 0 {
        Norm6Layout {
            tiles: (h / 3) * (w / 2),
            residual_columns: w & 1,
            residual_column_len: h,
            residual_rows: 0,
            residual_row_len: 0,
        }
    } else {
        Norm6Layout {
            tiles: (h / 2) * (w / 3),
            residual_columns: w % 3,
            residual_column_len: h,
            residual_rows: h & 1,
            residual_row_len: w - w % 3,
        }
    }
}

fn read_lines(r: &mut BitReader, count: usize, len: usize) -> Result<Vec<SkipLine>, FrameError> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(if r.bit()? {
            let mut bits = Vec::with_capacity(len);
            for _ in 0..len {
                bits.push(r.bit()?);
            }
            SkipLine::Coded(bits)
        } else {
            SkipLine::Zero
        });
    }
    Ok(out)
}

fn write_lines(
    w: &mut BitWriter,
    lines: &[SkipLine],
    count: usize,
    len: usize,
) -> Result<(), FrameError> {
    if lines.len() != count {
        return Err(FrameError::BitplaneShape {
            part: BitplanePart::SkipLineCount,
            expected: count,
            found: lines.len(),
        });
    }
    for line in lines {
        match line {
            SkipLine::Zero => w.bit(false),
            SkipLine::Coded(bits) => {
                if bits.len() != len {
                    return Err(FrameError::BitplaneShape {
                        part: BitplanePart::SkipLineLength,
                        expected: len,
                        found: bits.len(),
                    });
                }
                w.bit(true);
                for &b in bits {
                    w.bit(b);
                }
            }
        }
    }
    Ok(())
}

fn expect_len(part: BitplanePart, expected: usize, found: usize) -> Result<(), FrameError> {
    if expected == found {
        Ok(())
    } else {
        Err(FrameError::BitplaneShape {
            part,
            expected,
            found,
        })
    }
}

impl Bitplane {
    pub fn read(r: &mut BitReader, dims: MbDims) -> Result<Self, FrameError> {
        let invert = r.bit()?;
        let count = dims.count();
        let coding = match IMODE.read(r)? {
            0 => BitplaneCoding::Raw,
            m @ (1 | 2) => {
                let leading = if count & 1 == 1 { Some(r.bit()?) } else { None };
                let mut pairs = Vec::with_capacity(count / 2);
                for _ in 0..count / 2 {
                    pairs.push(NORM2.read(r)? as u8);
                }
                BitplaneCoding::Norm2 {
                    differential: m == 2,
                    leading,
                    pairs,
                }
            }
            m @ (3 | 4) => {
                let l = norm6_layout(dims);
                let mut tiles = Vec::with_capacity(l.tiles);
                for _ in 0..l.tiles {
                    tiles.push(NORM6.read(r)? as u8);
                }
                let residual_columns = read_lines(r, l.residual_columns, l.residual_column_len)?;
                let residual_rows = read_lines(r, l.residual_rows, l.residual_row_len)?;
                BitplaneCoding::Norm6 {
                    differential: m == 4,
                    tiles,
                    residual_columns,
                    residual_rows,
                }
            }
            5 => BitplaneCoding::RowSkip(read_lines(r, dims.height.into(), dims.width.into())?),
            _ => BitplaneCoding::ColSkip(read_lines(r, dims.width.into(), dims.height.into())?),
        };
        Ok(Bitplane { invert, coding })
    }

    pub fn write(&self, w: &mut BitWriter, dims: MbDims) -> Result<(), FrameError> {
        w.bit(self.invert);
        let count = dims.count();
        match &self.coding {
            BitplaneCoding::Raw => IMODE.write(w, 0)?,
            BitplaneCoding::Norm2 {
                differential,
                leading,
                pairs,
            } => {
                IMODE.write(w, if *differential { 2 } else { 1 })?;
                expect_len(
                    BitplanePart::Norm2Leading,
                    count & 1,
                    usize::from(leading.is_some()),
                )?;
                expect_len(BitplanePart::Norm2Pairs, count / 2, pairs.len())?;
                if let Some(b) = leading {
                    w.bit(*b);
                }
                for &p in pairs {
                    NORM2.write(w, p.into())?;
                }
            }
            BitplaneCoding::Norm6 {
                differential,
                tiles,
                residual_columns,
                residual_rows,
            } => {
                IMODE.write(w, if *differential { 4 } else { 3 })?;
                let l = norm6_layout(dims);
                expect_len(BitplanePart::Norm6Tiles, l.tiles, tiles.len())?;
                for &t in tiles {
                    NORM6.write(w, t.into())?;
                }
                write_lines(
                    w,
                    residual_columns,
                    l.residual_columns,
                    l.residual_column_len,
                )?;
                write_lines(w, residual_rows, l.residual_rows, l.residual_row_len)?;
            }
            BitplaneCoding::RowSkip(lines) => {
                IMODE.write(w, 5)?;
                write_lines(w, lines, dims.height.into(), dims.width.into())?;
            }
            BitplaneCoding::ColSkip(lines) => {
                IMODE.write(w, 6)?;
                write_lines(w, lines, dims.width.into(), dims.height.into())?;
            }
        }
        Ok(())
    }

    /// Plane values in raster order. The coded shape is checked first, so a
    /// malformed plane is an error rather than a guess.
    pub fn decode(&self, dims: MbDims) -> Result<PlaneValues, FrameError> {
        self.write(&mut BitWriter::new(), dims)?;
        Ok(match self.values(dims) {
            Some(v) => PlaneValues::Values(v),
            None => PlaneValues::Raw,
        })
    }

    /// Values of a plane whose shape matches `dims`; `None` when raw.
    fn values(&self, dims: MbDims) -> Option<Vec<bool>> {
        let (w, h) = (usize::from(dims.width), usize::from(dims.height));
        let mut p = vec![false; w * h];
        let fill_column = |p: &mut Vec<bool>, x: usize, line: &SkipLine| {
            if let SkipLine::Coded(bits) = line {
                for (y, &b) in bits.iter().enumerate() {
                    p[y * w + x] = b;
                }
            }
        };
        let differential = match &self.coding {
            BitplaneCoding::Raw => return None,
            BitplaneCoding::Norm2 {
                differential,
                leading,
                pairs,
            } => {
                let mut i = 0;
                if let Some(b) = leading {
                    p[0] = *b;
                    i = 1;
                }
                for &pair in pairs {
                    p[i] = pair & 1 == 1;
                    p[i + 1] = pair >> 1 == 1;
                    i += 2;
                }
                *differential
            }
            BitplaneCoding::Norm6 {
                differential,
                tiles,
                residual_columns,
                residual_rows,
            } => {
                let mut t = tiles.iter();
                let set = |p: &mut Vec<bool>, x: usize, y: usize, code: u8, bit: u8| {
                    p[y * w + x] = (code >> bit) & 1 == 1;
                };
                if h % 3 == 0 && w % 3 != 0 {
                    for y in (0..h).step_by(3) {
                        for x in ((w & 1)..w).step_by(2) {
                            let c = *t.next()?;
                            for (bit, (dx, dy)) in [(0, 0), (1, 0), (0, 1), (1, 1), (0, 2), (1, 2)]
                                .into_iter()
                                .enumerate()
                            {
                                set(&mut p, x + dx, y + dy, c, bit as u8);
                            }
                        }
                    }
                } else {
                    for y in ((h & 1)..h).step_by(2) {
                        for x in ((w % 3)..w).step_by(3) {
                            let c = *t.next()?;
                            for (bit, (dx, dy)) in [(0, 0), (1, 0), (2, 0), (0, 1), (1, 1), (2, 1)]
                                .into_iter()
                                .enumerate()
                            {
                                set(&mut p, x + dx, y + dy, c, bit as u8);
                            }
                        }
                    }
                }
                for (x, line) in residual_columns.iter().enumerate() {
                    fill_column(&mut p, x, line);
                }
                if let Some(SkipLine::Coded(bits)) = residual_rows.first() {
                    let x0 = w - bits.len();
                    for (i, &b) in bits.iter().enumerate() {
                        p[x0 + i] = b;
                    }
                }
                *differential
            }
            BitplaneCoding::RowSkip(lines) => {
                for (y, line) in lines.iter().enumerate() {
                    if let SkipLine::Coded(bits) = line {
                        p[y * w..(y + 1) * w].copy_from_slice(bits);
                    }
                }
                false
            }
            BitplaneCoding::ColSkip(lines) => {
                for (x, line) in lines.iter().enumerate() {
                    fill_column(&mut p, x, line);
                }
                false
            }
        };
        if differential {
            p[0] ^= self.invert;
            for x in 1..w {
                p[x] ^= p[x - 1];
            }
            for y in 1..h {
                let row = y * w;
                p[row] ^= p[row - w];
                for x in 1..w {
                    let i = row + x;
                    if p[i - 1] != p[i - w] {
                        p[i] ^= self.invert;
                    } else {
                        p[i] ^= p[i - 1];
                    }
                }
            }
        } else if self.invert {
            for b in &mut p {
                *b = !*b;
            }
        }
        Some(p)
    }

    /// IMODE index as numbered by the specification (0 = raw ... 6 = colskip).
    pub fn imode(&self) -> u8 {
        match &self.coding {
            BitplaneCoding::Raw => 0,
            BitplaneCoding::Norm2 { differential, .. } => 1 + u8::from(*differential),
            BitplaneCoding::Norm6 { differential, .. } => 3 + u8::from(*differential),
            BitplaneCoding::RowSkip(_) => 5,
            BitplaneCoding::ColSkip(_) => 6,
        }
    }
}
