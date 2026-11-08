//! Reads the pictures and sound of any file FFmpeg reads: the pictures at
//! a constant frame rate in YUV 4:2:0, the sound as 16-bit mono or stereo
//! at a sample rate WMA 9 Standard has, starting with the first picture.

use std::path::{Path, PathBuf};

use ff::format::stream::Disposition;
use ff::media::Type;
use ffmpeg_next as ff;
use wmv9::wav::{Pcm, PcmFormat};
use wmv9::wmv3::encode::{FrameRate, Yuv420};

use crate::{CliError, FrameSize, Streams};

/// Sample rates of the WMA 9 Standard rate tables.
const SAMPLE_RATES: [u32; 7] = [8000, 11025, 16000, 22050, 32000, 44100, 48000];

/// Largest denominator of a frame rate approximated from one FrameRate
/// does not take: that of the NTSC rates.
const MAX_RATE_DENOMINATOR: u64 = 1001;

/// Decoder errors in a row after which a stream is taken as ended.
const MAX_ERRORS_IN_A_ROW: u32 = 64;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("{path}: {source}")]
    Ffmpeg { path: PathBuf, source: ff::Error },
    #[error("{path}: no {wanted} stream")]
    NoStream { path: PathBuf, wanted: Streams },
    #[error("{path}: frame rate {num}/{den} cannot be used; give --fps")]
    FrameRate { path: PathBuf, num: i32, den: i32 },
    #[error("{path}: pictures of {width}x{height}; give --scale")]
    Size {
        path: PathBuf,
        width: u32,
        height: u32,
    },
    #[error("{path}: the audio stream decodes to no sound")]
    NoSound { path: PathBuf },
}

/// Which streams to read, and how to shape the pictures.
pub struct Selection {
    pub streams: Streams,
    /// Picture size to scale to, or the stream's own.
    pub scale: Option<FrameSize>,
    /// Frame rate to convert to, or the stream's own.
    pub fps: Option<FrameRate>,
}

/// The picture stream as the encoder receives it.
#[derive(Debug, Clone, Copy)]
pub struct VideoInfo {
    pub size: FrameSize,
    pub frame_rate: FrameRate,
}

/// A time in seconds of the input file.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
struct Seconds(f64);

/// A timestamp in a stream's time base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Ticks(i64);

impl Ticks {
    fn seconds(self, time_base: ff::Rational) -> Seconds {
        Seconds(
            self.0 as f64 * f64::from(time_base.numerator()) / f64::from(time_base.denominator()),
        )
    }
}

/// What went wrong in decoding without stopping it.
#[derive(Debug, Default, Clone, Copy)]
pub struct Damage {
    /// Packets and frames the decoders rejected.
    pub decoder_errors: u64,
    /// Pictures whose time was before that of an earlier one, which the
    /// frame rate conversion drops.
    pub pictures_out_of_order: u64,
}

/// What the file decodes to besides the pictures.
pub struct Decoded {
    pub sound: Option<Pcm>,
    pub damage: Damage,
}

struct VideoPath {
    stream: usize,
    /// Of the stream's packets, and so of the decoded frames' timestamps.
    time_base: ff::Rational,
    decoder: ff::decoder::Video,
    /// Built from the first decoded frame.
    graph: Option<ff::filter::Graph>,
    info: VideoInfo,
    /// One frame time at the output rate, in the stream's time base.
    period: Ticks,
    /// Timestamp of the first frame; the graph receives timestamps from
    /// it, so the output frame times start on it.
    first: Option<Ticks>,
    /// Timestamp of the latest frame, and the step to it from the one
    /// before, once two frames had timestamps.
    last: Option<Ticks>,
    step: Option<Ticks>,
    times: StreamTimes,
}

impl VideoPath {
    /// The time of the first picture.
    fn start(&self) -> Option<Seconds> {
        self.times
            .start(self.first.map(|t| t.seconds(self.time_base)))
    }
}

