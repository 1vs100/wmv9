//! Encoder and decoder output, as hashes: speed changes must not change a byte of it.

#[path = "../benches/common/mod.rs"]
mod common;

use md5::{Digest, Md5};
use std::path::Path;
use wmv9::wma::Channels;

fn hash(out: &mut String, name: &str, chunks: Vec<Vec<u8>>) {
    let mut h = Md5::new();
    for c in &chunks {
        h.update((c.len() as u64).to_le_bytes());
        h.update(c);
    }
    let d = h.finalize();
    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
    out.push_str(&format!("{name}: {} chunks {hex}\n", chunks.len()));
}

#[test]
fn codec_output_is_unchanged() {
    let mut out = String::new();
    let mono = common::audio(Channels::Mono);
    let stereo = common::audio(Channels::Stereo);
    hash(
        &mut out,
        "wmapro stereo",
        common::wmapro(common::format(Channels::Stereo, 16000), &stereo),
    );
    hash(
        &mut out,
        "wmapro mono",
        common::wmapro(common::format(Channels::Mono, 8000), &mono),
    );
    hash(
        &mut out,
        "wmapro mono 32k",
        common::wmapro(common::format(Channels::Mono, 4000), &mono),
    );
    hash(
        &mut out,
        "xma stereo",
        common::wmapro_xma(common::format(Channels::Stereo, 16000), &stereo),
    );
    hash(
        &mut out,
        "xma mono",
        common::wmapro_xma(common::format(Channels::Mono, 8000), &mono),
    );
    hash(
        &mut out,
        "wma2 stereo",
        common::wma2(common::format(Channels::Stereo, 16000), &stereo),
    );
    hash(
        &mut out,
        "wma2 mono",
        common::wma2(common::format(Channels::Mono, 4000), &mono),
    );
    let video = common::video();
    let coded = common::wmv3_encode(&video);
    hash(&mut out, "wmv3 encode", coded.clone());
    hash(
        &mut out,
        "wmv3 recon",
        common::wmv3_decode(&common::VideoStream {
            seq: wmv9::wmv3::encode::sample_sequence(),
            width: common::VIDEO_WIDTH,
            height: common::VIDEO_HEIGHT,
            frames: coded,
        }),
    );
    let target = wmv9::wmv3::encode::RateTarget::new(
        std::num::NonZeroU32::new(400_000).unwrap(),
        wmv9::wmv3::encode::FrameRate::new(30, 1).unwrap(),
        wmv9::asf::Millis(1000),
    )
    .unwrap();
    hash(
        &mut out,
        "wmv3 encode rate",
        common::wmv3_encode_with(&video, wmv9::wmv3::encode::Quantizer::Rate(target)),
    );
    // The game samples are not in the repository; without them only the synthetic
    // streams are checked.
    let Some(stream) = common::read_video(Path::new(common::SAMPLE_FILE)) else {
        insta::assert_snapshot!("synthetic", out);
        return;
    };
    let pictures = common::sample_pictures(&stream, common::SAMPLE_PICTURES);
    hash(
        &mut out,
        "wmv3 encode sample",
        common::wmv3_encode(&pictures),
    );
    hash(
        &mut out,
        "wmv3 encode sample q20",
        common::wmv3_encode_with(
            &pictures,
            wmv9::wmv3::encode::Quantizer::Fixed(wmv9::wmv3::PqIndex::new(20).unwrap()),
        ),
    );
    for f in ["hvideo15_1", "promo_1", "usered_1"] {
        let p = format!("testdata/ads/{f}.wmv");
        if let Some(s) = common::read_video(Path::new(&p)) {
            hash(&mut out, &format!("decode {f}"), common::wmv3_decode(&s));
        }
    }
    insta::assert_snapshot!("with_samples", out);
}
