//! Random syntax-valid WMV3 streams decoded by FFmpeg and by this crate.
//! Each stream is an I picture followed by P, B and BI pictures built from
//! random bytes, so motion vectors, intensity compensation, quantizers and
//! transform types take values the samples never use. Every picture FFmpeg
//! outputs must have the same MD5 here. Requires `ffmpeg` on PATH.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use md5::{Digest, Md5};
use wmv9::wmv3::decode::{Decoder, PictureRole, display_order};
use wmv9::wmv3::{
    BMbBody, Bounded, Frame, FrameBody, FrameContext, OpaqueBits, PBlock, PMbBody, PictureKind,
    Profile, QuantizerMode, SequenceHeader,
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

/// Picture types of one stream in coded order.
#[derive(Clone, Copy)]
enum Pattern {
    /// I followed by P pictures.
    IP,
    /// I, then BI and P alternating, so one BI directly follows the I
    /// picture and the others sit between P pictures.
    WithBi,
    /// I, then P and B alternating, with a B right after the I picture,
    /// which FFmpeg drops for lack of a forward reference.
    WithB,
}

struct Case {
    name: &'static str,
    seq: SequenceHeader,
    width: u16,
    height: u16,
    pattern: Pattern,
}

fn sequence(f: impl Fn(&mut SequenceHeader)) -> SequenceHeader {
    let mut s = SequenceHeader::parse(&[0x4e, 0x79, 0x0a, 0x01, 0x00]).unwrap();
    s.profile = Profile::Main;
    s.res_fasttx = true;
    s.fasttx_extra = None;
    s.multires = false;
    s.rangered = false;
    s.syncmarker = false;
    s.dquant = Bounded::new(0).unwrap();
    s.max_b_frames = Bounded::new(0).unwrap();
    s.finterpflag = false;
    s.trailing.clear();
    f(&mut s);
    s
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "filters on",
            seq: sequence(|s| {
                s.loop_filter = true;
                s.overlap = true;
                s.fastuvmc = true;
                s.vstransform = true;
                s.quantizer = QuantizerMode::Implicit;
            }),
            width: 48,
            height: 48,
            pattern: Pattern::IP,
        },
        Case {
            name: "filters off",
            seq: sequence(|s| {
                s.loop_filter = false;
                s.overlap = false;
                s.fastuvmc = false;
                s.vstransform = true;
                s.quantizer = QuantizerMode::NonUniform;
            }),
            width: 48,
            height: 32,
            pattern: Pattern::IP,
        },
        Case {
            name: "extended mv",
            seq: sequence(|s| {
                s.loop_filter = true;
                s.overlap = true;
                s.extended_mv = true;
                s.quantizer = QuantizerMode::Explicit;
            }),
            width: 32,
            height: 48,
            pattern: Pattern::IP,
        },
        Case {
            name: "uncropped edge",
            seq: sequence(|s| {
                s.loop_filter = true;
                s.overlap = false;
                s.res_rtm_flag = false;
                s.vstransform = false;
                s.quantizer = QuantizerMode::Uniform;
            }),
            width: 40,
            height: 24,
            pattern: Pattern::IP,
        },
        Case {
            name: "sample tools with bi",
            seq: sequence(|s| {
                s.loop_filter = true;
                s.overlap = true;
                s.extended_mv = true;
                s.vstransform = true;
                s.max_b_frames = Bounded::new(1).unwrap();
                s.quantizer = QuantizerMode::Implicit;
            }),
            width: 48,
            height: 32,
            pattern: Pattern::WithBi,
        },
        Case {
            name: "b pictures",
            seq: sequence(|s| {
                s.loop_filter = true;
                s.overlap = true;
                s.extended_mv = true;
                s.vstransform = true;
                s.max_b_frames = Bounded::new(1).unwrap();
                s.quantizer = QuantizerMode::Implicit;
            }),
            width: 48,
            height: 32,
            pattern: Pattern::WithB,
        },
        Case {
            name: "b pictures fastuvmc",
            seq: sequence(|s| {
                s.loop_filter = false;
                s.overlap = false;
                s.fastuvmc = true;
                s.vstransform = true;
                s.max_b_frames = Bounded::new(2).unwrap();
                s.quantizer = QuantizerMode::Explicit;
            }),
            width: 32,
            height: 48,
            pattern: Pattern::WithB,
        },
        Case {
            name: "simple profile",
            seq: sequence(|s| {
                s.profile = Profile::Simple;
                // FFmpeg rejects Simple profile without FASTUVMC.
                s.fastuvmc = true;
                s.loop_filter = false;
                s.overlap = false;
                s.vstransform = true;
                s.quantizer = QuantizerMode::Implicit;
            }),
            width: 32,
            height: 32,
            pattern: Pattern::IP,
        },
    ]
}

