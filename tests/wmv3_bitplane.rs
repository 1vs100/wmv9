//! Bitplane layouts the sample sizes never reach: NORM-6 residual columns
//! in both tilings. Bits assembled by hand from SMPTE 421M 8.7 and the
//! IMODE / NORM-6 code tables.

use wmv9::wmv3::bits::{BitReader, BitWriter};
use wmv9::wmv3::{Bitplane, BitplaneCoding, MbDims, SkipLine};

fn round_trip(bytes: &[u8], dims: MbDims, expected: &Bitplane) {
    let mut r = BitReader::new(bytes);
    let bp = Bitplane::read(&mut r, dims).unwrap();
    assert_eq!(&bp, expected);
    let mut w = BitWriter::new();
    bp.write(&mut w, dims).unwrap();
    assert_eq!(w.finish(), bytes);
}

/// 5x4 macroblocks: height not a multiple of 3, so 3x2 tiles cover columns
/// 2..5 and two residual columns precede them.
///   1        invert
///   11       IMODE NORM-6
///   1        tile 0 = 0
///   000111   tile 1 = 63
///   0        residual column 0 all zero
///   1 1011   residual column 1 coded
#[test]
fn norm6_3x2_with_residual_columns() {
    round_trip(
        &[0xf1, 0xdb],
        MbDims {
            width: 5,
            height: 4,
        },
        &Bitplane {
            invert: true,
            coding: BitplaneCoding::Norm6 {
                differential: false,
                tiles: vec![0, 63],
                residual_columns: vec![
                    SkipLine::Zero,
                    SkipLine::Coded(vec![true, false, true, true]),
                ],
                residual_rows: vec![],
            },
        },
    );
}

/// 5x3 macroblocks: height a multiple of 3 and width not, so 2x3 tiles
/// cover columns 1..5 and one residual column precedes them.
///   0          invert
///   0001       IMODE DIFF-6
///   0010       tile 0 = 1
///   00000001   tile 1 = 5
///   1 001      residual column coded
#[test]
fn diff6_2x3_with_residual_column() {
    round_trip(
        &[0x09, 0x00, 0xc8],
        MbDims {
            width: 5,
            height: 3,
        },
        &Bitplane {
            invert: false,
            coding: BitplaneCoding::Norm6 {
                differential: true,
                tiles: vec![1, 5],
                residual_columns: vec![SkipLine::Coded(vec![false, false, true])],
                residual_rows: vec![],
            },
        },
    );
}
