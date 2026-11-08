mod common;

use std::collections::BTreeMap;
use std::fmt::Write;

use wmv9::asf::{Asf, TypeSpecificData};

/// Container structure per sample. Stream object counts and key frame
/// counts were checked against `ffprobe -count_packets` and
/// `-show_packets` when the snapshots were accepted.
fn summary(asf: &Asf) -> String {
    let mut s = String::new();
    let props = asf.file_properties().unwrap();
    let data = asf.data().unwrap();
    writeln!(s, "packet_size: {}", props.max_data_packet_size).unwrap();
    writeln!(s, "packets: {}", data.packets.len()).unwrap();
    writeln!(s, "preroll_ms: {}", props.preroll.0).unwrap();
    for sp in asf.stream_properties() {
        let n = sp.flags.stream_number();
        match &sp.type_specific {
            TypeSpecificData::Audio(w) => writeln!(
                s,
                "stream {}: audio tag={:#06x} ch={} rate={} align={} extra={:02x?}",
                n.get(),
                w.format_tag,
                w.channels,
                w.samples_per_sec,
                w.block_align,
                w.extra
            ),
            TypeSpecificData::Video(v) => writeln!(
                s,
                "stream {}: video {:?} {}x{} codec_data={:02x?}",
                n.get(),
                v.bitmap.compression,
                v.encoded_width,
                v.encoded_height,
                v.codec_data
            ),
            TypeSpecificData::Raw(r) => writeln!(s, "stream {}: raw {} bytes", n.get(), r.len()),
        }
        .unwrap();
    }
    let mut per_stream: BTreeMap<u8, (usize, usize, usize)> = BTreeMap::new();
    for obj in asf.media_objects().unwrap() {
        let e = per_stream.entry(obj.stream.get()).or_default();
        e.0 += 1;
        e.1 += usize::from(obj.key_frame);
        e.2 += obj.data.len();
    }
    for (n, (count, keys, bytes)) in per_stream {
        writeln!(s, "stream {n}: objects={count} key={keys} bytes={bytes}").unwrap();
    }
    s
}

#[test]
fn sample_structure() {
    for path in common::sample_files() {
        let asf = Asf::parse(&std::fs::read(&path).unwrap()).unwrap();
        insta::assert_snapshot!(common::snapshot_name(&path), summary(&asf));
    }
}
