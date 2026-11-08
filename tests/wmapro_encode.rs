//! Encoded WMA Pro streams in audio-only files decode in FFmpeg to the
//! input.

use std::path::{Path, PathBuf};
use std::process::Command;

use wmv9::asf::mux::{AudioObject, AudioTrack, FileTime, MuxSettings, PacketSize, mux};
use wmv9::asf::{Guid, Millis};
use wmv9::wma::pro::encode::Encoder;
use wmv9::wma::{ByteRate, Channels, Format, SampleRate};

/// Interleaved test signal: tones and a sweep that differ between the
/// channels, and quiet noise.
fn signal(rate: u32, channels: Channels, seconds: f64) -> Vec<i16> {
    let rate = f64::from(rate);
    let n = (rate * seconds) as usize;
    let mut seed = 1u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(seed >> 8) / f64::from(1u32 << 24) - 0.5
    };
    let tau = std::f64::consts::TAU;
    let mut out = Vec::new();
    for i in 0..n {
        let t = i as f64 / rate;
        let sweep = (tau * (200.0 * t + 400.0 * t * t)).sin();
        let left = 0.3 * (tau * 440.0 * t).sin() + 0.1 * (tau * 3150.0 * t).sin() + 0.1 * sweep;
        let right = 0.25 * (tau * 660.0 * t).sin() + 0.12 * sweep;
        let values: &[f64] = match channels {
            Channels::Mono => &[left],
            Channels::Stereo => &[left, right],
        };
        for v in values {
            out.push(((v + 0.01 * noise()) * 32767.0).round() as i16);
        }
    }
    out
}

/// Writes an audio-only file and returns the packets' bytes.
fn write_asf(path: &Path, encoder: Encoder) -> usize {
    let format = encoder.asf_format();
    let rate = u64::from(format.wave.samples_per_sec);
    let frame_len = encoder.frame_len() as u64;
    let block_align = usize::from(format.wave.block_align);
    let objects: Vec<AudioObject> = encoder
        .finish()
        .unwrap()
        .into_iter()
        .map(|p| AudioObject {
            data: p.data,
            presentation: Millis((p.frame.0 * frame_len * 1000 + rate / 2) / rate),
        })
        .collect();
    let count = objects.len();
    let settings = MuxSettings {
        packet_size: PacketSize::SDK,
        preroll: Millis(3000),
        file_id: Guid([9; 16]),
        creation_date: FileTime(0),
        sdk_version: None,
    };
    let asf = mux(
        None,
        Some(AudioTrack {
            format,
            delay: Millis(0),
            objects,
        }),
        &settings,
    )
    .unwrap();
    std::fs::write(path, asf.to_bytes().unwrap()).unwrap();
    count * block_align
}