/// Number of luma blocks of a 4MV macroblock that are intra.
fn intra_luma(blocks: &[PBlock; 6]) -> usize {
    blocks[..4]
        .iter()
        .filter(|b| matches!(b, PBlock::Intra(_)))
        .count()
}

/// Picture forms that plain random frames rarely give. One P and one B
/// picture of each stream are drawn until they contain the stream's form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Steer {
    Any,
    /// 4MV with two intra luma blocks: chroma averages two vectors.
    FourMvTwoIntra,
    /// 4MV with three or four intra luma blocks: chroma is intra.
    FourMvChromaIntra,
    /// 4MV with ACPRED set.
    FourMvAcPred,
    /// P picture with intensity compensation, which also applies to the
    /// forward prediction of the B picture that follows it in coded order.
    Intensity,
    /// Direct B macroblock with coefficients.
    BDirect,
    /// Interpolated B macroblock with a second MVDATA.
    BInterpolated,
    /// Intra B macroblock.
    BIntra,
    /// Interpolated B macroblock whose second MVDATA is intra without
    /// coefficients (symbol 35), which FFmpeg compensates with the previous
    /// macroblock's vectors.
    BStale,
}

const P_STEERS: [Steer; 5] = [
    Steer::Any,
    Steer::FourMvTwoIntra,
    Steer::FourMvChromaIntra,
    Steer::FourMvAcPred,
    Steer::Intensity,
];

const B_STEERS: [Steer; 5] = [
    Steer::Any,
    Steer::BDirect,
    Steer::BInterpolated,
    Steer::BIntra,
    Steer::BStale,
];

fn is_p(frame: &Frame) -> bool {
    matches!(frame.header.kind, PictureKind::P(_))
}

fn is_b(frame: &Frame) -> bool {
    matches!(frame.header.kind, PictureKind::B { .. })
}

fn has(frame: &Frame, steer: Steer) -> bool {
    match (&frame.body, &frame.header.kind) {
        (FrameBody::P(_), PictureKind::P(p)) if steer == Steer::Intensity => p.intensity.is_some(),
        (FrameBody::P(layer), _) => layer.macroblocks.iter().any(|mb| match (&mb.body, steer) {
            (_, Steer::Any) => true,
            (PMbBody::FourMv(m), Steer::FourMvTwoIntra) => intra_luma(&m.blocks) == 2,
            (PMbBody::FourMv(m), Steer::FourMvChromaIntra) => intra_luma(&m.blocks) >= 3,
            (PMbBody::FourMv(m), Steer::FourMvAcPred) => m.ac_pred == Some(true),
            _ => false,
        }),
        (FrameBody::B(layer), _) => layer.macroblocks.iter().any(|mb| match (&mb.body, steer) {
            (_, Steer::Any) => true,
            (BMbBody::Direct(_), Steer::BDirect) => true,
            (BMbBody::Coded(m), Steer::BInterpolated) => m.mv2.is_some(),
            (BMbBody::Coded(m), Steer::BIntra) => m.ac_pred.is_some(),
            (BMbBody::Coded(m), Steer::BStale) => m.mv2.is_some_and(|d| d.symbol.get() == 35),
            _ => false,
        }),
        _ => false,
    }
}

const ATTEMPTS: usize = 5_000_000;

/// Random bytes until one parses as a wanted picture, rewritten without
/// the bits after the macroblock layer.
fn random_frame(
    rng: &mut Rng,
    ctx: &FrameContext,
    wanted: impl Fn(&Frame) -> bool,
) -> (Frame, Vec<u8>) {
    for _ in 0..ATTEMPTS {
        let Ok(mut frame) = Frame::parse(&rng.bytes(160), ctx) else {
            continue;
        };
        if !wanted(&frame) {
            continue;
        }
        match &mut frame.body {
            FrameBody::Intra(l) => l.trailing = OpaqueBits::empty(),
            FrameBody::P(l) => l.trailing = OpaqueBits::empty(),
            FrameBody::B(l) => l.trailing = OpaqueBits::empty(),
        }
        let data = frame.to_bytes(ctx).unwrap();
        return (Frame::parse(&data, ctx).unwrap(), data);
    }
    panic!("no wanted frame in {ATTEMPTS} random byte strings");
}

/// SMPTE 421M Annex L container for FFmpeg's vc1test demuxer.
fn write_rcv(path: &Path, case: &Case, frames: &[Vec<u8>]) {
    let mut out = Vec::new();
    out.extend((frames.len() as u32 | 0xc5 << 24).to_le_bytes());
    out.extend(4u32.to_le_bytes());
    let seq = case.seq.to_bytes().unwrap();
    assert_eq!(seq.len(), 4);
    out.extend(&seq);
    out.extend(u32::from(case.height).to_le_bytes());
    out.extend(u32::from(case.width).to_le_bytes());
    out.extend(0xcu32.to_le_bytes());
    out.extend([0; 8]);
    out.extend(25u32.to_le_bytes());
    for (i, f) in frames.iter().enumerate() {
        let key = if i == 0 { 0x8000_0000 } else { 0 };
        out.extend((f.len() as u32 | key).to_le_bytes());
        out.extend((i as u32 * 40).to_le_bytes());
        out.extend(f);
    }
    std::fs::write(path, out).unwrap();
}

