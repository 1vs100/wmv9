//! The header object encoders rebuild the objects of an SDK 11 sample byte
//! for byte from their values.

mod common;

use wmv9::asf::guid::{self, Guid};
use wmv9::asf::objects::{
    self, Attribute, AttributeValue, Bitrate, CodecEntry, CodecKind, ExtendedStreamFlags,
    ExtendedStreamProperties, IndexSpecifier, IndexType, LeakyBucket, PayloadExtension,
    StreamAttribute, StreamBitrate, WindowMs,
};
use wmv9::asf::{Asf, HeaderObject, Hns, RawObject, StreamNumber};

fn header_objects(asf: &Asf) -> Vec<&RawObject> {
    let mut out = Vec::new();
    for o in &asf.header.objects {
        match o {
            HeaderObject::Raw(r) => out.push(r),
            HeaderObject::HeaderExtension(h) => {
                out.extend(h.objects.iter().filter_map(|o| match o {
                    HeaderObject::Raw(r) => Some(r),
                    _ => None,
                }))
            }
            _ => {}
        }
    }
    out
}

fn stream(n: u8) -> StreamNumber {
    StreamNumber::new(n).unwrap()
}

fn rebuilt() -> Vec<RawObject> {
    let attr = |s, name: &str, value| StreamAttribute {
        stream: stream(s),
        name: name.into(),
        value,
    };
    // A leading zero word, then (bitrate, buffer window ms) pairs.
    let pairs: Vec<u8> = 0u16
        .to_le_bytes()
        .into_iter()
        .chain(
            [
                (24000u32, 277005u32),
                (30000, 218338),
                (45000, 140116),
                (58000, 105051),
                (115200, 44783),
                (240000, 14285),
                (350000, 6111),
                (500000, 1638),
                (730000, 259),
                (1000000, 128),
                (1400000, 91),
                (2100000, 60),
                (5000000, 25),
                (10000000, 12),
            ]
            .iter()
            .flat_map(|(r, w)| r.to_le_bytes().into_iter().chain(w.to_le_bytes())),
        )
        .collect();
    vec![
        objects::codec_list(&[
            CodecEntry {
                kind: CodecKind::Video,
                name: "VC-1".into(),
                description: "VC-1 Simple/Main Profile".into(),
                info: b"WMV3".to_vec(),
            },
            CodecEntry {
                kind: CodecKind::Audio,
                name: "Windows Media Audio 10 Professional".into(),
                description: "128 kbps, 44 kHz, 2 channel 24 bit 2-pass VBR".into(),
                info: vec![0x62, 0x01],
            },
        ]),
        objects::language_list(&["en-us"]),
        objects::compatibility(),
        objects::metadata(&[
            attr(1, "IsVBR", AttributeValue::Bool(true)),
            attr(
                1,
                "DeviceConformanceTemplate",
                AttributeValue::String("M1".into()),
            ),
            attr(2, "IsVBR", AttributeValue::Bool(true)),
            attr(
                2,
                "DeviceConformanceTemplate",
                AttributeValue::String("MP".into()),
            ),
            attr(1, "WM/WMADRCPeakReference", AttributeValue::Dword(15214)),
            attr(1, "WM/WMADRCPeakTarget", AttributeValue::Dword(15214)),
            attr(1, "WM/WMADRCAverageReference", AttributeValue::Dword(4574)),
            attr(1, "WM/WMADRCAverageTarget", AttributeValue::Dword(4574)),
        ]),
        objects::extended_stream_properties(&ExtendedStreamProperties {
            start_time: Hns(0),
            end_time: Hns(0),
            bucket: LeakyBucket {
                bitrate: Bitrate(131206),
                buffer: WindowMs(784),
                initial_fullness: WindowMs(0),
            },
            alternate_bucket: LeakyBucket {
                bitrate: Bitrate(124000),
                buffer: WindowMs(1486),
                initial_fullness: WindowMs(0),
            },
            maximum_object_size: 5945,
            flags: ExtendedStreamFlags::SEEKABLE,
            stream: stream(1),
            language_index: 0,
            average_time_per_frame: Hns(2904146),
            payload_extensions: vec![],
        }),
        objects::extended_stream_properties(&ExtendedStreamProperties {
            start_time: Hns(0),
            end_time: Hns(0),
            bucket: LeakyBucket {
                bitrate: Bitrate(325000),
                buffer: WindowMs(3000),
                initial_fullness: WindowMs(0),
            },
            alternate_bucket: LeakyBucket {
                bitrate: Bitrate(480000),
                buffer: WindowMs(5000),
                initial_fullness: WindowMs(0),
            },
            maximum_object_size: 19124,
            flags: ExtendedStreamFlags::SEEKABLE,
            stream: stream(2),
            language_index: 0,
            average_time_per_frame: Hns(333667),
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
        }),
        objects::index_parameters(
            1000,
            &[
                IndexSpecifier {
                    stream: stream(1),
                    kind: IndexType::NearestPastCleanPoint,
                },
                IndexSpecifier {
                    stream: stream(2),
                    kind: IndexType::NearestPastCleanPoint,
                },
            ],
        ),
        objects::extended_content_description(&[
            Attribute {
                name: "WMFSDKVersion".into(),
                value: AttributeValue::String("11.0.6001.7006".into()),
            },
            Attribute {
                name: "WMFSDKNeeded".into(),
                value: AttributeValue::String("0.0.0.0000".into()),
            },
            Attribute {
                name: "VBR Peak".into(),
                value: AttributeValue::Dword(480000),
            },
            Attribute {
                name: "IsVBR".into(),
                value: AttributeValue::Bool(true),
            },
            Attribute {
                name: "ASFLeakyBucketPairs".into(),
                value: AttributeValue::Bytes(pairs),
            },
        ]),
        objects::stream_bitrate_properties(&[
            StreamBitrate {
                stream: stream(1),
                average: Bitrate(128496),
            },
            StreamBitrate {
                stream: stream(2),
                average: Bitrate(485740),
            },
        ]),
    ]
}

#[test]
fn encoders_rebuild_the_header_objects_of_an_sdk_sample() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/ads/hvideo15_1.wmv");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!("{} missing; skipping", path.display());
        return;
    };
    let asf = Asf::parse(&bytes).unwrap();
    let mut sample: Vec<&RawObject> = header_objects(&asf)
        .into_iter()
        .filter(|r| r.guid != guid::PADDING)
        .collect();
    let mut ours = rebuilt();
    let key = |g: &Guid| g.0;
    sample.sort_by_key(|r| key(&r.guid));
    ours.sort_by_key(|r| key(&r.guid));
    assert_eq!(sample.len(), ours.len());
    for (s, o) in sample.iter().zip(&ours) {
        assert_eq!(s.guid, o.guid);
        let first = s.body.iter().zip(&o.body).position(|(a, b)| a != b);
        assert!(
            first.is_none() && s.body.len() == o.body.len(),
            "{:?}: lengths {} and {}, first difference at {first:?}: sample {:02x?} ours {:02x?}",
            s.guid,
            s.body.len(),
            o.body.len(),
            first.map(|i| &s.body[i.saturating_sub(4)..(i + 8).min(s.body.len())]),
            first.map(|i| &o.body[i.saturating_sub(4)..(i + 8).min(o.body.len())]),
        );
    }
}
