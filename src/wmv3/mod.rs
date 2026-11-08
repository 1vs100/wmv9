//! WMV3 (VC-1 Simple and Main profile) bitstream syntax.

mod bitplane;
pub mod bits;
mod coeff;
pub mod decode;
pub mod encode;
mod frame;
mod inter;
mod intra;
mod picture;
mod sequence;
mod tables;
mod vlc;

pub use bitplane::{Bitplane, BitplaneCoding, SkipLine};
pub use coeff::{AcCoeff, CodingSet, DcDiff, Esc3Lengths, IntraBlock, PictureCodingSets};
pub use frame::{Frame, FrameBody, OpaqueBits};
pub use inter::{
    BCodedMb, BDirectMb, BLayer, BMb, BMbBody, BmvType, FourMvMb, InterBlock, InterLayer, MvData,
    OneMvMb, PBlock, PMb, PMbBody,
};
pub use intra::{IntraLayer, IntraMb};
pub use picture::{
    BFractionIndex, BHeader, IntensityCompensation, MvMode, PHeader, PictureHeader, PictureKind,
    PqIndex, TransformSignal,
};
pub use picture::{Pquant, pquant};
pub use sequence::{Profile, QuantizerMode, SequenceError, SequenceHeader};

use bits::OutOfBits;

/// Frame size in macroblocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MbDims {
    pub width: u16,
    pub height: u16,
}

impl MbDims {
    pub fn count(self) -> usize {
        usize::from(self.width) * usize::from(self.height)
    }
}

/// What a frame needs from outside its own bits.
#[derive(Debug, Clone, Copy)]
pub struct FrameContext<'a> {
    pub seq: &'a SequenceHeader,
    /// Coded picture size in pixels.
    pub width: u16,
    pub height: u16,
}

/// Largest frame in macroblocks: the Main profile High level limit of
/// SMPTE 421M Annex D (1920x1080 and the like).
pub const MAX_FRAME_MBS: usize = 8192;

impl FrameContext<'_> {
    /// Rejects empty pictures and pictures above the Main profile limit,
    /// which also bounds every per-macroblock allocation.
    pub fn check_size(&self) -> Result<(), FrameError> {
        let mbs = self.mb_dims().count();
        if mbs == 0 || mbs > MAX_FRAME_MBS {
            return Err(FrameError::PictureSize {
                width: self.width,
                height: self.height,
            });
        }
        Ok(())
    }

    pub fn mb_dims(&self) -> MbDims {
        MbDims {
            width: self.width.div_ceil(16),
            height: self.height.div_ceil(16),
        }
    }
}

/// Unsigned field value no greater than `MAX`, the largest value its coded
/// width or code can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bounded<const MAX: u8>(u8);

impl<const MAX: u8> Bounded<MAX> {
    pub const fn new(v: u8) -> Option<Self> {
        if v <= MAX { Some(Bounded(v)) } else { None }
    }

    pub const fn get(self) -> u8 {
        self.0
    }

    /// For values read from a field of at most MAX.
    pub(crate) fn from_field(v: u32) -> Self {
        debug_assert!(v <= u32::from(MAX));
        Bounded(v as u8)
    }
}

/// Picture header fields whose presence depends on the sequence header or
/// picture type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderField {
    Interpfrm,
    Rangeredfrm,
    Ptype,
    Halfqp,
    Pquantizer,
    Mvrange,
    Respic,
    IntraX8,
    Mvmode2,
    Mvtypemb,
    Ttmbf,
    Transacfrm2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// Macroblock quantizer (DQUANT in the sequence header).
    Dquant,
    /// IntraX8 coded intra pictures (X8_TYPE set).
    IntraX8,
    /// Reduced resolution pictures (RESPIC not zero).
    ReducedResolution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeTable {
    Imode,
    Norm2,
    Norm6,
    IntraCbpcy,
    Dc { table: u8, chroma: bool },
    Ac(u8),
    CbpcyP(u8),
    MvDiff(u8),
    Ttmb(u8),
    Ttblk(u8),
    Subblkpat(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitplanePart {
    SkipLineCount,
    SkipLineLength,
    Norm2Leading,
    Norm2Pairs,
    Norm6Tiles,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error(transparent)]
    OutOfBits(#[from] OutOfBits),
    #[error("invalid {table:?} code at bit {bit}")]
    InvalidCode { table: CodeTable, bit: usize },
    #[error("symbol {symbol} has no {table:?} code")]
    UncodableSymbol { table: CodeTable, symbol: u16 },
    #[error("PQINDEX 0 is forbidden")]
    ZeroPqIndex,
    #[error("reserved BFRACTION code ending at bit {bit}")]
    ReservedBFraction { bit: usize },
    #[error("{0:?} is not supported")]
    Unsupported(Feature),
    #[error("{0:?} presence does not match the sequence header or picture type")]
    HeaderField(HeaderField),
    #[error("block coefficients run past position 63 at bit {bit}")]
    CoefficientOverrun { bit: usize },
    #[error(transparent)]
    Block(#[from] BlockError),
    #[error("{found} macroblocks for a picture of {expected}")]
    MacroblockCount { expected: usize, found: usize },
    #[error("picture size {width}x{height} is empty or above {MAX_FRAME_MBS} macroblocks")]
    PictureSize { width: u16, height: u16 },
    #[error("picture body does not match the picture type")]
    BodyMismatch,
    #[error("macroblock {mb}: {fault:?}")]
    Macroblock { mb: usize, fault: MbFault },
    #[error("bitplane {part:?}: expected {expected}, found {found}")]
    BitplaneShape {
        part: BitplanePart,
        expected: usize,
        found: usize,
    },
}

/// A macroblock layer value the writer cannot encode in context.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("macroblock {mb} block {block}: {fault:?}")]
pub struct BlockError {
    pub mb: usize,
    pub block: usize,
    pub fault: BlockFault,
}

/// A macroblock level value the writer cannot encode in context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MbFault {
    /// A raw bitplane bit is missing, or present for a coded plane.
    RawPlaneBit,
    /// Body kind disagrees with the MVTYPEMB or SKIPMB value.
    Kind,
    /// HYBRIDPRED presence disagrees with the motion vector predictors.
    Hybrid,
    /// MVDATA extra bits exceed the widths its symbol gives.
    MvBits,
    /// 4MV block vectors present where CBPCY says absent, or the reverse.
    FourMvData,
    AcPred,
    Cbpcy,
    Ttmb,
    /// BMVTYPE present for an intra macroblock, or absent otherwise.
    Bmvtype,
    /// Second MVDATA present where the macroblock is not interpolated with
    /// coefficients, or absent where it is.
    SecondMv,
    /// TTBLK, SUBBLKPAT or the half pattern present where not signalled,
    /// or absent where signalled.
    TransformFields,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockFault {
    /// Coefficient list empty for a coded block, present for an uncoded
    /// one, or with LAST set before the final entry.
    CoefficientList,
    /// Runs carry the coefficients past position 63.
    CoefficientOverrun,
    /// DC symbol, escape or extra bits do not fit the widths PQUANT gives.
    DcRange,
    /// ESC3 widths must come with the frame's first mode 3 escape only,
    /// within the ranges their codes allow.
    Esc3Lengths,
    /// A value exceeds the ESC3 run or level width in effect.
    Esc3Range,
    /// A table index is the escape symbol itself or outside the table.
    EscapedIndex,
}