fn ffmpeg_md5s(path: &Path) -> Vec<String> {
    let out = Command::new("ffmpeg")
        .args([
            "-nostdin", "-v", "error", "-threads", "1", "-f", "vc1test", "-i",
        ])
        .arg(path)
        .args([
            "-fps_mode",
            "passthrough",
            "-enc_time_base",
            "-1",
            "-f",
            "framemd5",
            "-",
        ])
        .output()
        .unwrap_or_else(|e| panic!("cannot run ffmpeg, the oracle of this test: {e}"));
    assert!(
        out.status.success(),
        "{}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect()
}

const STREAMS: usize = 40;
const P_PICTURES: usize = 7;

#[test]
fn random_streams_match_ffmpeg() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wmv3_differential");
    std::fs::create_dir_all(&dir).unwrap();
    let mut reached: BTreeMap<Steer, usize> = BTreeMap::new();
    let mut non_reference = 0;
    for (seed, case) in cases().into_iter().enumerate() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d ^ (seed as u64 + 1) << 32);
        let ctx = FrameContext {
            seq: &case.seq,
            width: case.width,
            height: case.height,
        };
        for stream in 0..STREAMS {
            let p_steer = P_STEERS[stream % P_STEERS.len()];
            let b_steer = B_STEERS[stream % B_STEERS.len()];
            let mut frames = vec![random_frame(&mut rng, &ctx, |f| {
                matches!(f.header.kind, PictureKind::I { .. })
            })];
            for k in 0..P_PICTURES {
                match case.pattern {
                    Pattern::IP => {}
                    Pattern::WithBi => {
                        frames.push(random_frame(&mut rng, &ctx, |f| {
                            matches!(f.header.kind, PictureKind::Bi { .. })
                        }));
                        non_reference += 1;
                    }
                    Pattern::WithB if k == 0 => {
                        frames.push(random_frame(&mut rng, &ctx, is_b));
                        non_reference += 1;
                    }
                    Pattern::WithB => {}
                }
                // P1 is the later reference of the first decoded B picture.
                let want = if k == 1 { p_steer } else { Steer::Any };
                frames.push(random_frame(&mut rng, &ctx, |f| is_p(f) && has(f, want)));
                if matches!(case.pattern, Pattern::WithB) && k > 0 {
                    // The second decoded B picture inherits FFmpeg's backward
                    // vector (s->mv[1][0]) from the first.
                    let want = if k == 2 { b_steer } else { Steer::Any };
                    frames.push(random_frame(&mut rng, &ctx, |f| is_b(f) && has(f, want)));
                    non_reference += 1;
                }
            }
            for s in P_STEERS.iter().chain(&B_STEERS) {
                if frames.iter().any(|(f, _)| has(f, *s)) {
                    *reached.entry(*s).or_default() += 1;
                }
            }
            let path = dir.join(format!("{}_{stream}.rcv", case.name.replace(' ', "_")));
            let data: Vec<Vec<u8>> = frames.iter().map(|(_, d)| d.clone()).collect();
            write_rcv(&path, &case, &data);
            let expected = ffmpeg_md5s(&path);
            let order = display_order(frames.iter().map(|(f, _)| PictureRole::of(&f.header.kind)));
            assert_eq!(
                order.iter().flatten().count(),
                expected.len(),
                "{}",
                path.display()
            );
            let mut decoder = Decoder::new(FrameContext {
                seq: &case.seq,
                width: case.width,
                height: case.height,
            });
            for (i, (frame, _)) in frames.iter().enumerate() {
                let pic = decoder
                    .decode(frame)
                    .unwrap_or_else(|e| panic!("{} frame {i}: {e}", path.display()));
                let (Some(pic), Some(display)) = (pic, order[i]) else {
                    assert_eq!(order[i], None, "{} frame {i}", path.display());
                    continue;
                };
                let yuv = pic.to_yuv420p(case.width.into(), case.height.into());
                let got: String = Md5::digest(&yuv)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                assert_eq!(
                    got,
                    expected[display],
                    "{} frame {i}: {:?}",
                    path.display(),
                    frame.header
                );
            }
        }
    }
    eprintln!("streams reaching each form: {reached:?}");
    for s in P_STEERS.iter().chain(&B_STEERS) {
        assert!(
            reached.get(s).copied().unwrap_or(0) > 0,
            "{s:?} never reached"
        );
    }
    assert!(non_reference > 0);
}
