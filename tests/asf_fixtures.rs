//! Hand-assembled packets that exercise encodings the sample files do not
//! use. Expected values come from the byte layouts in ASF 1.20.03 section 5.2.

use wmv9::asf::{
    Asf, Error, FieldWidth, MediaObjectNumber, Millis, PacketDuration, PacketError, PayloadKind,
    Payloads, SendTime, StreamNumber, VarField,
};

const HEADER: [u8; 16] = hex16("3026B2758E66CF11A6D900AA0062CE6C");
const FILE_PROPERTIES: [u8; 16] = hex16("A1DCAB8C47A9CF118EE400C00C205365");
const DATA: [u8; 16] = hex16("3626B2758E66CF11A6D900AA0062CE6C");

const fn hex16(s: &str) -> [u8; 16] {
    let b = s.as_bytes();
    let mut out = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        out[i] = nibble(b[2 * i]) << 4 | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        _ => c - b'A' + 10,
    }
}

/// A file with a header holding only file properties, then one data object.
fn asf_file(packet_size: u32, packets: &[&[u8]]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&HEADER);
    f.extend_from_slice(&(30u64 + 104).to_le_bytes());
    f.extend_from_slice(&1u32.to_le_bytes());
    f.extend_from_slice(&[1, 2]);
    f.extend_from_slice(&FILE_PROPERTIES);
    f.extend_from_slice(&104u64.to_le_bytes());
    f.extend_from_slice(&[0; 16 + 8 + 8]);
    f.extend_from_slice(&(packets.len() as u64).to_le_bytes());
    f.extend_from_slice(&[0; 8 * 3 + 4]);
    f.extend_from_slice(&packet_size.to_le_bytes());
    f.extend_from_slice(&packet_size.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&DATA);
    let total: usize = packets.iter().map(|p| p.len()).sum();
    f.extend_from_slice(&(50 + total as u64).to_le_bytes());
    f.extend_from_slice(&[0; 16]);
    f.extend_from_slice(&(packets.len() as u64).to_le_bytes());
    f.extend_from_slice(&[1, 1]);
    for p in packets {
        assert_eq!(p.len(), packet_size as usize, "fixture packet size");
        f.extend_from_slice(p);
    }
    f
}

/// No error correction; packet length Word, sequence Byte, padding Dword;
/// replicated length Word, offset Byte, object number Dword; one payload.
#[rustfmt::skip]
const SINGLE: [u8; 38] = [
    0x5a, 0x76,
    0x26, 0x00,             // packet length 38
    0x07,                   // sequence 7
    0x03, 0x00, 0x00, 0x00, // padding 3
    0xe8, 0x03, 0x00, 0x00, // send time 1000
    0x28, 0x00,             // duration 40
    0x82,                   // key frame, stream 2
    0x04, 0x03, 0x02, 0x01, // object number 0x01020304
    0x00,                   // offset 0
    0x08, 0x00,             // replicated length 8
    0x04, 0x00, 0x00, 0x00, 0xf4, 0x01, 0x00, 0x00, // size 4, time 500
    0xde, 0xad, 0xbe, 0xef,
    0x00, 0x00, 0x00,
];

/// Error correction with bit 7 of the length flags set; padding Byte;
/// replicated length Byte, offset Dword, object number Byte; a compressed
/// payload whose object numbers wrap at 255, then a fragment.
#[rustfmt::skip]
const MULTIPLE: [u8; 48] = [
    0x82, 0x00, 0x00,       // error correction, 2 bytes
    0x89, 0x5d,
    0x02,                   // padding 2
    0xd0, 0x07, 0x00, 0x00, // send time 2000
    0x00, 0x00,
    0x42,                   // payload length Byte, 2 payloads
    0x01, 0xff,             // stream 1, object 255
    0x64, 0x00, 0x00, 0x00, // presentation time 100
    0x01, 0x0a,             // compressed, delta 10
    0x05, 0x02, 0xaa, 0xbb, 0x01, 0xcc,
    0x82, 0x05,             // key frame, stream 2, object 5
    0x00, 0x00, 0x00, 0x00, // offset 0
    0x08, 0x03, 0x00, 0x00, 0x00, 0x78, 0x00, 0x00, 0x00, // size 3, time 120
    0x03, 0x11, 0x22, 0x33,
    0x00, 0x00,
];

fn round_trip(file: &[u8]) -> Asf {
    let asf = Asf::parse(file).unwrap();
    assert_eq!(asf.to_bytes().unwrap(), file);
    asf
}

fn stream(n: u8) -> StreamNumber {
    StreamNumber::new(n).unwrap()
}

