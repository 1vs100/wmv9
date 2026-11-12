//! Frames coded for XMA read back under XMA's stream parameters.

use wmv9::wma::pro::encode::Encoder;
use wmv9::wma::pro::syntax::Codec;
use wmv9::wma::{ByteRate, Channels, Format, SampleRate};

const XMA_DECODE_FLAGS: u16 = 0x10d6;
const XMA_FRAME_LEN: usize = 512;
/// A packet of 2048 bytes less its 32-bit header.
const XMA_PAYLOAD_BITS: usize = 2048 * 8 - 32;

fn tone(rate: u32, channels: usize, samples: usize) -> Vec<i16> {
    (0..samples * channels)
        .map(|i| {
            (8000.0
                * (i as f64 / channels as f64 * 440.0 * std::f64::consts::TAU / f64::from(rate))
                    .sin()) as i16
        })
        .collect()
}

#[test]
fn xma_frames_read_back_with_one_more_frame_than_the_input_fills() {
    for (rate, channels) in [
        (48000, Channels::Mono),
        (44100, Channels::Stereo),
        (47959, Channels::Mono),
    ] {
        let encoder = Encoder::xma(Format {
            sample_rate: SampleRate(rate),
            channels,
            byte_rate: ByteRate(12000),
        })
        .unwrap();
        let codec = Codec::new(encoder.config());
        assert_eq!(codec.config().header.decode_flags.0, XMA_DECODE_FLAGS);
        assert_eq!(codec.config().frame_len, XMA_FRAME_LEN);
        let samples = 20_000;
        let mut encoder = encoder;
        encoder
            .push(&tone(rate, channels.count(), samples))
            .unwrap();
        let frames = encoder.finish_frames().unwrap();
        assert_eq!(frames.len(), samples.div_ceil(XMA_FRAME_LEN) + 1);
        for bits in &frames {
            assert!(bits.len() <= XMA_PAYLOAD_BITS);
            codec.read(bits).unwrap();
        }
    }
}
