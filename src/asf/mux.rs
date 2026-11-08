//! Writes a video stream and optionally an audio stream into a new ASF
//! file laid out as the Windows Media Format SDK 11 writes them: the same
//! header objects in the same order, fixed-size packets with multiple
//! payloads, video replicated data carrying the pixel aspect ratio and
//! sample duration extensions, and both an Index and a Simple Index.

use super::guid::{self, Guid};
use super::objects::{
    self, Attribute, AttributeValue, Bitrate, CodecEntry, CodecKind, DataOffset,
    ExtendedStreamFlags, ExtendedStreamProperties, IndexSpecifier, IndexType, LeakyBucket,
    PacketNumber, PayloadExtension, SimpleIndexEntry, StreamAttribute, StreamBitrate, WindowMs,
};
use super::{
    Asf, BitmapInfoHeader, Data, ErrorCorrection, FieldWidth, FileProperties, FilePropertiesFlags,
    FourCc, Header, HeaderExtension, HeaderObject, Hns, MediaObjectNumber, Millis, Packet,
    PacketDuration, Payload, PayloadFieldWidths, PayloadKind, Payloads, SendTime, StreamNumber,
    StreamProperties, StreamPropertiesFlags, TopLevelObject, TypeSpecificData, VarField,
    VideoMediaType, WaveFormatEx,
};

/// One coded picture in decoding order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub data: Vec<u8>,
    pub key: bool,
    /// Display time from the start of the stream, preroll not included.
    pub presentation: Millis,
    /// Display duration, carried in the sample duration extension.
    pub duration: Millis,
}

/// The video stream's format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub fourcc: FourCc,
    /// Codec private data; for WMV3 the sequence header.
    pub codec_data: Vec<u8>,
    pub average_time_per_frame: Hns,
    pub codec_name: String,
    pub codec_description: String,
    /// DeviceConformanceTemplate of the stream, such as "MP".
    pub conformance: String,
}

/// One block of an audio codec, which the samples store as one media
/// object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioObject {
    pub data: Vec<u8>,
    /// Time of its first sample in the decoder's output, from the start of
    /// the stream, preroll not included.
    pub presentation: Millis,
}

/// The audio stream's format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFormat {
    /// WAVEFORMATEX fields; `block_align` is the size of every object.
    pub wave: WaveFormatEx,
    /// Duration of one object.
    pub average_time_per_object: Hns,
    pub codec_name: String,
    pub codec_description: String,
    /// DeviceConformanceTemplate of the stream.
    pub conformance: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrack {
    pub format: AudioFormat,
    /// Decoder output before the first sample of the source, which decoders
    /// drop. Every object is presented this much earlier, so the source
    /// starts at time 0.
    pub delay: Millis,
    pub objects: Vec<AudioObject>,
}

/// Size of every data packet in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketSize(u32);

impl PacketSize {
    /// Room for the packet header and one video payload of one byte.
    pub const MIN: u32 = PACKET_HEADER + payload_header(VIDEO_REPLICATED) + 1;
    /// The largest size whose padding and payload lengths fit their 16-bit
    /// fields.
    pub const MAX: u32 = u16::MAX as u32;
    /// The SDK's packet size.
    pub const SDK: PacketSize = PacketSize(16000);

    pub fn new(v: u32) -> Option<Self> {
        (Self::MIN..=Self::MAX)
            .contains(&v)
            .then_some(PacketSize(v))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// FILETIME: 100 ns units since 1601-01-01.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileTime(pub u64);

/// File-level choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxSettings {
    pub packet_size: PacketSize,
    pub preroll: Millis,
    pub file_id: Guid,
    pub creation_date: FileTime,
    /// WMFSDKVersion attribute, written only when given.
    pub sdk_version: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("no video frames to write")]
    NoFrames,
    #[error("neither a video nor an audio stream to write")]
    NoStreams,
    #[error("no video frame is a key frame, so the index has no entries")]
    NoKeyFrame,
    #[error("the audio track has no objects")]
    NoAudioObjects,
    #[error("stream {stream:?} object {object} is empty")]
    EmptyObject { stream: StreamNumber, object: usize },
    #[error("stream {stream:?} object {object} has {bytes} bytes, more than a media object holds")]
    ObjectTooLarge {
        stream: StreamNumber,
        object: usize,
        bytes: usize,
    },
    #[error(
        "stream {stream:?} object {object}: presentation time with preroll is outside 0 to 2^32 - 1 ms"
    )]
    TimeOutOfRange { stream: StreamNumber, object: usize },
    #[error("frame {frame}: duration {duration:?} exceeds 16 bits of ms")]
    DurationOutOfRange { frame: usize, duration: Millis },
    #[error("audio object {object} has {bytes} bytes, not the block alignment {block_align}")]
    AudioObjectSize {
        object: usize,
        bytes: usize,
        block_align: u16,
    },
    #[error("a packet of {packet_size:?} cannot hold an audio object of {block_align} bytes")]
    PacketTooSmallForAudio {
        packet_size: PacketSize,
        block_align: u16,
    },
    #[error("packets {packet} and {next} are {gap_ms} ms apart, more than 16 bits")]
    PacketGap {
        packet: usize,
        next: usize,
        gap_ms: u64,
    },
    #[error("the data exceeds the 4 GiB one index block addresses")]
    DataTooLarge,
    #[error(transparent)]
    Asf(#[from] super::Error),
}

/// The stream numbers the samples give their streams.
pub const AUDIO_STREAM: StreamNumber = match StreamNumber::new(1) {
    Some(n) => n,
    None => panic!("1 is a valid stream number"),
};
pub const VIDEO_STREAM: StreamNumber = match StreamNumber::new(2) {
    Some(n) => n,
    None => panic!("2 is a valid stream number"),
};

/// Seconds-spaced index time base, as the SDK writes it.
const INDEX_INTERVAL: Millis = Millis(1000);

/// Leaky bucket rates of the SDK's ASFLeakyBucketPairs attribute.
const BUCKET_RATES: [Bitrate; 14] = [
    Bitrate(24000),
    Bitrate(30000),
    Bitrate(45000),
    Bitrate(58000),
    Bitrate(115200),
    Bitrate(240000),
    Bitrate(350000),
    Bitrate(500000),
    Bitrate(730000),
    Bitrate(1000000),
    Bitrate(1400000),
    Bitrate(2100000),
    Bitrate(5000000),
    Bitrate(10000000),
];