/// Where a stream's start time comes from when its first decoded frame
/// has no timestamp of its own. VC-1 and WMA Pro frames have none, and
/// FFmpeg's best-effort guess for the first is a later packet's time: the
/// next one for VC-1, whose decoder holds a frame back, a later audio
/// packet for WMA Pro.
#[derive(Debug, Default)]
struct StreamTimes {
    /// Whether the first decoded frame had a timestamp of its own.
    first_timed: Option<bool>,
    /// Decoding time, else presentation time, of the stream's first
    /// packet.
    first_packet: Option<Seconds>,
    /// The container's start time of the stream; FFmpeg may take it from
    /// another stream.
    container: Option<Seconds>,
}

impl StreamTimes {
    fn new(s: &ff::format::stream::Stream) -> Self {
        StreamTimes {
            first_timed: None,
            first_packet: None,
            container: (s.start_time() != ff::ffi::AV_NOPTS_VALUE)
                .then(|| Ticks(s.start_time()).seconds(s.time_base())),
        }
    }

    fn packet(&mut self, p: &ff::Packet, time_base: ff::Rational) {
        if self.first_packet.is_none() {
            self.first_packet = p.dts().or(p.pts()).map(|t| Ticks(t).seconds(time_base));
        }
    }

    fn frame(&mut self, own: bool) {
        self.first_timed.get_or_insert(own);
    }

    /// The start: `from_frames` when the first frame had a timestamp,
    /// else the first packet's time, else the container's.
    fn start(&self, from_frames: Option<Seconds>) -> Option<Seconds> {
        match self.first_timed {
            Some(true) => from_frames,
            _ => self.first_packet.or(self.container),
        }
    }
}

/// What the audio graph was built for; another input format needs a new
/// graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioInput {
    rate: u32,
    format: ff::format::Sample,
    layout: u64,
}

struct AudioPath {
    stream: usize,
    time_base: ff::Rational,
    decoder: ff::decoder::Audio,
    graph: Option<(AudioInput, ff::filter::Graph)>,
    /// From the first decoded frame.
    format: Option<PcmFormat>,
    /// Time of the first sample out of the graph.
    start: Option<Seconds>,
    times: StreamTimes,
    samples: Vec<i16>,
}

impl AudioPath {
    /// The time of the first sample.
    fn start(&self) -> Option<Seconds> {
        self.times.start(self.start)
    }
}

pub struct Media {
    path: PathBuf,
    input: ff::format::context::Input,
    video: Option<VideoPath>,
    audio: Option<AudioPath>,
    damage: Damage,
}

impl Media {
    pub fn open(path: &Path, sel: &Selection) -> Result<Self, MediaError> {
        let fail = |source| MediaError::Ffmpeg {
            path: path.to_owned(),
            source,
        };
        ff::init().map_err(fail)?;
        // Decoders report recoverable damage on standard error; it is
        // counted here instead.
        ff::util::log::set_level(ff::util::log::Level::Fatal);
        // "-" is standard input, as with FFmpeg's tool.
        let source = if path == Path::new("-") {
            Path::new("pipe:")
        } else {
            path
        };
        let input = ff::format::input(source).map_err(fail)?;
        // Cover pictures of sound files are video streams of one picture.
        let video = sel
            .streams
            .video()
            .then(|| {
                input.streams().find(|s| {
                    s.parameters().medium() == Type::Video
                        && !s.disposition().contains(Disposition::ATTACHED_PIC)
                })
            })
            .flatten()
            .map(|s| video_path(path, &s, sel))
            .transpose()?;
        let audio = sel
            .streams
            .audio()
            .then(|| input.streams().best(Type::Audio))
            .flatten()
            .map(|s| audio_path(path, &s))
            .transpose()?;
        if video.is_none() && audio.is_none() {
            return Err(MediaError::NoStream {
                path: path.to_owned(),
                wanted: sel.streams,
            });
        }
        Ok(Media {
            path: path.to_owned(),
            input,
            video,
            audio,
            damage: Damage::default(),
        })
    }

