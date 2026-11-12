//! Inputs and runs shared by the benchmarks.

use std::f64::consts::PI;
use std::path::Path;

use wmv9::asf::{Asf, TypeSpecificData};
use wmv9::wma::pro::syntax::pack;
use wmv9::wma::{ByteRate, Channels, Format, SampleRate};
use wmv9::wmv3::decode::Decoder;
use wmv9::wmv3::encode::{Encoder, Quantizer, Settings, Yuv420, sample_sequence};
use wmv9::wmv3::{Frame, FrameContext, PqIndex, SequenceHeader};

pub const SAMPLE_RATE: u32 = 44100;
pub const AUDIO_SECONDS: usize = 5;
pub const VIDEO_WIDTH: u16 = 320;
pub const VIDEO_HEIGHT: u16 = 240;
pub const VIDEO_FRAMES: u32 = 30;
pub const VIDEO_PQINDEX: u8 = 6;
/// Pictures of the sample stream encoded.
pub const SAMPLE_PICTURES: usize = 60;
/// A Microsoft sample stream for decoding, and the source of real
/// pictures to encode; the benchmarks of it skip without it.
pub const SAMPLE_FILE: &str = "testdata/ads/hvideo15_1.wmv";

pub fn format(channels: Channels, byte_rate: u32) -> Format {
    Format {
        sample_rate: SampleRate(SAMPLE_RATE),
        channels,
        byte_rate: ByteRate(byte_rate),
    }
}

