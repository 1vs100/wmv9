mod common;

use std::collections::BTreeMap;
use std::fmt::Write;

use wmv9::asf::Asf;
use wmv9::wmv3::{
    AcCoeff, BMbBody, Bitplane, DcDiff, Frame, FrameBody, FrameContext, InterBlock, PBlock,
    PMbBody, PictureKind,
};

fn type_letter(k: &PictureKind) -> &'static str {
    match k {
        PictureKind::I { .. } => "I",
        PictureKind::Bi { .. } => "BI",
        PictureKind::P(_) => "P",
        PictureKind::B { .. } => "B",
    }
}

#[test]
fn every_sample_frame_round_trips_byte_exact() {
    for path in common::sample_files() {
        let asf = Asf::parse(&std::fs::read(&path).unwrap()).unwrap();
        let v = common::video_stream(&asf);
        let ctx = FrameContext {
            seq: &v.seq,
            width: v.width,
            height: v.height,
        };
        for (i, obj) in v.frames.iter().enumerate() {
            let frame = Frame::parse(&obj.data, &ctx)
                .unwrap_or_else(|e| panic!("{} frame {i}: {e}", path.display()));
            let t = match &frame.body {
                FrameBody::Intra(layer) => &layer.trailing,
                FrameBody::P(layer) => &layer.trailing,
                FrameBody::B(layer) => &layer.trailing,
            };
            assert!(
                t.bit_len() < 8 && t.is_zero(),
                "{} frame {i}: macroblock layer leaves {} bits",
                path.display(),
                t.bit_len()
            );
            let written = frame.to_bytes(&ctx).unwrap();
            assert_eq!(written, obj.data, "{} frame {i}", path.display());
        }
    }
}

/// Picture types and bitplane modes per sample. Checked against FFmpeg's
/// debug log ("MB Skip plane encoding: Imode: %i, Invert: %i" and kin) and
/// `ffprobe -show_frames` type counts when accepted.
fn summary(path: &std::path::Path) -> String {
    let asf = Asf::parse(&std::fs::read(path).unwrap()).unwrap();
    let v = common::video_stream(&asf);
    let ctx = FrameContext {
        seq: &v.seq,
        width: v.width,
        height: v.height,
    };
    let mut types: BTreeMap<&str, usize> = BTreeMap::new();
    let mut pq: BTreeMap<u8, usize> = BTreeMap::new();
    let mut planes: BTreeMap<String, usize> = BTreeMap::new();
    let mut intra_tail: BTreeMap<String, usize> = BTreeMap::new();
    let mut forms: BTreeMap<&str, usize> = BTreeMap::new();
    let mut p_forms: BTreeMap<&str, usize> = BTreeMap::new();
    let mut b_forms: BTreeMap<&str, usize> = BTreeMap::new();
    let mut plane = |name: &str, bp: &Bitplane| {
        *planes
            .entry(format!(
                "{name} imode={} invert={}",
                bp.imode(),
                u8::from(bp.invert)
            ))
            .or_default() += 1;
    };
    for obj in &v.frames {
        let f = Frame::parse(&obj.data, &ctx).unwrap();
        if let FrameBody::P(layer) = &f.body {
            p_summary(&mut p_forms, layer);
        }
        if let FrameBody::B(layer) = &f.body {
            b_summary(&mut b_forms, layer);
        }
        if let FrameBody::Intra(layer) = &f.body {
            let t = &layer.trailing;
            let zero = if t.is_zero() { "zero" } else { "NONZERO" };
            *intra_tail
                .entry(format!("{} bits {zero}", t.bit_len()))
                .or_default() += 1;
            for b in layer.macroblocks.iter().flat_map(|m| &m.blocks) {
                *forms
                    .entry(match b.dc {
                        DcDiff::Zero => "dc zero",
                        DcDiff::Coded { .. } => "dc coded",
                        DcDiff::Escape { .. } => "dc escape",
                    })
                    .or_default() += 1;
                for c in &b.ac {
                    *forms
                        .entry(match c {
                            AcCoeff::Direct { .. } => "ac direct",
                            AcCoeff::LevelEscape { .. } => "ac level escape",
                            AcCoeff::RunEscape { .. } => "ac run escape",
                            AcCoeff::Explicit {
                                lengths: Some(_), ..
                            } => "ac explicit first",
                            AcCoeff::Explicit { lengths: None, .. } => "ac explicit",
                        })
                        .or_default() += 1;
                }
            }
        }
        *types.entry(type_letter(&f.header.kind)).or_default() += 1;
        *pq.entry(f.header.pqindex.get()).or_default() += 1;
        match &f.header.kind {
            PictureKind::P(p) => {
                if let Some(mt) = &p.mvtypemb {
                    plane("mvtype", mt);
                }
                plane("skip", &p.skipmb);
            }
            PictureKind::B { header, .. } => {
                plane("direct", &header.directmb);
                plane("skip", &header.skipmb);
            }
            _ => {}
        }
    }
    let mut s = String::new();
    writeln!(s, "types: {types:?}").unwrap();
    writeln!(s, "pqindex: {pq:?}").unwrap();
    writeln!(s, "intra trailing: {intra_tail:?}").unwrap();
    writeln!(s, "p forms: {p_forms:?}").unwrap();
    writeln!(s, "b forms: {b_forms:?}").unwrap();
    writeln!(s, "intra forms: {forms:?}").unwrap();
    for (k, n) in planes {
        writeln!(s, "{k}: {n}").unwrap();
    }
    s
}

