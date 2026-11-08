//! Random bitstreams in sequence contexts the samples never use. Every
//! stream that parses must write back unchanged, and nothing may panic.
//! This checks reader/writer symmetry on those paths; it is not a check
//! against a reference decoder.

use std::collections::BTreeMap;

use wmv9::wmv3::{
    BMbBody, Bounded, Frame, FrameBody, FrameContext, PMbBody, QuantizerMode, SequenceHeader,
};

/// xorshift64*, fixed seed for reproducible runs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let len = 1 + (self.next() as usize) % max;
        (0..len).map(|_| self.next() as u8).collect()
    }
}

struct Case {
    name: &'static str,
    seq: SequenceHeader,
    width: u16,
    height: u16,
}

fn cases() -> Vec<Case> {
    let base = SequenceHeader::parse(&[0x4e, 0x79, 0x0a, 0x01, 0x00]).unwrap();
    let with = |f: fn(&mut SequenceHeader)| {
        let mut s = base.clone();
        f(&mut s);
        s
    };
    vec![
        Case {
            name: "rtm off",
            seq: with(|s| s.res_rtm_flag = false),
            width: 48,
            height: 48,
        },
        Case {
            name: "extended mv",
            seq: with(|s| {
                s.extended_mv = true;
                s.quantizer = QuantizerMode::Explicit;
            }),
            width: 48,
            height: 32,
        },
        Case {
            name: "one macroblock wide",
            seq: with(|s| s.max_b_frames = Bounded::new(1).unwrap()),
            width: 16,
            height: 48,
        },
        Case {
            name: "vstransform off",
            seq: with(|s| s.vstransform = false),
            width: 32,
            height: 32,
        },
    ]
}

#[test]
fn parsed_random_streams_round_trip() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut report = BTreeMap::new();
    for case in cases() {
        let ctx = FrameContext {
            seq: &case.seq,
            width: case.width,
            height: case.height,
        };
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for _ in 0..200_000 {
            let data = rng.bytes(48);
            let Ok(frame) = Frame::parse(&data, &ctx) else {
                continue;
            };
            let written = frame
                .to_bytes(&ctx)
                .unwrap_or_else(|e| panic!("{}: {e} for {data:02x?}", case.name));
            assert_eq!(written, data, "{}", case.name);
            *seen.entry("parsed").or_default() += 1;
            if let FrameBody::B(layer) = &frame.body {
                *seen.entry("b").or_default() += 1;
                for mb in &layer.macroblocks {
                    if mb.direct_bit.is_some() || mb.skip_bit.is_some() {
                        *seen.entry("b raw plane bit").or_default() += 1;
                    }
                    let key = match &mb.body {
                        BMbBody::Coded(m) if m.mv2.is_some() => "b interpolated",
                        BMbBody::Coded(m) if m.ac_pred.is_some() => "b intra",
                        BMbBody::Coded(_) => "b coded",
                        BMbBody::Direct(_) => "b direct",
                        BMbBody::Skipped { .. } | BMbBody::SkippedDirect => "b skipped",
                    };
                    *seen.entry(key).or_default() += 1;
                }
            }
            if let FrameBody::P(layer) = &frame.body {
                *seen.entry("p").or_default() += 1;
                for mb in &layer.macroblocks {
                    if mb.mvtype_bit.is_some() || mb.skip_bit.is_some() {
                        *seen.entry("p raw plane bit").or_default() += 1;
                    }
                    let key = match &mb.body {
                        PMbBody::FourMv(m) if m.hybrid.iter().any(Option::is_some) => "4mv hybrid",
                        PMbBody::FourMv(_) => "4mv",
                        PMbBody::OneMv(m) if m.hybrid.is_some() => "1mv hybrid",
                        PMbBody::OneMv(_) => "1mv",
                        PMbBody::SkippedOneMv { .. } | PMbBody::SkippedFourMv { .. } => "skipped",
                    };
                    *seen.entry(key).or_default() += 1;
                }
            }
        }
        report.insert(case.name, seen);
    }
    eprintln!("coverage: {report:?}");
    for (name, seen) in &report {
        for key in ["p", "1mv", "4mv"] {
            assert!(
                seen.get(key).copied().unwrap_or(0) > 0,
                "{name}: no {key} macroblocks reached: {seen:?}"
            );
        }
    }
    let hybrid: usize = report
        .values()
        .map(|s| s.get("1mv hybrid").unwrap_or(&0) + s.get("4mv hybrid").unwrap_or(&0))
        .sum();
    assert!(hybrid > 0, "no HYBRIDPRED reached: {report:?}");
    let b = &report["one macroblock wide"];
    for key in [
        "b coded",
        "b interpolated",
        "b intra",
        "b direct",
        "b skipped",
        "b raw plane bit",
        "p raw plane bit",
    ] {
        assert!(
            b.get(key).copied().unwrap_or(0) > 0,
            "no {key} reached: {b:?}"
        );
    }
}
