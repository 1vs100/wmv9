mod common;

use wmv9::asf::{Asf, TypeSpecificData};
use wmv9::wmv3::{Profile, QuantizerMode, SequenceHeader};

/// Header the genuine WMV9 encoder DMO produced for 320x240 at 500 kbps
/// (see MODLOG.md). Field values decoded by hand per SMPTE 421M Annex J.
#[test]
fn dmo_oracle_header_fields() {
    let bytes = [0x4e, 0x79, 0x0a, 0x01, 0x00];
    let h = SequenceHeader::parse(&bytes).unwrap();
    assert_eq!(h.profile, Profile::Main);
    assert_eq!(h.frmrtq_postproc.get(), 7);
    assert_eq!(h.bitrtq_postproc.get(), 7);
    assert!(h.loop_filter);
    assert!(!h.multires);
    assert!(h.res_fasttx);
    assert!(!h.fastuvmc);
    assert!(!h.extended_mv);
    assert_eq!(h.dquant.get(), 0);
    assert!(h.vstransform);
    assert!(h.overlap);
    assert!(!h.syncmarker);
    assert!(!h.rangered);
    assert_eq!(h.max_b_frames.get(), 0);
    assert_eq!(h.quantizer, QuantizerMode::Implicit);
    assert!(!h.finterpflag);
    assert!(h.res_rtm_flag);
    assert_eq!(h.trailing, [0x00]);
    assert_eq!(h.to_bytes().unwrap(), bytes);
}

/// FFmpeg's vc1.c reads 16 more bits when RES_FASTTX is clear and notes
/// they are always 0x402F.
#[test]
fn res_fasttx_clear_reads_sixteen_more_bits() {
    let bytes = [0x4e, 0x78, 0x0a, 0x01, 0x40, 0x2f];
    let h = SequenceHeader::parse(&bytes).unwrap();
    assert!(!h.res_fasttx);
    assert_eq!(h.fasttx_extra, Some(0x402f));
    assert!(h.trailing.is_empty());
    assert_eq!(h.to_bytes().unwrap(), bytes);
}

#[test]
fn every_sample_sequence_header_round_trips() {
    for path in common::sample_files() {
        let asf = Asf::parse(&std::fs::read(&path).unwrap()).unwrap();
        for sp in asf.stream_properties() {
            if let TypeSpecificData::Video(v) = &sp.type_specific {
                let h = SequenceHeader::parse(&v.codec_data)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                assert_eq!(h.to_bytes().unwrap(), v.codec_data, "{}", path.display());
            }
        }
    }
}

#[test]
fn res_fasttx_clear_without_extra_bits_keeps_trailing() {
    let bytes = [0x4e, 0x78, 0x0a, 0x01, 0x00];
    let h = SequenceHeader::parse(&bytes).unwrap();
    assert!(!h.res_fasttx);
    assert_eq!(h.fasttx_extra, None);
    assert_eq!(h.trailing, [0x00]);
    assert_eq!(h.to_bytes().unwrap(), bytes);
}
