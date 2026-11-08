//! Advanced Systems Format container, modelled closely enough that writing a
//! parsed file reproduces it byte for byte. Every encoded field width,
//! reserved field and flag that the format leaves to the muxer is kept.
//!
//! Inputs that cannot be reproduced exactly from this model are rejected
//! rather than normalized: variable packet sizes, broadcast files whose
//! data packet count is not filled in, and bare 16-byte WAVEFORMAT blocks.

pub mod guid;
mod media;
pub mod mux;
pub mod objects;
mod read;
mod write;

pub use guid::Guid;
pub use media::{MediaObject, MediaObjectError};

use crate::bytes::Truncated;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asf {
    pub header: Header,
    /// Top-level objects after the header, in file order.
    pub objects: Vec<TopLevelObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopLevelObject {
    Data(Data),
    Raw(RawObject),
}

/// An object kept as opaque bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawObject {
    pub guid: Guid,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub reserved1: u8,
    pub reserved2: u8,
    pub objects: Vec<HeaderObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderObject {
    FileProperties(FileProperties),
    StreamProperties(StreamProperties),
    HeaderExtension(HeaderExtension),
    Raw(RawObject),
}

/// Time in 100 ns units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hns(pub u64);

/// Time in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Millis(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilePropertiesFlags(pub u32);

impl FilePropertiesFlags {
    pub fn broadcast(self) -> bool {
        self.0 & 1 != 0
    }
    pub fn seekable(self) -> bool {
        self.0 & 2 != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProperties {
    pub file_id: Guid,
    pub file_size: u64,
    /// FILETIME: 100 ns units since 1601-01-01.
    pub creation_date: u64,
    pub data_packets_count: u64,
    pub play_duration: Hns,
    pub send_duration: Hns,
    pub preroll: Millis,
    pub flags: FilePropertiesFlags,
    pub min_data_packet_size: u32,
    pub max_data_packet_size: u32,
    pub max_bitrate: u32,
}

/// 7-bit stream number. Valid streams are 1 to 127.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamNumber(u8);

impl StreamNumber {
    pub const fn new(n: u8) -> Option<Self> {
        if n <= 0x7f {
            Some(StreamNumber(n))
        } else {
            None
        }
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPropertiesFlags(pub u16);

impl StreamPropertiesFlags {
    pub fn stream_number(self) -> StreamNumber {
        StreamNumber((self.0 & 0x7f) as u8)
    }
    pub fn encrypted(self) -> bool {
        self.0 & 0x8000 != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamProperties {
    pub stream_type: Guid,
    pub error_correction_type: Guid,
    pub time_offset: Hns,
    pub flags: StreamPropertiesFlags,
    pub reserved: u32,
    pub type_specific: TypeSpecificData,
    pub error_correction_data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeSpecificData {
    Audio(WaveFormatEx),
    Video(VideoMediaType),
    Raw(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveFormatEx {
    pub format_tag: u16,
    pub channels: u16,
    pub samples_per_sec: u32,
    pub avg_bytes_per_sec: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,
    /// Codec specific data; its length is the cbSize field.
    pub extra: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoMediaType {
    pub encoded_width: u32,
    pub encoded_height: u32,
    pub reserved_flags: u8,
    pub bitmap: BitmapInfoHeader,
    /// Codec private data following the BITMAPINFOHEADER. For WMV3 this
    /// holds the sequence header.
    pub codec_data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapInfoHeader {
    /// biSize as stored; usually 40 plus the codec data length.
    pub size: u32,
    pub width: i32,
    pub height: i32,
    pub planes: u16,
    pub bit_count: u16,
    pub compression: FourCc,
    pub size_image: u32,
    pub x_pels_per_meter: i32,
    pub y_pels_per_meter: i32,
    pub clr_used: u32,
    pub clr_important: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCc(pub [u8; 4]);

impl std::fmt::Debug for FourCc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.iter().all(|b| b.is_ascii_graphic()) {
            write!(f, "FourCc({:?})", std::str::from_utf8(&self.0).unwrap())
        } else {
            write!(f, "FourCc({:02x?})", self.0)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderExtension {
    pub reserved_field1: Guid,
    pub reserved_field2: u16,
    pub objects: Vec<HeaderObject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Data {
    pub file_id: Guid,
    pub reserved: u16,
    pub packets: Vec<Packet>,
}

/// A variable-width packet field whose width is chosen by the muxer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarField {
    Absent,
    Byte(u8),
    Word(u16),
    Dword(u32),
}

impl VarField {
    pub fn value(self) -> Option<u32> {
        match self {
            VarField::Absent => None,
            VarField::Byte(v) => Some(v.into()),
            VarField::Word(v) => Some(v.into()),
            VarField::Dword(v) => Some(v),
        }
    }

    pub fn width(self) -> FieldWidth {
        match self {
            VarField::Absent => FieldWidth::Absent,
            VarField::Byte(_) => FieldWidth::Byte,
            VarField::Word(_) => FieldWidth::Word,
            VarField::Dword(_) => FieldWidth::Dword,
        }
    }
}

/// Encoded width of a variable-width packet field (2-bit length type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldWidth {
    Absent,
    Byte,
    Word,
    Dword,
}

impl FieldWidth {
    pub fn from_bits(bits: u8) -> Self {
        match bits & 3 {
            0 => FieldWidth::Absent,
            1 => FieldWidth::Byte,
            2 => FieldWidth::Word,
            _ => FieldWidth::Dword,
        }
    }

    pub fn bits(self) -> u8 {
        match self {
            FieldWidth::Absent => 0,
            FieldWidth::Byte => 1,
            FieldWidth::Word => 2,
            FieldWidth::Dword => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorCorrection {
    pub opaque: bool,
    /// At most 15 bytes.
    pub data: Vec<u8>,
    /// Bit 7 of the length type flags byte, which the specification names
    /// Error Correction Present. It can only be set when error correction
    /// data precedes it, since a leading byte with bit 7 set is read as the
    /// error correction flags.
    pub length_flags_bit: bool,
}

/// Widths declared by the packet's property flags byte. The stream number
/// width is fixed at one byte by the specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadFieldWidths {
    pub replicated_data_length: FieldWidth,
    pub offset_into_media_object: FieldWidth,
    pub media_object_number: FieldWidth,
}

/// Packet send time in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SendTime(pub u32);

/// Packet duration in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketDuration(pub u16);

/// Media object number as stored; wraps at the width of its field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MediaObjectNumber(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub error_correction: Option<ErrorCorrection>,
    pub packet_length: VarField,
    pub sequence: VarField,
    pub padding_length: VarField,
    pub widths: PayloadFieldWidths,
    pub send_time: SendTime,
    pub duration: PacketDuration,
    pub payloads: Payloads,
}

impl Packet {
    /// An absent padding length field means the packet has no padding.
    pub fn padding_len(&self) -> u32 {
        self.padding_length.value().unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payloads {
    Single(Payload),
    Multiple {
        payload_length_width: FieldWidth,
        payloads: Vec<Payload>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub stream: StreamNumber,
    pub key_frame: bool,
    pub media_object_number: MediaObjectNumber,
    pub kind: PayloadKind,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadKind {
    /// Part of one media object starting at byte `offset`. Replicated data
    /// is never exactly one byte long, which would mark a compressed payload.
    Fragment {
        offset: u32,
        replicated_data: Vec<u8>,
    },
    /// The payload data is a list of byte-length-prefixed media objects.
    Compressed {
        presentation_time: u32,
        time_delta: u8,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Truncated(#[from] Truncated),
    #[error("object {guid} at {offset:#x} has invalid size {size}")]
    ObjectSize {
        guid: Guid,
        offset: usize,
        size: u64,
    },
    #[error("file does not start with a header object (found {found})")]
    MissingHeader { found: Guid },
    #[error("header declares {declared} objects but contains {found}")]
    HeaderObjectCount { declared: u32, found: usize },
    #[error("header has no file properties object")]
    MissingFileProperties,
    #[error("variable packet size (min {min}, max {max}) is not supported")]
    VariablePacketSize { min: u32, max: u32 },
    #[error("{what} at {offset:#x}: {len} unparsed trailing bytes")]
    TrailingBytes {
        what: &'static str,
        offset: usize,
        len: usize,
    },
    #[error("format data at {offset:#x} is {len} bytes, shorter than BITMAPINFOHEADER")]
    FormatTooShort { offset: usize, len: u16 },
    #[error("{what} at {offset:#x}: declared {declared} bytes, {available} available")]
    LengthMismatch {
        what: &'static str,
        offset: usize,
        declared: u64,
        available: u64,
    },
    #[error("data object declares {declared} packets but contains {found}")]
    PacketCount { declared: u64, found: u64 },
    #[error("packet {packet}: {reason}")]
    Packet { packet: usize, reason: PacketError },
    #[error("{field} value {value} does not fit width {width:?}")]
    FieldOverflow {
        field: &'static str,
        value: u64,
        width: FieldWidth,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PacketError {
    #[error(transparent)]
    Truncated(#[from] Truncated),
    #[error("error correction length type {0} is reserved")]
    ErrorCorrectionLengthType(u8),
    #[error("{len} bytes of error correction data exceed the 4-bit length field")]
    ErrorCorrectionTooLong { len: usize },
    #[error("stream number length type {0} is not one byte")]
    StreamNumberWidth(u8),
    #[error("declared packet length {declared} differs from packet size {size}")]
    PacketLength { declared: u32, size: u32 },
    #[error("contents use {used} bytes plus {padding} padding of {size}")]
    SizeMismatch {
        used: usize,
        padding: u32,
        size: u32,
    },
    #[error("padding contains non-zero bytes")]
    NonZeroPadding,
    #[error("multiple payloads with an absent payload length width")]
    MissingPayloadLength,
    #[error("more than 63 payloads")]
    TooManyPayloads,
    #[error("fragment replicated data of one byte would read as a compressed payload")]
    AmbiguousReplicatedData,
    #[error("compressed payload needs a one-byte replicated data length field")]
    CompressedWithoutReplicatedLength,
    #[error("{field} value {value} does not fit width {width:?}")]
    FieldOverflow {
        field: &'static str,
        value: u64,
        width: FieldWidth,
    },
}

impl Asf {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        read::parse(bytes)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        write::write(self)
    }

    pub fn file_properties(&self) -> Option<&FileProperties> {
        self.header.objects.iter().find_map(|o| match o {
            HeaderObject::FileProperties(p) => Some(p),
            _ => None,
        })
    }

    pub fn stream_properties(&self) -> impl Iterator<Item = &StreamProperties> {
        self.header.objects.iter().filter_map(|o| match o {
            HeaderObject::StreamProperties(p) => Some(p),
            _ => None,
        })
    }

    /// Media objects of all streams, in completion order.
    pub fn media_objects(&self) -> Result<Vec<MediaObject>, MediaObjectError> {
        media::media_objects(self)
    }

    pub fn data(&self) -> Option<&Data> {
        self.objects.iter().find_map(|o| match o {
            TopLevelObject::Data(d) => Some(d),
            _ => None,
        })
    }
}
