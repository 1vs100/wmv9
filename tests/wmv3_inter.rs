//! P picture paths the samples never reach, and writer validation. Bits
//! assembled by hand from SMPTE 421M 7.1.3 and FFmpeg's vc1_decode_p_mb.

use wmv9::wmv3::{Frame, FrameBody, FrameContext, FrameError, MbFault, PMbBody, SequenceHeader};

/// Packs a string of '0' and '1' (spaces ignored) MSB first, zero padded.
fn bits(s: &str) -> Vec<u8> {
    let bits: Vec<u8> = s.bytes().filter(|b| *b != b' ').map(|b| b - b'0').collect();
    bits.chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0, |acc, (i, b)| acc | b << (7 - i))
        })
        .collect()
}

/// The WMV9 DMO header: no B-frames, implicit quantizer, VSTRANSFORM.
fn sequence() -> SequenceHeader {
    SequenceHeader::parse(&[0x4e, 0x79, 0x0a, 0x01, 0x00]).unwrap()
}

fn ctx(seq: &SequenceHeader) -> FrameContext<'_> {
    FrameContext {
        seq,
        width: 16,
        height: 16,
    }
}

/// PQINDEX 3 maps to PQUANT 3, whose MVMODE row puts mixed MV at index 1,
/// so MVTYPEMB is sent; both bitplanes are raw. The single macroblock is
/// 4MV and skipped. None of its four predictions have both the A and C
/// neighbours with differing vectors, so no HYBRIDPRED bits follow.
fn raw_planes_frame() -> Vec<u8> {
    bits(concat!(
        "00 1 00011 0 ", // FRAMECNT PTYPE=P PQINDEX=3 HALFQP
        "01 ",           // MVMODE index 1: mixed MV
        "0 0000 ",       // MVTYPEMB: raw
        "0 0000 ",       // SKIPMB: raw
        "00 00 1 00 ",   // MVTAB CBPTAB TTMBF TTFRM=8x8
        "0 0 ",          // TRANSACFRM TRANSDCTAB
        "1 1",           // macroblock: MVTYPEMB=4MV, SKIPMB=skipped
    ))
}

#[test]
fn raw_bitplanes_send_one_bit_per_macroblock() {
    let seq = sequence();
    let data = raw_planes_frame();
    let f = Frame::parse(&data, &ctx(&seq)).unwrap();
    let FrameBody::P(layer) = &f.body else {
        panic!("expected a P body");
    };
    let mb = &layer.macroblocks[0];
    assert_eq!(mb.mvtype_bit, Some(true));
    assert_eq!(mb.skip_bit, Some(true));
    assert_eq!(mb.body, PMbBody::SkippedFourMv { hybrid: [None; 4] });
    assert_eq!(layer.trailing.bit_len(), 0);
    assert_eq!(f.to_bytes(&ctx(&seq)).unwrap(), data);
}

fn mutated(f: impl FnOnce(&mut wmv9::wmv3::PMb)) -> FrameError {
    let seq = sequence();
    let mut frame = Frame::parse(&raw_planes_frame(), &ctx(&seq)).unwrap();
    let FrameBody::P(layer) = &mut frame.body else {
        panic!("expected a P body");
    };
    f(&mut layer.macroblocks[0]);
    frame.to_bytes(&ctx(&seq)).unwrap_err()
}

#[test]
fn writer_requires_raw_plane_bits() {
    assert_eq!(
        mutated(|mb| mb.skip_bit = None),
        FrameError::Macroblock {
            mb: 0,
            fault: MbFault::RawPlaneBit
        }
    );
}

#[test]
fn writer_rejects_hybridpred_without_disagreeing_predictors() {
    assert_eq!(
        mutated(|mb| mb.body = PMbBody::SkippedFourMv {
            hybrid: [Some(true), None, None, None]
        }),
        FrameError::Macroblock {
            mb: 0,
            fault: MbFault::Hybrid
        }
    );
}

#[test]
fn writer_rejects_body_kind_against_plane() {
    assert_eq!(
        mutated(|mb| mb.body = PMbBody::SkippedOneMv { hybrid: None }),
        FrameError::Macroblock {
            mb: 0,
            fault: MbFault::Kind
        }
    );
}