/// Interleaved audio of `AUDIO_SECONDS`: chords of three tones that change
/// every half second, noise, and a click every 0.7 s for short blocks.
pub fn audio(channels: Channels) -> Vec<i16> {
    let count = channels.count();
    let mut seed = 1u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(seed >> 8) / f64::from(1u32 << 24) - 0.5
    };
    let rate = f64::from(SAMPLE_RATE);
    (0..AUDIO_SECONDS * SAMPLE_RATE as usize)
        .flat_map(|i| {
            let t = i as f64 / rate;
            let root = 220.0 * (1.0 + (i / (SAMPLE_RATE as usize / 2) % 5) as f64 * 0.25);
            let tone = [1.0, 1.25, 1.5]
                .iter()
                .map(|r| (2.0 * PI * root * r * t).sin())
                .sum::<f64>()
                * 4000.0;
            let click = if i % (7 * SAMPLE_RATE as usize / 10) == 0 {
                15000.0
            } else {
                0.0
            };
            (0..count)
                .map(|ch| {
                    let pan = if ch == 0 { 1.0 } else { 0.7 };
                    (tone * pan + 800.0 * noise() + click).round() as i16
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn wmapro(format: Format, pcm: &[i16]) -> Vec<Vec<u8>> {
    let mut e = wmv9::wma::pro::encode::Encoder::new(format).unwrap();
    e.push(pcm).unwrap();
    e.finish().unwrap().into_iter().map(|p| p.data).collect()
}

pub fn wmapro_xma(format: Format, pcm: &[i16]) -> Vec<Vec<u8>> {
    let mut e = wmv9::wma::pro::encode::Encoder::xma(format).unwrap();
    e.push(pcm).unwrap();
    e.finish_frames()
        .unwrap()
        .into_iter()
        .map(|bits| pack(&bits))
        .collect()
}

pub fn wma2(format: Format, pcm: &[i16]) -> Vec<Vec<u8>> {
    let mut e = wmv9::wma::encode::Encoder::new(format).unwrap();
    let mut out = e.push(pcm).unwrap();
    out.extend(e.finish().unwrap());
    out
}

/// A textured pan with a moving square and fine noise.
pub fn video_frame(t: u32) -> Yuv420 {
    let (w, h) = (usize::from(VIDEO_WIDTH), usize::from(VIDEO_HEIGHT));
    let mut seed = t.wrapping_mul(2_654_435_761).wrapping_add(1);
    let shift = (1.25 * f64::from(t), 0.5 * f64::from(t));
    let left = 20 + 4 * t as usize;
    let y = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = f64::from((seed >> 24) % 9) - 4.0;
            if (left..left + 40).contains(&x) && (60..100).contains(&y) {
                return 230;
            }
            let (fx, fy) = (x as f64 + shift.0, y as f64 + shift.1);
            (128.0
                + 60.0 * (fx * 0.21).sin() * (fy * 0.17).cos()
                + 30.0 * ((fx + fy) * 0.05).sin()
                + n)
                .clamp(0.0, 255.0) as u8
        })
        .collect();
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    Yuv420 {
        width: VIDEO_WIDTH,
        height: VIDEO_HEIGHT,
        y,
        cb: (0..cw * ch)
            .map(|i| (64 + (i % cw + t as usize) % 128) as u8)
            .collect(),
        cr: (0..cw * ch).map(|i| (200 - (i / cw) / 2) as u8).collect(),
    }
}

pub fn video() -> Vec<Yuv420> {
    (0..VIDEO_FRAMES).map(video_frame).collect()
}

/// The first `count` pictures of a sample stream, in decoding order.
pub fn sample_pictures(stream: &VideoStream, count: usize) -> Vec<Yuv420> {
    let (w, h) = (usize::from(stream.width), usize::from(stream.height));
    let luma = w * h;
    let chroma = w.div_ceil(2) * h.div_ceil(2);
    wmv3_decode(&VideoStream {
        frames: stream.frames[..count].to_vec(),
        ..stream.clone()
    })
    .into_iter()
    .map(|p| Yuv420 {
        width: stream.width,
        height: stream.height,
        y: p[..luma].to_vec(),
        cb: p[luma..luma + chroma].to_vec(),
        cr: p[luma + chroma..].to_vec(),
    })
    .collect()
}

/// Encodes `frames` at `VIDEO_PQINDEX` and returns the coded frames.
pub fn wmv3_encode(frames: &[Yuv420]) -> Vec<Vec<u8>> {
    wmv3_encode_with(
        frames,
        Quantizer::Fixed(PqIndex::new(VIDEO_PQINDEX).unwrap()),
    )
}

pub fn wmv3_encode_with(frames: &[Yuv420], quantizer: Quantizer) -> Vec<Vec<u8>> {
    let seq = sample_sequence();
    let ctx = FrameContext {
        seq: &seq,
        width: frames[0].width,
        height: frames[0].height,
    };
    let settings = Settings {
        quantizer,
        key_interval: 300.try_into().unwrap(),
    };
    let mut e = Encoder::new(ctx, settings).unwrap();
    frames
        .iter()
        .map(|f| e.encode(f).unwrap().to_bytes(&ctx).unwrap())
        .collect()
}

/// The video stream of an ASF file: its sequence header, coded size and
/// frames.
#[derive(Clone)]
pub struct VideoStream {
    pub seq: SequenceHeader,
    pub width: u16,
    pub height: u16,
    pub frames: Vec<Vec<u8>>,
}

pub fn read_video(path: &Path) -> Option<VideoStream> {
    let asf = Asf::parse(&std::fs::read(path).ok()?).unwrap();
    let (sp, video) = asf
        .stream_properties()
        .find_map(|s| match &s.type_specific {
            TypeSpecificData::Video(v) => Some((s, v)),
            _ => None,
        })
        .unwrap();
    let number = sp.flags.stream_number();
    Some(VideoStream {
        seq: SequenceHeader::parse(&video.codec_data).unwrap(),
        width: video.encoded_width.try_into().unwrap(),
        height: video.encoded_height.try_into().unwrap(),
        frames: asf
            .media_objects()
            .unwrap()
            .into_iter()
            .filter(|o| o.stream == number)
            .map(|o| o.data)
            .collect(),
    })
}

/// Parses and decodes every frame; returns the decoded pictures as YUV.
pub fn wmv3_decode(stream: &VideoStream) -> Vec<Vec<u8>> {
    let ctx = FrameContext {
        seq: &stream.seq,
        width: stream.width,
        height: stream.height,
    };
    let mut decoder = Decoder::new(ctx);
    stream
        .frames
        .iter()
        .filter_map(|data| {
            let frame = Frame::parse(data, &ctx).unwrap();
            decoder
                .decode(&frame)
                .unwrap()
                .map(|p| p.to_yuv420p(stream.width.into(), stream.height.into()))
        })
        .collect()
}
