//! Picture header paths the samples never reach. Bits assembled by hand
//! from SMPTE 421M 7.1.1 and FFmpeg's ff_vc1_parse_frame_header.

use wmv9::wmv3::bits::{BitReader, BitWriter};
use wmv9::wmv3::{
    BFractionIndex, Bitplane, BitplaneCoding, FrameContext, FrameError, MvMode, PictureHeader,
    PictureKind, QuantizerMode, SequenceHeader, TransformSignal,
};

/// Packs a string of '0' and '1' (spaces ignored) MSB first, zero padded.
/// Returns the bytes and the bit count.
fn bits(s: &str) -> (Vec<u8>, usize) {
    let bits: Vec<u8> = s.bytes().filter(|b| *b != b' ').map(|b| b - b'0').collect();
    let bytes = bits
        .chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0, |acc, (i, b)| acc | b << (7 - i))
        })
        .collect();
    (bytes, bits.len())
}

/// Main profile with B-frames, MULTIRES, EXTENDED_MV, explicit quantizer
/// and VSTRANSFORM.
fn sequence() -> SequenceHeader {
    let mut s = SequenceHeader::parse(&[0x4e, 0x79, 0x0a, 0x01, 0x00]).unwrap();
    s.max_b_frames = wmv9::wmv3::Bounded::new(1).unwrap();
    s.multires = true;
    s.extended_mv = true;
    s.quantizer = QuantizerMode::Explicit;
    assert!(s.vstransform);
    s
}

/// One macroblock, so every bitplane is a single bit.
fn ctx(seq: &SequenceHeader) -> FrameContext<'_> {
    FrameContext {
        seq,
        width: 16,
        height: 16,
    }
}

fn raw_plane(invert: bool) -> Bitplane {
    Bitplane {
        invert,
        coding: BitplaneCoding::Raw,
    }
}

/// Reads a header that must span exactly the given bits, and writes it back.
fn round_trip((data, len): (Vec<u8>, usize), ctx: &FrameContext) -> PictureHeader {
    let mut r = BitReader::new(&data);
    let h = PictureHeader::read(&mut r, ctx).unwrap();
    assert_eq!(r.position(), len, "header length in bits");
    let mut w = BitWriter::new();
    h.write(&mut w, ctx).unwrap();
    assert_eq!(w.bit_len(), len);
    assert_eq!(w.finish(), data);
    h
}

/// BI pictures carry RESPIC like I and P pictures; only true B omits it.
#[test]
fn bi_picture_reads_respic() {
    let seq = sequence();
    let data = bits(
        "01 00 111 1111 0000011 01010 0 0 10 0 10 1",
        // FRAMECNT PTYPE=B BFRACTION=22(BI) BUFFER_FULLNESS PQINDEX=10
        // PQUANTIZER MVRANGE=0 RESPIC=2 TRANSACFRM=0 TRANSACFRM2=1 TRANSDCTAB
    );
    let h = &round_trip(data, &ctx(&seq));
    assert!(matches!(h.kind, PictureKind::Bi { buffer_fullness } if buffer_fullness.get() == 3));
    assert_eq!(h.respic.map(|v| v.get()), Some(2));
    assert_eq!(h.transacfrm2.map(|v| v.get()), Some(1));
    assert!(h.transdctab);
}

#[test]
fn b_picture_with_escaped_bfraction() {
    let seq = sequence();
    let data = bits(
        "00 00 111 0010 00011 1 0 110 1 0 0000 1 0000 10 01 1 11 11 0",
        // FRAMECNT PTYPE=B BFRACTION=9 PQINDEX=3 HALFQP PQUANTIZER
        // MVRANGE=2 MVMODE DIRECTMB SKIPMB MVTAB CBPTAB TTMBF TTFRM
        // TRANSACFRM=2 TRANSDCTAB
    );
    let h = &round_trip(data, &ctx(&seq));
    let PictureKind::B { bfraction, header } = &h.kind else {
        panic!("expected B, got {:?}", h.kind);
    };
    assert_eq!(bfraction.get(), 9);
    assert_eq!(h.halfqp, Some(true));
    assert_eq!(h.pquantizer, Some(false));
    assert_eq!(h.mvrange.map(|v| v.get()), Some(2));
    assert_eq!(h.respic, None);
    assert!(header.mvmode_quarter_pel);
    assert_eq!(header.directmb, raw_plane(false));
    assert_eq!(header.skipmb, raw_plane(true));
    assert_eq!(header.mvtab.get(), 2);
    assert_eq!(header.cbptab.get(), 1);
    assert!(matches!(header.transform, TransformSignal::Frame(t) if t.get() == 3));
    assert_eq!(h.transacfrm.get(), 2);
    assert_eq!(h.transacfrm2, None);
}

#[test]
fn reserved_bfraction_is_rejected() {
    let seq = sequence();
    let (data, _) = bits("00 00 111 1110 00000000");
    assert_eq!(
        PictureHeader::read(&mut BitReader::new(&data), &ctx(&seq)),
        Err(FrameError::ReservedBFraction { bit: 11 })
    );
    assert_eq!(BFractionIndex::new(21), None);
}

/// PQUANT 20 selects the high-quant MVMODE row, where index 3 is intensity
/// compensation; MVMODE2 index 3 (no stop bit at the maximum) is mixed MV.
#[test]
fn p_picture_with_intensity_compensation() {
    let seq = sequence();
    let data = bits(
        "11 1 10100 1 0 01 0001 000 101010 010101 0 0000 0 0000 00 11 0 0 1",
        // FRAMECNT PTYPE=P PQINDEX=20 PQUANTIZER MVRANGE=0 RESPIC=1
        // MVMODE=3 MVMODE2=3 LUMSCALE LUMSHIFT MVTYPEMB SKIPMB MVTAB CBPTAB
        // TTMBF TRANSACFRM TRANSDCTAB
    );
    let h = &round_trip(data, &ctx(&seq));
    let PictureKind::P(p) = &h.kind else {
        panic!("expected P, got {:?}", h.kind);
    };
    let pquant = h.pquant(seq.quantizer);
    assert_eq!(pquant.get(), 20);
    assert_eq!(p.mv_mode(pquant), MvMode::IntensityCompensation);
    assert_eq!(p.effective_mv_mode(pquant), MvMode::Mixed);
    let ic = p.intensity.as_ref().unwrap();
    assert_eq!((ic.lumscale.get(), ic.lumshift.get()), (42, 21));
    assert_eq!(p.mvtypemb, Some(raw_plane(false)));
    assert_eq!(p.cbptab.get(), 3);
    assert_eq!(p.transform, TransformSignal::PerMacroblock);
    assert_eq!(h.respic.map(|v| v.get()), Some(1));
}
