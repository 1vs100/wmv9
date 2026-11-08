// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Sample media lives in the untracked `testdata/` directory. Returns an
/// empty list when it is absent so the suite still passes on a clean checkout.
pub fn sample_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut out = Vec::new();
    collect(&root, &mut out);
    out.sort();
    if out.is_empty() {
        eprintln!("no sample files under {}; skipping", root.display());
    }
    out
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("wmv") || e.eq_ignore_ascii_case("asf"))
        {
            out.push(path);
        }
    }
}

/// Snapshot name derived from the path below `testdata/`.
pub fn snapshot_name(path: &Path) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    path.strip_prefix(&root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(['/', '\\', '.'], "_")
}

/// The video stream of a sample: its sequence header, coded size and frames
/// in coded order.
pub struct VideoStream {
    pub seq: wmv9::wmv3::SequenceHeader,
    pub width: u16,
    pub height: u16,
    pub frames: Vec<wmv9::asf::MediaObject>,
}

pub fn video_stream(asf: &wmv9::asf::Asf) -> VideoStream {
    use wmv9::asf::TypeSpecificData;
    let sp = asf
        .stream_properties()
        .find(|s| matches!(s.type_specific, TypeSpecificData::Video(_)))
        .expect("sample has a video stream");
    let TypeSpecificData::Video(v) = &sp.type_specific else {
        unreachable!()
    };
    let number = sp.flags.stream_number();
    VideoStream {
        seq: wmv9::wmv3::SequenceHeader::parse(&v.codec_data).unwrap(),
        width: u16::try_from(v.encoded_width).unwrap(),
        height: u16::try_from(v.encoded_height).unwrap(),
        frames: asf
            .media_objects()
            .unwrap()
            .into_iter()
            .filter(|o| o.stream == number)
            .collect(),
    }
}
