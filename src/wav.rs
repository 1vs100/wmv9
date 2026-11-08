//! Reads 16-bit PCM WAV files, as `ffmpeg -f wav` writes them.

/// Sample layout of a PCM WAV file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub channels: u16,
    pub sample_rate: u32,
}

/// The samples of a WAV file, interleaved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pcm {
    pub format: PcmFormat,
    pub samples: Vec<i16>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WavError {
    #[error("not a RIFF WAVE file")]
    NotWave,
    #[error("no fmt chunk before the data chunk")]
    NoFormat,
    #[error("the fmt chunk has {size} bytes, fewer than 16")]
    ShortFormat { size: u32 },
    #[error("format tag {tag:#x} is not PCM")]
    NotPcm { tag: u16 },
    #[error("{bits} bits per sample; 16 are supported")]
    BitsPerSample { bits: u16 },
    #[error("the format declares no channels")]
    NoChannels,
    #[error("{bytes} bytes of data do not make whole sample frames of {channels} channels")]
    PartialSampleFrame { bytes: usize, channels: u16 },
    #[error("no data chunk")]
    NoData,
    #[error("a chunk extends past the end of the file")]
    Truncated,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Size field of a data chunk written to a stream, before its length was
/// known: the data runs to the end of the file.
const STREAMED_SIZE: u32 = u32::MAX;

pub fn parse(bytes: &[u8]) -> Result<Pcm, WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWave);
    }
    let mut pos = 12;
    let mut format = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(bytes, pos + 4);
        let body = pos + 8;
        if id == b"data" {
            let format: PcmFormat = format.ok_or(WavError::NoFormat)?;
            let end = if size == STREAMED_SIZE {
                bytes.len()
            } else {
                body.checked_add(size as usize)
                    .filter(|&e| e <= bytes.len())
                    .ok_or(WavError::Truncated)?
            };
            let data = &bytes[body..end];
            if !data.len().is_multiple_of(2 * usize::from(format.channels)) {
                return Err(WavError::PartialSampleFrame {
                    bytes: data.len(),
                    channels: format.channels,
                });
            }
            let samples = data
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            return Ok(Pcm { format, samples });
        }
        let end = body
            .checked_add(size as usize)
            .filter(|&e| e <= bytes.len())
            .ok_or(WavError::Truncated)?;
        if id == b"fmt " {
            if size < 16 {
                return Err(WavError::ShortFormat { size });
            }
            let tag = u16_at(bytes, body);
            // WAVE_FORMAT_PCM, or WAVE_FORMAT_EXTENSIBLE with its subformat
            // GUID starting with the PCM tag.
            let pcm = tag == 1 || (tag == 0xfffe && size >= 40 && u16_at(bytes, body + 24) == 1);
            if !pcm {
                return Err(WavError::NotPcm { tag });
            }
            let bits = u16_at(bytes, body + 14);
            if bits != 16 {
                return Err(WavError::BitsPerSample { bits });
            }
            let channels = u16_at(bytes, body + 2);
            if channels == 0 {
                return Err(WavError::NoChannels);
            }
            format = Some(PcmFormat {
                channels,
                sample_rate: u32_at(bytes, body + 4),
            });
        }
        // Chunks are padded to an even size.
        pos = end + (size as usize & 1);
    }
    Err(WavError::NoData)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(data_size: u32, samples: &[i16]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend(0u32.to_le_bytes());
        b.extend(b"WAVE");
        b.extend(b"LIST");
        b.extend(3u32.to_le_bytes());
        b.extend([1, 2, 3, 0]);
        b.extend(b"fmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(44100u32.to_le_bytes());
        b.extend((44100u32 * 4).to_le_bytes());
        b.extend(4u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend(data_size.to_le_bytes());
        b.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
        b
    }

    #[test]
    fn reads_samples_after_an_odd_sized_chunk() {
        let pcm = parse(&wav(8, &[1, -2, 3, -4])).unwrap();
        assert_eq!(
            pcm,
            Pcm {
                format: PcmFormat {
                    channels: 2,
                    sample_rate: 44100,
                },
                samples: vec![1, -2, 3, -4],
            }
        );
    }

    #[test]
    fn streamed_data_runs_to_the_end() {
        let pcm = parse(&wav(STREAMED_SIZE, &[5, 6])).unwrap();
        assert_eq!(pcm.samples, [5, 6]);
    }

    #[test]
    fn rejects_what_it_cannot_read() {
        assert_eq!(parse(b"RIFF....AVI "), Err(WavError::NotWave));
        assert_eq!(parse(&wav(100, &[1])), Err(WavError::Truncated));
        let mut eight_bit = wav(2, &[1]);
        // bits per sample of the fmt chunk.
        let o = eight_bit.len() - 2 - 8 - 2;
        eight_bit[o] = 8;
        assert_eq!(parse(&eight_bit), Err(WavError::BitsPerSample { bits: 8 }));
        assert_eq!(
            parse(&wav(6, &[1, 2, 3])),
            Err(WavError::PartialSampleFrame {
                bytes: 6,
                channels: 2
            })
        );
        assert_eq!(
            parse(&wav(STREAMED_SIZE, &[1, 2, 3])),
            Err(WavError::PartialSampleFrame {
                bytes: 6,
                channels: 2
            })
        );
    }
}
