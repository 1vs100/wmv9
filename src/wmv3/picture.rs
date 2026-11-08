//! Picture layer header for progressive Simple and Main profile frames
//! (SMPTE 421M 7.1.1).

use super::bitplane::Bitplane;
use super::bits::{BitReader, BitWriter};
use super::{Bounded, Feature, FrameContext, FrameError, HeaderField, QuantizerMode};

/// PQINDEX, 1 to 31. Higher indexes quantize more coarsely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PqIndex(u8);

impl PqIndex {
    pub fn new(v: u8) -> Option<Self> {
        (1..=31).contains(&v).then_some(PqIndex(v))
    }
    pub fn get(self) -> u8 {
        self.0
    }
}

/// Index into the BFRACTION table, 0 to 22 except the reserved 21. Index 22
/// marks a BI picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BFractionIndex(u8);

impl BFractionIndex {
    pub const BI: BFractionIndex = BFractionIndex(22);

    pub fn new(v: u8) -> Option<Self> {
        (v <= 22 && v != 21).then_some(BFractionIndex(v))
    }
    pub fn get(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PictureHeader {
    pub interpfrm: Option<bool>,
    pub framecnt: Bounded<3>,
    pub rangeredfrm: Option<bool>,
    pub kind: PictureKind,
    pub pqindex: PqIndex,
    pub halfqp: Option<bool>,
    pub pquantizer: Option<bool>,
    /// MVRANGE, present when EXTENDED_MV is set.
    pub mvrange: Option<Bounded<3>>,
    /// RESPIC, present for non-B pictures when MULTIRES is set.
    pub respic: Option<Bounded<3>>,
    /// X8_TYPE, present for I and BI pictures when RES_X8 is set. Only a
    /// clear flag is supported.
    pub x8_type: Option<bool>,
    /// TRANSACFRM: AC coding set index for chroma (and luma in P/B).
    pub transacfrm: Bounded<2>,
    /// TRANSACFRM2: AC coding set index for luma in I/BI pictures.
    pub transacfrm2: Option<Bounded<2>>,
    pub transdctab: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PictureKind {
    I {
        buffer_fullness: Bounded<127>,
    },
    Bi {
        buffer_fullness: Bounded<127>,
    },
    P(PHeader),
    B {
        bfraction: BFractionIndex,
        header: BHeader,
    },
}

impl PictureKind {
    pub fn is_intra(&self) -> bool {
        matches!(self, PictureKind::I { .. } | PictureKind::Bi { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PHeader {
    /// MVMODE unary index into the mode table chosen by the quantizer.
    pub mvmode: Bounded<4>,
    pub intensity: Option<IntensityCompensation>,
    pub mvtypemb: Option<Bitplane>,
    pub skipmb: Bitplane,
    pub mvtab: Bounded<3>,
    pub cbptab: Bounded<3>,
    pub transform: TransformSignal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntensityCompensation {
    /// MVMODE2 unary index.
    pub mvmode2: Bounded<3>,
    pub lumscale: Bounded<63>,
    pub lumshift: Bounded<63>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BHeader {
    /// MVMODE bit: set for 1MV quarter-pel bicubic, clear for half-pel bilinear.
    pub mvmode_quarter_pel: bool,
    pub directmb: Bitplane,
    pub skipmb: Bitplane,
    pub mvtab: Bounded<3>,
    pub cbptab: Bounded<3>,
    pub transform: TransformSignal,
}

/// Frame level transform type signalling (TTMBF, TTFRM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformSignal {
    /// VSTRANSFORM is off; every block uses 8x8.
    Absent,
    /// TTMBF = 0: each macroblock signals its own type.
    PerMacroblock,
    /// TTMBF = 1 with TTFRM (0 = 8x8, 1 = 8x4, 2 = 4x8, 3 = 4x4).
    Frame(Bounded<3>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MvMode {
    OneMvHalfPelBilinear,
    OneMv,
    OneMvHalfPel,
    Mixed,
    IntensityCompensation,
}

const MVMODE_TABLE: [[MvMode; 5]; 2] = [
    [
        MvMode::OneMvHalfPelBilinear,
        MvMode::OneMv,
        MvMode::OneMvHalfPel,
        MvMode::IntensityCompensation,
        MvMode::Mixed,
    ],
    [
        MvMode::OneMv,
        MvMode::Mixed,
        MvMode::OneMvHalfPel,
        MvMode::IntensityCompensation,
        MvMode::OneMvHalfPelBilinear,
    ],
];

const MVMODE2_TABLE: [[MvMode; 4]; 2] = [
    [
        MvMode::OneMvHalfPelBilinear,
        MvMode::OneMv,
        MvMode::OneMvHalfPel,
        MvMode::Mixed,
    ],
    [
        MvMode::OneMv,
        MvMode::Mixed,
        MvMode::OneMvHalfPel,
        MvMode::OneMvHalfPelBilinear,
    ],
];

/// Picture quantizer step, 1 to 31.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pquant(u8);

impl Pquant {
    pub fn get(self) -> u8 {
        self.0
    }

    /// Selects the low-quant row of the MVMODE tables.
    fn low(self) -> usize {
        usize::from(self.0 <= 12)
    }
}

/// PQUANT for a PQINDEX: the implicit mapping table, or the identity
/// table in the other quantizer modes.
pub fn pquant(pqindex: PqIndex, mode: QuantizerMode) -> Pquant {
    let table = usize::from(mode != QuantizerMode::Implicit);
    Pquant(vc1_tables::VC1_PQUANT_TABLE[table][usize::from(pqindex.get())])
}

impl PictureHeader {
    pub fn pquant(&self, mode: QuantizerMode) -> Pquant {
        pquant(self.pqindex, mode)
    }
}

impl PHeader {
    pub fn mv_mode(&self, pquant: Pquant) -> MvMode {
        MVMODE_TABLE[pquant.low()][usize::from(self.mvmode.get())]
    }

    /// The motion vector mode in effect, looking through intensity
    /// compensation to MVMODE2.
    pub fn effective_mv_mode(&self, pquant: Pquant) -> MvMode {
        match &self.intensity {
            Some(ic) => MVMODE2_TABLE[pquant.low()][usize::from(ic.mvmode2.get())],
            None => self.mv_mode(pquant),
        }
    }
}

/// Unary code of up to `MAX` symbols, each `stop` bit ending it early.
fn read_unary<const MAX: u8>(r: &mut BitReader, stop: bool) -> Result<Bounded<MAX>, FrameError> {
    let mut n = 0;
    while n < MAX && r.bit()? != stop {
        n += 1;
    }
    Ok(Bounded::from_field(n.into()))
}

fn write_unary<const MAX: u8>(w: &mut BitWriter, stop: bool, n: Bounded<MAX>) {
    for _ in 0..n.get() {
        w.bit(!stop);
    }
    if n.get() < MAX {
        w.bit(stop);
    }
}

/// DECODE012: 0 -> "0", 1 -> "10", 2 -> "11".
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

fn read_fixed<const MAX: u8>(r: &mut BitReader, bits: u32) -> Result<Bounded<MAX>, FrameError> {
    debug_assert_eq!(u32::from(MAX), (1 << bits) - 1);
    Ok(Bounded::from_field(r.read(bits)?))
}

fn read_bfraction(r: &mut BitReader) -> Result<BFractionIndex, FrameError> {
    let mut v = r.read(3)? as u8;
    if v == 7 {
        v += r.read(4)? as u8;
    }
    BFractionIndex::new(v).ok_or(FrameError::ReservedBFraction { bit: r.position() })
}

fn write_bfraction(w: &mut BitWriter, b: BFractionIndex) {
    if b.0 < 7 {
        w.write(3, b.0.into());
    } else {
        w.write(3, 7);
        w.write(4, u32::from(b.0 - 7));
    }
}

fn read_transform(r: &mut BitReader, ctx: &FrameContext) -> Result<TransformSignal, FrameError> {
    if !ctx.seq.vstransform {
        return Ok(TransformSignal::Absent);
    }
    Ok(if r.bit()? {
        TransformSignal::Frame(read_fixed(r, 2)?)
    } else {
        TransformSignal::PerMacroblock
    })
}

fn write_transform(
    w: &mut BitWriter,
    ctx: &FrameContext,
    t: TransformSignal,
) -> Result<(), FrameError> {
    match (ctx.seq.vstransform, t) {
        (false, TransformSignal::Absent) => {}
        (true, TransformSignal::PerMacroblock) => w.bit(false),
        (true, TransformSignal::Frame(v)) => {
            w.bit(true);
            w.write(2, v.get().into());
        }
        _ => return Err(FrameError::HeaderField(HeaderField::Ttmbf)),
    }
    Ok(())
}

fn read_if<T>(
    present: bool,
    f: impl FnOnce() -> Result<T, FrameError>,
) -> Result<Option<T>, FrameError> {
    if present { f().map(Some) } else { Ok(None) }
}

/// Writes an optional field whose presence the sequence header or picture
/// type decides.
fn write_if<T: Copy>(
    present: bool,
    value: Option<T>,
    field: HeaderField,
    f: impl FnOnce(T),
) -> Result<(), FrameError> {
    match (present, value) {
        (true, Some(v)) => {
            f(v);
            Ok(())
        }
        (false, None) => Ok(()),
        _ => Err(FrameError::HeaderField(field)),
    }
}

/// Picture type as coded in PTYPE, before BFRACTION separates BI from B.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CodedType {
    I,
    P,
    B,
}

impl PictureHeader {
    pub fn read(r: &mut BitReader, ctx: &FrameContext) -> Result<Self, FrameError> {
        let seq = ctx.seq;
        if seq.dquant.get() != 0 {
            return Err(FrameError::Unsupported(Feature::Dquant));
        }
        let interpfrm = read_if(seq.finterpflag, || Ok(r.bit()?))?;
        let framecnt = read_fixed(r, 2)?;
        let rangeredfrm = read_if(seq.rangered, || Ok(r.bit()?))?;
        let coded = if r.bit()? {
            CodedType::P
        } else if seq.max_b_frames.get() == 0 || r.bit()? {
            CodedType::I
        } else {
            CodedType::B
        };
        let bfraction = read_if(coded == CodedType::B, || read_bfraction(r))?;
        let intra = coded == CodedType::I || bfraction == Some(BFractionIndex::BI);
        let buffer_fullness = read_if(intra, || read_fixed(r, 7))?;
        let pqindex = PqIndex::new(r.read(5)? as u8).ok_or(FrameError::ZeroPqIndex)?;
        let halfqp = read_if(pqindex.0 <= 8, || Ok(r.bit()?))?;
        let pquantizer = read_if(seq.quantizer == QuantizerMode::Explicit, || Ok(r.bit()?))?;
        let mvrange = read_if(seq.extended_mv, || read_unary(r, false))?;
        let true_b = coded == CodedType::B && !intra;
        let respic = read_if(seq.multires && !true_b, || read_fixed(r, 2))?;
        let x8_type = read_if(seq.res_x8 && intra, || Ok(r.bit()?))?;
        if x8_type == Some(true) {
            return Err(FrameError::Unsupported(Feature::IntraX8));
        }
        let pquant = pquant(pqindex, seq.quantizer);
        let dims = ctx.mb_dims();
        let kind = match (coded, bfraction, buffer_fullness) {
            (CodedType::I, None, Some(buffer_fullness)) => PictureKind::I { buffer_fullness },
            (CodedType::B, Some(BFractionIndex::BI), Some(buffer_fullness)) => {
                PictureKind::Bi { buffer_fullness }
            }
            (CodedType::P, None, None) => {
                let mvmode = read_unary(r, true)?;
                let lowquant_mode = MVMODE_TABLE[pquant.low()][usize::from(mvmode.get())];
                let intensity = read_if(lowquant_mode == MvMode::IntensityCompensation, || {
                    Ok(IntensityCompensation {
                        mvmode2: read_unary(r, true)?,
                        lumscale: read_fixed(r, 6)?,
                        lumshift: read_fixed(r, 6)?,
                    })
                })?;
                let effective = match &intensity {
                    Some(ic) => MVMODE2_TABLE[pquant.low()][usize::from(ic.mvmode2.get())],
                    None => lowquant_mode,
                };
                let mvtypemb = read_if(effective == MvMode::Mixed, || Bitplane::read(r, dims))?;
                PictureKind::P(PHeader {
                    mvmode,
                    intensity,
                    mvtypemb,
                    skipmb: Bitplane::read(r, dims)?,
                    mvtab: read_fixed(r, 2)?,
                    cbptab: read_fixed(r, 2)?,
                    transform: read_transform(r, ctx)?,
                })
            }
            (CodedType::B, Some(bfraction), None) => PictureKind::B {
                bfraction,
                header: BHeader {
                    mvmode_quarter_pel: r.bit()?,
                    directmb: Bitplane::read(r, dims)?,
                    skipmb: Bitplane::read(r, dims)?,
                    mvtab: read_fixed(r, 2)?,
                    cbptab: read_fixed(r, 2)?,
                    transform: read_transform(r, ctx)?,
                },
            },
            _ => unreachable!("picture type and its fields are read together"),
        };
        let transacfrm = read_012(r)?;
        let transacfrm2 = read_if(intra, || read_012(r))?;
        let transdctab = r.bit()?;
        Ok(PictureHeader {
            interpfrm,
            framecnt,
            rangeredfrm,
            kind,
            pqindex,
            halfqp,
            pquantizer,
            mvrange,
            respic,
            x8_type,
            transacfrm,
            transacfrm2,
            transdctab,
        })
    }

    pub fn write(&self, w: &mut BitWriter, ctx: &FrameContext) -> Result<(), FrameError> {
        let seq = ctx.seq;
        if seq.dquant.get() != 0 {
            return Err(FrameError::Unsupported(Feature::Dquant));
        }
        write_if(
            seq.finterpflag,
            self.interpfrm,
            HeaderField::Interpfrm,
            |v| w.bit(v),
        )?;
        w.write(2, self.framecnt.get().into());
        write_if(
            seq.rangered,
            self.rangeredfrm,
            HeaderField::Rangeredfrm,
            |v| w.bit(v),
        )?;
        let has_b = seq.max_b_frames.get() != 0;
        match &self.kind {
            PictureKind::P(_) => w.bit(true),
            PictureKind::I { .. } => {
                w.bit(false);
                if has_b {
                    w.bit(true);
                }
            }
            PictureKind::B { bfraction, .. } if has_b => {
                w.write(2, 0);
                write_bfraction(w, *bfraction);
            }
            PictureKind::Bi { .. } if has_b => {
                w.write(2, 0);
                write_bfraction(w, BFractionIndex::BI);
            }
            PictureKind::B { .. } | PictureKind::Bi { .. } => {
                return Err(FrameError::HeaderField(HeaderField::Ptype));
            }
        }
        let intra = self.kind.is_intra();
        if let PictureKind::I { buffer_fullness } | PictureKind::Bi { buffer_fullness } = &self.kind
        {
            w.write(7, buffer_fullness.get().into());
        }
        w.write(5, self.pqindex.0.into());
        write_if(self.pqindex.0 <= 8, self.halfqp, HeaderField::Halfqp, |v| {
            w.bit(v)
        })?;
        write_if(
            seq.quantizer == QuantizerMode::Explicit,
            self.pquantizer,
            HeaderField::Pquantizer,
            |v| w.bit(v),
        )?;
        write_if(seq.extended_mv, self.mvrange, HeaderField::Mvrange, |v| {
            write_unary(w, false, v)
        })?;
        let true_b = matches!(self.kind, PictureKind::B { .. });
        write_if(
            seq.multires && !true_b,
            self.respic,
            HeaderField::Respic,
            |v| w.write(2, v.get().into()),
        )?;
        write_if(
            seq.res_x8 && intra,
            self.x8_type,
            HeaderField::IntraX8,
            |v| w.bit(v),
        )?;
        if self.x8_type == Some(true) {
            return Err(FrameError::Unsupported(Feature::IntraX8));
        }
        let pquant = self.pquant(seq.quantizer);
        let dims = ctx.mb_dims();
        match &self.kind {
            PictureKind::I { .. } | PictureKind::Bi { .. } => {}
            PictureKind::P(p) => {
                write_unary(w, true, p.mvmode);
                let ic = p.mv_mode(pquant) == MvMode::IntensityCompensation;
                match (&p.intensity, ic) {
                    (Some(i), true) => {
                        write_unary(w, true, i.mvmode2);
                        w.write(6, i.lumscale.get().into());
                        w.write(6, i.lumshift.get().into());
                    }
                    (None, false) => {}
                    _ => return Err(FrameError::HeaderField(HeaderField::Mvmode2)),
                }
                let mixed = p.effective_mv_mode(pquant) == MvMode::Mixed;
                match (&p.mvtypemb, mixed) {
                    (Some(bp), true) => bp.write(w, dims)?,
                    (None, false) => {}
                    _ => return Err(FrameError::HeaderField(HeaderField::Mvtypemb)),
                }
                p.skipmb.write(w, dims)?;
                w.write(2, p.mvtab.get().into());
                w.write(2, p.cbptab.get().into());
                write_transform(w, ctx, p.transform)?;
            }
            PictureKind::B { header: b, .. } => {
                w.bit(b.mvmode_quarter_pel);
                b.directmb.write(w, dims)?;
                b.skipmb.write(w, dims)?;
                w.write(2, b.mvtab.get().into());
                w.write(2, b.cbptab.get().into());
                write_transform(w, ctx, b.transform)?;
            }
        }
        write_012(w, self.transacfrm);
        write_if(intra, self.transacfrm2, HeaderField::Transacfrm2, |v| {
            write_012(w, v)
        })?;
        w.bit(self.transdctab);
        Ok(())
    }
}
