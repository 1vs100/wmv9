//! Encoders for the header and index objects the reader keeps as opaque
//! bodies, laid out as the Windows Media Format SDK writes them.

use super::guid::{self, Guid};
use super::{Hns, RawObject, StreamNumber};
use crate::bytes::PutLe;

/// UTF-16LE with a terminating NUL.
fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Length of a count that the format stores in 16 bits.
fn len16(n: usize) -> u16 {
    u16::try_from(n).expect("ASF object field fits 16 bits")
}

fn len32(n: usize) -> u32 {
    u32::try_from(n).expect("ASF object field fits 32 bits")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    Video,
    Audio,
}

/// One entry of the Codec List object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecEntry {
    pub kind: CodecKind,
    pub name: String,
    pub description: String,
    /// FOURCC for video, format tag for audio.
    pub info: Vec<u8>,
}

pub fn codec_list(entries: &[CodecEntry]) -> RawObject {
    let mut b = Vec::new();
    b.extend(guid::RESERVED_2.0);
    b.put_u32(len32(entries.len()));
    for e in entries {
        b.put_u16(match e.kind {
            CodecKind::Video => 1,
            CodecKind::Audio => 2,
        });
        b.put_u16(len16(e.name.encode_utf16().count() + 1));
        b.extend(utf16z(&e.name));
        b.put_u16(len16(e.description.encode_utf16().count() + 1));
        b.extend(utf16z(&e.description));
        b.put_u16(len16(e.info.len()));
        b.extend(&e.info);
    }
    RawObject {
        guid: guid::CODEC_LIST,
        body: b,
    }
}

/// Language List object with RFC 1766 language tags.
pub fn language_list(languages: &[&str]) -> RawObject {
    let mut b = Vec::new();
    b.put_u16(len16(languages.len()));
    for l in languages {
        let s = utf16z(l);
        b.push(u8::try_from(s.len()).expect("language tag fits 255 bytes"));
        b.extend(s);
    }
    RawObject {
        guid: guid::LANGUAGE_LIST,
        body: b,
    }
}

/// Compatibility object as the SDK writes it (profile 2, mode 1).
pub fn compatibility() -> RawObject {
    RawObject {
        guid: guid::COMPATIBILITY,
        body: vec![2, 1],
    }
}

/// An attribute value. BOOL takes 2 bytes in the Metadata object and 4 in
/// the Extended Content Description object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeValue {
    String(String),
    Bytes(Vec<u8>),
    Bool(bool),
    Dword(u32),
    Qword(u64),
    Word(u16),
}

impl AttributeValue {
    fn kind(&self) -> u16 {
        match self {
            AttributeValue::String(_) => 0,
            AttributeValue::Bytes(_) => 1,
            AttributeValue::Bool(_) => 2,
            AttributeValue::Dword(_) => 3,
            AttributeValue::Qword(_) => 4,
            AttributeValue::Word(_) => 5,
        }
    }

    fn bytes(&self, bool_width: usize) -> Vec<u8> {
        match self {
            AttributeValue::String(s) => utf16z(s),
            AttributeValue::Bytes(b) => b.clone(),
            AttributeValue::Bool(v) => {
                let mut b = vec![0; bool_width];
                b[0] = u8::from(*v);
                b
            }
            AttributeValue::Dword(v) => v.to_le_bytes().to_vec(),
            AttributeValue::Qword(v) => v.to_le_bytes().to_vec(),
            AttributeValue::Word(v) => v.to_le_bytes().to_vec(),
        }
    }
}

/// One record of the Metadata object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamAttribute {
    pub stream: StreamNumber,
    pub name: String,
    pub value: AttributeValue,
}

pub fn metadata(records: &[StreamAttribute]) -> RawObject {
    let mut b = Vec::new();
    b.put_u16(len16(records.len()));
    for r in records {
        let name = utf16z(&r.name);
        let value = r.value.bytes(2);
        b.put_u16(0);
        b.put_u16(r.stream.get().into());
        b.put_u16(len16(name.len()));
        b.put_u16(r.value.kind());
        b.put_u32(len32(value.len()));
        b.extend(name);
        b.extend(value);
    }
    RawObject {
        guid: guid::METADATA,
        body: b,
    }
}

/// One descriptor of the Extended Content Description object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub value: AttributeValue,
}

pub fn extended_content_description(attributes: &[Attribute]) -> RawObject {
    let mut b = Vec::new();
    b.put_u16(len16(attributes.len()));
    for a in attributes {
        let name = utf16z(&a.name);
        let value = a.value.bytes(4);
        b.put_u16(len16(name.len()));
        b.extend(name);
        b.put_u16(a.value.kind());
        b.put_u16(len16(value.len()));
        b.extend(value);
    }
    RawObject {
        guid: guid::EXTENDED_CONTENT_DESCRIPTION,
        body: b,
    }
}

/// A payload extension system declared for a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadExtension {
    pub system: Guid,
    /// Bytes each payload carries for this system.
    pub size: u16,
}

/// Bits per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bitrate(pub u32);

/// A buffer window in milliseconds, as the 32-bit fields store it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WindowMs(pub u32);

