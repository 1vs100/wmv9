//! wmv3-encode reads media files through FFmpeg: video and audio in step
//! when the streams start at different times, audio alone, video alone.
#![cfg(feature = "ffmpeg")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("media_input");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(program: &str, args: &[&str]) -> Vec<u8> {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn encode(args: &[&str]) {
    run(env!("CARGO_BIN_EXE_wmv3-encode"), args);
}

/// `codec_type` of each stream of a file.
fn streams(path: &Path) -> Vec<String> {
    let out = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
            path.to_str().unwrap(),
        ],
    );
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Luma of every decoded picture at `size`, one byte per sample.
fn pictures(path: &Path, size: usize) -> Vec<Vec<u8>> {
    let out = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-i",
            path.to_str().unwrap(),
            "-map",
            "0:v",
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "gray",
            "-",
        ],
    );
    out.chunks(size).map(<[u8]>::to_vec).collect()
}

/// Decoded audio as mono at 48 kHz.
fn sound(path: &Path) -> Vec<i16> {
    let out = run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-i",
            path.to_str().unwrap(),
            "-map",
            "0:a",
            "-ac",
            "1",
            "-ar",
            "48000",
            "-f",
            "s16le",
            "-",
        ],
    );
    out.chunks(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

/// A Matroska file of 160x120 pictures at 25 per second with one white
/// picture, and a 1 kHz tone burst coded with `audio_codec` at the time of
/// that picture. The streams start `video_start` and `audio_start` seconds
/// into the file, in milliseconds so that a start between frame times
/// stays there; the flash comes on the first picture at or after 1 s of
/// file time, the burst at the same time.
fn flash_and_beep(name: &str, video_start: f64, audio_start: f64, audio_codec: &str) -> PathBuf {
    let path = dir().join(name);
    let flash = ((1.0 - video_start) * 25.0).ceil() / 25.0;
    let beep = video_start + flash - audio_start;
    let video = format!(
        "color=c=black:s=160x120:r=25:d=3,\
         geq=lum='if(gte(T\\,{flash}-0.001)*lt(T\\,{flash}+0.039)\\,235\\,16)':cb=128:cr=128,\
         settb=1/1000"
    );
    let audio = format!(
        "aevalsrc='if(gte(t\\,{beep})*lt(t\\,{beep}+0.1)\\,0.8*sin(2*PI*1000*t)\\,0)':s=44100:d=3"
    );
    let (video_start, audio_start) = (video_start.to_string(), audio_start.to_string());
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-y",
            "-itsoffset",
            &video_start,
            "-f",
            "lavfi",
            "-i",
            &video,
            "-itsoffset",
            &audio_start,
            "-f",
            "lavfi",
            "-i",
            &audio,
            "-c:v",
            "mpeg4",
            "-q:v",
            "2",
            "-enc_time_base:v",
            "1/1000",
            "-fps_mode:v",
            "passthrough",
            "-c:a",
            audio_codec,
            path.to_str().unwrap(),
        ],
    );
    path
}

