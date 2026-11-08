//! Encoded WMA version 2 streams in audio-only files decode in FFmpeg to
//! the input, within a signal to noise floor per bit rate, and start at
//! time 0.

use std::path::{Path, PathBuf};
use std::process::Command;

use wmv9::wma::encode::Encoder;
use wmv9::wma::{ByteRate, Channels, Format, SampleRate};

/// Interleaved test signal: tones and a sweep that differ between the
/// channels, and quiet noise.
fn signal(format: &Format, seconds: f64) -> Vec<i16> {
    let rate = f64::from(format.sample_rate.0);
    let n = (rate * seconds) as usize;
    let mut seed = 1u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(seed >> 8) / f64::from(1u32 << 24) - 0.5
    };
    let tau = std::f64::consts::TAU;
    let mut out = Vec::with_capacity(n * format.channels.count());
    for i in 0..n {
        let t = i as f64 / rate;
        let sweep = (tau * (200.0 * t + 400.0 * t * t)).sin();
        let left = 0.3 * (tau * 440.0 * t).sin() + 0.1 * (tau * 3150.0 * t).sin() + 0.1 * sweep;
        let right = 0.25 * (tau * 660.0 * t).sin() + 0.12 * sweep;
        let values: &[f64] = match format.channels {
            Channels::Mono => &[left],
            Channels::Stereo => &[left, right],
        };
        for v in values {
            out.push(((v + 0.01 * noise()) * 32767.0).round() as i16);
        }
    }
    out
}

/// An audio-only ASF file with the packets, one per media object.
fn write_asf(path: &Path, encoder: &Encoder, packets: Vec<Vec<u8>>) {
    use wmv9::asf::mux::{AudioObject, AudioTrack, FileTime, MuxSettings, PacketSize, mux};
    use wmv9::asf::{Guid, Millis};

    let format = encoder.asf_format();
    let rate = u64::from(format.wave.samples_per_sec);
    let frame_len = encoder.frame_len() as u64;
    let ms = |samples: u64| Millis((samples * 1000 + rate / 2) / rate);
    let objects: Vec<AudioObject> = packets
        .into_iter()
        .enumerate()
        .map(|(i, data)| AudioObject {
            data,
            presentation: ms(i as u64 * frame_len),
        })
        .collect();
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
            delay: ms(encoder.delay() as u64),
            objects,
        }),
        &settings,
    )
    .unwrap();
    std::fs::write(path, asf.to_bytes().unwrap()).unwrap();
}

fn ffmpeg_decode(path: &Path) -> Vec<i16> {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a", "-f", "s16le", "-"])
        .output()
        .unwrap_or_else(|e| panic!("cannot run ffmpeg, the oracle of this test: {e}"));
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

/// Time in seconds of the first decoded audio frame.
fn audio_start(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a", "-show_frames"])
        .args(["-show_entries", "frame=best_effort_timestamp_time"])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("cannot run ffprobe: {e}"));
    let text = String::from_utf8(out.stdout).unwrap();
    text.lines()
        .next()
        .and_then(|l| l.trim().trim_end_matches(',').parse().ok())
        .unwrap_or_else(|| panic!("no time in {text:?}"))
}

/// Signal to noise ratio in dB over `range` of interleaved samples.
fn snr(source: &[i16], decoded: &[i16], range: std::ops::Range<usize>) -> f64 {
    let (mut signal, mut noise) = (0.0, 0.0);
    for i in range {
        let s = f64::from(source[i]);
        signal += s * s;
        noise += (s - f64::from(decoded[i])).powi(2);
    }
    10.0 * (signal / noise).log10()
}

struct Case {
    sample_rate: u32,
    channels: Channels,
    byte_rate: u32,
    min_snr: f64,
}

const CASES: [Case; 9] = [
    Case {
        sample_rate: 44100,
        channels: Channels::Stereo,
        byte_rate: 16000,
        min_snr: 37.5,
    },
    Case {
        sample_rate: 44100,
        channels: Channels::Stereo,
        byte_rate: 8000,
        min_snr: 34.5,
    },
    // Noise coding and the lowest rate coefficient tables.
    Case {
        sample_rate: 44100,
        channels: Channels::Stereo,
        byte_rate: 4000,
        min_snr: 32.8,
    },
    Case {
        sample_rate: 22050,
        channels: Channels::Mono,
        byte_rate: 3000,
        min_snr: 35.5,
    },
    Case {
        sample_rate: 48000,
        channels: Channels::Mono,
        byte_rate: 6000,
        min_snr: 37.0,
    },
    // The middle rate coefficient tables.
    Case {
        sample_rate: 32000,
        channels: Channels::Mono,
        byte_rate: 4000,
        min_snr: 36.5,
    },
    // At the single precision value of a rate threshold of ff_wma_init.
    Case {
        sample_rate: 25000,
        channels: Channels::Mono,
        byte_rate: 3625,
        min_snr: 37.0,
    },
    // Frames of 512 samples.
    Case {
        sample_rate: 11025,
        channels: Channels::Stereo,
        byte_rate: 3000,
        min_snr: 31.5,
    },
    Case {
        sample_rate: 8000,
        channels: Channels::Mono,
        byte_rate: 3000,
        min_snr: 40.5,
    },
];

#[test]
fn encoded_streams_decode_in_ffmpeg_to_the_input() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wma_encode");
    std::fs::create_dir_all(&dir).unwrap();
    for case in &CASES {
        let format = Format {
            sample_rate: SampleRate(case.sample_rate),
            channels: case.channels,
            byte_rate: ByteRate(case.byte_rate),
        };
        let pcm = signal(&format, 3.0);
        let mut encoder = Encoder::new(format).unwrap();
        let (block_align, frame_len) = (encoder.block_align(), encoder.frame_len());
        let format_encoder = Encoder::new(format).unwrap();
        let mut packets = encoder.push(&pcm).unwrap();
        packets.extend(encoder.finish().unwrap());
        let channels = format.channels.count();
        let frames = pcm.len() / channels;
        // Two packets for the delay.
        assert_eq!(packets.len(), frames.div_ceil(frame_len) + 2);
        assert!(packets.iter().all(|p| p.len() == block_align));

        let path = dir.join(format!(
            "{}_{}_{}.wma",
            case.sample_rate, channels, case.byte_rate
        ));
        let packet_count = packets.len();
        write_asf(&path, &format_encoder, packets);
        let decoded = ffmpeg_decode(&path);
        // FFmpeg drops the delay and adds the remainder of the last block
        // at the end of the stream.
        assert_eq!(decoded.len(), (packet_count - 1) * frame_len * channels);
        let got = snr(&pcm, &decoded, 0..pcm.len());
        eprintln!(
            "{} Hz {channels} ch {} B/s: SNR {got:.2} dB",
            case.sample_rate, case.byte_rate
        );
        assert!(got >= case.min_snr, "SNR {got:.2} below {}", case.min_snr);
        // Video would start at 0.
        let start = audio_start(&path);
        assert!(start.abs() <= 0.001, "audio starts at {start} s");
    }
}
