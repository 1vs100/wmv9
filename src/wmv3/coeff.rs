//! Block coefficient syntax shared by intra and inter macroblocks, kept in
//! coded form: DC differentials and AC coefficients each have more than one
//! way to code the same value.

use vc1_tables as t;

use super::bits::{BitReader, BitWriter};
use super::picture::{PictureHeader, Pquant};
use super::tables::{AC, DC};
use super::{BlockFault, FrameError, MbFault};

struct RunLevel {
    run: u32,
    level: u32,
}

/// AC coding set, numbered as in FFmpeg's `enum CodingSet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodingSet(u8);

impl CodingSet {
    pub const HIGH_MOT_INTRA: CodingSet = CodingSet(0);
    pub const HIGH_MOT_INTER: CodingSet = CodingSet(1);
    pub const LOW_MOT_INTRA: CodingSet = CodingSet(2);
    pub const LOW_MOT_INTER: CodingSet = CodingSet(3);
    pub const MID_RATE_INTRA: CodingSet = CodingSet(4);
    pub const MID_RATE_INTER: CodingSet = CodingSet(5);
    pub const HIGH_RATE_INTRA: CodingSet = CodingSet(6);
    pub const HIGH_RATE_INTER: CodingSet = CodingSet(7);

    pub(crate) fn index(self) -> usize {
        self.0.into()
    }

    fn escape(self) -> u16 {
        (t::VC1_AC_SIZES[self.index()] - 1) as u16
    }

    fn is_last(self, index: u8) -> bool {
        i32::from(index) >= t::VC1_LAST_DECODE_TABLE[self.index()]
    }

    fn run_level(self, index: u8) -> RunLevel {
        let e = t::VC1_INDEX_DECODE_TABLE[self.index()][usize::from(index)];
        RunLevel {
            run: e[0].into(),
            level: e[1].into(),
        }
    }

    /// Zero coefficients a coefficient skips, or `None` for an index the
    /// table cannot carry.
    fn run(self, coeff: &AcCoeff) -> Option<u32> {
        let table_index = |index: u8| (u16::from(index) < self.escape()).then_some(index);
        match *coeff {
            AcCoeff::Direct { index, .. } | AcCoeff::LevelEscape { index, .. } => {
                Some(self.run_level(table_index(index)?).run)
            }
            AcCoeff::RunEscape { index, .. } => {
                let rl = self.run_level(table_index(index)?);
                let delta = if self.is_last(index) {
                    &t::VC1_LAST_DELTA_RUN_TABLE[self.index()][..]
                } else {
                    &t::VC1_DELTA_RUN_TABLE[self.index()][..]
                };
                Some(rl.run + u32::from(*delta.get(rl.level as usize)?) + 1)
            }
            AcCoeff::Explicit { run, .. } => Some(run.into()),
        }
    }

    /// LAST flag of a coefficient whose index `run` accepted.
    fn last(self, coeff: &AcCoeff) -> bool {
        match *coeff {
            AcCoeff::Direct { index, .. }
            | AcCoeff::LevelEscape { index, .. }
            | AcCoeff::RunEscape { index, .. } => self.is_last(index),
            AcCoeff::Explicit { last, .. } => last,
        }
    }

    fn intra_set(index: u8, high_rate: bool) -> CodingSet {
        match index {
            1 => CodingSet::HIGH_MOT_INTRA,
            2 => CodingSet::MID_RATE_INTRA,
            _ if high_rate => CodingSet::HIGH_RATE_INTRA,
            _ => CodingSet::LOW_MOT_INTRA,
        }
    }

    fn inter_set(index: u8, high_rate: bool) -> CodingSet {
        match index {
            1 => CodingSet::HIGH_MOT_INTER,
            2 => CodingSet::MID_RATE_INTER,
            _ if high_rate => CodingSet::HIGH_RATE_INTER,
            _ => CodingSet::LOW_MOT_INTER,
        }
    }

