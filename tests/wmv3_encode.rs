//! Encoded pictures must write and read back unchanged, reconstruct close
//! to their source, and decode in FFmpeg to the same pixels as here.

use std::path::{Path, PathBuf};
use std::process::Command;

use md5::{Digest, Md5};
use wmv9::wmv3::decode::Decoder;
use wmv9::wmv3::encode::{
    EncodeError, EncodeFeature, Encoder, Quantizer, Settings, Yuv420, encode_intra_picture,
    sample_sequence,
};
use wmv9::wmv3::{
    AcCoeff, Bounded, Frame, FrameBody, FrameContext, OpaqueBits, PBlock, PMbBody, PqIndex,
};

/// A picture with flat areas, gradients, edges and noise, at a size that is
/// not a multiple of the macroblock size.
fn source(width: u16, height: u16, seed: u32) -> Yuv420 {
    let (w, h) = (usize::from(width), usize::from(height));
    let mut noise = seed;
    let mut next = move || {
        noise ^= noise << 13;
        noise ^= noise >> 17;
        noise ^= noise << 5;
        (noise % 9) as i32 - 4
    };
    let luma = |x: usize, y: usize, n: i32| -> u8 {
        let v = if x < w / 3 {
            40
        } else if y < h / 2 {
            (x * 255 / w) as i32
        } else if (x / 4 + y / 4).is_multiple_of(2) {
            220
        } else {
            30
        };
        (v + n).clamp(0, 255) as u8
    };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    Yuv420 {
        width,
        height,
        y: (0..w * h).map(|i| luma(i % w, i / w, next())).collect(),
        cb: (0..cw * ch).map(|i| (64 + (i % cw) * 2) as u8).collect(),
        cr: (0..cw * ch).map(|i| (200 - (i / cw) * 3) as u8).collect(),
    }
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let se: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum();
    let mse = se / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

fn md5_hex(data: &[u8]) -> String {
    Md5::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// SMPTE 421M Annex L container for FFmpeg's vc1test demuxer.
fn write_rcv(path: &Path, seq: &[u8], width: u16, height: u16, frames: &[(Vec<u8>, bool)]) {
    let mut out = Vec::new();
    out.extend((frames.len() as u32 | 0xc5 << 24).to_le_bytes());
    out.extend(4u32.to_le_bytes());
    out.extend(&seq[..4]);
    out.extend(u32::from(height).to_le_bytes());
    out.extend(u32::from(width).to_le_bytes());
    out.extend(0xcu32.to_le_bytes());
    out.extend([0; 8]);
    out.extend(25u32.to_le_bytes());
    for (i, (f, key)) in frames.iter().enumerate() {
        let flag = if *key { 0x8000_0000 } else { 0 };
        out.extend((f.len() as u32 | flag).to_le_bytes());
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
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect()
}

/// Minimum luma PSNR of the reconstruction per PQINDEX. Values are set
/// below what the encoder reaches so that a regression in quantization or
/// prediction fails while normal variation does not.
const QUALITY: [(u8, f64); 6] = [
    (1, 49.0),
    (2, 44.0),
    (4, 39.5),
    (9, 36.5),
    (20, 30.0),
    (31, 26.0),
];

#[test]
fn intra_pictures_round_trip_reconstruct_and_match_ffmpeg() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wmv3_encode");
    std::fs::create_dir_all(&dir).unwrap();
    let seq = sample_sequence();
    let (width, height) = (77u16, 45u16);
    let ctx = FrameContext {
        seq: &seq,
        width,
        height,
    };
    let src = source(width, height, 7);
    for (pqindex, min_psnr) in QUALITY {
        let frame = encode_intra_picture(
            &src,
            &ctx,
            PqIndex::new(pqindex).unwrap(),
            Bounded::new(0).unwrap(),
        )
        .unwrap();
        let bytes = frame.to_bytes(&ctx).unwrap();
        let mut parsed = Frame::parse(&bytes, &ctx).unwrap();
        let FrameBody::Intra(layer) = &mut parsed.body else {
            panic!("pq {pqindex}: parsed as {:?}", parsed.header.kind);
        };
        assert!(
            layer.trailing.bit_len() < 8 && layer.trailing.is_zero(),
            "pq {pqindex}: {} trailing bits",
            layer.trailing.bit_len()
        );
        layer.trailing = OpaqueBits::empty();
        assert!(parsed == frame, "pq {pqindex}: parsed frame differs");

        let pic = Decoder::new(ctx)
            .decode(&frame)
            .unwrap()
            .expect("an I picture is output");
        let yuv = pic.to_yuv420p(width.into(), height.into());
        let luma = usize::from(width) * usize::from(height);
        let got = psnr(&yuv[..luma], &src.y);
        assert!(
            got >= min_psnr,
            "pq {pqindex}: luma PSNR {got:.2} below {min_psnr}"
        );
        eprintln!("pq {pqindex}: {} bytes, luma PSNR {got:.2}", bytes.len());

        let path = dir.join(format!("intra_{pqindex}.rcv"));
        write_rcv(
            &path,
            &seq.to_bytes().unwrap(),
            width,
            height,
            &[(bytes, true)],
        );
        assert_eq!(ffmpeg_md5s(&path), vec![md5_hex(&yuv)], "pq {pqindex}");
    }
}

#[test]
fn sequences_the_encoder_cannot_honour_are_refused() {
    let src = source(16, 16, 1);
    let attempt = |f: fn(&mut wmv9::wmv3::SequenceHeader)| {
        let mut seq = sample_sequence();
        f(&mut seq);
        let ctx = FrameContext {
            seq: &seq,
            width: 16,
            height: 16,
        };
        encode_intra_picture(
            &src,
            &ctx,
            PqIndex::new(4).unwrap(),
            Bounded::new(0).unwrap(),
        )
        .map(|_| ())
    };
    assert_eq!(
        attempt(|s| s.dquant = Bounded::new(1).unwrap()),
        Err(EncodeError::Unsupported(EncodeFeature::Dquant))
    );
    assert_eq!(
        attempt(|s| {
            s.res_fasttx = false;
            s.fasttx_extra = None;
        }),
        Err(EncodeError::Unsupported(EncodeFeature::SlowTransform))
    );
}

/// Frame `t` of a scene: a smooth texture panning by 1.25 samples right and
/// 0.5 down per frame, a flat square moving 3 samples right per frame and
/// a striped block moving 10 left and 2 down, against the pan, so that
/// neighbouring vectors disagree enough for HYBRIDPRED.
fn moving(width: u16, height: u16, t: u32) -> Yuv420 {
    let (w, h) = (usize::from(width), usize::from(height));
    let shift_x = 1.25 * f64::from(t);
    let shift_y = 0.5 * f64::from(t);
    let texture = |x: f64, y: f64| {
        128.0 + 60.0 * (x * 0.21).sin() * (y * 0.17).cos() + 30.0 * ((x + y) * 0.05).sin()
    };
    let square = |x: usize, y: usize| {
        let left = 8 + 3 * t as usize;
        (left..left + 20).contains(&x) && (12..32).contains(&y)
    };
    let striped = |x: usize, y: usize| -> Option<u8> {
        let left = 70 - 10 * t as i32;
        let top = 30 + 2 * t as i32;
        let (dx, dy) = (x as i32 - left, y as i32 - top);
        ((0..24).contains(&dx) && (0..20).contains(&dy)).then_some(if (dx / 3) % 2 == 0 {
            20
        } else {
            200
        })
    };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    Yuv420 {
        width,
        height,
        y: (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                if let Some(v) = striped(x, y) {
                    v
                } else if square(x, y) {
                    230
                } else {
                    texture(x as f64 + shift_x, y as f64 + shift_y).clamp(0.0, 255.0) as u8
                }
            })
            .collect(),
        cb: (0..cw * ch)
            .map(|i| texture((i % cw) as f64 * 2.0 + shift_x, 0.0).clamp(0.0, 255.0) as u8 / 2 + 64)
            .collect(),
        cr: (0..cw * ch).map(|i| (100 + (i / cw)) as u8).collect(),
    }
}

/// One quantizer of the P picture test.
struct PCase {
    pqindex: u8,
    /// Minimum luma PSNR of every picture.
    min_psnr: f64,
    /// Largest mean P picture size as a fraction of the I picture.
    max_p_ratio: f64,
    /// Whether P pictures must contain escape mode 3 coefficients.
    needs_mode3: bool,
    /// Without the loop filter `Encoder` checks each P picture against
    /// its own reconstruction.
    loop_filter: bool,
}

/// PQINDEX 3 gives levels large enough for escape mode 3 alongside skipped
/// macroblocks. 6 uses the uniform quantizer and sends HALFQP. 9 is the
/// first without HALFQP and with the non-uniform quantizer. 20 gives PQUANT 17,
/// which selects the other row of the MVMODE table. The last case repeats
/// 6 without the loop filter.
const P_CASES: [PCase; 5] = [
    PCase {
        pqindex: 3,
        min_psnr: 44.0,
        max_p_ratio: 0.6,
        needs_mode3: true,
        loop_filter: true,
    },
    PCase {
        pqindex: 6,
        min_psnr: 39.0,
        max_p_ratio: 0.5,
        needs_mode3: false,
        loop_filter: true,
    },
    PCase {
        pqindex: 9,
        min_psnr: 36.0,
        max_p_ratio: 0.6,
        needs_mode3: false,
        loop_filter: true,
    },
    PCase {
        pqindex: 20,
        min_psnr: 29.0,
        max_p_ratio: 0.5,
        needs_mode3: false,
        loop_filter: true,
    },
    PCase {
        pqindex: 6,
        min_psnr: 39.0,
        max_p_ratio: 0.5,
        needs_mode3: false,
        loop_filter: false,
    },
];

#[test]
fn p_pictures_compress_reconstruct_and_match_ffmpeg() {
    for case in &P_CASES {
        p_pictures_at(case);
    }
}

fn p_pictures_at(case: &PCase) {
    let PCase {
        pqindex, min_psnr, ..
    } = *case;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wmv3_encode");
    std::fs::create_dir_all(&dir).unwrap();
    let seq = wmv9::wmv3::SequenceHeader {
        loop_filter: case.loop_filter,
        ..sample_sequence()
    };
    let (width, height) = (88u16, 56u16);
    let ctx = FrameContext {
        seq: &seq,
        width,
        height,
    };
    let settings = Settings {
        quantizer: Quantizer::Fixed(PqIndex::new(pqindex).unwrap()),
        key_interval: 100.try_into().unwrap(),
    };
    let mut encoder = Encoder::new(ctx, settings).unwrap();
    let mut decoder = Decoder::new(ctx);
    let mut stream = Vec::new();
    let mut ours = Vec::new();
    let (mut skipped, mut moved) = (0, 0);
    // HYBRIDPRED bits sent with each value: left, above.
    let mut hybrid = [0; 2];
    let mut explicit = 0;
    let luma = usize::from(width) * usize::from(height);
    for t in 0..10 {
        let src = moving(width, height, t);
        let frame = encoder.encode(&src).unwrap();
        let bytes = frame.to_bytes(&ctx).unwrap();
        let mut parsed = Frame::parse(&bytes, &ctx).unwrap();
        let trailing = match &mut parsed.body {
            FrameBody::Intra(l) => &mut l.trailing,
            FrameBody::P(l) => &mut l.trailing,
            FrameBody::B(l) => &mut l.trailing,
        };
        assert!(trailing.bit_len() < 8 && trailing.is_zero(), "frame {t}");
        *trailing = OpaqueBits::empty();
        assert!(parsed == frame, "frame {t}: parsed frame differs");
        if let FrameBody::P(layer) = &frame.body {
            for mb in &layer.macroblocks {
                match &mb.body {
                    PMbBody::SkippedOneMv { hybrid: h } => {
                        skipped += 1;
                        if let Some(above) = h {
                            hybrid[usize::from(*above)] += 1;
                        }
                    }
                    PMbBody::OneMv(m) => {
                        moved += 1;
                        if let Some(above) = m.hybrid {
                            hybrid[usize::from(above)] += 1;
                        }
                        explicit += m
                            .blocks
                            .iter()
                            .filter_map(|b| match b {
                                PBlock::Inter(i) => Some(i),
                                _ => None,
                            })
                            .flat_map(|i| i.subblocks.iter().flatten())
                            .filter(|c| matches!(c, AcCoeff::Explicit { .. }))
                            .count();
                    }
                    _ => {}
                }
            }
        }
        let pic = decoder
            .decode(&frame)
            .unwrap()
            .expect("I and P pictures are output");
        let yuv = pic.to_yuv420p(width.into(), height.into());
        let quality = psnr(&yuv[..luma], &src.y);
        eprintln!(
            "pq {pqindex} frame {t}: {} bytes, luma PSNR {quality:.2}",
            bytes.len()
        );
        assert!(
            quality >= min_psnr,
            "pq {pqindex} frame {t}: luma PSNR {quality:.2}"
        );
        ours.push(md5_hex(&yuv));
        stream.push((bytes, t == 0));
    }
    eprintln!("pq {pqindex}: {explicit} mode 3 coefficients in P pictures");
    assert!(
        !case.needs_mode3 || explicit > 0,
        "pq {pqindex}: no mode 3 coefficient"
    );
    let i_size = stream[0].0.len();
    let p_mean = stream[1..].iter().map(|(b, _)| b.len()).sum::<usize>() / (stream.len() - 1);
    assert!(
        (p_mean as f64) < case.max_p_ratio * i_size as f64,
        "P pictures average {p_mean} bytes against {i_size} for the I picture"
    );
    assert!(
        skipped > 0 && moved > 0 && hybrid.iter().all(|&n| n > 0),
        "skipped {skipped}, coded {moved}, HYBRIDPRED left and above {hybrid:?}"
    );
    let path = dir.join(format!("p_pictures_{pqindex}_{}.rcv", case.loop_filter));
    write_rcv(&path, &seq.to_bytes().unwrap(), width, height, &stream);
    assert_eq!(ffmpeg_md5s(&path), ours);
}

/// A scene cut becomes an I picture, the pictures after it P pictures
/// again, and FFmpeg decodes the stream to the same samples.
#[test]
fn scene_cuts_start_with_an_i_picture() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wmv3_encode");
    std::fs::create_dir_all(&dir).unwrap();
    let seq = sample_sequence();
    let (width, height) = (88u16, 56u16);
    let ctx = FrameContext {
        seq: &seq,
        width,
        height,
    };
    let settings = Settings {
        quantizer: Quantizer::Fixed(PqIndex::new(6).unwrap()),
        key_interval: 100.try_into().unwrap(),
    };
    let mut encoder = Encoder::new(ctx, settings).unwrap();
    let mut decoder = Decoder::new(ctx);
    let mut stream = Vec::new();
    let mut ours = Vec::new();
    let mut kinds = String::new();
    for t in 0..10u32 {
        let src = if t < 5 {
            moving(width, height, t)
        } else {
            source(width, height, t)
        };
        let frame = encoder.encode(&src).unwrap();
        let key = matches!(frame.body, FrameBody::Intra(_));
        kinds.push(if key { 'I' } else { 'P' });
        let pic = decoder.decode(&frame).unwrap().unwrap();
        ours.push(md5_hex(&pic.to_yuv420p(width.into(), height.into())));
        stream.push((frame.to_bytes(&ctx).unwrap(), key));
    }
    assert_eq!(kinds, "IPPPPIPPPP");
    let path = dir.join("scene_cut.rcv");
    write_rcv(&path, &seq.to_bytes().unwrap(), width, height, &stream);
    assert_eq!(ffmpeg_md5s(&path), ours);
}

