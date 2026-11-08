//! Intra macroblock paths the samples never reach, and writer validation.
//! Bits assembled by hand from SMPTE 421M 7.1.4 and the MSMPEG4 DC and
//! intra CBPCY code tables.

use wmv9::wmv3::{
    AcCoeff, BlockError, BlockFault, Bounded, DcDiff, Esc3Lengths, Frame, FrameBody, FrameContext,
    FrameError, IntraMb, SequenceHeader,
};

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

/// The WMV9 DMO header: no B-frames, implicit quantizer.
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

/// One-macroblock I picture at PQINDEX 1 (PQUANT 1): DC differentials
/// carry two extra bits and escapes ten bits.
fn pquant1_frame() -> Vec<u8> {
    bits(concat!(
        "00 0 0000000 00001 0 0 0 0 ", // FRAMECNT PTYPE=I BF PQINDEX HALFQP AC AC2 DCTAB
        "1 0 ",                        // CBPCY=0 ACPRED
        "01 10 1 ",                    // Y0: DC index 1, extra 2, negative
        "000001100000011110001111 1000000001 0 ", // Y1: DC escape 513
        "1 1 00 00",                   // Y2, Y3, Cb, Cr: DC zero
    ))
}

fn intra(f: &mut Frame) -> &mut IntraMb {
    let FrameBody::Intra(layer) = &mut f.body else {
        panic!("expected an intra body");
    };
    &mut layer.macroblocks[0]
}

#[test]
fn pquant1_dc_extra_bits_and_escape() {
    let seq = sequence();
    let data = pquant1_frame();
    let mut f = Frame::parse(&data, &ctx(&seq)).unwrap();
    assert_eq!(f.header.pquant(seq.quantizer).get(), 1);
    let mb = intra(&mut f);
    assert_eq!(mb.cbpcy.get(), 0);
    assert_eq!(
        mb.blocks[0].dc,
        DcDiff::Coded {
            index: 1,
            extra: 2,
            negative: true
        }
    );
    assert_eq!(
        mb.blocks[1].dc,
        DcDiff::Escape {
            value: 513,
            negative: false
        }
    );
    assert!(
        mb.blocks[2..]
            .iter()
            .all(|b| b.dc == DcDiff::Zero && b.ac.is_empty())
    );
    assert_eq!(f.to_bytes(&ctx(&seq)).unwrap(), data);
}

fn write_error(f: &Frame, seq: &SequenceHeader) -> FrameError {
    f.to_bytes(&ctx(seq)).unwrap_err()
}

fn block_error(block: usize, fault: BlockFault) -> FrameError {
    FrameError::Block(BlockError {
        mb: 0,
        block,
        fault,
    })
}

#[test]
fn writer_rejects_dc_extra_bits_out_of_range() {
    let seq = sequence();
    let mut f = Frame::parse(&pquant1_frame(), &ctx(&seq)).unwrap();
    intra(&mut f).blocks[0].dc = DcDiff::Coded {
        index: 1,
        extra: 4,
        negative: false,
    };
    assert_eq!(write_error(&f, &seq), block_error(0, BlockFault::DcRange));
}

#[test]
fn writer_rejects_coefficients_on_uncoded_block() {
    let seq = sequence();
    let mut f = Frame::parse(&pquant1_frame(), &ctx(&seq)).unwrap();
    intra(&mut f).blocks[2].ac.push(AcCoeff::Direct {
        index: 0,
        negative: false,
    });
    assert_eq!(
        write_error(&f, &seq),
        block_error(2, BlockFault::CoefficientList)
    );
}

fn explicit(run: u8, level: u16) -> AcCoeff {
    AcCoeff::Explicit {
        lengths: Some(Esc3Lengths { level: 8, run: 6 }),
        last: true,
        run,
        negative: false,
        level,
    }
}

/// CBPCY 2 codes the Cb block, whose coded flag is not predicted.
#[test]
fn writer_rejects_run_past_last_position() {
    let seq = sequence();
    let mut f = Frame::parse(&pquant1_frame(), &ctx(&seq)).unwrap();
    let mb = intra(&mut f);
    mb.cbpcy = Bounded::new(2).unwrap();
    mb.blocks[4].ac.push(explicit(63, 1));
    assert_eq!(
        write_error(&f, &seq),
        block_error(4, BlockFault::CoefficientOverrun)
    );
}

/// At PQUANT below 8 the ESC3 level width uses table 59; a written escape
/// must read back unchanged.
#[test]
fn explicit_escape_written_with_table59_reads_back() {
    let seq = sequence();
    let mut f = Frame::parse(&pquant1_frame(), &ctx(&seq)).unwrap();
    let mb = intra(&mut f);
    mb.cbpcy = Bounded::new(2).unwrap();
    mb.blocks[4].ac.push(explicit(62, 200));
    let written = f.to_bytes(&ctx(&seq)).unwrap();
    let mut back = Frame::parse(&written, &ctx(&seq)).unwrap();
    assert_eq!(back.header, f.header);
    assert_eq!(intra(&mut back), intra(&mut f));
}
