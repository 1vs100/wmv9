/// GUID in its on-disk (little-endian fields) byte order.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    pub const fn new(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Self {
        let a = d1.to_le_bytes();
        let b = d2.to_le_bytes();
        let c = d3.to_le_bytes();
        Guid([
            a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4],
            d4[5], d4[6], d4[7],
        ])
    }

    pub fn name(&self) -> Option<&'static str> {
        KNOWN.iter().find(|(g, _)| g == self).map(|(_, n)| *n)
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u16::from_le_bytes([b[4], b[5]]),
            u16::from_le_bytes([b[6], b[7]]),
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15]
        )
    }
}

impl std::fmt::Debug for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.name() {
            Some(n) => write!(f, "{n}"),
            None => write!(f, "{{{self}}}"),
        }
    }
}

const ASF: [u8; 8] = [0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];

pub const HEADER: Guid = Guid::new(0x75B22630, 0x668E, 0x11CF, ASF);
pub const DATA: Guid = Guid::new(0x75B22636, 0x668E, 0x11CF, ASF);
pub const FILE_PROPERTIES: Guid = Guid::new(
    0x8CABDCA1,
    0xA947,
    0x11CF,
    [0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65],
);
pub const STREAM_PROPERTIES: Guid = Guid::new(
    0xB7DC0791,
    0xA9B7,
    0x11CF,
    [0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65],
);
pub const HEADER_EXTENSION: Guid = Guid::new(
    0x5FBF03B5,
    0xA92E,
    0x11CF,
    [0x8E, 0xE3, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65],
);
pub const AUDIO_MEDIA: Guid = Guid::new(
    0xF8699E40,
    0x5B4D,
    0x11CF,
    [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B],
);
pub const VIDEO_MEDIA: Guid = Guid::new(
    0xBC19EFC0,
    0x5B4D,
    0x11CF,
    [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B],
);

pub const SIMPLE_INDEX: Guid = Guid::new(
    0x33000890,
    0xE5B1,
    0x11CF,
    [0x89, 0xF4, 0x00, 0xA0, 0xC9, 0x03, 0x49, 0xCB],
);
pub const INDEX: Guid = Guid::new(
    0xD6E229D3,
    0x35DA,
    0x11D1,
    [0x90, 0x34, 0x00, 0xA0, 0xC9, 0x03, 0x49, 0xBE],
);
pub const CODEC_LIST: Guid = Guid::new(
    0x86D15240,
    0x311D,
    0x11D0,
    [0xA3, 0xA4, 0x00, 0xA0, 0xC9, 0x03, 0x48, 0xF6],
);
pub const CONTENT_DESCRIPTION: Guid = Guid::new(0x75B22633, 0x668E, 0x11CF, ASF);
pub const EXTENDED_CONTENT_DESCRIPTION: Guid = Guid::new(
    0xD2D0A440,
    0xE307,
    0x11D2,
    [0x97, 0xF0, 0x00, 0xA0, 0xC9, 0x5E, 0xA8, 0x50],
);
pub const STREAM_BITRATE_PROPERTIES: Guid = Guid::new(
    0x7BF875CE,
    0x468D,
    0x11D1,
    [0x8D, 0x82, 0x00, 0x60, 0x97, 0xC9, 0xA2, 0xB2],
);
pub const PADDING: Guid = Guid::new(
    0x1806D474,
    0xCADF,
    0x4509,
    [0xA4, 0xBA, 0x9A, 0xAB, 0xCB, 0x96, 0xAA, 0xE8],
);
pub const EXTENDED_STREAM_PROPERTIES: Guid = Guid::new(
    0x14E6A5CB,
    0xC672,
    0x4332,
    [0x83, 0x99, 0xA9, 0x69, 0x52, 0x06, 0x5B, 0x5A],
);
pub const LANGUAGE_LIST: Guid = Guid::new(
    0x7C4346A9,
    0xEFE0,
    0x4BFC,
    [0xB2, 0x29, 0x39, 0x3E, 0xDE, 0x41, 0x5C, 0x85],
);
pub const METADATA: Guid = Guid::new(
    0xC5F8CBEA,
    0x5BAF,
    0x4877,
    [0x84, 0x67, 0xAA, 0x8C, 0x44, 0xFA, 0x4C, 0xCA],
);
pub const COMPATIBILITY: Guid = Guid::new(
    0x26F18B5D,
    0x4584,
    0x47EC,
    [0x9F, 0x5F, 0x0E, 0x65, 0x1F, 0x04, 0x52, 0xC9],
);
pub const INDEX_PARAMETERS: Guid = Guid::new(
    0xD6E229DF,
    0x35DA,
    0x11D1,
    [0x90, 0x34, 0x00, 0xA0, 0xC9, 0x03, 0x49, 0xBE],
);
pub const NO_ERROR_CORRECTION: Guid = Guid::new(
    0x20FB5700,
    0x5B55,
    0x11CF,
    [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B],
);
pub const AUDIO_SPREAD: Guid = Guid::new(
    0xBFC3CD50,
    0x618F,
    0x11CF,
    [0x8B, 0xB2, 0x00, 0xAA, 0x00, 0xB4, 0xE2, 0x20],
);
pub const RESERVED_2: Guid = Guid::new(
    0x86D15241,
    0x311D,
    0x11D0,
    [0xA3, 0xA4, 0x00, 0xA0, 0xC9, 0x03, 0x48, 0xF6],
);
pub const RESERVED_1: Guid = Guid::new(
    0xABD3D211,
    0xA9BA,
    0x11CF,
    [0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65],
);
/// Payload extension: pixel aspect ratio, 2 bytes (x, y).
pub const PAYLOAD_EXT_PIXEL_ASPECT_RATIO: Guid = Guid::new(
    0x1B1EE554,
    0xF9EA,
    0x4BC8,
    [0x82, 0x1A, 0x37, 0x6B, 0x74, 0xE4, 0xC4, 0xB8],
);
/// Payload extension: sample duration in milliseconds, 2 bytes.
pub const PAYLOAD_EXT_SAMPLE_DURATION: Guid = Guid::new(
    0xC6BD9450,
    0x867F,
    0x4907,
    [0x83, 0xA3, 0xC7, 0x79, 0x21, 0xB7, 0x33, 0xAD],
);

const KNOWN: &[(Guid, &str)] = &[
    (HEADER, "Header"),
    (DATA, "Data"),
    (FILE_PROPERTIES, "FileProperties"),
    (STREAM_PROPERTIES, "StreamProperties"),
    (HEADER_EXTENSION, "HeaderExtension"),
    (AUDIO_MEDIA, "AudioMedia"),
    (VIDEO_MEDIA, "VideoMedia"),
    (SIMPLE_INDEX, "SimpleIndex"),
    (INDEX, "Index"),
    (CODEC_LIST, "CodecList"),
    (CONTENT_DESCRIPTION, "ContentDescription"),
    (EXTENDED_CONTENT_DESCRIPTION, "ExtendedContentDescription"),
    (STREAM_BITRATE_PROPERTIES, "StreamBitrateProperties"),
    (PADDING, "Padding"),
    (EXTENDED_STREAM_PROPERTIES, "ExtendedStreamProperties"),
    (LANGUAGE_LIST, "LanguageList"),
    (METADATA, "Metadata"),
    (COMPATIBILITY, "Compatibility"),
    (INDEX_PARAMETERS, "IndexParameters"),
    (NO_ERROR_CORRECTION, "NoErrorCorrection"),
    (AUDIO_SPREAD, "AudioSpread"),
];