#[test]
fn single_payload_fields_follow_declared_widths() {
    let asf = round_trip(&asf_file(38, &[&SINGLE]));
    let p = &asf.data().unwrap().packets[0];
    assert_eq!(p.error_correction, None);
    assert_eq!(p.packet_length, VarField::Word(38));
    assert_eq!(p.sequence, VarField::Byte(7));
    assert_eq!(p.padding_length, VarField::Dword(3));
    assert_eq!(p.widths.replicated_data_length, FieldWidth::Word);
    assert_eq!(p.widths.offset_into_media_object, FieldWidth::Byte);
    assert_eq!(p.widths.media_object_number, FieldWidth::Dword);
    assert_eq!(p.send_time, SendTime(1000));
    assert_eq!(p.duration, PacketDuration(40));
    let Payloads::Single(pl) = &p.payloads else {
        panic!("expected a single payload");
    };
    assert_eq!(pl.stream, stream(2));
    assert!(pl.key_frame);
    assert_eq!(pl.media_object_number, MediaObjectNumber(0x01020304));
    assert_eq!(pl.data, [0xde, 0xad, 0xbe, 0xef]);

    let objs = asf.media_objects().unwrap();
    assert_eq!(objs.len(), 1);
    assert_eq!(objs[0].presentation_time, Millis(500));
    assert_eq!(objs[0].data, [0xde, 0xad, 0xbe, 0xef]);
}

#[test]
fn compressed_payload_splits_into_objects() {
    let asf = round_trip(&asf_file(48, &[&MULTIPLE]));
    let p = &asf.data().unwrap().packets[0];
    let ec = p.error_correction.as_ref().unwrap();
    assert!(!ec.opaque);
    assert_eq!(ec.data, [0, 0]);
    assert!(ec.length_flags_bit);
    let Payloads::Multiple {
        payload_length_width,
        payloads,
    } = &p.payloads
    else {
        panic!("expected multiple payloads");
    };
    assert_eq!(*payload_length_width, FieldWidth::Byte);
    assert_eq!(
        payloads[0].kind,
        PayloadKind::Compressed {
            presentation_time: 100,
            time_delta: 10
        }
    );

    let objs = asf.media_objects().unwrap();
    let summary: Vec<_> = objs
        .iter()
        .map(|o| {
            (
                o.stream.get(),
                o.number.0,
                o.presentation_time.0,
                o.key_frame,
                o.data.clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (1, 255, 100, false, vec![0xaa, 0xbb]),
            (1, 0, 110, false, vec![0xcc]),
            (2, 5, 120, true, vec![0x11, 0x22, 0x33]),
        ]
    );
}

fn packet_error(file: &[u8]) -> (usize, PacketError) {
    match Asf::parse(file) {
        Err(Error::Packet { packet, reason }) => (packet, reason),
        other => panic!("expected a packet error, got {other:?}"),
    }
}

fn patched<const N: usize>(base: &[u8; N], at: usize, byte: u8) -> [u8; N] {
    let mut p = *base;
    p[at] = byte;
    p
}

#[test]
fn malformed_packets_report_index_and_reason() {
    let single = |p: &[u8; 38]| asf_file(38, &[&SINGLE, p]);
    let multiple = |p: &[u8; 48]| asf_file(48, &[p]);

    assert_eq!(
        packet_error(&single(&patched(&SINGLE, 0, 0xa2))),
        (1, PacketError::ErrorCorrectionLengthType(1))
    );
    assert_eq!(
        packet_error(&single(&patched(&SINGLE, 1, 0x36))),
        (1, PacketError::StreamNumberWidth(0))
    );
    assert_eq!(
        packet_error(&single(&patched(&SINGLE, 2, 37))),
        (
            1,
            PacketError::PacketLength {
                declared: 37,
                size: 38
            }
        )
    );
    assert_eq!(
        packet_error(&single(&patched(&SINGLE, 37, 1))),
        (1, PacketError::NonZeroPadding)
    );
    assert_eq!(
        packet_error(&multiple(&patched(&MULTIPLE, 12, 0x02))),
        (0, PacketError::MissingPayloadLength)
    );
    assert_eq!(
        packet_error(&multiple(&patched(&MULTIPLE, 5, 0x01))),
        (
            0,
            PacketError::SizeMismatch {
                used: 46,
                padding: 1,
                size: 48
            }
        )
    );
    assert!(matches!(
        packet_error(&multiple(&patched(&MULTIPLE, 21, 0xff))),
        (0, PacketError::Truncated(_))
    ));
}