/// The flash and the burst decode at the same time, within 1 ms.
fn in_step(name: &str, video_start: f64, audio_start: f64, audio_codec: &str) {
    let input = flash_and_beep(
        &format!("{name}.mkv"),
        video_start,
        audio_start,
        audio_codec,
    );
    let output = dir().join(format!("{name}.wmv"));
    encode(&[
        "-i",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_flash_with_beep(&output);
}

/// The white picture and the tone burst of a file decode within 1 ms of
/// each other.
fn assert_flash_with_beep(path: &Path) {
    assert_eq!(streams(path), ["audio", "video"]);
    let pictures = pictures(path, 160 * 120);
    let flash = pictures
        .iter()
        .position(|p| p.iter().map(|&v| u32::from(v)).sum::<u32>() / (160 * 120) > 128)
        .expect("a white picture");
    let flash_time = flash as f64 / 25.0;
    let samples = sound(path);
    let beep = samples
        .iter()
        .position(|s| s.unsigned_abs() > 8000)
        .expect("a tone burst");
    let beep_time = beep as f64 / 48000.0;
    assert!(
        (beep_time - flash_time).abs() <= 0.001,
        "{}: burst at {beep_time:.3} s, flash at {flash_time:.3} s",
        path.display()
    );
}

/// A file written here keeps the sound in step, and so does a file
/// written from it. Decoded VC-1 and WMA Pro frames carry no timestamps;
/// the first packet of each stream gives its time.
fn two_passes(name: &str, codec: &str) {
    let input = flash_and_beep(&format!("{name}_source.mkv"), 0.0, 0.0, "flac");
    let first = dir().join(format!("{name}_first.wmv"));
    encode(&[
        "-i",
        input.to_str().unwrap(),
        "--audio-codec",
        codec,
        "-o",
        first.to_str().unwrap(),
    ]);
    assert_flash_with_beep(&first);
    let second = dir().join(format!("{name}_second.wmv"));
    encode(&[
        "-i",
        first.to_str().unwrap(),
        "--audio-codec",
        codec,
        "-o",
        second.to_str().unwrap(),
    ]);
    assert_flash_with_beep(&second);
}

#[test]
fn wma_pro_sound_stays_in_step_through_two_passes() {
    two_passes("pro", "pro");
}

/// The WMA 9 Standard stream starts 93 ms before the video; FFmpeg gives
/// the video stream that start too.
#[test]
fn wma_standard_sound_stays_in_step_through_two_passes() {
    two_passes("standard", "standard");
}

#[test]
fn audio_that_starts_later_stays_in_step() {
    in_step("audio_later", 0.0, 0.5, "flac");
}

#[test]
fn audio_that_starts_earlier_stays_in_step() {
    in_step("audio_earlier", 0.4, 0.0, "flac");
}

/// The fps filter puts its pictures on a grid from 0; video that starts
/// between grid points stays in step with the sound.
#[test]
fn video_that_starts_between_frame_times_stays_in_step() {
    in_step("video_off_grid", 0.3, 0.0, "flac");
}

/// MP3 and Opus decoders drop encoder priming, which moves the first
/// timestamp of the sound.
#[test]
fn mp3_stays_in_step() {
    in_step("mp3", 0.0, 0.0, "libmp3lame");
}

#[test]
fn opus_stays_in_step() {
    in_step("opus", 0.0, 0.0, "libopus");
}

/// Sample rates of the WMA tables are kept; others go to the next one up.
#[test]
fn sample_rates_map_to_wma_rates() {
    for (rate, expected) in [(44100, 44100), (11025, 11025), (12000, 16000), (8000, 8000)] {
        let input = dir().join(format!("rate_{rate}.flac"));
        run(
            "ffmpeg",
            &[
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("sine=r={rate}:d=1"),
                input.to_str().unwrap(),
            ],
        );
        let output = dir().join(format!("rate_{rate}.wma"));
        encode(&[
            "-i",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ]);
        let probe = run(
            "ffprobe",
            &[
                "-v",
                "error",
                "-show_entries",
                "stream=sample_rate",
                "-of",
                "csv=p=0",
                output.to_str().unwrap(),
            ],
        );
        assert_eq!(
            String::from_utf8(probe).unwrap().trim(),
            expected.to_string(),
            "{rate} Hz"
        );
    }
}

/// The cover picture of a sound file is not video.
#[test]
fn a_cover_picture_is_not_video() {
    let cover = dir().join("cover.png");
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=64x64:d=1",
            "-frames:v",
            "1",
            cover.to_str().unwrap(),
        ],
    );
    let input = dir().join("with_cover.mp3");
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=d=1",
            "-i",
            cover.to_str().unwrap(),
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-c:a",
            "libmp3lame",
            "-c:v",
            "png",
            "-disposition:v",
            "attached_pic",
            input.to_str().unwrap(),
        ],
    );
    let output = dir().join("with_cover.wma");
    encode(&[
        "-i",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(streams(&output), ["audio"]);
}

/// A raw H.264 stream carries no timestamps; its pictures are numbered at
/// the frame rate.
#[test]
fn video_without_timestamps() {
    let input = dir().join("raw.h264");
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x120:r=25:d=3",
            "-c:v",
            "libx264",
            "-f",
            "h264",
            input.to_str().unwrap(),
        ],
    );
    let output = dir().join("raw_h264.wmv");
    encode(&[
        "-i",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(pictures(&output, 160 * 120).len(), 75);
}

/// A single picture makes one frame.
#[test]
fn a_single_picture() {
    let input = dir().join("still.png");
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x120:d=1",
            "-frames:v",
            "1",
            input.to_str().unwrap(),
        ],
    );
    let output = dir().join("still.wmv");
    encode(&[
        "-i",
        input.to_str().unwrap(),
        "--fps",
        "25/1",
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(pictures(&output, 160 * 120).len(), 1);
}

/// MP4 with dropped frames has an average frame rate of large terms; the
/// stream's base rate serves instead.
#[test]
fn variable_frame_rate() {
    let input = dir().join("vfr.mp4");
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x120:r=30000/1001:d=3",
            "-vf",
            "select='not(between(n\\,10\\,12))'",
            "-fps_mode",
            "vfr",
            "-c:v",
            "libx264",
            input.to_str().unwrap(),
        ],
    );
    let output = dir().join("vfr.wmv");
    encode(&[
        "-i",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    let n = pictures(&output, 160 * 120).len();
    assert!((88..=91).contains(&n), "{n} pictures for 3 s");
}

/// Sound that changes from mono to stereo in the middle, as joined
/// transport streams do, is read to the end.
#[test]
fn audio_that_changes_format() {
    let parts: Vec<PathBuf> = [("mono", 0.0), ("stereo", 1.0)]
        .into_iter()
        .map(|(layout, offset)| {
            let part = dir().join(format!("part_{layout}.ts"));
            run(
                "ffmpeg",
                &[
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("sine=d=1,aformat=channel_layouts={layout}"),
                    "-c:a",
                    "mp2",
                    "-output_ts_offset",
                    &offset.to_string(),
                    part.to_str().unwrap(),
                ],
            );
            part
        })
        .collect();
    let joined = dir().join("joined.ts");
    let bytes: Vec<u8> = parts
        .iter()
        .flat_map(|p| std::fs::read(p).unwrap())
        .collect();
    std::fs::write(&joined, bytes).unwrap();
    let output = dir().join("joined.wma");
    encode(&[
        "-i",
        joined.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    let seconds = sound(&output).len() as f64 / 48000.0;
    assert!((seconds - 2.0).abs() < 0.1, "{seconds} s of sound");
}

/// 96 kHz 5.1 sound alone becomes a 48 kHz stereo WMA file of the same
/// length.
#[test]
fn audio_alone() {
    let input = dir().join("surround.flac");
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=f=440:r=96000:d=2,aformat=channel_layouts=5.1",
            input.to_str().unwrap(),
        ],
    );
    let output = dir().join("surround.wma");
    encode(&[
        "-i",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(streams(&output), ["audio"]);
    let probe = run(
        "ffprobe",
        &[
            "-v",
            "error",
            "-show_entries",
            "stream=sample_rate,channels",
            "-of",
            "csv=p=0",
            output.to_str().unwrap(),
        ],
    );
    assert_eq!(String::from_utf8(probe).unwrap().trim(), "48000,2");
    let seconds = sound(&output).len() as f64 / 48000.0;
    assert!((seconds - 2.0).abs() < 0.05, "{seconds} s of sound");
}

/// The video of a file with sound, without it, scaled and at a bit rate.
#[test]
fn video_alone() {
    let input = flash_and_beep("video_alone.mkv", 0.0, 0.0, "flac");
    let output = dir().join("video_alone.wmv");
    encode(&[
        "-i",
        input.to_str().unwrap(),
        "--no-audio",
        "--scale",
        "96x64",
        "--bitrate",
        "200",
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(streams(&output), ["video"]);
    assert_eq!(pictures(&output, 96 * 64).len(), 75);
}

/// In ASF with B pictures the last decoded frame has no timestamp; it is
/// kept, after the one before.
#[test]
fn a_last_frame_without_a_timestamp_is_kept() {
    let input = dir().join("b_pictures.asf");
    run(
        "ffmpeg",
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x120:r=25:d=3",
            "-c:v",
            "mpeg4",
            "-bf",
            "2",
            "-f",
            "asf",
            input.to_str().unwrap(),
        ],
    );
    let output = dir().join("b_pictures.wmv");
    encode(&[
        "-i",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    assert_eq!(pictures(&output, 160 * 120).len(), 75);
}
