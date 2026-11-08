//! Encodes video and sound into a WMV3 (Windows Media Video 9) and WMA 9
//! Standard file laid out like the Windows Media Format SDK 11 writes
//! them. The input is any media file FFmpeg reads, or raw YUV 4:2:0
//! frames with an optional sound file.
//!
//! wmv3-encode -i in.mp4 --bitrate 800 -o out.wmv
//! ffmpeg -i in.mp4 -f rawvideo -pix_fmt yuv420p - |
//!     wmv3-encode --size 424x240 --fps 30000/1001 --audio audio.wav -o out.wmv

#[cfg(feature = "ffmpeg")]
mod media;

use std::io::Read;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use wmv9::asf::mux::{
    AudioTrack, FileTime, MuxError, MuxSettings, PacketSize, VideoFormat, VideoFrame, VideoTrack,
    mux,
};
use wmv9::asf::{FourCc, Guid, Millis};
use wmv9::wav::Pcm;
use wmv9::wma::{ByteRate, Channels, Codec, Format as AudioFormat, SampleRate, encode_track};
use wmv9::wmv3::encode::{
    Encoder, FrameRate, Quantizer, RateTarget, Settings, Yuv420, sample_sequence,
};
use wmv9::wmv3::{FrameBody, FrameContext, PqIndex, SequenceHeader};

const USAGE: &str =
    "usage: wmv3-encode -i INPUT [--scale WxH] [--fps N/D] [--no-audio] [--no-video]
                   [--quality 1-31 | --bitrate KBPS] [--key-interval N]
                   [--audio FILE] [--audio-bitrate KBPS] [--audio-codec standard|pro]
                   -o OUTPUT
       wmv3-encode --size WxH [--fps N/D] [-i RAW] ... -o OUTPUT

Reads the first video and audio stream of INPUT, any file FFmpeg reads, or
with --size raw planar YUV 4:2:0 frames from RAW (default: standard input).
Without video the output is audio only; name it .wma.
--scale         picture size to scale the video of INPUT to
--fps           frame rate as a fraction, 1 to 240 per second; default the
                video's own, or 30000/1001 for raw frames
--no-audio      leave out the audio of INPUT
--no-video      leave out the video of INPUT
--quality       PQINDEX for every picture, 1 (best) to 31 (smallest), default 6
--bitrate       kbit/s of the video data before ASF overhead, 1 to 100000,
                instead of --quality
--key-interval  pictures from one I picture to the next, at least 1, default 300
--audio         sound from this file instead of from INPUT
--audio-bitrate kbit/s of the audio stream, 24 to 320, default 128
--audio-codec   standard (WMA 9 Standard, the default) or pro (WMA 10
                Professional)";

/// The --audio-bitrate default: the audio bit rate of the sample files.
const DEFAULT_AUDIO_KBPS: u32 = 128;

/// The --key-interval default: ten seconds at 30 frames per second.
const DEFAULT_KEY_INTERVAL: u32 = 300;

/// The --quality default.
const DEFAULT_QUALITY: u8 = 6;

/// Largest --bitrate in kbit/s.
const MAX_VIDEO_KBPS: u32 = 100_000;

/// Preroll of the files, which is also the time a player buffers under
/// rate control.
const PREROLL: Millis = Millis(3000);

/// Which streams to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Streams {
    Both,
    Video,
    Audio,
}

impl Streams {
    pub fn video(self) -> bool {
        self != Streams::Audio
    }

    pub fn audio(self) -> bool {
        self != Streams::Video
    }
}