/// Display time of frame `i` at 30000/1001 frames per second, in ms.
fn ntsc_ms(i: u64) -> u64 {
    (i * 1001 + 15) / 30
}

#[test]
fn muxed_files_carry_the_frames_and_play_in_ffmpeg() {
    use wmv9::asf::mux::{
        FileTime, MuxSettings, PacketSize, VideoFormat, VideoFrame, VideoTrack, mux,
    };
    use wmv9::asf::{Asf, FourCc, Hns, Millis};

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wmv3_encode");
    std::fs::create_dir_all(&dir).unwrap();
    let seq = sample_sequence();
    let (width, height) = (88u16, 56u16);
    let ctx = FrameContext {
        seq: &seq,
        width,
        height,
    };
    let settings = Settings {
        quantizer: Quantizer::Fixed(PqIndex::new(6).unwrap()),
        key_interval: 15.try_into().unwrap(),
    };
    let mut encoder = Encoder::new(ctx, settings).unwrap();
    let mut decoder = Decoder::new(ctx);
    let mut frames = Vec::new();
    let mut ours = Vec::new();
    for t in 0..40u64 {
        let frame = encoder.encode(&moving(width, height, t as u32)).unwrap();
        let pic = decoder.decode(&frame).unwrap().unwrap();
        ours.push(md5_hex(&pic.to_yuv420p(width.into(), height.into())));
        frames.push(VideoFrame {
            data: frame.to_bytes(&ctx).unwrap(),
            key: matches!(frame.body, FrameBody::Intra(_)),
            presentation: Millis(ntsc_ms(t)),
            duration: Millis(ntsc_ms(t + 1) - ntsc_ms(t)),
        });
    }
    let format = VideoFormat {
        width: width.into(),
        height: height.into(),
        fourcc: FourCc(*b"WMV3"),
        codec_data: seq.to_bytes().unwrap(),
        average_time_per_frame: Hns(333_667),
        codec_name: "VC-1".into(),
        codec_description: "VC-1 Simple/Main Profile".into(),
        conformance: "MP".into(),
    };
    let settings = MuxSettings {
        // Small enough that I pictures span several packets.
        packet_size: PacketSize::new(400).unwrap(),
        preroll: Millis(3000),
        file_id: wmv9::asf::Guid([7; 16]),
        creation_date: FileTime(0),
        sdk_version: None,
    };
    let bytes = mux(
        Some(VideoTrack {
            format: format.clone(),
            frames: frames.clone(),
        }),
        None,
        &settings,
    )
    .unwrap()
    .to_bytes()
    .unwrap();
    let asf = Asf::parse(&bytes).unwrap();
    assert_eq!(asf.to_bytes().unwrap(), bytes);
    let fp = asf.file_properties().unwrap();
    assert_eq!(fp.file_size, bytes.len() as u64);
    let split = asf
        .data()
        .unwrap()
        .packets
        .iter()
        .flat_map(|p| match &p.payloads {
            wmv9::asf::Payloads::Single(x) => vec![x],
            wmv9::asf::Payloads::Multiple { payloads, .. } => payloads.iter().collect(),
        })
        .filter(|x| matches!(x.kind, wmv9::asf::PayloadKind::Fragment { offset, .. } if offset > 0))
        .count();
    assert!(split > 0, "no frame spans packets");
    // The scene has no cut, so I pictures follow the key interval.
    let keys: Vec<usize> = (0..frames.len()).filter(|&t| frames[t].key).collect();
    assert_eq!(keys, [0, 15, 30]);
    let objects = asf.media_objects().unwrap();
    assert_eq!(objects.len(), frames.len());
    for (o, f) in objects.iter().zip(&frames) {
        assert_eq!(o.data, f.data);
        assert_eq!(o.key_frame, f.key);
        assert_eq!(o.presentation_time.0, f.presentation.0 + 3000);
    }

    let path = dir.join("muxed.wmv");
    std::fs::write(&path, &bytes).unwrap();
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-threads", "1", "-i"])
        .arg(&path)
        .args([
            "-map",
            "0:v",
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
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect();
    assert_eq!(expected, ours);
}

/// Tiny frames, as a still scene gives, must not overflow the six-bit
/// payload count of a packet.
#[test]
fn packets_of_many_tiny_frames_stay_within_63_payloads() {
    use wmv9::asf::mux::{
        FileTime, MuxError, MuxSettings, PacketSize, VIDEO_STREAM, VideoFormat, VideoFrame,
        VideoTrack, mux,
    };
    use wmv9::asf::{Asf, FourCc, Hns, Millis, Payloads};
    let frames: Vec<VideoFrame> = (0..200u64)
        .map(|t| VideoFrame {
            data: vec![0x5a; 6],
            key: t == 0,
            presentation: Millis(ntsc_ms(t)),
            duration: Millis(ntsc_ms(t + 1) - ntsc_ms(t)),
        })
        .collect();
    let format = VideoFormat {
        width: 64,
        height: 48,
        fourcc: FourCc(*b"WMV3"),
        codec_data: sample_sequence().to_bytes().unwrap(),
        average_time_per_frame: Hns(333_667),
        codec_name: "VC-1".into(),
        codec_description: "VC-1 Simple/Main Profile".into(),
        conformance: "MP".into(),
    };
    let settings = MuxSettings {
        packet_size: PacketSize::SDK,
        preroll: Millis(3000),
        file_id: wmv9::asf::Guid([7; 16]),
        creation_date: FileTime(0),
        sdk_version: None,
    };
    let bytes = mux(
        Some(VideoTrack {
            format: format.clone(),
            frames: frames.clone(),
        }),
        None,
        &settings,
    )
    .unwrap()
    .to_bytes()
    .unwrap();
    let parsed = Asf::parse(&bytes).unwrap();
    for p in &parsed.data().unwrap().packets {
        if let Payloads::Multiple { payloads, .. } = &p.payloads {
            assert!(payloads.len() <= 63);
        }
    }
    assert_eq!(parsed.media_objects().unwrap().len(), frames.len());

    let mut empty = frames.clone();
    empty[5].data.clear();
    assert!(matches!(
        mux(
            Some(VideoTrack {
                format: format.clone(),
                frames: empty
            }),
            None,
            &settings
        ),
        Err(MuxError::EmptyObject {
            stream: VIDEO_STREAM,
            object: 5
        })
    ));
    assert!(matches!(
        mux(
            Some(VideoTrack {
                format: format.clone(),
                frames: Vec::new()
            }),
            None,
            &settings
        ),
        Err(MuxError::NoFrames)
    ));
    assert_eq!(PacketSize::new(PacketSize::MIN - 1), None);
    assert_eq!(PacketSize::new(PacketSize::MAX + 1), None);
}

/// `moving` with new noise of up to 24 levels on every luma sample of
/// every picture, which motion does not predict.
fn noisy(width: u16, height: u16, t: u32) -> Yuv420 {
    let mut pic = moving(width, height, t);
    let mut seed = t.wrapping_mul(2_654_435_761).wrapping_add(1);
    for v in &mut pic.y {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let n = (seed >> 24) as i32 % 49 - 24;
        *v = (i32::from(*v) + n).clamp(0, 255) as u8;
    }
    pic
}

/// Under rate control a stream of easy, then hard, then easy content
/// stays within its bucket and near the target rate, codes the hard part
/// at coarser quantizers, and decodes in FFmpeg to the same samples.
#[test]
fn rate_control_keeps_to_the_bit_rate() {
    use std::num::NonZeroU32;
    use wmv9::asf::Millis;
    use wmv9::wmv3::encode::{FrameRate, RateTarget};

    const RATE: u32 = 150_000;
    const BUFFER_MS: u64 = 1000;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wmv3_encode");
    std::fs::create_dir_all(&dir).unwrap();
    let seq = sample_sequence();
    let (width, height) = (88u16, 56u16);
    let ctx = FrameContext {
        seq: &seq,
        width,
        height,
    };
    let target = RateTarget::new(
        NonZeroU32::new(RATE).unwrap(),
        FrameRate::new(30, 1).unwrap(),
        Millis(BUFFER_MS),
    )
    .unwrap();
    let mut encoder = Encoder::new(
        ctx,
        Settings {
            quantizer: Quantizer::Rate(target),
            key_interval: 300.try_into().unwrap(),
        },
    )
    .unwrap();
    let mut decoder = Decoder::new(ctx);
    let hard = 60..150;
    let mut stream = Vec::new();
    let mut ours = Vec::new();
    let mut quantizers = Vec::new();
    for t in 0..210u32 {
        let src = if hard.contains(&t) {
            noisy(width, height, t)
        } else {
            moving(width, height, t)
        };
        let frame = encoder.encode(&src).unwrap();
        quantizers.push(frame.header.pqindex.get());
        let key = matches!(frame.body, FrameBody::Intra(_));
        let pic = decoder.decode(&frame).unwrap().unwrap();
        ours.push(md5_hex(&pic.to_yuv420p(width.into(), height.into())));
        stream.push((frame.to_bytes(&ctx).unwrap(), key));
    }

    // The bucket drains RATE / 30 bits per picture.
    let bucket = f64::from(RATE) * BUFFER_MS as f64 / 1000.0;
    let mut level = 0.0;
    for (i, (bytes, _)) in stream.iter().enumerate() {
        level += (bytes.len() * 8) as f64;
        assert!(
            level <= bucket,
            "picture {i}: {level} bits in a bucket of {bucket}"
        );
        level = (level - f64::from(RATE) / 30.0).max(0.0);
    }
    let bits: usize = stream.iter().map(|(b, _)| b.len() * 8).sum();
    let rate = bits as f64 / 7.0;
    assert!(
        (rate - f64::from(RATE)).abs() < f64::from(RATE) * 0.1,
        "{rate} bits per second"
    );
    let median = |r: std::ops::Range<usize>| {
        let mut q = quantizers[r].to_vec();
        q.sort();
        q[q.len() / 2]
    };
    let (easy, hard) = (median(30..60), median(90..150));
    assert!(
        hard >= easy + 5,
        "PQINDEX {easy} on easy and {hard} on hard content: {quantizers:?}"
    );
    // The hard content starts with a scene cut. The pictures after it
    // follow the new content instead of the cheap pictures before it.
    let after_cut: usize = stream[61..66].iter().map(|(b, _)| b.len() * 8).sum();
    assert!(
        after_cut as f64 <= bucket * 2.0 / 3.0,
        "{after_cut} bits in the 5 pictures after the cut"
    );

    let path = dir.join("rate_control.rcv");
    write_rcv(&path, &seq.to_bytes().unwrap(), width, height, &stream);
    assert_eq!(ffmpeg_md5s(&path), ours);
}
