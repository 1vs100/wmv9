//! Sequence header (STRUCT_C, SMPTE 421M Annex J), carried as the codec
//! private data of the video stream.

use super::Bounded;
use super::bits::{BitReader, BitWriter, OutOfBits};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Simple,
    Main,
    /// WMV3 Complex profile, decodable only in part by known decoders.
    Complex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantizerMode {
    /// Quantizer type follows from PQINDEX.
    Implicit,
    /// Each picture header carries PQUANTIZER.
    Explicit,
    NonUniform,
    Uniform,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceHeader {
    pub profile: Profile,
    pub res_y411: bool,
    pub frmrtq_postproc: Bounded<7>,
    pub bitrtq_postproc: Bounded<31>,
    pub loop_filter: bool,
    pub res_x8: bool,
    pub multires: bool,
    pub res_fasttx: bool,
    pub fastuvmc: bool,
    pub extended_mv: bool,
    pub dquant: Bounded<3>,
    pub vstransform: bool,
    pub res_transtab: bool,
    pub overlap: bool,
    pub syncmarker: bool,
    pub rangered: bool,
    pub max_b_frames: Bounded<7>,
    pub quantizer: QuantizerMode,
    pub finterpflag: bool,
    pub res_rtm_flag: bool,
    /// 16 bits FFmpeg reads after the header when RES_FASTTX is clear
    /// (vc1.c notes they are always 0x402F). Their meaning is undocumented.
    /// `None` when RES_FASTTX is set or fewer than 16 bits follow.
    pub fasttx_extra: Option<u16>,
    /// Bytes after the header. The WMV9 encoder appends one zero byte.
    pub trailing: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SequenceError {
    #[error(transparent)]
    OutOfBits(#[from] OutOfBits),
    #[error("Advanced profile uses a different sequence header")]
    AdvancedProfile,
    #[error("sprite (WMVP) sequence headers are not supported")]
    Sprite,
    #[error("RES_FASTTX extra bits present while RES_FASTTX is set")]
    FasttxExtraWithFasttx,
}

impl SequenceHeader {
    pub fn parse(data: &[u8]) -> Result<Self, SequenceError> {
        let mut r = BitReader::new(data);
        let profile = match r.read(2)? {
            0 => Profile::Simple,
            1 => Profile::Main,
            2 => Profile::Complex,
            _ => return Err(SequenceError::AdvancedProfile),
        };
        let res_y411 = r.bit()?;
        if r.bit()? {
            return Err(SequenceError::Sprite);
        }
        let frmrtq_postproc = Bounded::from_field(r.read(3)?);
        let bitrtq_postproc = Bounded::from_field(r.read(5)?);
        let loop_filter = r.bit()?;
        let res_x8 = r.bit()?;
        let multires = r.bit()?;
        let res_fasttx = r.bit()?;
        let fastuvmc = r.bit()?;
        let extended_mv = r.bit()?;
        let dquant = Bounded::from_field(r.read(2)?);
        let vstransform = r.bit()?;
        let res_transtab = r.bit()?;
        let overlap = r.bit()?;
        let syncmarker = r.bit()?;
        let rangered = r.bit()?;
        let max_b_frames = Bounded::from_field(r.read(3)?);
        let quantizer = match r.read(2)? {
            0 => QuantizerMode::Implicit,
            1 => QuantizerMode::Explicit,
            2 => QuantizerMode::NonUniform,
            _ => QuantizerMode::Uniform,
        };
        let finterpflag = r.bit()?;
        let res_rtm_flag = r.bit()?;
        let fasttx_extra = if !res_fasttx && r.remaining() >= 16 {
            Some(r.read(16)? as u16)
        } else {
            None
        };
        let trailing = r
            .rest_bytes()
            .expect("header fields total a whole number of bytes")
            .to_vec();
        Ok(SequenceHeader {
            profile,
            res_y411,
            frmrtq_postproc,
            bitrtq_postproc,
            loop_filter,
            res_x8,
            multires,
            res_fasttx,
            fastuvmc,
            extended_mv,
            dquant,
            vstransform,
            res_transtab,
            overlap,
            syncmarker,
            rangered,
            max_b_frames,
            quantizer,
            finterpflag,
            res_rtm_flag,
            fasttx_extra,
            trailing,
        })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, SequenceError> {
        if self.res_fasttx && self.fasttx_extra.is_some() {
            return Err(SequenceError::FasttxExtraWithFasttx);
        }
        let mut w = BitWriter::new();
        w.write(
            2,
            match self.profile {
                Profile::Simple => 0,
                Profile::Main => 1,
                Profile::Complex => 2,
            },
        );
        w.bit(self.res_y411);
        w.bit(false);
        w.write(3, self.frmrtq_postproc.get().into());
        w.write(5, self.bitrtq_postproc.get().into());
        w.bit(self.loop_filter);
        w.bit(self.res_x8);
        w.bit(self.multires);
        w.bit(self.res_fasttx);
        w.bit(self.fastuvmc);
        w.bit(self.extended_mv);
        w.write(2, self.dquant.get().into());
        w.bit(self.vstransform);
        w.bit(self.res_transtab);
        w.bit(self.overlap);
        w.bit(self.syncmarker);
        w.bit(self.rangered);
        w.write(3, self.max_b_frames.get().into());
        w.write(
            2,
            match self.quantizer {
                QuantizerMode::Implicit => 0,
                QuantizerMode::Explicit => 1,
                QuantizerMode::NonUniform => 2,
                QuantizerMode::Uniform => 3,
            },
        );
        w.bit(self.finterpflag);
        w.bit(self.res_rtm_flag);
        if let Some(extra) = self.fasttx_extra {
            w.write(16, extra.into());
        }
        w.bytes(&self.trailing);
        Ok(w.finish())
    }
}