#[test]
fn sample_picture_headers() {
    for path in common::sample_files() {
        insta::assert_snapshot!(common::snapshot_name(&path), summary(&path));
    }
}

fn p_summary(forms: &mut BTreeMap<&'static str, usize>, layer: &wmv9::wmv3::InterLayer) {
    let mut add = |k: &'static str, n: usize| *forms.entry(k).or_default() += n;
    let inter = |add: &mut dyn FnMut(&'static str, usize), b: &InterBlock| {
        add("inter blocks", 1);
        add("ttblk", usize::from(b.ttblk.is_some()));
        add("subblkpat", usize::from(b.subblkpat.is_some()));
        add("halves", usize::from(b.halves.is_some()));
        add("subblocks", b.subblocks.len());
    };
    let blocks = |add: &mut dyn FnMut(&'static str, usize), blocks: &[PBlock]| {
        for b in blocks {
            match b {
                PBlock::Intra(_) => add("intra blocks", 1),
                PBlock::Inter(ib) => inter(add, ib),
                PBlock::Empty => {}
            }
        }
    };
    for mb in &layer.macroblocks {
        add("raw mvtype bits", usize::from(mb.mvtype_bit.is_some()));
        add("raw skip bits", usize::from(mb.skip_bit.is_some()));
        match &mb.body {
            PMbBody::SkippedOneMv { hybrid } => {
                add("skipped 1mv", 1);
                add("hybrid", usize::from(hybrid.is_some()));
            }
            PMbBody::SkippedFourMv { hybrid } => {
                add("skipped 4mv", 1);
                add("hybrid", hybrid.iter().flatten().count());
            }
            PMbBody::OneMv(m) => {
                add(
                    if m.ac_pred.is_some() {
                        "1mv intra"
                    } else {
                        "1mv"
                    },
                    1,
                );
                add("hybrid", usize::from(m.hybrid.is_some()));
                add("ttmb", usize::from(m.ttmb.is_some()));
                add(
                    "mv escape",
                    usize::from(matches!(m.mv.symbol.get(), 34 | 71)),
                );
                blocks(&mut add, &m.blocks);
            }
            PMbBody::FourMv(m) => {
                add("4mv", 1);
                add("4mv ac_pred", usize::from(m.ac_pred.is_some()));
                add("hybrid", m.hybrid.iter().flatten().count());
                add("ttmb", usize::from(m.ttmb.is_some()));
                blocks(&mut add, &m.blocks);
            }
        }
    }
}

fn b_summary(forms: &mut BTreeMap<&'static str, usize>, layer: &wmv9::wmv3::BLayer) {
    let mut add = |k: &'static str, n: usize| *forms.entry(k).or_default() += n;
    for mb in &layer.macroblocks {
        add("raw direct bits", usize::from(mb.direct_bit.is_some()));
        add("raw skip bits", usize::from(mb.skip_bit.is_some()));
        match &mb.body {
            BMbBody::SkippedDirect => add("skipped direct", 1),
            BMbBody::Skipped { .. } => add("skipped", 1),
            BMbBody::Direct(d) => {
                add("direct", 1);
                add("direct ttmb", usize::from(d.ttmb.is_some()));
            }
            BMbBody::Coded(m) => {
                add("coded", 1);
                add("interpolated mv2", usize::from(m.mv2.is_some()));
                add("intra", usize::from(m.ac_pred.is_some()));
                add("cbpcy", usize::from(m.cbpcy.is_some()));
                add(
                    "inter blocks",
                    m.blocks
                        .iter()
                        .filter(|b| matches!(b, PBlock::Inter(_)))
                        .count(),
                );
            }
        }
    }
}
