//! A whole sound encoded into the media objects of an ASF audio stream.

use super::{Format, FormatError, encode, pro};
use crate::asf::Millis;
use crate::asf::mux::{AudioObject, AudioTrack};

/// Which WMA codec codes the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// WMA 9 Standard (WMA version 2, format tag 0x0161).
    Standard,
    /// WMA 10 Professional (WMA version 3, format tag 0x0162).
    Pro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TrackError {
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error(transparent)]
    Standard(#[from] encode::EncodeError),
    #[error(transparent)]
    Pro(#[from] pro::encode::EncodeError),
}

/// Encodes interleaved samples into an audio track whose first input
/// sample plays at time 0.
pub fn encode_track(codec: Codec, format: Format, pcm: &[i16]) -> Result<AudioTrack, TrackError> {
    let rate = u64::from(format.sample_rate.0);
    let ms = |samples: u64| Millis((samples * 1000 + rate / 2) / rate);
    let channels = format.channels.count();
    match codec {
        Codec::Standard => {
            let mut encoder = encode::Encoder::new(format)?;
            let asf = encoder.asf_format();
            let frame_len = encoder.frame_len();
            let delay = ms(encoder.delay() as u64);
            let mut packets = Vec::new();
            for chunk in pcm.chunks(frame_len * channels) {
                packets.extend(encoder.push(chunk)?);
            }
            packets.extend(encoder.finish()?);
            Ok(AudioTrack {
                format: asf,
                delay,
                objects: packets
                    .into_iter()
                    .enumerate()
                    .map(|(k, data)| AudioObject {
                        data,
                        presentation: ms(k as u64 * frame_len as u64),
                    })
                    .collect(),
            })
        }
        Codec::Pro => {
            let mut encoder = pro::encode::Encoder::new(format)?;
            let asf = encoder.asf_format();
            let frame_len = encoder.frame_len() as u64;
            for chunk in pcm.chunks(frame_len as usize * channels) {
                encoder.push(chunk)?;
            }
            // A packet carries the time of the first frame that starts in
            // it, frame 0 at time 0, as Microsoft's streams do; decoders
            // drop the output of frame 0, so the input starts at 0.
            Ok(AudioTrack {
                format: asf,
                delay: Millis(0),
                objects: encoder
                    .finish()?
                    .into_iter()
                    .map(|p| AudioObject {
                        data: p.data,
                        presentation: ms(p.frame.0 * frame_len),
                    })
                    .collect(),
            })
        }
    }
}