/// Packet header bytes before the payloads: error correction flags and
/// data, length type flags, property flags, padding length (up to 2), send
/// time, duration and the payload flags byte.
const PACKET_HEADER: u32 = 3 + 1 + 1 + 2 + 4 + 2 + 1;

/// Video replicated data: object size, presentation time, pixel aspect
/// ratio and sample duration.
const VIDEO_REPLICATED: u32 = 12;
/// Audio replicated data: object size and presentation time.
const AUDIO_REPLICATED: u32 = 8;

/// Per payload: stream, object number, offset, replicated data length and
/// data, payload length.
const fn payload_header(replicated: u32) -> u32 {
    1 + 1 + 4 + 1 + replicated + 2
}

/// The payload flags byte counts payloads in six bits.
const MAX_PAYLOADS: usize = 63;

/// Bytes of a Data object before its packets: GUID, size, file id, packet
/// count and reserved field.
const DATA_OBJECT_HEADER: u64 = 16 + 8 + 16 + 8 + 2;

/// Where a media object landed: its first packet and how many packets
/// carry part of it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Placement {
    pub presentation: Millis,
    pub clean_point: bool,
    pub first_packet: PacketNumber,
    pub packets: u16,
}

/// A media object ready for packetizing.
struct Object {
    stream: StreamNumber,
    key: bool,
    number: MediaObjectNumber,
    replicated: Vec<u8>,
    data: Vec<u8>,
    /// Presentation time with preroll, in ms.
    pts: u32,
}

/// `presentation + preroll - delay`.
fn pts_with_preroll(
    presentation: Millis,
    preroll: Millis,
    delay: Millis,
    stream: StreamNumber,
    object: usize,
) -> Result<u32, MuxError> {
    presentation
        .0
        .checked_add(preroll.0)
        .and_then(|v| v.checked_sub(delay.0))
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(MuxError::TimeOutOfRange { stream, object })
}

fn object_size(data: &[u8], stream: StreamNumber, object: usize) -> Result<u32, MuxError> {
    if data.is_empty() {
        return Err(MuxError::EmptyObject { stream, object });
    }
    u32::try_from(data.len()).map_err(|_| MuxError::ObjectTooLarge {
        stream,
        object,
        bytes: data.len(),
    })
}

/// Media object numbers count each stream's objects from 1, modulo 256.
fn object_number(i: usize) -> MediaObjectNumber {
    MediaObjectNumber(((i + 1) & 0xff) as u32)
}

fn video_objects(frames: Vec<VideoFrame>, preroll: Millis) -> Result<Vec<Object>, MuxError> {
    frames
        .into_iter()
        .enumerate()
        .map(|(i, f)| {
            let size = object_size(&f.data, VIDEO_STREAM, i)?;
            let pts = pts_with_preroll(f.presentation, preroll, Millis(0), VIDEO_STREAM, i)?;
            let duration =
                u16::try_from(f.duration.0).map_err(|_| MuxError::DurationOutOfRange {
                    frame: i,
                    duration: f.duration,
                })?;
            let mut r = Vec::with_capacity(VIDEO_REPLICATED as usize);
            r.extend(size.to_le_bytes());
            r.extend(pts.to_le_bytes());
            // Pixel aspect ratio 1:1.
            r.extend([1, 1]);
            r.extend(duration.to_le_bytes());
            Ok(Object {
                stream: VIDEO_STREAM,
                key: f.key,
                number: object_number(i),
                replicated: r,
                data: f.data,
                pts,
            })
        })
        .collect()
}

fn audio_objects(
    track: AudioTrack,
    preroll: Millis,
    packet_size: PacketSize,
) -> Result<Vec<Object>, MuxError> {
    let block_align = track.format.wave.block_align;
    let delay = track.delay;
    if packet_size.get() < PACKET_HEADER + payload_header(AUDIO_REPLICATED) + u32::from(block_align)
    {
        return Err(MuxError::PacketTooSmallForAudio {
            packet_size,
            block_align,
        });
    }
    track
        .objects
        .into_iter()
        .enumerate()
        .map(|(i, o)| {
            let size = object_size(&o.data, AUDIO_STREAM, i)?;
            if o.data.len() != usize::from(block_align) {
                return Err(MuxError::AudioObjectSize {
                    object: i,
                    bytes: o.data.len(),
                    block_align,
                });
            }
            let pts = pts_with_preroll(o.presentation, preroll, delay, AUDIO_STREAM, i)?;
            let mut r = Vec::with_capacity(AUDIO_REPLICATED as usize);
            r.extend(size.to_le_bytes());
            r.extend(pts.to_le_bytes());
            // The samples clear the key frame flag of audio payloads.
            Ok(Object {
                stream: AUDIO_STREAM,
                key: false,
                number: object_number(i),
                replicated: r,
                data: o.data,
                pts,
            })
        })
        .collect()
}