    /// Coding sets of a picture. I and BI pictures choose luma by
    /// TRANSACFRM2; P and B pictures choose both by TRANSACFRM, the inter set
    /// serving chroma intra blocks and all inter blocks.
    pub fn for_picture(h: &PictureHeader) -> PictureCodingSets {
        let high_rate = h.pqindex.get() <= 8;
        let luma_index = h.transacfrm2.unwrap_or(h.transacfrm).get();
        PictureCodingSets {
            intra_luma: CodingSet::intra_set(luma_index, high_rate),
            inter: CodingSet::inter_set(h.transacfrm.get(), high_rate),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureCodingSets {
    /// Luma blocks of intra macroblocks.
    pub intra_luma: CodingSet,
    /// Chroma blocks of intra macroblocks and every inter block.
    pub inter: CodingSet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DcDiff {
    Zero,
    /// Table symbol 1 to 118, followed by `extra` low bits when PQUANT is
    /// 1 or 2.
    Coded {
        index: u8,
        extra: u8,
        negative: bool,
    },
    /// Escape symbol followed by the magnitude in 8, 9 or 10 bits.
    Escape {
        value: u16,
        negative: bool,
    },
}

const DC_ESCAPE: u16 = 119;

/// ESC3 field widths, sent with the first mode 3 escape of a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Esc3Lengths {
    /// ESCLVLSZ: 1 to 11 bits with table 59, 2 to 8 with table 60.
    pub level: u8,
    /// ESCRUNSZ: 3 to 6 bits.
    pub run: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcCoeff {
    /// Table symbol giving run, level and last.
    Direct { index: u8, negative: bool },
    /// Escape mode 1: table symbol with the level extended.
    LevelEscape { index: u8, negative: bool },
    /// Escape mode 2: table symbol with the run extended.
    RunEscape { index: u8, negative: bool },
    /// Escape mode 3: run and level sent as fixed width fields.
    Explicit {
        lengths: Option<Esc3Lengths>,
        last: bool,
        run: u8,
        negative: bool,
        level: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntraBlock {
    pub dc: DcDiff,
    /// Empty exactly when the block is not coded.
    pub ac: Vec<AcCoeff>,
}

/// Per-picture state of the coefficient syntax.
pub(crate) struct CoeffCoder {
    dc_table: usize,
    pquant: Pquant,
    esc3: Option<Esc3Lengths>,
}

impl CoeffCoder {
    pub fn new(h: &PictureHeader, pquant: Pquant) -> Self {
        CoeffCoder {
            dc_table: usize::from(h.transdctab),
            pquant,
            esc3: None,
        }
    }

    /// Extra DC bits for PQUANT 1 and 2.
    fn dc_extra_bits(&self) -> u32 {
        match self.pquant.get() {
            1 => 2,
            2 => 1,
            _ => 0,
        }
    }

    /// ESC3 level width coding: table 59 below PQUANT 8, else table 60.
    fn esc3_table59(&self) -> bool {
        self.pquant.get() < 8
    }
}

/// Coefficient positions a run of coefficients may occupy.
#[derive(Clone, Copy)]
pub(crate) struct Span {
    /// First position: 1 after an intra DC, else 0.
    pub first: u32,
    /// Last position: 63, 31 or 15 by transform size.
    pub last: u32,
}

/// The coefficients after the DC of an 8x8 intra block.
pub(crate) const INTRA_AC: Span = Span { first: 1, last: 63 };

/// A write failure local to one block, located by the caller.
pub(crate) enum Fault {
    Frame(FrameError),
    Block(BlockFault),
    Mb(MbFault),
}

impl From<FrameError> for Fault {
    fn from(e: FrameError) -> Self {
        Fault::Frame(e)
    }
}

/// Reads coefficients up to and including the one with LAST set.
pub(crate) fn read_coeffs(
    r: &mut BitReader,
    c: &mut CoeffCoder,
    cs: CodingSet,
    span: Span,
) -> Result<Vec<AcCoeff>, FrameError> {
    let mut out = Vec::new();
    let mut pos = span.first;
    loop {
        let coeff = read_ac(r, c, cs)?;
        let run = cs.run(&coeff).ok_or(FrameError::InvalidCode {
            table: super::CodeTable::Ac(cs.0),
            bit: r.position(),
        })?;
        let last = cs.last(&coeff);
        out.push(coeff);
        pos += run;
        if pos > span.last {
            return Err(FrameError::CoefficientOverrun { bit: r.position() });
        }
        pos += 1;
        if last {
            return Ok(out);
        }
    }
}

/// Writes a non-empty coefficient list whose final entry alone has LAST.
pub(crate) fn write_coeffs(
    w: &mut BitWriter,
    c: &mut CoeffCoder,
    cs: CodingSet,
    span: Span,
    coeffs: &[AcCoeff],
) -> Result<(), Fault> {
    if coeffs.is_empty() {
        return Err(Fault::Block(BlockFault::CoefficientList));
    }
    let mut pos = span.first;
    for (i, coeff) in coeffs.iter().enumerate() {
        let run = cs
            .run(coeff)
            .ok_or(Fault::Block(BlockFault::EscapedIndex))?;
        pos += run;
        if pos > span.last {
            return Err(Fault::Block(BlockFault::CoefficientOverrun));
        }
        pos += 1;
        if cs.last(coeff) != (i + 1 == coeffs.len()) {
            return Err(Fault::Block(BlockFault::CoefficientList));
        }
        write_ac(w, c, cs, coeff)?;
    }
    Ok(())
}

pub(crate) fn read_intra_block(
    r: &mut BitReader,
    c: &mut CoeffCoder,
    cs: CodingSet,
    chroma: bool,
    coded: bool,
) -> Result<IntraBlock, FrameError> {
    let dc = read_dc(r, c, chroma)?;
    let ac = if coded {
        read_coeffs(r, c, cs, INTRA_AC)?
    } else {
        Vec::new()
    };
    Ok(IntraBlock { dc, ac })
}

pub(crate) fn write_intra_block(
    w: &mut BitWriter,
    c: &mut CoeffCoder,
    cs: CodingSet,
    chroma: bool,
    coded: bool,
    b: &IntraBlock,
) -> Result<(), Fault> {
    write_dc(w, c, &b.dc, chroma)?;
    match (coded, b.ac.is_empty()) {
        (true, _) => write_coeffs(w, c, cs, INTRA_AC, &b.ac),
        (false, true) => Ok(()),
        (false, false) => Err(Fault::Block(BlockFault::CoefficientList)),
    }
}

fn read_dc(r: &mut BitReader, c: &CoeffCoder, chroma: bool) -> Result<DcDiff, FrameError> {
    let index = DC[c.dc_table][usize::from(chroma)].read(r)?;
    if index == 0 {
        return Ok(DcDiff::Zero);
    }
    let m = c.dc_extra_bits();
    Ok(if index == DC_ESCAPE {
        let value = r.read(8 + m)? as u16;
        DcDiff::Escape {
            value,
            negative: r.bit()?,
        }
    } else {
        let extra = r.read(m)? as u8;
        DcDiff::Coded {
            index: index as u8,
            extra,
            negative: r.bit()?,
        }
    })
}

/// DECODE210: "1" -> 0, "01" -> 1, "00" -> 2.
fn read_210(r: &mut BitReader) -> Result<u8, FrameError> {
    Ok(if r.bit()? { 0 } else { 2 - u8::from(r.bit()?) })
}

fn write_210(w: &mut BitWriter, v: u8) {
    match v {
        0 => w.bit(true),
        1 => w.write(2, 1),
        _ => w.write(2, 0),
    }
}

fn read_ac(r: &mut BitReader, c: &mut CoeffCoder, cs: CodingSet) -> Result<AcCoeff, FrameError> {
    let ac = &AC[cs.index()];
    let index = ac.read(r)?;
    if index != cs.escape() {
        return Ok(AcCoeff::Direct {
            index: index as u8,
            negative: r.bit()?,
        });
    }
    match read_210(r)? {
        mode @ (0 | 1) => {
            let index = ac.read(r)?;
            if index == cs.escape() {
                return Err(FrameError::InvalidCode {
                    table: super::CodeTable::Ac(cs.0),
                    bit: r.position(),
                });
            }
            let index = index as u8;
            let negative = r.bit()?;
            Ok(if mode == 0 {
                AcCoeff::LevelEscape { index, negative }
            } else {
                AcCoeff::RunEscape { index, negative }
            })
        }
        _ => {
            let last = r.bit()?;
            let lengths = if c.esc3.is_none() {
                let level = if c.esc3_table59() {
                    match r.read(3)? as u8 {
                        0 => r.read(2)? as u8 + 8,
                        v => v,
                    }
                } else {
                    let mut n = 0;
                    while n < 6 && !r.bit()? {
                        n += 1;
                    }
                    n + 2
                };
                let l = Esc3Lengths {
                    level,
                    run: 3 + r.read(2)? as u8,
                };
                c.esc3 = Some(l);
                Some(l)
            } else {
                None
            };
            let widths = c.esc3.expect("set above");
            let run = r.read(widths.run.into())? as u8;
            let negative = r.bit()?;
            let level = r.read(widths.level.into())? as u16;
            Ok(AcCoeff::Explicit {
                lengths,
                last,
                run,
                negative,
                level,
            })
        }
    }
}

fn write_dc(w: &mut BitWriter, c: &CoeffCoder, dc: &DcDiff, chroma: bool) -> Result<(), Fault> {
    let vlc = &DC[c.dc_table][usize::from(chroma)];
    let m = c.dc_extra_bits();
    let range = Fault::Block(BlockFault::DcRange);
    match *dc {
        DcDiff::Zero => vlc.write(w, 0)?,
        DcDiff::Coded {
            index,
            extra,
            negative,
        } => {
            if index == 0 || u16::from(index) >= DC_ESCAPE || u32::from(extra) >> m != 0 {
                return Err(range);
            }
            vlc.write(w, index.into())?;
            w.write(m, extra.into());
            w.bit(negative);
        }
        DcDiff::Escape { value, negative } => {
            if u32::from(value) >> (8 + m) != 0 {
                return Err(range);
            }
            vlc.write(w, DC_ESCAPE)?;
            w.write(8 + m, value.into());
            w.bit(negative);
        }
    }
    Ok(())
}

/// Writes one coefficient whose index and position the caller checked.
fn write_ac(
    w: &mut BitWriter,
    c: &mut CoeffCoder,
    cs: CodingSet,
    coeff: &AcCoeff,
) -> Result<(), Fault> {
    let ac = &AC[cs.index()];
    match *coeff {
        AcCoeff::Direct { index, negative } => {
            ac.write(w, index.into())?;
            w.bit(negative);
        }
        AcCoeff::LevelEscape { index, negative } | AcCoeff::RunEscape { index, negative } => {
            ac.write(w, cs.escape())?;
            write_210(w, u8::from(matches!(coeff, AcCoeff::RunEscape { .. })));
            ac.write(w, index.into())?;
            w.bit(negative);
        }
        AcCoeff::Explicit {
            lengths,
            last,
            run,
            negative,
            level,
        } => {
            let lengths_error = Fault::Block(BlockFault::Esc3Lengths);
            ac.write(w, cs.escape())?;
            write_210(w, 2);
            w.bit(last);
            match (c.esc3, lengths) {
                (None, Some(l)) => {
                    write_esc3_lengths(w, c.esc3_table59(), l).ok_or(lengths_error)?;
                    c.esc3 = Some(l);
                }
                (Some(_), None) => {}
                _ => return Err(lengths_error),
            }
            let widths = c.esc3.expect("set above");
            if u32::from(run) >> widths.run != 0 || u32::from(level) >> widths.level != 0 {
                return Err(Fault::Block(BlockFault::Esc3Range));
            }
            w.write(widths.run.into(), run.into());
            w.bit(negative);
            w.write(widths.level.into(), level.into());
        }
    }
    Ok(())
}

fn write_esc3_lengths(w: &mut BitWriter, table59: bool, l: Esc3Lengths) -> Option<()> {
    if !(3..=6).contains(&l.run) {
        return None;
    }
    if table59 {
        match l.level {
            1..=7 => w.write(3, l.level.into()),
            8..=11 => {
                w.write(3, 0);
                w.write(2, u32::from(l.level - 8));
            }
            _ => return None,
        }
    } else {
        if !(2..=8).contains(&l.level) {
            return None;
        }
        let n = l.level - 2;
        for _ in 0..n {
            w.bit(false);
        }
        if n < 6 {
            w.bit(true);
        }
    }
    w.write(2, u32::from(l.run - 3));
    Some(())
}

/// A coefficient's run of zeros, signed level and LAST flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CoeffValue {
    pub run: u32,
    pub level: i32,
    pub last: bool,
}

impl CodingSet {
    /// Value of a coefficient as vc1_decode_ac_coeff computes it. The
    /// coefficient must have come from the reader or passed the writer's
    /// checks, so its index is within the table.
    pub(crate) fn value(self, coeff: &AcCoeff) -> CoeffValue {
        let signed = |level: i32, negative: bool| if negative { -level } else { level };
        match *coeff {
            AcCoeff::Direct { index, negative } => {
                let rl = self.run_level(index);
                CoeffValue {
                    run: rl.run,
                    level: signed(rl.level as i32, negative),
                    last: self.is_last(index),
                }
            }
            AcCoeff::LevelEscape { index, negative } => {
                let rl = self.run_level(index);
                let last = self.is_last(index);
                let delta = if last {
                    t::VC1_LAST_DELTA_LEVEL_TABLE[self.index()][rl.run as usize]
                } else {
                    t::VC1_DELTA_LEVEL_TABLE[self.index()][rl.run as usize]
                };
                CoeffValue {
                    run: rl.run,
                    level: signed(rl.level as i32 + i32::from(delta), negative),
                    last,
                }
            }
            AcCoeff::RunEscape { index, negative } => CoeffValue {
                run: self.run(coeff).expect("checked index"),
                level: signed(self.run_level(index).level as i32, negative),
                last: self.is_last(index),
            },
            AcCoeff::Explicit {
                last,
                run,
                negative,
                level,
                ..
            } => CoeffValue {
                run: run.into(),
                level: signed(level.into(), negative),
                last,
            },
        }
    }
}

impl DcDiff {
    /// DC differential value at the given PQUANT (vc1_decode_i_block).
    pub(crate) fn value(&self, pquant: Pquant) -> i32 {
        let m = match pquant.get() {
            1 => 2,
            2 => 1,
            _ => 0,
        };
        let signed = |v: i32, negative: bool| if negative { -v } else { v };
        match *self {
            DcDiff::Zero => 0,
            DcDiff::Coded {
                index,
                extra,
                negative,
            } => signed(
                (i32::from(index) << m) + i32::from(extra) - ((1 << m) - 1),
                negative,
            ),
            DcDiff::Escape { value, negative } => signed(value.into(), negative),
        }
    }
}

/// ESC3 field widths of an encoder's picture. Coefficients are coded
/// without them; `place` then attaches them to the first mode 3 coefficient
/// of the finished picture in bitstream order, where the reader expects
/// them.
#[derive(Clone, Copy)]
pub(crate) struct Esc3Plan {
    lengths: Esc3Lengths,
}

impl Esc3Plan {
    /// The widest fields the picture's table allows, so that any run and any
    /// level the table can carry fit.
    pub(crate) fn widest(pquant: Pquant) -> Self {
        Esc3Plan {
            lengths: Esc3Lengths {
                level: if pquant.get() < 8 { 11 } else { 8 },
                run: 6,
            },
        }
    }

    /// Sends the widths with the first mode 3 coefficient among `coeffs`,
    /// given in bitstream order, and with no other.
    pub(crate) fn place<'a>(&self, coeffs: impl IntoIterator<Item = &'a mut AcCoeff>) {
        let mut first = true;
        for c in coeffs {
            if let AcCoeff::Explicit { lengths, .. } = c {
                *lengths = first.then_some(self.lengths);
                first = false;
            }
        }
    }
}

/// A table entry's run, level magnitude and LAST flag.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CodeKey {
    run: u32,
    level: u32,
    last: bool,
}

/// Table index of each (run, level, last) per coding set.
fn code_tables() -> &'static [crate::int_map::IntMap<CodeKey, u8>; 8] {
    static TABLES: std::sync::OnceLock<[crate::int_map::IntMap<CodeKey, u8>; 8]> =
        std::sync::OnceLock::new();
    TABLES.get_or_init(|| {
        std::array::from_fn(|set| {
            let cs = CodingSet(set as u8);
            (0..cs.escape())
                .map(|i| {
                    let i = i as u8;
                    let rl = cs.run_level(i);
                    let key = CodeKey {
                        run: rl.run,
                        level: rl.level,
                        last: cs.is_last(i),
                    };
                    (key, i)
                })
                .collect()
        })
    })
}

impl CodingSet {
    /// Coded form of a coefficient: a table symbol when one exists, else
    /// the first of escape modes 1, 2 and 3 that carries it. A mode 3
    /// coefficient comes without ESC3 widths; see `Esc3Plan::place`.
    /// `None` when the level does not fit mode 3 either. `level` must not
    /// be zero.
    pub(crate) fn code(self, v: CoeffValue, esc3: &Esc3Plan) -> Option<AcCoeff> {
        let table = &code_tables()[self.index()];
        let negative = v.level < 0;
        let level = v.level.unsigned_abs();
        if let Some(&index) = table.get(&CodeKey {
            run: v.run,
            level,
            last: v.last,
        }) {
            return Some(AcCoeff::Direct { index, negative });
        }
        let (delta_level, delta_run) = if v.last {
            (
                &t::VC1_LAST_DELTA_LEVEL_TABLE[self.index()][..],
                &t::VC1_LAST_DELTA_RUN_TABLE[self.index()][..],
            )
        } else {
            (
                &t::VC1_DELTA_LEVEL_TABLE[self.index()][..],
                &t::VC1_DELTA_RUN_TABLE[self.index()][..],
            )
        };
        if let Some(&d) = delta_level.get(v.run as usize) {
            let base = level.checked_sub(u32::from(d)).filter(|&l| l > 0);
            if let Some(&index) = base.and_then(|l| {
                table.get(&CodeKey {
                    run: v.run,
                    level: l,
                    last: v.last,
                })
            }) {
                return Some(AcCoeff::LevelEscape { index, negative });
            }
        }
        if let Some(&d) = delta_run.get(level as usize) {
            let base = v.run.checked_sub(u32::from(d) + 1);
            if let Some(&index) = base.and_then(|r| {
                table.get(&CodeKey {
                    run: r,
                    level,
                    last: v.last,
                })
            }) {
                return Some(AcCoeff::RunEscape { index, negative });
            }
        }
        let l = esc3.lengths;
        if level >> l.level != 0 || v.run >> l.run != 0 {
            return None;
        }
        Some(AcCoeff::Explicit {
            lengths: None,
            last: v.last,
            run: v.run as u8,
            negative,
            level: level as u16,
        })
    }
}

impl DcDiff {
    /// Coded form of a DC differential at the given PQUANT; `None` when it
    /// exceeds the escape field.
    pub(crate) fn code(value: i32, pquant: Pquant) -> Option<DcDiff> {
        let m = match pquant.get() {
            1 => 2,
            2 => 1,
            _ => 0,
        };
        let negative = value < 0;
        let v = value.unsigned_abs();
        if v == 0 {
            return Some(DcDiff::Zero);
        }
        let biased = v + (1 << m) - 1;
        let index = biased >> m;
        if index >= 1 && index < u32::from(DC_ESCAPE) {
            return Some(DcDiff::Coded {
                index: index as u8,
                extra: (biased & ((1 << m) - 1)) as u8,
                negative,
            });
        }
        (v >> (8 + m) == 0).then_some(DcDiff::Escape {
            value: v as u16,
            negative,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wmv3::QuantizerMode;
    use crate::wmv3::picture::{PqIndex, pquant};

    /// PQUANT equal to `v`: the explicit quantizer's table is the identity.
    fn pq(v: u8) -> Pquant {
        let p = pquant(PqIndex::new(v).unwrap(), QuantizerMode::Explicit);
        assert_eq!(p.get(), v);
        p
    }

    #[test]
    fn coded_coefficients_read_back_as_their_value() {
        let mut forms = [0usize; 4];
        for pq in [1u8, 7, 8, 31] {
            let pquant = super::tests::pq(pq);
            for set in 0..8 {
                let cs = CodingSet(set);
                let esc3 = Esc3Plan::widest(pquant);
                for last in [false, true] {
                    for run in 0..64 {
                        let top = if pq < 8 { 2047 } else { 255 };
                        let over = CoeffValue {
                            run,
                            level: top + 1,
                            last,
                        };
                        assert_eq!(cs.code(over, &esc3), None);
                        for level in [-top, -40, -9, -3, -1, 1, 2, 5, 17, 60, top] {
                            let v = CoeffValue { run, level, last };
                            let c = cs.code(v, &esc3).expect("fits mode 3");
                            forms[match c {
                                AcCoeff::Direct { .. } => 0,
                                AcCoeff::LevelEscape { .. } => 1,
                                AcCoeff::RunEscape { .. } => 2,
                                AcCoeff::Explicit { .. } => 3,
                            }] += 1;
                            assert_eq!(cs.value(&c), v, "set {set} pq {pq}: {c:?}");
                        }
                    }
                }
            }
        }
        assert!(forms.iter().all(|&n| n > 0), "forms reached: {forms:?}");
    }

    #[test]
    fn esc3_widths_go_with_the_first_mode_3_coefficient_only() {
        let plan = Esc3Plan::widest(pq(4));
        let explicit = |lengths| AcCoeff::Explicit {
            lengths,
            last: false,
            run: 1,
            negative: false,
            level: 3,
        };
        let direct = AcCoeff::Direct {
            index: 0,
            negative: false,
        };
        let mut coeffs = [
            direct.clone(),
            explicit(Some(plan.lengths)),
            explicit(None),
            direct,
            explicit(Some(plan.lengths)),
        ];
        coeffs.swap(1, 2);
        plan.place(coeffs.iter_mut());
        let carried: Vec<bool> = coeffs
            .iter()
            .map(|c| {
                matches!(
                    c,
                    AcCoeff::Explicit {
                        lengths: Some(_),
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(carried, [false, true, false, false, false]);
    }

    #[test]
    fn coded_dc_differentials_read_back_as_their_value() {
        for pq in [1u8, 2, 3, 31] {
            let pquant = super::tests::pq(pq);
            let extra_bits = match pq {
                1 => 2,
                2 => 1,
                _ => 0,
            };
            let max = (1 << (8 + extra_bits)) - 1;
            for v in -max..=max {
                let d = DcDiff::code(v, pquant).expect("within the escape field");
                assert_eq!(d.value(pquant), v, "pq {pq}: {d:?}");
            }
            assert_eq!(DcDiff::code(max + 1, pquant), None);
        }
    }
}