    pub fn video_info(&self) -> Option<VideoInfo> {
        self.video.as_ref().map(|v| v.info)
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// Decodes the file, giving each picture to `on_picture`. The sound
    /// starts with the first picture when both start times are known.
    pub fn run(
        mut self,
        mut on_picture: impl FnMut(Yuv420) -> Result<(), CliError>,
    ) -> Result<Decoded, CliError> {
        let path = self.path.clone();
        let fail = |source| {
            CliError::Media(MediaError::Ffmpeg {
                path: path.clone(),
                source,
            })
        };
        let damage = &mut self.damage;
        for (stream, packet) in self.input.packets() {
            let index = stream.index();
            if let Some(v) = self.video.as_mut().filter(|v| v.stream == index) {
                v.times.packet(&packet, v.time_base);
                send(&mut v.decoder, &packet, damage);
                v.drain(&mut on_picture, damage, false)
                    .map_err(|e| lift(e, &fail))?;
            } else if let Some(a) = self.audio.as_mut().filter(|a| a.stream == index) {
                a.times.packet(&packet, a.time_base);
                send(&mut a.decoder, &packet, damage);
                a.drain(damage, false).map_err(fail)?;
            }
        }
        if let Some(v) = self.video.as_mut() {
            v.decoder.send_eof().map_err(fail)?;
            v.drain(&mut on_picture, damage, true)
                .map_err(|e| lift(e, &fail))?;
        }
        let Some(mut a) = self.audio else {
            return Ok(Decoded {
                sound: None,
                damage: self.damage,
            });
        };
        a.decoder.send_eof().map_err(fail)?;
        a.drain(damage, true).map_err(fail)?;
        let format = a
            .format
            .ok_or_else(|| CliError::Media(MediaError::NoSound { path: path.clone() }))?;
        let channels = usize::from(format.channels);
        let audio_start = a.start();
        let mut samples = a.samples;
        let video_start = self.video.as_ref().and_then(VideoPath::start);
        // Line the sound up with the first picture.
        if let (Some(video), Some(audio)) = (video_start, audio_start) {
            let frames = ((audio.0 - video.0) * f64::from(format.sample_rate)).round();
            let count = (frames.abs() as usize).saturating_mul(channels);
            if frames > 0.0 {
                let mut padded = vec![0; count];
                padded.extend(samples);
                samples = padded;
            } else {
                samples.drain(..count.min(samples.len()));
            }
        }
        Ok(Decoded {
            sound: Some(Pcm { format, samples }),
            damage: self.damage,
        })
    }
}

/// Errors out of a picture callback pass through; FFmpeg's are tagged
/// with the file.
enum DrainError {
    Ffmpeg(ff::Error),
    Picture(CliError),
}

fn lift(e: DrainError, fail: &impl Fn(ff::Error) -> CliError) -> CliError {
    match e {
        DrainError::Ffmpeg(e) => fail(e),
        DrainError::Picture(e) => e,
    }
}

/// Sends a packet to a decoder. A packet the decoder rejects, such as one
/// before the first key frame or a damaged one, is counted and skipped,
/// as FFmpeg's own tool does.
fn send(decoder: &mut ff::decoder::Opened, packet: &ff::Packet, damage: &mut Damage) {
    if decoder.send_packet(packet).is_err() {
        damage.decoder_errors += 1;
    }
}

/// The next decoded frame, skipping and counting frames the decoder
/// fails on; `false` when the decoder needs input or has ended.
fn receive(decoder: &mut ff::decoder::Opened, frame: &mut ff::Frame, damage: &mut Damage) -> bool {
    for _ in 0..MAX_ERRORS_IN_A_ROW {
        match decoder.receive_frame(frame) {
            Ok(()) => return true,
            Err(ff::Error::Other {
                errno: ff::util::error::EAGAIN,
            })
            | Err(ff::Error::Eof) => return false,
            Err(_) => damage.decoder_errors += 1,
        }
    }
    false
}

/// A decoder for the stream that takes its packets' time base, so it can
/// move timestamps when it drops encoder priming.
fn decoder(s: &ff::format::stream::Stream) -> Result<ff::decoder::Decoder, ff::Error> {
    let mut c = ff::codec::context::Context::from_parameters(s.parameters())?;
    c.set_threading(ff::codec::threading::Config {
        kind: ff::codec::threading::Type::Frame,
        // As many as the processor has.
        count: 0,
    });
    let mut d = c.decoder();
    d.set_packet_time_base(s.time_base());
    Ok(d)
}

/// The stream's frame rate as av_guess_frame_rate chooses it: the base
/// rate, unless it is implausibly high against the average. Rates whose
/// terms are too large become the closest one with a denominator up to
/// `MAX_RATE_DENOMINATOR`.
fn stream_rate(s: &ff::format::stream::Stream) -> Option<FrameRate> {
    let (base, average) = (s.rate(), s.avg_frame_rate());
    let valid = |r: ff::Rational| r.numerator() > 0 && r.denominator() > 0;
    let candidates = if valid(base)
        && !(valid(average) && f64::from(base) > 210.0 && f64::from(average) < 70.0)
    {
        [base, average]
    } else {
        [average, base]
    };
    let exact = candidates.iter().find_map(|r| {
        let n = u64::try_from(r.numerator()).ok()?;
        let d = u64::try_from(r.denominator()).ok()?;
        FrameRate::new(n, d)
    });
    exact.or_else(|| {
        candidates.iter().find_map(|r| {
            let rate = f64::from(*r);
            if !rate.is_finite() || rate <= 0.0 {
                return None;
            }
            (1..=MAX_RATE_DENOMINATOR)
                .filter_map(|d| {
                    let n = (rate * d as f64).round() as u64;
                    FrameRate::new(n, d).map(|f| ((n as f64 / d as f64 - rate).abs(), f))
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, f)| f)
        })
    })
}

fn video_path(
    path: &Path,
    s: &ff::format::stream::Stream,
    sel: &Selection,
) -> Result<VideoPath, MediaError> {
    let decoder = decoder(s)
        .and_then(|d| d.video())
        .map_err(|source| MediaError::Ffmpeg {
            path: path.to_owned(),
            source,
        })?;
    let frame_rate = match sel.fps {
        Some(f) => f,
        None => stream_rate(s).ok_or(MediaError::FrameRate {
            path: path.to_owned(),
            num: s.avg_frame_rate().numerator(),
            den: s.avg_frame_rate().denominator(),
        })?,
    };
    let size = match sel.scale {
        Some(size) => size,
        None => {
            let (width, height) = (decoder.width(), decoder.height());
            match (u16::try_from(width), u16::try_from(height)) {
                (Ok(w), Ok(h)) if w > 0 && h > 0 => FrameSize {
                    width: w,
                    height: h,
                },
                _ => {
                    return Err(MediaError::Size {
                        path: path.to_owned(),
                        width,
                        height,
                    });
                }
            }
        }
    };
    let tb = s.time_base();
    // Ticks per output frame: den / num seconds in units of the time base.
    let period = (frame_rate.den() as f64 * f64::from(tb.denominator())
        / (frame_rate.num() as f64 * f64::from(tb.numerator())))
    .round()
    .max(1.0) as i64;
    Ok(VideoPath {
        stream: s.index(),
        time_base: tb,
        decoder,
        graph: None,
        info: VideoInfo { size, frame_rate },
        period: Ticks(period),
        first: None,
        last: None,
        step: None,
        times: StreamTimes::new(s),
    })
}

/// The standard rate at or above `rate`, or the highest.
fn sample_rate(rate: u32) -> u32 {
    SAMPLE_RATES
        .into_iter()
        .find(|&r| r >= rate)
        .unwrap_or(SAMPLE_RATES[SAMPLE_RATES.len() - 1])
}

fn audio_path(path: &Path, s: &ff::format::stream::Stream) -> Result<AudioPath, MediaError> {
    let decoder = decoder(s)
        .and_then(|d| d.audio())
        .map_err(|source| MediaError::Ffmpeg {
            path: path.to_owned(),
            source,
        })?;
    Ok(AudioPath {
        stream: s.index(),
        time_base: s.time_base(),
        decoder,
        graph: None,
        format: None,
        start: None,
        times: StreamTimes::new(s),
        samples: Vec::new(),
    })
}

/// A graph from a `buffer`/`abuffer` source named "in" through `spec` to a
/// sink named "out".
fn graph(source: &str, args: &str, sink: &str, spec: &str) -> Result<ff::filter::Graph, ff::Error> {
    let mut g = ff::filter::Graph::new();
    let find = |name| ff::filter::find(name).ok_or(ff::Error::FilterNotFound);
    g.add(&find(source)?, "in", args)?;
    g.add(&find(sink)?, "out", "")?;
    g.output("in", 0)?.input("out", 0)?.parse(spec)?;
    g.validate()?;
    Ok(g)
}

impl VideoPath {
    /// The timestamp of a decoded frame: its own, or one step after the
    /// latest, as the last frame of a stream with B pictures and every
    /// frame of a raw H.264 stream lack one.
    fn timestamp(&mut self, own: Option<i64>, damage: &mut Damage) -> Ticks {
        let step = self.step.unwrap_or(self.period);
        let t = own
            .map(Ticks)
            .unwrap_or_else(|| self.last.map_or(Ticks(0), |l| Ticks(l.0 + step.0)));
        match self.last {
            Some(l) if t > l => self.step = Some(Ticks(t.0 - l.0)),
            Some(l) if t < l => damage.pictures_out_of_order += 1,
            _ => {}
        }
        self.last = Some(self.last.map_or(t, |l| l.max(t)));
        t
    }

