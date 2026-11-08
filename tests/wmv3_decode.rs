//! Decoded pictures against FFmpeg's per-frame MD5s in tests/reference/,
//! written by tools/update_reference_md5.sh.

mod common;

use std::path::Path;

use md5::{Digest, Md5};
use wmv9::asf::Asf;
use wmv9::wmv3::decode::{Decoder, PictureRole, display_order};
use wmv9::wmv3::{Frame, FrameContext};

fn reference(path: &Path) -> Option<Vec<String>> {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/reference")
        .join(format!("{}.md5", common::snapshot_name(path)));
    let text = std::fs::read_to_string(file).ok()?;
    Some(text.lines().map(str::to_owned).collect())
}

/// Every picture of every sample, decoded in coded order, has the MD5
/// FFmpeg gives at its display position.
#[test]
fn pictures_match_ffmpeg() {
    for path in common::sample_files() {
        let expected = reference(&path).unwrap_or_else(|| {
            panic!(
                "no reference MD5s for {}; run tools/update_reference_md5.sh",
                path.display()
            )
        });
        let asf = Asf::parse(&std::fs::read(&path).unwrap()).unwrap();
        let v = common::video_stream(&asf);
        let ctx = FrameContext {
            seq: &v.seq,
            width: v.width,
            height: v.height,
        };
        let frames: Vec<Frame> = v
            .frames
            .iter()
            .map(|o| Frame::parse(&o.data, &ctx).unwrap())
            .collect();
        let order = display_order(frames.iter().map(|f| PictureRole::of(&f.header.kind)));
        let shown = order.iter().flatten().count();
        assert_eq!(shown, expected.len(), "{}", path.display());
        let mut decoder = Decoder::new(ctx);
        let mut checked = 0;
        for (i, f) in frames.iter().enumerate() {
            let pic = decoder
                .decode(f)
                .unwrap_or_else(|e| panic!("{} coded frame {i}: {e}", path.display()));
            let (Some(pic), Some(display)) = (pic, order[i]) else {
                assert_eq!(order[i], None, "{} coded frame {i}", path.display());
                continue;
            };
            let yuv = pic.to_yuv420p(v.width.into(), v.height.into());
            let got: String = Md5::digest(&yuv)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(
                got,
                expected[display],
                "{} coded frame {i} display {display}",
                path.display(),
            );
            checked += 1;
        }
        assert!(checked > 0, "{}", path.display());
    }
}