impl std::fmt::Display for Streams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Streams::Both => "video or audio",
            Streams::Video => "video",
            Streams::Audio => "audio",
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct FrameSize {
    width: u16,
    height: u16,
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("{0}\n\n{USAGE}")]
    Usage(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot read standard input: {0}")]
    Stdin(std::io::Error),
    #[error("cannot read random bytes for the file id: {0}")]
    Random(std::io::Error),
    #[error("the system clock is before 1970")]
    Clock,
    #[error("input ends inside frame {frame}")]
    PartialFrame { frame: u64 },
    #[error(transparent)]
    Encode(#[from] wmv9::wmv3::encode::EncodeError),
    #[error(transparent)]
    Rate(#[from] wmv9::wmv3::encode::RateError),
    #[error(transparent)]
    Frame(#[from] wmv9::wmv3::FrameError),
    #[error(transparent)]
    Mux(#[from] MuxError),
    #[error(transparent)]
    Asf(#[from] wmv9::asf::Error),
    #[cfg(not(feature = "ffmpeg"))]
    #[error("{path}: {source}")]
    Wav {
        path: PathBuf,
        source: wmv9::wav::WavError,
    },
    #[cfg(feature = "ffmpeg")]
    #[error(transparent)]
    Media(#[from] media::MediaError),
    #[error("{path}: {channels} channels; mono and stereo are supported")]
    AudioChannels { path: PathBuf, channels: u16 },
    #[cfg(feature = "ffmpeg")]
    #[error("{path}: no video for --quality or --bitrate")]
    NoVideo { path: PathBuf },
    #[cfg(feature = "ffmpeg")]
    #[error("{path}: no audio for --audio-bitrate")]
    NoAudio { path: PathBuf },
    #[error(transparent)]
    AudioEncode(#[from] wmv9::wma::TrackError),
}

/// What sets the quantizer of the pictures.
#[derive(Clone, Copy)]
enum VideoQuality {
    Fixed(PqIndex),
    /// kbit/s.
    Rate(NonZeroU32),
}

/// Where the pictures, and possibly the sound, come from.
enum Source {
    /// Raw YUV 4:2:0 frames of `size`, from a file or standard input.
    Raw {
        size: FrameSize,
        rate: FrameRate,
        path: Option<PathBuf>,
    },
    /// A media file FFmpeg reads.
    #[cfg(feature = "ffmpeg")]
    Media {
        path: PathBuf,
        streams: Streams,
        scale: Option<FrameSize>,
        fps: Option<FrameRate>,
    },
    /// No pictures: the sound file alone.
    None,
}

impl Source {
    fn has_video(&self) -> bool {
        match self {
            Source::Raw { .. } => true,
            #[cfg(feature = "ffmpeg")]
            Source::Media { streams, .. } => streams.video(),
            Source::None => false,
        }
    }

    fn has_audio(&self) -> bool {
        match self {
            #[cfg(feature = "ffmpeg")]
            Source::Media { streams, .. } => streams.audio(),
            _ => false,
        }
    }
}

#[cfg(feature = "ffmpeg")]
fn media_source(
    path: PathBuf,
    streams: Streams,
    scale: Option<FrameSize>,
    fps: Option<FrameRate>,
) -> Result<Source, CliError> {
    Ok(Source::Media {
        path,
        streams,
        scale,
        fps,
    })
}

#[cfg(not(feature = "ffmpeg"))]
fn media_source(
    _: PathBuf,
    _: Streams,
    _: Option<FrameSize>,
    _: Option<FrameRate>,
) -> Result<Source, CliError> {
    Err(CliError::Usage(
        "built without FFmpeg: give --size for raw YUV input".into(),
    ))
}

struct Options {
    source: Source,
    /// `None` when neither --quality nor --bitrate is given.
    quality: Option<VideoQuality>,
    key_interval: NonZeroU32,
    output: PathBuf,
    /// A sound file apart from the pictures.
    audio: Option<PathBuf>,
    /// `None` when --audio-bitrate is not given.
    audio_kbps: Option<u32>,
    audio_codec: Codec,
}

/// What the command line asks for.
enum Request {
    Help,
    Encode(Options),
}

fn split_numbers(s: &str, sep: char, what: &str) -> Result<[u64; 2], CliError> {
    let bad = || CliError::Usage(format!("{what}: expected two numbers separated by '{sep}'"));
    let (a, b) = s.split_once(sep).ok_or_else(bad)?;
    Ok([a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?])
}

fn frame_size(text: &str, name: &str) -> Result<FrameSize, CliError> {
    let [w, h] = split_numbers(text, 'x', name)?;
    let side = |v: u64| {
        u16::try_from(v)
            .ok()
            .filter(|&v| v > 0)
            .ok_or_else(|| CliError::Usage(format!("{name}: 1 to 65535 per side")))
    };
    Ok(FrameSize {
        width: side(w)?,
        height: side(h)?,
    })
}

fn parse_args() -> Result<Request, CliError> {
    let mut args = std::env::args_os().skip(1);
    let mut size = None;
    let mut scale = None;
    let mut fps = None;
    let mut quality = None;
    let mut video_kbps = None;
    let mut key_interval = None;
    let mut input = None;
    let mut output = None;
    let mut audio = None;
    let mut audio_kbps = None;
    let mut audio_codec = Codec::Standard;
    let mut no_audio = false;
    let mut no_video = false;
    while let Some(a) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| CliError::Usage(format!("{name} needs a value")))
        };
        let text = |v: std::ffi::OsString, name: &str| {
            v.into_string()
                .map_err(|_| CliError::Usage(format!("{name}: not valid text")))
        };
        match a.to_str() {
            Some("--size") => {
                size = Some(frame_size(&text(value("--size")?, "--size")?, "--size")?)
            }
            Some("--scale") => {
                scale = Some(frame_size(&text(value("--scale")?, "--scale")?, "--scale")?);
            }
            Some("--fps") => {
                let [n, d] = split_numbers(&text(value("--fps")?, "--fps")?, '/', "--fps")?;
                fps = Some(FrameRate::new(n, d).ok_or_else(|| {
                    CliError::Usage("--fps: 1 to 240 frames per second, terms up to 1000000".into())
                })?);
            }
            Some("--quality") => {
                quality = Some(
                    text(value("--quality")?, "--quality")?
                        .parse()
                        .ok()
                        .and_then(PqIndex::new)
                        .ok_or_else(|| CliError::Usage("--quality: expected 1 to 31".into()))?,
                );
            }
            Some("--bitrate") => {
                video_kbps = Some(
                    text(value("--bitrate")?, "--bitrate")?
                        .parse::<NonZeroU32>()
                        .ok()
                        .filter(|v| v.get() <= MAX_VIDEO_KBPS)
                        .ok_or_else(|| CliError::Usage("--bitrate: expected 1 to 100000".into()))?,
                );
            }
            Some("--key-interval") => {
                key_interval = Some(
                    text(value("--key-interval")?, "--key-interval")?
                        .parse()
                        .map_err(|_| {
                            CliError::Usage("--key-interval: a whole number, at least 1".into())
                        })?,
                );
            }
            Some("--audio") => audio = Some(PathBuf::from(value("--audio")?)),
            Some("--audio-bitrate") => {
                audio_kbps = Some(
                    text(value("--audio-bitrate")?, "--audio-bitrate")?
                        .parse()
                        .ok()
                        .filter(|v| (24..=320).contains(v))
                        .ok_or_else(|| {
                            CliError::Usage("--audio-bitrate: expected 24 to 320".into())
                        })?,
                );
            }
            Some("--audio-codec") => {
                audio_codec = match text(value("--audio-codec")?, "--audio-codec")?.as_str() {
                    "standard" => Codec::Standard,
                    "pro" => Codec::Pro,
                    _ => {
                        return Err(CliError::Usage(
                            "--audio-codec: expected standard or pro".into(),
                        ));
                    }
                };
            }
            Some("--no-audio") => no_audio = true,
            Some("--no-video") => no_video = true,
            Some("-i") => input = Some(PathBuf::from(value("-i")?)),
            Some("-o") => output = Some(PathBuf::from(value("-o")?)),
            Some("-h" | "--help") => return Ok(Request::Help),
            _ => {
                return Err(CliError::Usage(format!(
                    "unknown argument {}",
                    a.to_string_lossy()
                )));
            }
        }
    }
    let usage = |m: &str| Err(CliError::Usage(m.into()));
    if no_audio && audio.is_some() {
        return usage("--no-audio and --audio exclude each other");
    }
    let source = match (size, input) {
        (Some(size), path) => {
            if scale.is_some() || no_video || no_audio {
                return usage(
                    "--scale, --no-video and --no-audio apply to media input, not --size",
                );
            }
            Source::Raw {
                size,
                rate: fps.unwrap_or(FrameRate::new(30000, 1001).expect("NTSC rate is in range")),
                path,
            }
        }
        (None, Some(path)) => {
            if no_video && (no_audio || audio.is_some()) {
                return usage("-i: nothing left to read from it");
            }
            let streams = match (no_video, no_audio || audio.is_some()) {
                (false, false) => Streams::Both,
                (false, true) => Streams::Video,
                (true, _) => Streams::Audio,
            };
            media_source(path, streams, scale, fps)?
        }
        (None, None) if audio.is_some() && no_video => Source::None,
        (None, None) => {
            return usage(
                "-i INPUT is required, or --size for raw YUV on standard input, \
                 or --audio FILE with --no-video",
            );
        }
    };
    if audio_kbps.is_some() && audio.is_none() && !source.has_audio() {
        return usage("--audio-bitrate needs audio");
    }
    if !source.has_video()
        && (quality.is_some()
            || video_kbps.is_some()
            || key_interval.is_some()
            || fps.is_some()
            || scale.is_some())
    {
        return usage("--quality, --bitrate, --key-interval, --fps and --scale need video");
    }
    let quality = match (quality, video_kbps) {
        (Some(_), Some(_)) => return usage("--quality and --bitrate exclude each other"),
        (Some(q), None) => Some(VideoQuality::Fixed(q)),
        (None, Some(kbps)) => Some(VideoQuality::Rate(kbps)),
        (None, None) => None,
    };
    Ok(Request::Encode(Options {
        source,
        quality,
        key_interval: key_interval
            .unwrap_or(NonZeroU32::new(DEFAULT_KEY_INTERVAL).expect("not zero")),
        output: output.ok_or_else(|| CliError::Usage("-o is required".into()))?,
        audio,
        audio_kbps,
        audio_codec,
    }))
}

/// A random (version 4) GUID from the system's random source.
fn file_id() -> Result<Guid, CliError> {
    let mut id = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut id))
        .map_err(CliError::Random)?;
    // Version 4 in the high nibble of the third field (byte 7 on disk),
    // the RFC 4122 variant in byte 8.
    id[7] = (id[7] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;
    Ok(Guid(id))
}

/// Now as FILETIME: 100 ns units since 1601-01-01.
fn filetime_now() -> Result<FileTime, CliError> {
    const UNIX_TO_1601_SECONDS: u64 = 11_644_473_600;
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| CliError::Clock)?;
    Ok(FileTime(
        (d.as_secs() + UNIX_TO_1601_SECONDS) * 10_000_000 + u64::from(d.subsec_nanos() / 100),
    ))
}

/// Reads one frame and returns the bytes read, `None` at a clean end of
/// input.
fn read_frame(input: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok((filled > 0).then_some(filled))
}

/// The sound of a file, through FFmpeg.
#[cfg(feature = "ffmpeg")]
fn read_audio(path: &Path) -> Result<Pcm, CliError> {
    let sel = media::Selection {
        streams: Streams::Audio,
        scale: None,
        fps: None,
    };
    let decoded = media::Media::open(path, &sel)?.run(|_| unreachable!("no video selected"))?;
    report_damage(path, decoded.damage);
    Ok(decoded.sound.expect("open found an audio stream"))
}

/// Tells of damage the decoders passed over.
#[cfg(feature = "ffmpeg")]
fn report_damage(path: &Path, d: media::Damage) {
    if d.decoder_errors > 0 {
        eprintln!(
            "wmv3-encode: {}: {} damaged packets or frames skipped",
            path.display(),
            d.decoder_errors
        );
    }
    if d.pictures_out_of_order > 0 {
        eprintln!(
            "wmv3-encode: {}: {} pictures dropped whose time goes back, as in joined files",
            path.display(),
            d.pictures_out_of_order
        );
    }
}

/// The sound of a 16-bit PCM WAV file.
#[cfg(not(feature = "ffmpeg"))]
fn read_audio(path: &Path) -> Result<Pcm, CliError> {
    let bytes = std::fs::read(path).map_err(|source| CliError::Io {
        path: path.to_owned(),
        source,
    })?;
    wmv9::wav::parse(&bytes).map_err(|source| CliError::Wav {
        path: path.to_owned(),
        source,
    })
}

/// Encodes the sound into one ASF audio object per WMA packet.
fn encode_audio(pcm: &Pcm, codec: Codec, kbps: u32, path: &Path) -> Result<AudioTrack, CliError> {
    let channels = match pcm.format.channels {
        1 => Channels::Mono,
        2 => Channels::Stereo,
        n => {
            return Err(CliError::AudioChannels {
                path: path.to_owned(),
                channels: n,
            });
        }
    };
    let format = AudioFormat {
        sample_rate: SampleRate(pcm.format.sample_rate),
        channels,
        byte_rate: ByteRate(kbps * 1000 / 8),
    };
    Ok(encode_track(codec, format, &pcm.samples)?)
}

/// The video encoder and the frames it has written.
struct VideoOut<'a> {
    ctx: FrameContext<'a>,
    encoder: Encoder<'a>,
    rate: FrameRate,
    frames: Vec<VideoFrame>,
}

impl<'a> VideoOut<'a> {
    fn new(
        seq: &'a SequenceHeader,
        size: FrameSize,
        rate: FrameRate,
        quality: Option<VideoQuality>,
        key_interval: NonZeroU32,
    ) -> Result<Self, CliError> {
        let ctx = FrameContext {
            seq,
            width: size.width,
            height: size.height,
        };
        let quantizer = match quality.unwrap_or(VideoQuality::Fixed(
            PqIndex::new(DEFAULT_QUALITY).expect("1 to 31"),
        )) {
            VideoQuality::Fixed(pq) => Quantizer::Fixed(pq),
            VideoQuality::Rate(kbps) => Quantizer::Rate(RateTarget::new(
                kbps.checked_mul(NonZeroU32::new(1000).expect("not zero"))
                    .expect("--bitrate is at most 100000"),
                rate,
                PREROLL,
            )?),
        };
        Ok(VideoOut {
            ctx,
            encoder: Encoder::new(
                ctx,
                Settings {
                    quantizer,
                    key_interval,
                },
            )?,
            rate,
            frames: Vec::new(),
        })
    }

    fn push(&mut self, src: &Yuv420) -> Result<(), CliError> {
        let frame = self.encoder.encode(src)?;
        let index = self.frames.len() as u64;
        let start = self.rate.ms(index);
        self.frames.push(VideoFrame {
            data: frame.to_bytes(&self.ctx)?,
            key: matches!(frame.body, FrameBody::Intra(_)),
            presentation: start,
            duration: Millis(self.rate.ms(index + 1).0 - start.0),
        });
        Ok(())
    }

    /// The track, and the frames that exceeded the rate control buffer.
    fn finish(self) -> (VideoTrack, Option<u64>) {
        let overflows = self.encoder.buffer_overflows();
        let format = VideoFormat {
            width: self.ctx.width.into(),
            height: self.ctx.height.into(),
            fourcc: FourCc(*b"WMV3"),
            codec_data: self
                .ctx
                .seq
                .to_bytes()
                .expect("the sample sequence header writes"),
            average_time_per_frame: self.rate.frame_time(),
            codec_name: "VC-1".into(),
            codec_description: "VC-1 Simple/Main Profile".into(),
            conformance: "MP".into(),
        };
        (
            VideoTrack {
                format,
                frames: self.frames,
            },
            overflows,
        )
    }
}

/// Encodes raw frames of `size` from `path` or standard input.
fn read_raw(path: Option<&Path>, size: FrameSize, out: &mut VideoOut) -> Result<(), CliError> {
    let (w, h) = (usize::from(size.width), usize::from(size.height));
    let chroma = w.div_ceil(2) * h.div_ceil(2);
    let mut input: Box<dyn Read> = match path {
        Some(p) => Box::new(std::fs::File::open(p).map_err(|source| CliError::Io {
            path: p.to_owned(),
            source,
        })?),
        None => Box::new(std::io::stdin().lock()),
    };
    let read_error = |source| match path {
        Some(p) => CliError::Io {
            path: p.to_owned(),
            source,
        },
        None => CliError::Stdin(source),
    };
    let mut buf = vec![0u8; w * h + 2 * chroma];
    for index in 0u64.. {
        let Some(filled) = read_frame(&mut *input, &mut buf).map_err(read_error)? else {
            break;
        };
        if filled < buf.len() {
            return Err(CliError::PartialFrame { frame: index });
        }
        out.push(&Yuv420 {
            width: size.width,
            height: size.height,
            y: buf[..w * h].to_vec(),
            cb: buf[w * h..w * h + chroma].to_vec(),
            cr: buf[w * h + chroma..].to_vec(),
        })?;
    }
    Ok(())
}

fn encode(o: Options) -> Result<(), CliError> {
    // Before the video, which may come from a pipe that cannot be read
    // again after an error in the audio.
    let sound = match &o.audio {
        Some(path) => Some((read_audio(path)?, path.clone())),
        None => None,
    };
    let seq = sample_sequence();
    let (video, media_sound) = match &o.source {
        Source::Raw { size, rate, path } => {
            let mut out = VideoOut::new(&seq, *size, *rate, o.quality, o.key_interval)?;
            read_raw(path.as_deref(), *size, &mut out)?;
            (Some(out), None)
        }
        #[cfg(feature = "ffmpeg")]
        Source::Media {
            path,
            streams,
            scale,
            fps,
        } => {
            let sel = media::Selection {
                streams: *streams,
                scale: *scale,
                fps: *fps,
            };
            let input = media::Media::open(path, &sel)?;
            if input.video_info().is_none() && o.quality.is_some() {
                return Err(CliError::NoVideo { path: path.clone() });
            }
            if !input.has_audio() && o.audio.is_none() && o.audio_kbps.is_some() {
                return Err(CliError::NoAudio { path: path.clone() });
            }
            let mut out = input
                .video_info()
                .map(|info| {
                    VideoOut::new(&seq, info.size, info.frame_rate, o.quality, o.key_interval)
                })
                .transpose()?;
            let decoded = input.run(|picture| {
                out.as_mut()
                    .expect("pictures come only from a video stream")
                    .push(&picture)
            })?;
            report_damage(path, decoded.damage);
            (out, decoded.sound.map(|pcm| (pcm, path.clone())))
        }
        Source::None => (None, None),
    };
    let sound = media_sound.or(sound);
    let audio = sound
        .as_ref()
        .map(|(pcm, path)| {
            encode_audio(
                pcm,
                o.audio_codec,
                o.audio_kbps.unwrap_or(DEFAULT_AUDIO_KBPS),
                path,
            )
        })
        .transpose()?;
    let audio_seconds = sound.as_ref().map(|(pcm, _)| {
        pcm.samples.len() as f64
            / f64::from(pcm.format.channels)
            / f64::from(pcm.format.sample_rate)
    });
    let (video, overflows) = match video {
        Some(v) => {
            let (track, overflows) = v.finish();
            (Some(track), overflows)
        }
        None => (None, None),
    };
    let frames = video.as_ref().map(|v| v.frames.len());
    let settings = MuxSettings {
        packet_size: PacketSize::SDK,
        preroll: PREROLL,
        file_id: file_id()?,
        creation_date: filetime_now()?,
        sdk_version: None,
    };
    let bytes = mux(video, audio, &settings)?.to_bytes()?;
    std::fs::write(&o.output, bytes).map_err(|source| CliError::Io {
        path: o.output.clone(),
        source,
    })?;
    let what = match (frames, audio_seconds) {
        (Some(n), Some(s)) => format!("{n} frames and {s:.1} s of audio"),
        (Some(n), None) => format!("{n} frames"),
        (None, Some(s)) => format!("{s:.1} s of audio"),
        (None, None) => unreachable!("mux needs a stream"),
    };
    eprintln!("{what} written to {}", o.output.display());
    if let Some(n @ 1..) = overflows {
        eprintln!(
            "wmv3-encode: {n} frames exceed the {} ms buffer even at the coarsest quantizer; \
             the bit rate is too low for this video, and a player may pause",
            PREROLL.0
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    let result = parse_args().and_then(|r| match r {
        Request::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Request::Encode(o) => encode(o),
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wmv3-encode: {e}");
            ExitCode::FAILURE
        }
    }
}