    fn drain(
        &mut self,
        on_picture: &mut impl FnMut(Yuv420) -> Result<(), CliError>,
        damage: &mut Damage,
        end: bool,
    ) -> Result<(), DrainError> {
        let mut frame = ff::frame::Video::empty();
        while receive(&mut self.decoder, &mut frame, damage) {
            self.times.frame(frame.pts().is_some());
            let t = self.timestamp(frame.timestamp(), damage);
            let first = *self.first.get_or_insert(t);
            frame.set_pts(Some(t.0 - first.0));
            if self.graph.is_none() {
                let tb = self.time_base;
                let sar = frame.aspect_ratio();
                let args = format!(
                    "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect={}/{}:\
                     colorspace={}:range={}",
                    frame.width(),
                    frame.height(),
                    ff::ffi::AVPixelFormat::from(frame.format()) as i32,
                    tb.numerator(),
                    tb.denominator(),
                    sar.numerator().max(1),
                    sar.denominator().max(1),
                    ff::ffi::AVColorSpace::from(frame.color_space()) as i32,
                    ff::ffi::AVColorRange::from(frame.color_range()) as i32,
                );
                // WMV3 carries no range; decoders take samples as limited
                // range.
                let spec = format!(
                    "fps={},scale={}:{}:out_range=tv,format=yuv420p",
                    self.info.frame_rate, self.info.size.width, self.info.size.height
                );
                self.graph =
                    Some(graph("buffer", &args, "buffersink", &spec).map_err(DrainError::Ffmpeg)?);
            }
            let g = self.graph.as_mut().expect("built above");
            g.get("in")
                .expect("named in")
                .source()
                .add(&frame)
                .map_err(DrainError::Ffmpeg)?;
            self.pull(on_picture)?;
        }
        if end
            && let (Some(g), Some(last), Some(first)) = (self.graph.as_mut(), self.last, self.first)
        {
            // Closed where the last frame ends, so the frame rate
            // conversion covers its duration, as FFmpeg's tool does.
            let step = self.step.unwrap_or(self.period);
            g.get("in")
                .expect("named in")
                .source()
                .close(last.0 + step.0 - first.0)
                .map_err(DrainError::Ffmpeg)?;
            self.pull(on_picture)?;
        }
        Ok(())
    }