/// The two streams' objects in one sequence by presentation time, audio
/// first on equal times; each stream keeps its own order.
fn interleave(video: Vec<Object>, audio: Vec<Object>) -> Vec<Object> {
    let mut out = Vec::with_capacity(video.len() + audio.len());
    let mut v = video.into_iter().peekable();
    let mut a = audio.into_iter().peekable();
    loop {
        let take_audio = match (a.peek(), v.peek()) {
            (Some(x), Some(y)) => x.pts <= y.pts,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        out.extend(if take_audio { a.next() } else { v.next() });
    }
    out
}

/// A filled packet before its times are known.
struct ClosedPacket {
    payloads: Vec<Payload>,
    padding: u32,
    /// Presentation time of the first payload, with preroll, in ms.
    first_pts: u32,
}

/// Packets being filled.
struct Packer {
    packet_size: u32,
    done: Vec<ClosedPacket>,
    current: Vec<Payload>,
    used: u32,
    first_pts: Option<u32>,
}

impl Packer {
    fn close(&mut self) {
        if let Some(first_pts) = self.first_pts.take() {
            self.done.push(ClosedPacket {
                payloads: std::mem::take(&mut self.current),
                padding: self.packet_size - self.used,
                first_pts,
            });
        }
        self.used = PACKET_HEADER;
    }

    fn next_packet(&self) -> PacketNumber {
        PacketNumber(self.done.len() as u32)
    }

    /// Whether a payload with `header` bytes of header and at least one
    /// byte of data no longer fits.
    fn full(&self, header: u32) -> bool {
        self.packet_size - self.used <= header || self.current.len() == MAX_PAYLOADS
    }
}

/// Each stream's placements of its objects, in its own order.
struct Placements {
    video: Vec<Placement>,
    audio: Vec<Placement>,
}

/// Splits the objects into packets, each object into as many fragments as
/// it takes.
fn packetize(
    objects: Vec<Object>,
    end_pts: u64,
    s: &MuxSettings,
) -> Result<(Vec<Packet>, Placements), MuxError> {
    let mut placements = Placements {
        video: Vec::new(),
        audio: Vec::new(),
    };
    let mut k = Packer {
        packet_size: s.packet_size.get(),
        done: Vec::new(),
        current: Vec::new(),
        used: PACKET_HEADER,
        first_pts: None,
    };
    for o in objects {
        let header = payload_header(o.replicated.len() as u32);
        // The samples never split an audio object.
        let whole = o.stream == AUDIO_STREAM;
        if whole && k.packet_size - k.used < header + o.data.len() as u32 {
            k.close();
        }
        let mut offset = 0usize;
        let mut first_packet = None;
        let mut spanned = 0u16;
        while offset < o.data.len() {
            if k.full(header) {
                k.close();
            }
            first_packet.get_or_insert(k.next_packet());
            spanned += 1;
            let room = (k.packet_size - k.used - header) as usize;
            let take = room.min(o.data.len() - offset);
            k.first_pts.get_or_insert(o.pts);
            k.current.push(Payload {
                stream: o.stream,
                key_frame: o.key,
                media_object_number: o.number,
                kind: PayloadKind::Fragment {
                    offset: offset as u32,
                    replicated_data: o.replicated.clone(),
                },
                data: o.data[offset..offset + take].to_vec(),
            });
            k.used += header + take as u32;
            offset += take;
        }
        let placement = Placement {
            presentation: Millis(o.pts.into()),
            // Every audio object is a clean point for the Index.
            clean_point: o.key || o.stream == AUDIO_STREAM,
            first_packet: first_packet.expect("an object is not empty"),
            packets: spanned,
        };
        if o.stream == AUDIO_STREAM {
            placements.audio.push(placement);
        } else {
            placements.video.push(placement);
        }
    }
    k.close();
    let preroll = s.preroll.0;
    let mut send: Vec<u64> = Vec::with_capacity(k.done.len());
    for p in &k.done {
        let t = u64::from(p.first_pts).saturating_sub(preroll);
        send.push(send.last().map_or(t, |&prev| prev.max(t)));
    }
    let end_send = end_pts - preroll.min(end_pts);
    let mut packets = Vec::with_capacity(k.done.len());
    for (i, p) in k.done.into_iter().enumerate() {
        let next = send.get(i + 1).copied().unwrap_or(end_send.max(send[i]));
        let gap_ms = next - send[i];
        let duration = u16::try_from(gap_ms).map_err(|_| MuxError::PacketGap {
            packet: i,
            next: i + 1,
            gap_ms,
        })?;
        packets.push(Packet {
            error_correction: Some(ErrorCorrection {
                opaque: false,
                data: vec![0, 0],
                length_flags_bit: false,
            }),
            packet_length: VarField::Absent,
            sequence: VarField::Absent,
            // The header reserves two bytes for the padding length; a
            // one-byte field leaves one more byte of padding.
            padding_length: if p.padding + 1 < 256 {
                VarField::Byte((p.padding + 1) as u8)
            } else {
                VarField::Word(p.padding as u16)
            },
            widths: PayloadFieldWidths {
                replicated_data_length: FieldWidth::Byte,
                offset_into_media_object: FieldWidth::Dword,
                media_object_number: FieldWidth::Byte,
            },
            send_time: SendTime(send[i] as u32),
            duration: PacketDuration(duration),
            payloads: Payloads::Multiple {
                payload_length_width: FieldWidth::Word,
                payloads: p.payloads,
            },
        });
    }
    Ok((packets, placements))
}

/// Index entries per `INDEX_INTERVAL` from 0 to `play`: for each, the
/// latest clean point at or before that time (the first one before any),
/// as its first packet and the packets that carry it.
pub(crate) fn index_entries(placements: &[Placement], play: Millis) -> Vec<SimpleIndexEntry> {
    let mut clean: Vec<&Placement> = placements.iter().filter(|p| p.clean_point).collect();
    // Stable: of clean points at the same time, the last one in the file
    // is taken.
    clean.sort_by_key(|p| p.presentation);
    if clean.is_empty() {
        return Vec::new();
    }
    let count = play.0 / INDEX_INTERVAL.0 + 1;
    let mut i = 0;
    (0..count)
        .map(|t| {
            let at = Millis(t * INDEX_INTERVAL.0);
            while clean.get(i + 1).is_some_and(|p| p.presentation <= at) {
                i += 1;
            }
            SimpleIndexEntry {
                packet: clean[i].first_packet,
                count: clean[i].packets,
            }
        })
        .collect()
}

/// When data enters a leaky bucket and how many bits it brings.
struct Arrival {
    send_ms: u64,
    bits: u64,
}

/// Smallest leaky bucket window that holds the arrivals at `rate`.
fn bucket_window(arrivals: &[Arrival], rate: Bitrate) -> WindowMs {
    let mut fullness = 0.0f64;
    let mut peak = 0.0f64;
    let mut last = 0u64;
    for a in arrivals {
        let drained = (a.send_ms - last) as f64 * f64::from(rate.0) / 1000.0;
        fullness = (fullness - drained).max(0.0) + a.bits as f64;
        peak = peak.max(fullness);
        last = a.send_ms;
    }
    WindowMs(
        (peak * 1000.0 / f64::from(rate.0))
            .ceil()
            .min(f64::from(u32::MAX)) as u32,
    )
}

/// Largest number of bits sent in any one second.
fn peak_bitrate(arrivals: &[Arrival]) -> u64 {
    let mut best = 0;
    let mut sum = 0;
    let mut start = 0;
    for end in 0..arrivals.len() {
        sum += arrivals[end].bits;
        while arrivals[end].send_ms - arrivals[start].send_ms >= 1000 {
            sum -= arrivals[start].bits;
            start += 1;
        }
        best = best.max(sum);
    }
    best
}

fn bitrate(bits_per_second: u64) -> Bitrate {
    // Rates of a 32-bit field; a stream of 4 Gbit/s is out of scope.
    Bitrate(u32::try_from(bits_per_second).unwrap_or(u32::MAX))
}

/// Each packet's bits as one stream's arrivals: the stream's share of the
/// packet, header and padding split by payload bytes, so that the streams'
/// rates add up to the file's.
fn stream_arrivals(packets: &[Packet], packet_size: u32, stream: StreamNumber) -> Vec<Arrival> {
    packets
        .iter()
        .filter_map(|p| {
            let Payloads::Multiple { payloads, .. } = &p.payloads else {
                unreachable!("the packetizer writes multiple payloads")
            };
            let bytes = |x: &Payload| {
                let PayloadKind::Fragment {
                    replicated_data, ..
                } = &x.kind
                else {
                    unreachable!("the packetizer writes fragments")
                };
                u64::from(payload_header(replicated_data.len() as u32)) + x.data.len() as u64
            };
            let total: u64 = payloads.iter().map(bytes).sum();
            let mine: u64 = payloads
                .iter()
                .filter(|x| x.stream == stream)
                .map(bytes)
                .sum();
            (mine > 0).then(|| Arrival {
                send_ms: p.send_time.0.into(),
                bits: u64::from(packet_size) * 8 * mine / total,
            })
        })
        .collect()
}

/// One stream's rates.
struct StreamRates {
    average: Bitrate,
    peak: Bitrate,
    arrivals: Vec<Arrival>,
}

impl StreamRates {
    fn new(arrivals: Vec<Arrival>, duration_ms: u64) -> Self {
        let total: u64 = arrivals.iter().map(|a| a.bits).sum();
        let average = bitrate(total * 1000 / duration_ms);
        StreamRates {
            peak: bitrate(peak_bitrate(&arrivals)).max(average),
            average,
            arrivals,
        }
    }

    fn buckets(&self) -> [LeakyBucket; 2] {
        [self.average, self.peak].map(|rate| LeakyBucket {
            bitrate: rate,
            buffer: bucket_window(&self.arrivals, rate),
            initial_fullness: WindowMs(0),
        })
    }
}

fn index_offsets(entries: &[SimpleIndexEntry], packet_size: u32) -> Result<Vec<u32>, MuxError> {
    entries
        .iter()
        .map(|e| {
            e.packet
                .0
                .checked_mul(packet_size)
                .ok_or(MuxError::DataTooLarge)
        })
        .collect()
}

/// The video stream's format and frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrack {
    pub format: VideoFormat,
    pub frames: Vec<VideoFrame>,
}

