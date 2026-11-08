//! Lazily built code tables.

use std::sync::LazyLock;

use vc1_tables as t;

use super::CodeTable;
use super::vlc::Vlc;

pub static IMODE: LazyLock<Vlc> =
    LazyLock::new(|| Vlc::new(CodeTable::Imode, &t::VC1_IMODE_CODES, &t::VC1_IMODE_BITS));
pub static NORM2: LazyLock<Vlc> =
    LazyLock::new(|| Vlc::new(CodeTable::Norm2, &t::VC1_NORM2_CODES, &t::VC1_NORM2_BITS));
pub static NORM6: LazyLock<Vlc> =
    LazyLock::new(|| Vlc::new(CodeTable::Norm6, &t::VC1_NORM6_CODES, &t::VC1_NORM6_BITS));

/// Intra macroblock CBPCY (MSMPEG4 table), `[code, length]` as u16 pairs.
pub static INTRA_CBPCY: LazyLock<Vlc> = LazyLock::new(|| {
    let pairs: Vec<[u32; 2]> = t::MSMP4_MB_I_TABLE
        .iter()
        .map(|p| [p[0].into(), p[1].into()])
        .collect();
    Vlc::from_pairs(CodeTable::IntraCbpcy, &pairs)
});

/// DC differential tables by TRANSDCTAB, then luma (0) or chroma (1).
pub static DC: LazyLock<[[Vlc; 2]; 2]> = LazyLock::new(|| {
    std::array::from_fn(|table| {
        std::array::from_fn(|plane| {
            Vlc::from_pairs(
                CodeTable::Dc {
                    table: table as u8,
                    chroma: plane == 1,
                },
                &t::MSMP4_DC_TABLES[table][plane],
            )
        })
    })
});

/// AC coefficient tables by coding set; the last symbol is the escape.
pub static AC: LazyLock<[Vlc; 8]> = LazyLock::new(|| {
    std::array::from_fn(|cs| {
        let size = t::VC1_AC_SIZES[cs] as usize;
        Vlc::from_pairs(CodeTable::Ac(cs as u8), &t::VC1_AC_TABLES[cs][..size])
    })
});

fn per_table<const N: usize, C: Copy + Into<u32>, const M: usize>(
    name: fn(u8) -> CodeTable,
    codes: &[[C; M]; N],
    lens: &[[u8; M]; N],
) -> [Vlc; N] {
    std::array::from_fn(|i| Vlc::new(name(i as u8), &codes[i], &lens[i]))
}

/// P and B picture CBPCY by CBPTAB.
pub static CBPCY_P: LazyLock<[Vlc; 4]> = LazyLock::new(|| {
    per_table(
        CodeTable::CbpcyP,
        &t::VC1_CBPCY_P_CODES,
        &t::VC1_CBPCY_P_BITS,
    )
});

/// Motion vector differential by MVTAB.
pub static MV_DIFF: LazyLock<[Vlc; 4]> = LazyLock::new(|| {
    per_table(
        CodeTable::MvDiff,
        &t::VC1_MV_DIFF_CODES,
        &t::VC1_MV_DIFF_BITS,
    )
});

/// Macroblock transform type by the quantizer class.
pub static TTMB: LazyLock<[Vlc; 3]> =
    LazyLock::new(|| per_table(CodeTable::Ttmb, &t::VC1_TTMB_CODES, &t::VC1_TTMB_BITS));

/// Block transform type by the quantizer class.
pub static TTBLK: LazyLock<[Vlc; 3]> =
    LazyLock::new(|| per_table(CodeTable::Ttblk, &t::VC1_TTBLK_CODES, &t::VC1_TTBLK_BITS));

/// 4x4 subblock pattern by the quantizer class.
pub static SUBBLKPAT: LazyLock<[Vlc; 3]> = LazyLock::new(|| {
    per_table(
        CodeTable::Subblkpat,
        &t::VC1_SUBBLKPAT_CODES,
        &t::VC1_SUBBLKPAT_BITS,
    )
});