    fn pull(
        &mut self,
        on_picture: &mut impl FnMut(Yuv420) -> Result<(), CliError>,
    ) -> Result<(), DrainError> {
        let g = self.graph.as_mut().expect("built before pulling");
        let mut out = ff::frame::Video::empty();
        let mut sink = g.get("out").expect("named out");
        while sink.sink().frame(&mut out).is_ok() {
            on_picture(planar(&out, self.info.size)).map_err(DrainError::Picture)?;
        }
        Ok(())
    }
}

/// The planes of a YUV 4:2:0 frame without their row padding.
fn planar(f: &ff::frame::Video, size: FrameSize) -> Yuv420 {
    let (w, h) = (usize::from(size.width), usize::from(size.height));
    let plane = |i: usize, pw: usize, ph: usize| {
        let (data, stride) = (f.data(i), f.stride(i));
        (0..ph)
            .flat_map(|y| &data[y * stride..y * stride + pw])
            .copied()
            .collect()
    };
    Yuv420 {
        width: size.width,
        height: size.height,
        y: plane(0, w, h),
        cb: plane(1, w.div_ceil(2), h.div_ceil(2)),
        cr: plane(2, w.div_ceil(2), h.div_ceil(2)),
    }
}

impl AudioPath {
    fn drain(&mut self, damage: &mut Damage, end: bool) -> Result<(), ff::Error> {
        let mut frame = ff::frame::Audio::empty();
        while receive(&mut self.decoder, &mut frame, damage) {
            self.times.frame(frame.pts().is_some());
            let ts = frame.timestamp();
            frame.set_pts(ts);
            let mut layout = frame.channel_layout();
            if layout.channels() != i32::from(frame.channels()) || layout.bits() == 0 {
                layout = ff::ChannelLayout::default(i32::from(frame.channels()));
            }
            let input = AudioInput {
                rate: frame.rate(),
                format: frame.format(),
                layout: layout.bits(),
            };
            let format = *self.format.get_or_insert(PcmFormat {
                channels: if frame.channels() == 1 { 1 } else { 2 },
                sample_rate: sample_rate(frame.rate()),
            });
            // The output format stays that of the first frame; another
            // input format, as in joined transport streams, gets a graph
            // of its own after the old one is emptied.
            if self.graph.as_ref().is_some_and(|(i, _)| *i != input) {
                self.flush()?;
                self.graph = None;
            }
            if self.graph.is_none() {
                let tb = self.time_base;
                let args = format!(
                    "time_base={}/{}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
                    tb.numerator(),
                    tb.denominator(),
                    input.rate,
                    input.format.name(),
                    input.layout
                );
                let spec = format!(
                    "aresample={},aformat=sample_fmts=s16:channel_layouts={}",
                    format.sample_rate,
                    if format.channels == 1 {
                        "mono"
                    } else {
                        "stereo"
                    }
                );
                self.graph = Some((input, graph("abuffer", &args, "abuffersink", &spec)?));
            }
            let (_, g) = self.graph.as_mut().expect("built above");
            g.get("in").expect("named in").source().add(&frame)?;
            self.pull();
        }
        if end {
            self.flush()?;
        }
        Ok(())
    }

    /// Ends the graph's input and takes what is left in it.
    fn flush(&mut self) -> Result<(), ff::Error> {
        if let Some((_, g)) = self.graph.as_mut() {
            g.get("in").expect("named in").source().flush()?;
            self.pull();
        }
        Ok(())
    }

    fn pull(&mut self) {
        let (Some((_, g)), Some(format)) = (self.graph.as_mut(), self.format) else {
            return;
        };
        let mut out = ff::frame::Audio::empty();
        let mut sink = g.get("out").expect("named out");
        let channels = usize::from(format.channels);
        while sink.sink().frame(&mut out).is_ok() {
            if self.start.is_none()
                && let Some(p) = out.pts()
            {
                self.start = Some(Ticks(p).seconds(sink.sink().time_base()));
            }
            let bytes = &out.data(0)[..out.samples() * channels * 2];
            self.samples.extend(
                bytes
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]])),
            );
        }
    }
}