/// Leaky bucket model of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeakyBucket {
    pub bitrate: Bitrate,
    pub buffer: WindowMs,
    pub initial_fullness: WindowMs,
}

/// Flags of the Extended Stream Properties object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtendedStreamFlags(pub u32);

impl ExtendedStreamFlags {
    pub const SEEKABLE: ExtendedStreamFlags = ExtendedStreamFlags(2);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendedStreamProperties {
    pub start_time: Hns,
    pub end_time: Hns,
    pub bucket: LeakyBucket,
    pub alternate_bucket: LeakyBucket,
    pub maximum_object_size: u32,
    pub flags: ExtendedStreamFlags,
    pub stream: StreamNumber,
    pub language_index: u16,
    pub average_time_per_frame: Hns,
    pub payload_extensions: Vec<PayloadExtension>,
}

pub fn extended_stream_properties(e: &ExtendedStreamProperties) -> RawObject {
    let mut b = Vec::new();
    b.put_u64(e.start_time.0);
    b.put_u64(e.end_time.0);
    for bucket in [e.bucket, e.alternate_bucket] {
        b.put_u32(bucket.bitrate.0);
        b.put_u32(bucket.buffer.0);
        b.put_u32(bucket.initial_fullness.0);
    }
    b.put_u32(e.maximum_object_size);
    b.put_u32(e.flags.0);
    b.put_u16(e.stream.get().into());
    b.put_u16(e.language_index);
    b.put_u64(e.average_time_per_frame.0);
    b.put_u16(0);
    b.put_u16(len16(e.payload_extensions.len()));
    for p in &e.payload_extensions {
        b.extend(p.system.0);
        b.put_u16(p.size);
        b.put_u32(0);
    }
    RawObject {
        guid: guid::EXTENDED_STREAM_PROPERTIES,
        body: b,
    }
}

/// Index type of an index specifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexType {
    NearestPastDataPacket,
    NearestPastMediaObject,
    NearestPastCleanPoint,
}

impl IndexType {
    fn code(self) -> u16 {
        match self {
            IndexType::NearestPastDataPacket => 1,
            IndexType::NearestPastMediaObject => 2,
            IndexType::NearestPastCleanPoint => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexSpecifier {
    pub stream: StreamNumber,
    pub kind: IndexType,
}

pub fn index_parameters(interval_ms: u32, specifiers: &[IndexSpecifier]) -> RawObject {
    let mut b = Vec::new();
    b.put_u32(interval_ms);
    b.put_u16(len16(specifiers.len()));
    for s in specifiers {
        b.put_u16(s.stream.get().into());
        b.put_u16(s.kind.code());
    }
    RawObject {
        guid: guid::INDEX_PARAMETERS,
        body: b,
    }
}

/// Average bitrate of one stream for the Stream Bitrate Properties object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamBitrate {
    pub stream: StreamNumber,
    pub average: Bitrate,
}

pub fn stream_bitrate_properties(rates: &[StreamBitrate]) -> RawObject {
    let mut b = Vec::new();
    b.put_u16(len16(rates.len()));
    for r in rates {
        b.put_u16(r.stream.get().into());
        b.put_u32(r.average.0);
    }
    RawObject {
        guid: guid::STREAM_BITRATE_PROPERTIES,
        body: b,
    }
}

/// Byte offset of a data packet from the first one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataOffset(pub u32);

/// Index object with one block, which addresses the first 4 GiB of data.
/// `entries[t][s]` is the packet to start from at time `t * interval_ms`
/// for specifier `s`.
pub fn index(
    interval_ms: u32,
    specifiers: &[IndexSpecifier],
    entries: &[Vec<DataOffset>],
) -> RawObject {
    let mut b = Vec::new();
    b.put_u32(interval_ms);
    b.put_u16(len16(specifiers.len()));
    b.put_u32(1);
    for s in specifiers {
        b.put_u16(s.stream.get().into());
        b.put_u16(s.kind.code());
    }
    b.put_u32(len32(entries.len()));
    for _ in specifiers {
        b.put_u64(0);
    }
    for e in entries {
        assert_eq!(e.len(), specifiers.len(), "one offset per specifier");
        for o in e {
            b.put_u32(o.0);
        }
    }
    RawObject {
        guid: guid::INDEX,
        body: b,
    }
}

/// Index of a data packet, from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PacketNumber(pub u32);

/// One Simple Index entry: the packet holding the key frame to start from
/// and how many packets carry part of that key frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimpleIndexEntry {
    pub packet: PacketNumber,
    pub count: u16,
}

pub fn simple_index(file_id: Guid, interval: Hns, entries: &[SimpleIndexEntry]) -> RawObject {
    let mut b = Vec::new();
    b.extend(file_id.0);
    b.put_u64(interval.0);
    b.put_u32(
        entries
            .iter()
            .map(|e| u32::from(e.count))
            .max()
            .unwrap_or(0),
    );
    b.put_u32(len32(entries.len()));
    for e in entries {
        b.put_u32(e.packet.0);
        b.put_u16(e.count);
    }
    RawObject {
        guid: guid::SIMPLE_INDEX,
        body: b,
    }
}