/// Builds the file from a video stream, an audio stream, or both.
pub fn mux(
    video: Option<VideoTrack>,
    audio: Option<AudioTrack>,
    s: &MuxSettings,
) -> Result<Asf, MuxError> {
    if video.is_none() && audio.is_none() {
        return Err(MuxError::NoStreams);
    }
    let video_end = match &video {
        Some(v) => {
            let end = v
                .frames
                .iter()
                .map(|f| f.presentation.0 + f.duration.0)
                .max()
                .ok_or(MuxError::NoFrames)?;
            if !v.frames.iter().any(|f| f.key) {
                return Err(MuxError::NoKeyFrame);
            }
            end
        }
        None => 0,
    };
    let audio_end = audio.as_ref().map_or(0, |t| {
        let per_object = t.format.average_time_per_object.0.div_ceil(10_000);
        t.objects
            .iter()
            .map(|o| (o.presentation.0 + per_object).saturating_sub(t.delay.0))
            .max()
            .unwrap_or(0)
    });
    if audio.as_ref().is_some_and(|t| t.objects.is_empty()) {
        return Err(MuxError::NoAudioObjects);
    }
    let end = video_end.max(audio_end);
    let max_video_object = video
        .as_ref()
        .and_then(|v| v.frames.iter().map(|f| f.data.len()).max());
    let video_format = video.as_ref().map(|v| v.format.clone());
    let audio_format = audio.as_ref().map(|t| t.format.clone());
    let objects = interleave(
        match video {
            Some(v) => video_objects(v.frames, s.preroll)?,
            None => Vec::new(),
        },
        match audio {
            Some(t) => audio_objects(t, s.preroll, s.packet_size)?,
            None => Vec::new(),
        },
    );
    let (packets, placements) = packetize(objects, end + s.preroll.0, s)?;
    let play = Millis(end + s.preroll.0);
    let packet_size = s.packet_size.get();
    let duration_ms = end.max(INDEX_INTERVAL.0);
    let file_arrivals: Vec<Arrival> = packets
        .iter()
        .map(|p| Arrival {
            send_ms: p.send_time.0.into(),
            bits: u64::from(packet_size) * 8,
        })
        .collect();
    let file_peak = bitrate(peak_bitrate(&file_arrivals));
    let rates_of =
        |stream| StreamRates::new(stream_arrivals(&packets, packet_size, stream), duration_ms);
    let video_rates = video_format.as_ref().map(|_| rates_of(VIDEO_STREAM));
    let audio_rates = audio_format.as_ref().map(|_| rates_of(AUDIO_STREAM));
    let last_send = packets
        .last()
        .map_or(0, |p| u64::from(p.send_time.0) + u64::from(p.duration.0));

    // Index columns in stream number order: audio, then video.
    let video_entries = video_format
        .as_ref()
        .map(|_| index_entries(&placements.video, play));
    let mut spec = Vec::new();
    let mut columns = Vec::new();
    if audio_format.is_some() {
        spec.push(IndexSpecifier {
            stream: AUDIO_STREAM,
            kind: IndexType::NearestPastCleanPoint,
        });
        columns.push(index_offsets(
            &index_entries(&placements.audio, play),
            packet_size,
        )?);
    }
    if let Some(entries) = &video_entries {
        spec.push(IndexSpecifier {
            stream: VIDEO_STREAM,
            kind: IndexType::NearestPastCleanPoint,
        });
        columns.push(index_offsets(entries, packet_size)?);
    }
    // Every column has one entry per interval up to `play`.
    let offsets: Vec<Vec<DataOffset>> = (0..columns[0].len())
        .map(|i| columns.iter().map(|c| DataOffset(c[i])).collect())
        .collect();

    let mut ecd = Vec::new();
    if let Some(v) = &s.sdk_version {
        ecd.push(Attribute {
            name: "WMFSDKVersion".into(),
            value: AttributeValue::String(v.clone()),
        });
        ecd.push(Attribute {
            name: "WMFSDKNeeded".into(),
            value: AttributeValue::String("0.0.0.0000".into()),
        });
    }
    // A leading zero word, then (bitrate, buffer window ms) pairs.
    let mut pairs = vec![0, 0];
    for rate in BUCKET_RATES {
        pairs.extend(rate.0.to_le_bytes());
        pairs.extend(bucket_window(&file_arrivals, rate).0.to_le_bytes());
    }
    let average_sum: u64 = [&video_rates, &audio_rates]
        .iter()
        .filter_map(|r| r.as_ref())
        .map(|r| u64::from(r.average.0))
        .sum();
    ecd.extend([
        Attribute {
            name: "VBR Peak".into(),
            value: AttributeValue::Dword(file_peak.max(bitrate(average_sum)).0),
        },
        Attribute {
            name: "IsVBR".into(),
            value: AttributeValue::Bool(true),
        },
        Attribute {
            name: "ASFLeakyBucketPairs".into(),
            value: AttributeValue::Bytes(pairs),
        },
    ]);

    // The codec list names video before audio; the other objects list
    // the streams in stream number order.
    let mut codecs = Vec::new();
    if let Some(v) = &video_format {
        codecs.push(CodecEntry {
            kind: CodecKind::Video,
            name: v.codec_name.clone(),
            description: v.codec_description.clone(),
            info: v.fourcc.0.to_vec(),
        });
    }
    let mut metadata = Vec::new();
    let mut extended = Vec::new();
    let mut stream_properties = Vec::new();
    let mut rates = Vec::new();
    if let (Some(a), Some(r)) = (&audio_format, &audio_rates) {
        codecs.push(CodecEntry {
            kind: CodecKind::Audio,
            name: a.codec_name.clone(),
            description: a.codec_description.clone(),
            info: a.wave.format_tag.to_le_bytes().to_vec(),
        });
        metadata.extend([
            StreamAttribute {
                stream: AUDIO_STREAM,
                name: "IsVBR".into(),
                value: AttributeValue::Bool(false),
            },
            StreamAttribute {
                stream: AUDIO_STREAM,
                name: "DeviceConformanceTemplate".into(),
                value: AttributeValue::String(a.conformance.clone()),
            },
        ]);
        let [bucket, alternate_bucket] = r.buckets();
        extended.push(ExtendedStreamProperties {
            start_time: Hns(0),
            end_time: Hns(0),
            bucket,
            alternate_bucket,
            maximum_object_size: a.wave.block_align.into(),
            flags: ExtendedStreamFlags::SEEKABLE,
            stream: AUDIO_STREAM,
            language_index: 0,
            average_time_per_frame: a.average_time_per_object,
            payload_extensions: Vec::new(),
        });
        let block_align = a.wave.block_align.to_le_bytes();
        let mut spread = vec![1];
        // Span 1; virtual packet and chunk lengths of one block; one byte
        // of silence.
        spread.extend(block_align);
        spread.extend(block_align);
        spread.extend([1, 0, 0]);
        stream_properties.push(HeaderObject::StreamProperties(StreamProperties {
            stream_type: guid::AUDIO_MEDIA,
            error_correction_type: guid::AUDIO_SPREAD,
            time_offset: Hns(0),
            flags: StreamPropertiesFlags(AUDIO_STREAM.get().into()),
            reserved: 0,
            type_specific: TypeSpecificData::Audio(a.wave.clone()),
            error_correction_data: spread,
        }));
        rates.push(StreamBitrate {
            stream: AUDIO_STREAM,
            average: r.average,
        });
    }
    if let (Some(video), Some(video_rates), Some(max_video_object)) =
        (&video_format, &video_rates, max_video_object)
    {
        metadata.extend([
            StreamAttribute {
                stream: VIDEO_STREAM,
                name: "IsVBR".into(),
                value: AttributeValue::Bool(true),
            },
            StreamAttribute {
                stream: VIDEO_STREAM,
                name: "DeviceConformanceTemplate".into(),
                value: AttributeValue::String(video.conformance.clone()),
            },
        ]);
        let [bucket, alternate_bucket] = video_rates.buckets();
        extended.push(ExtendedStreamProperties {
            start_time: Hns(0),
            end_time: Hns(0),
            bucket,
            alternate_bucket,
            maximum_object_size: u32::try_from(max_video_object)
                .expect("video_objects checked every frame size"),
            flags: ExtendedStreamFlags::SEEKABLE,
            stream: VIDEO_STREAM,
            language_index: 0,
            average_time_per_frame: video.average_time_per_frame,
            payload_extensions: vec![
                PayloadExtension {
                    system: guid::PAYLOAD_EXT_PIXEL_ASPECT_RATIO,
                    size: 2,
                },
                PayloadExtension {
                    system: guid::PAYLOAD_EXT_SAMPLE_DURATION,
                    size: 2,
                },
            ],
        });
        stream_properties.push(HeaderObject::StreamProperties(StreamProperties {
            stream_type: guid::VIDEO_MEDIA,
            error_correction_type: guid::NO_ERROR_CORRECTION,
            time_offset: Hns(0),
            flags: StreamPropertiesFlags(VIDEO_STREAM.get().into()),
            reserved: 0,
            type_specific: TypeSpecificData::Video(VideoMediaType {
                encoded_width: video.width,
                encoded_height: video.height,
                // Fixed by the specification.
                reserved_flags: 2,
                bitmap: BitmapInfoHeader {
                    size: 40 + video.codec_data.len() as u32,
                    width: video.width as i32,
                    height: video.height as i32,
                    planes: 1,
                    bit_count: 24,
                    compression: video.fourcc,
                    size_image: 0,
                    x_pels_per_meter: 0,
                    y_pels_per_meter: 0,
                    clr_used: 0,
                    clr_important: 0,
                },
                codec_data: video.codec_data.clone(),
            }),
            error_correction_data: Vec::new(),
        }));
        rates.push(StreamBitrate {
            stream: VIDEO_STREAM,
            average: video_rates.average,
        });
    }

    let mut extension = vec![
        HeaderObject::Raw(objects::language_list(&["en-us"])),
        HeaderObject::Raw(objects::compatibility()),
        HeaderObject::Raw(objects::metadata(&metadata)),
    ];
    extension.extend(
        extended
            .iter()
            .map(|e| HeaderObject::Raw(objects::extended_stream_properties(e))),
    );
    extension.push(HeaderObject::Raw(objects::index_parameters(
        INDEX_INTERVAL.0 as u32,
        &spec,
    )));
    let mut header_objects = vec![
        HeaderObject::Raw(objects::codec_list(&codecs)),
        HeaderObject::FileProperties(FileProperties {
            file_id: s.file_id,
            // Set below, once the size is known.
            file_size: 0,
            creation_date: s.creation_date.0,
            data_packets_count: packets.len() as u64,
            play_duration: Hns(play.0 * 10_000),
            send_duration: Hns(last_send * 10_000),
            preroll: s.preroll,
            // Seekable.
            flags: FilePropertiesFlags(2),
            min_data_packet_size: packet_size,
            max_data_packet_size: packet_size,
            // The sum of the streams' bitrates.
            max_bitrate: bitrate(average_sum).0,
        }),
        HeaderObject::HeaderExtension(HeaderExtension {
            reserved_field1: guid::RESERVED_1,
            // Fixed by the specification.
            reserved_field2: 6,
            objects: extension,
        }),
        HeaderObject::Raw(objects::extended_content_description(&ecd)),
    ];
    header_objects.extend(stream_properties);
    header_objects.push(HeaderObject::Raw(objects::stream_bitrate_properties(
        &rates,
    )));
    let header = Header {
        reserved1: 1,
        reserved2: 2,
        objects: header_objects,
    };
    let mut indexes = vec![TopLevelObject::Raw(objects::index(
        INDEX_INTERVAL.0 as u32,
        &spec,
        &offsets,
    ))];
    // The Simple Index covers the video stream only.
    if let Some(entries) = &video_entries {
        indexes.push(TopLevelObject::Raw(objects::simple_index(
            s.file_id,
            Hns(INDEX_INTERVAL.0 * 10_000),
            entries,
        )));
    }
    // Every packet has the same size, so the file size follows from the
    // header and indexes alone.
    let without_data = Asf {
        header: header.clone(),
        objects: indexes.clone(),
    }
    .to_bytes()?
    .len() as u64;
    let file_size =
        without_data + DATA_OBJECT_HEADER + packets.len() as u64 * u64::from(packet_size);
    let mut asf = Asf {
        header,
        objects: std::iter::once(TopLevelObject::Data(Data {
            file_id: s.file_id,
            // Fixed by the specification.
            reserved: 0x0101,
            packets,
        }))
        .chain(indexes)
        .collect(),
    };
    for o in &mut asf.header.objects {
        if let HeaderObject::FileProperties(fp) = o {
            fp.file_size = file_size;
        }
    }
    Ok(asf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asf::TypeSpecificData;

    fn samples() -> Vec<std::path::PathBuf> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let mut out = Vec::new();
        let mut dirs = vec![root];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().is_some_and(|x| x == "wmv") {
                    out.push(p);
                }
            }
        }
        out.sort();
        out
    }

    fn raw(asf: &Asf, g: Guid) -> Option<&[u8]> {
        asf.objects.iter().find_map(|o| match o {
            TopLevelObject::Raw(r) if r.guid == g => Some(&r.body[..]),
            _ => None,
        })
    }

    fn u32_at(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }

    /// "WMFSDKVersion" in UTF-16LE without its NUL.
    const WMFSDK_VERSION_UTF16: &[u8] = b"W\0M\0F\0S\0D\0K\0V\0e\0r\0s\0i\0o\0n\0";

    fn from_sdk(asf: &Asf) -> bool {
        asf.header.objects.iter().any(|o| {
            matches!(o, HeaderObject::Raw(r)
                if r.guid == guid::EXTENDED_CONTENT_DESCRIPTION
                    && r.body.windows(26).any(|w| w == WMFSDK_VERSION_UTF16))
        })
    }

    /// A stream's placements as a file's packets give them; with
    /// `all_clean` every object counts as a clean point.
    fn placements(asf: &Asf, stream: StreamNumber, all_clean: bool) -> Vec<Placement> {
        let mut out: Vec<Placement> = Vec::new();
        for (i, p) in asf.data().expect("data object").packets.iter().enumerate() {
            let payloads = match &p.payloads {
                Payloads::Single(x) => std::slice::from_ref(x),
                Payloads::Multiple { payloads, .. } => &payloads[..],
            };
            for x in payloads.iter().filter(|x| x.stream == stream) {
                let PayloadKind::Fragment {
                    offset,
                    replicated_data,
                } = &x.kind
                else {
                    panic!("the files carry fragments");
                };
                if *offset == 0 {
                    out.push(Placement {
                        presentation: Millis(u32_at(replicated_data, 4).into()),
                        clean_point: all_clean || x.key_frame,
                        first_packet: PacketNumber(i as u32),
                        packets: 1,
                    });
                } else {
                    out.last_mut().expect("continuation").packets += 1;
                }
            }
        }
        out
    }

    /// Entries of a Simple Index object body.
    fn simple_entries(b: &[u8]) -> Vec<SimpleIndexEntry> {
        (0..u32_at(b, 28) as usize)
            .map(|i| {
                let o = 32 + i * 6;
                SimpleIndexEntry {
                    packet: PacketNumber(u32_at(b, o)),
                    count: u16::from_le_bytes(b[o + 4..o + 6].try_into().unwrap()),
                }
            })
            .collect()
    }

    /// The offsets of specifier `s` in an Index object body with one block.
    fn index_column(b: &[u8], s: usize) -> Vec<u32> {
        let specs = u16::from_le_bytes(b[4..6].try_into().unwrap()) as usize;
        let o = 10 + specs * 4;
        let n = u32_at(b, o) as usize;
        let base = o + 4 + 8 * specs;
        (0..n)
            .map(|i| u32_at(b, base + (i * specs + s) * 4))
            .collect()
    }

    fn video_stream(asf: &Asf) -> (usize, StreamNumber) {
        asf.stream_properties()
            .enumerate()
            .find(|(_, s)| matches!(s.type_specific, TypeSpecificData::Video(_)))
            .map(|(i, s)| (i, s.flags.stream_number()))
            .expect("a video stream")
    }

    /// Every SDK sample's Simple Index and the video column of its Index
    /// follow from where its key frames lie.
    #[test]
    fn index_rule_reproduces_sample_indexes() {
        let files = samples();
        if files.is_empty() {
            eprintln!("no samples under testdata/; skipping");
            return;
        }
        let mut checked = 0;
        for path in files {
            let asf = Asf::parse(&std::fs::read(&path).unwrap()).unwrap();
            if !from_sdk(&asf) {
                continue;
            }
            let simple = raw(&asf, guid::SIMPLE_INDEX).expect("SDK files have a Simple Index");
            let index = raw(&asf, guid::INDEX).expect("SDK files have an Index");
            let (column, video) = video_stream(&asf);
            let fp = asf.file_properties().unwrap();
            let play = Millis(fp.play_duration.0 / 10_000);
            let entries = index_entries(&placements(&asf, video, false), play);
            let expected = simple_entries(simple);
            // The SDK writes one entry more on some files; the rule for its
            // count is not known. Every entry it shares with this rule must
            // agree.
            let shared = entries.len().min(expected.len());
            assert!(
                expected.len() - shared <= 1 && entries.len() == shared,
                "{}: {} entries against {}",
                path.display(),
                entries.len(),
                expected.len()
            );
            assert_eq!(entries[..shared], expected[..shared], "{}", path.display());
            let offsets: Vec<u32> = expected
                .iter()
                .map(|e| e.packet.0 * fp.min_data_packet_size)
                .collect();
            assert_eq!(index_column(index, column), offsets, "{}", path.display());
            checked += 1;
        }
        assert!(checked > 0, "no SDK sample among the files");
    }

    fn frames(sizes: &[usize]) -> Vec<VideoFrame> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| VideoFrame {
                data: (0..n).map(|b| (b + i) as u8).collect(),
                key: i == 0 || i == 7,
                presentation: Millis(i as u64 * 40),
                duration: Millis(40),
            })
            .collect()
    }

    fn format() -> VideoFormat {
        VideoFormat {
            width: 16,
            height: 16,
            fourcc: FourCc(*b"WMV3"),
            codec_data: vec![0x4e, 0x59, 0x4a, 0x11, 0x00],
            average_time_per_frame: Hns(400_000),
            codec_name: "VC-1".into(),
            codec_description: "VC-1 Simple/Main Profile".into(),
            conformance: "MP".into(),
        }
    }

    fn settings(packet_size: u32) -> MuxSettings {
        MuxSettings {
            packet_size: PacketSize::new(packet_size).unwrap(),
            preroll: Millis(3000),
            file_id: Guid([3; 16]),
            creation_date: FileTime(0),
            sdk_version: None,
        }
    }

    /// Our own files' indexes agree with their packets, which checks the
    /// placements the packetizer records, including frames that exactly
    /// fill a packet, frames spanning many packets and 63-payload packets.
    #[test]
    fn muxed_indexes_agree_with_the_packets() {
        let room = 200 - PACKET_HEADER - payload_header(VIDEO_REPLICATED);
        // Frames 0 and 7 are key frames; 7 spans many packets.
        let mut sizes = vec![room as usize, 1, 2, 3, 4, 5, 6, 2000, 5];
        sizes.extend(std::iter::repeat_n(1, 150));
        sizes.extend([room as usize - 1, 3, 700]);
        let asf = mux(
            Some(VideoTrack {
                format: format(),
                frames: frames(&sizes),
            }),
            None,
            &settings(200),
        )
        .unwrap();
        let bytes = asf.to_bytes().unwrap();
        let parsed = Asf::parse(&bytes).unwrap();
        let fp = parsed.file_properties().unwrap();
        assert_eq!(fp.file_size, bytes.len() as u64);
        let play = Millis(fp.play_duration.0 / 10_000);
        let entries = index_entries(&placements(&parsed, VIDEO_STREAM, false), play);
        assert_eq!(
            simple_entries(raw(&parsed, guid::SIMPLE_INDEX).unwrap()),
            entries
        );
        let offsets: Vec<u32> = entries.iter().map(|e| e.packet.0 * 200).collect();
        assert_eq!(index_column(raw(&parsed, guid::INDEX).unwrap(), 0), offsets);
        assert!(entries.iter().any(|e| e.count > 1));
        let objects = parsed.media_objects().unwrap();
        assert_eq!(objects.len(), sizes.len());
    }

    /// An audio track of `count` objects of `block_align` bytes, one per
    /// 46 ms, with a decoder delay of two objects.
    fn audio(count: usize, block_align: u16) -> AudioTrack {
        AudioTrack {
            format: AudioFormat {
                wave: WaveFormatEx {
                    format_tag: 0x161,
                    channels: 2,
                    samples_per_sec: 44100,
                    avg_bytes_per_sec: 8000,
                    block_align,
                    bits_per_sample: 16,
                    extra: vec![0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
                },
                average_time_per_object: Hns(464_399),
                codec_name: "Windows Media Audio 9.2".into(),
                codec_description: "64 kbps, 44 kHz, stereo 1-pass CBR".into(),
                conformance: "L1".into(),
            },
            delay: Millis(93),
            objects: (0..count)
                .map(|i| AudioObject {
                    data: vec![i as u8; usize::from(block_align)],
                    presentation: Millis((i as u64 * 2048 * 1000 + 22050) / 44100),
                })
                .collect(),
        }
    }

    /// With audio, both index columns follow the packets, every audio
    /// object is one payload-carried media object with the samples'
    /// replicated data and a clear key frame flag, and the objects come in
    /// presentation time order.
    #[test]
    fn audio_objects_interleave_and_index() {
        let mut sizes = vec![3000, 40, 50, 60, 70, 80, 90, 2500];
        sizes.extend(std::iter::repeat_n(30, 40));
        let video_frames = frames(&sizes);
        let track = audio(45, 371);
        let asf = mux(
            Some(VideoTrack {
                format: format(),
                frames: video_frames,
            }),
            Some(track.clone()),
            &settings(1000),
        )
        .unwrap();
        let bytes = asf.to_bytes().unwrap();
        let parsed = Asf::parse(&bytes).unwrap();
        assert_eq!(parsed.to_bytes().unwrap(), bytes);
        let fp = parsed.file_properties().unwrap();
        assert_eq!(fp.file_size, bytes.len() as u64);
        let play = Millis(fp.play_duration.0 / 10_000);
        let index = raw(&parsed, guid::INDEX).unwrap();
        for (column, stream) in [(0, AUDIO_STREAM), (1, VIDEO_STREAM)] {
            let entries = index_entries(&placements(&parsed, stream, stream == AUDIO_STREAM), play);
            let offsets: Vec<u32> = entries.iter().map(|e| e.packet.0 * 1000).collect();
            assert_eq!(index_column(index, column), offsets, "{stream:?}");
        }
        assert_eq!(
            simple_entries(raw(&parsed, guid::SIMPLE_INDEX).unwrap()),
            index_entries(&placements(&parsed, VIDEO_STREAM, false), play)
        );

        let mut last_pts = 0;
        let mut audio_seen = Vec::new();
        for p in &parsed.data().unwrap().packets {
            let Payloads::Multiple { payloads, .. } = &p.payloads else {
                panic!("multiple payloads");
            };
            for x in payloads {
                let PayloadKind::Fragment {
                    offset,
                    replicated_data,
                } = &x.kind
                else {
                    panic!("fragments");
                };
                let pts = u32_at(replicated_data, 4);
                if *offset == 0 {
                    assert!(pts >= last_pts, "objects out of time order");
                    last_pts = pts;
                }
                if x.stream == AUDIO_STREAM {
                    assert_eq!(*offset, 0, "an audio object fits one payload");
                    assert!(!x.key_frame);
                    assert_eq!(replicated_data.len(), 8);
                    assert_eq!(u32_at(replicated_data, 0), 371);
                    audio_seen.push((pts, x.data.clone()));
                }
            }
        }
        let expected: Vec<(u32, Vec<u8>)> = track
            .objects
            .iter()
            .map(|o| (o.presentation.0 as u32 + 3000 - 93, o.data.clone()))
            .collect();
        assert_eq!(audio_seen, expected);
    }

    /// An audio track alone makes a file of one stream, with an Index of
    /// the audio column and no Simple Index, which covers video only.
    #[test]
    fn audio_alone() {
        let track = audio(45, 371);
        let asf = mux(None, Some(track), &settings(1000)).unwrap();
        let bytes = asf.to_bytes().unwrap();
        let parsed = Asf::parse(&bytes).unwrap();
        assert_eq!(parsed.to_bytes().unwrap(), bytes);
        let streams: Vec<&TypeSpecificData> = parsed
            .header
            .objects
            .iter()
            .filter_map(|o| match o {
                HeaderObject::StreamProperties(sp) => Some(&sp.type_specific),
                _ => None,
            })
            .collect();
        assert!(matches!(streams[..], [TypeSpecificData::Audio(_)]));
        assert_eq!(raw(&parsed, guid::SIMPLE_INDEX), None);
        let fp = parsed.file_properties().unwrap();
        let play = Millis(fp.play_duration.0 / 10_000);
        let entries = index_entries(&placements(&parsed, AUDIO_STREAM, true), play);
        let offsets: Vec<u32> = entries.iter().map(|e| e.packet.0 * 1000).collect();
        let index = raw(&parsed, guid::INDEX).unwrap();
        assert_eq!(
            u16::from_le_bytes([index[4], index[5]]),
            1,
            "one index column"
        );
        assert_eq!(index_column(index, 0), offsets);
        assert_eq!(parsed.media_objects().unwrap().len(), 45);
    }

    #[test]
    fn a_file_needs_a_stream() {
        assert!(matches!(
            mux(None, None, &settings(1000)),
            Err(MuxError::NoStreams)
        ));
    }

    #[test]
    fn video_needs_a_key_frame() {
        let mut video_frames = frames(&[10, 20]);
        video_frames[0].key = false;
        assert!(matches!(
            mux(
                Some(VideoTrack {
                    format: format(),
                    frames: video_frames
                }),
                Some(audio(3, 371)),
                &settings(16000)
            ),
            Err(MuxError::NoKeyFrame)
        ));
    }

    #[test]
    fn audio_objects_must_be_one_block() {
        let mut track = audio(3, 371);
        track.objects[1].data.pop();
        assert!(matches!(
            mux(
                Some(VideoTrack {
                    format: format(),
                    frames: frames(&[10])
                }),
                Some(track),
                &settings(16000)
            ),
            Err(MuxError::AudioObjectSize {
                object: 1,
                bytes: 370,
                block_align: 371
            })
        ));
        assert!(matches!(
            mux(
                Some(VideoTrack {
                    format: format(),
                    frames: frames(&[10])
                }),
                Some(audio(0, 371)),
                &settings(16000)
            ),
            Err(MuxError::NoAudioObjects)
        ));
        assert!(matches!(
            mux(
                Some(VideoTrack {
                    format: format(),
                    frames: frames(&[10])
                }),
                Some(audio(3, 371)),
                &settings(400)
            ),
            Err(MuxError::PacketTooSmallForAudio {
                block_align: 371,
                ..
            })
        ));
    }

    /// Our header objects come in the SDK's order.
    #[test]
    fn header_order_follows_the_sdk() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/ads/hvideo15_1.wmv");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("{} missing; skipping", path.display());
            return;
        };
        let kinds = |asf: &Asf| -> Vec<String> {
            let mut out = Vec::new();
            for o in &asf.header.objects {
                match o {
                    HeaderObject::FileProperties(_) => out.push("FileProperties".to_string()),
                    HeaderObject::StreamProperties(s) => {
                        out.push(format!("StreamProperties {:?}", s.flags.stream_number()))
                    }
                    HeaderObject::HeaderExtension(h) => {
                        out.push("HeaderExtension".into());
                        for e in &h.objects {
                            if let HeaderObject::Raw(r) = e
                                && r.guid != guid::PADDING
                            {
                                out.push(format!("  {:?}", r.guid));
                            }
                        }
                    }
                    HeaderObject::Raw(r) => out.push(format!("{:?}", r.guid)),
                }
            }
            out.dedup();
            out
        };
        let sample = Asf::parse(&bytes).unwrap();
        let ours = mux(
            Some(VideoTrack {
                format: format(),
                frames: frames(&[100, 50]),
            }),
            Some(audio(2, 371)),
            &settings(16000),
        )
        .unwrap();
        assert_eq!(kinds(&ours), kinds(&sample));
    }
}