fn ffmpeg_decode(path: &Path) -> Vec<i16> {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a", "-f", "s16le", "-"])
        .output()
        .unwrap_or_else(|e| panic!("cannot run ffmpeg: {e}"));
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
        .chunks(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

fn snr(source: &[i16], decoded: &[i16]) -> f64 {
    let (mut signal, mut noise) = (0.0, 0.0);
    for (s, d) in source.iter().zip(decoded) {
        let s = f64::from(*s);
        signal += s * s;
        noise += (s - f64::from(*d)).powi(2);
    }
    10.0 * (signal / noise).log10()
}

struct Case {
    sample_rate: u32,
    channels: Channels,
    byte_rate: u32,
    min_snr: f64,
}

const CASES: [Case; 7] = [
    Case {
        sample_rate: 44100,
        channels: Channels::Stereo,
        byte_rate: 16000,
        min_snr: 38.2,
    },
    Case {
        sample_rate: 44100,
        channels: Channels::Stereo,
        byte_rate: 8000,
        min_snr: 34.7,
    },
    Case {
        sample_rate: 44100,
        channels: Channels::Mono,
        byte_rate: 8000,
        min_snr: 39.6,
    },
    Case {
        sample_rate: 48000,
        channels: Channels::Stereo,
        byte_rate: 24000,
        min_snr: 41.4,
    },
    // Frames of 1024 samples.
    Case {
        sample_rate: 22050,
        channels: Channels::Mono,
        byte_rate: 4000,
        min_snr: 37.2,
    },
    // Frames of 512 samples, at most 8 blocks.
    Case {
        sample_rate: 16000,
        channels: Channels::Stereo,
        byte_rate: 4000,
        min_snr: 32.2,
    },
    Case {
        sample_rate: 8000,
        channels: Channels::Mono,
        byte_rate: 3000,
        min_snr: 41.7,
    },
];

/// Offset of the decoded first channel against the source, in sample
/// frames.
fn lag(source: &[i16], decoded: &[i16], channels: usize, rate: u32) -> i64 {
    let a: Vec<i16> = source.iter().step_by(channels).copied().collect();
    let b: Vec<i16> = decoded.iter().step_by(channels).copied().collect();
    let at = rate as usize;
    (-4096i64..=4096)
        .max_by_key(|&lag| {
            (at..at + 4000)
                .map(|i| {
                    let j = i as i64 + lag;
                    if j < 0 || j as usize >= b.len() {
                        0
                    } else {
                        i64::from(a[i]) * i64::from(b[j as usize])
                    }
                })
                .sum::<i64>()
        })
        .unwrap()
}

#[test]
fn encoded_streams_decode_in_ffmpeg_to_the_input() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wmapro_encode");
    std::fs::create_dir_all(&dir).unwrap();
    for case in &CASES {
        let format = Format {
            sample_rate: SampleRate(case.sample_rate),
            channels: case.channels,
            byte_rate: ByteRate(case.byte_rate),
        };
        let channels = case.channels.count();
        let pcm = signal(case.sample_rate, case.channels, 3.0);
        let mut encoder = Encoder::new(format).unwrap();
        encoder.push(&pcm).unwrap();
        let path = dir.join(format!(
            "{}_{}_{}.wma",
            case.sample_rate, channels, case.byte_rate
        ));
        let bytes = write_asf(&path, encoder);
        // The input, one frame of delay and the rest of the last packet.
        let budget = f64::from(case.byte_rate) * 3.0;
        assert!(
            (bytes as f64) < budget * 1.25,
            "{bytes} bytes for {budget} at the byte rate"
        );
        let decoded = ffmpeg_decode(&path);
        let got = snr(&pcm, &decoded);
        eprintln!(
            "{} Hz {channels} ch {} B/s: SNR {got:.2} dB",
            case.sample_rate, case.byte_rate
        );
        assert_eq!(decoded.len(), pcm.len(), "decoded length");
        assert_eq!(lag(&pcm, &decoded, channels, case.sample_rate), 0);
        assert!(got >= case.min_snr, "SNR {got:.2} below {}", case.min_snr);
    }
}

/// Places in the frame for the bursts, in samples from the start of the
/// frame's blocks: within the first segment of 128 (0, 16, 40), where the
/// block before an attack matters (256), the end of the frame (1990,
/// 2047), and others.
const BURST_OFFSETS: [usize; 9] = [0, 16, 40, 256, 300, 1000, 1500, 1990, 2047];

/// A quiet 300 Hz tone with bursts of noise 5 ms long at `BURST_OFFSETS`
/// in every seventh frame from frame 2, the right channel's burst the
/// left's or its negation, and the sample frames at which the bursts
/// start. Input sample t lies in the frame (t + 1024) / 2048 at
/// (t + 1024) % 2048.
fn bursts(opposite: bool) -> (Vec<i16>, Vec<usize>) {
    let r = 44100.0;
    let onsets: Vec<usize> = BURST_OFFSETS
        .iter()
        .enumerate()
        .map(|(i, &o)| (2 + 7 * i) * 2048 - 1024 + o)
        .collect();
    let n = onsets.last().unwrap() + 4096;
    let len = (0.005 * r) as usize;
    let mut seed = 7u32;
    let mut out = Vec::with_capacity(2 * n);
    for i in 0..n {
        let tone = 300.0 * (std::f64::consts::TAU * 300.0 * i as f64 / r).sin();
        let burst = if onsets.iter().any(|&o| (o..o + len).contains(&i)) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (f64::from(seed >> 8) / f64::from(1u32 << 24) - 0.5) * 40_000.0
        } else {
            0.0
        };
        let right = if opposite { tone - burst } else { tone + burst };
        out.extend([(tone + burst).round() as i16, right.round() as i16]);
    }
    (out, onsets)
}

/// Short blocks keep the coding noise of a burst after its start: from
/// 20 ms to 5 ms before each burst, the error stays this far below the
/// burst, wherever in the frame the burst starts and whether the channels'
/// bursts have the same sign or not. The last 5 ms before it, under the
/// shortest block's spread, are left out; hearing masks noise that close
/// before a loud sound.
#[test]
fn bursts_keep_their_noise_after_them() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wmapro_encode");
    std::fs::create_dir_all(&dir).unwrap();
    let format = Format {
        sample_rate: SampleRate(44100),
        channels: Channels::Stereo,
        byte_rate: ByteRate(8000),
    };
    for opposite in [false, true] {
        let (pcm, onsets) = bursts(opposite);
        let mut encoder = Encoder::new(format).unwrap();
        encoder.push(&pcm).unwrap();
        let path = dir.join(format!("bursts_{opposite}.wma"));
        write_asf(&path, encoder);
        let decoded = ffmpeg_decode(&path);
        assert_eq!(decoded.len(), pcm.len());
        // 20 ms and 5 ms at 44.1 kHz.
        let before = 882;
        let near = 220;
        let burst_len = 220;
        for (&o, offset) in onsets.iter().zip(BURST_OFFSETS) {
            let err: f64 = (o - before..o - near)
                .map(|i| (f64::from(pcm[2 * i]) - f64::from(decoded[2 * i])).powi(2))
                .sum::<f64>()
                / (before - near) as f64;
            let burst: f64 = (o..o + burst_len)
                .map(|i| f64::from(pcm[2 * i]).powi(2))
                .sum::<f64>()
                / burst_len as f64;
            let below = 10.0 * (burst / err).log10();
            eprintln!("opposite {opposite} offset {offset}: {below:.1} dB");
            assert!(
                below >= 39.0,
                "opposite {opposite}, burst at {offset} in its frame: {below:.1} dB"
            );
        }
    }
}
