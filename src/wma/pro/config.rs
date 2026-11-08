//! What a WMA Pro decoder derives from the stream header (the 18 bytes after
//! WAVEFORMATEX), the packet size and the sample rate, as FFmpeg's
//! wmaprodec.c derives it.

use wma_tables::pro as t;

/// DecodeFlags of the stream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeFlags(pub u16);

impl DecodeFlags {
    /// Every frame starts with its length.
    pub fn len_prefix(self) -> bool {
        self.0 & 0x40 != 0
    }

    /// Every frame carries a dynamic range control gain.
    pub fn drc(self) -> bool {
        self.0 & 0x80 != 0
    }

    /// log2 of the largest number of subframes per channel.
    pub fn log2_max_subframes(self) -> u32 {
        u32::from((self.0 & 0x38) >> 3)
    }

    /// Change of the frame length, as a power of two.
    fn frame_len_shift(self) -> i32 {
        match self.0 & 0x6 {
            0x2 => 1,
            0x4 => -1,
            0x6 => -2,
            _ => 0,
        }
    }
}

/// The 18 bytes after WAVEFORMATEX.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub bits_per_sample: u16,
    /// Speaker positions, one bit per channel (WAVEFORMATEXTENSIBLE).
    pub channel_mask: u32,
    /// Bytes 6 to 13, which decoders do not read.
    pub reserved: [u8; 8],
    pub decode_flags: DecodeFlags,
    /// Bytes 16 and 17, which decoders do not read.
    pub advanced: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("the stream header has {len} bytes, 18 expected")]
    HeaderLength { len: usize },
    #[error("{bits} bits per sample")]
    BitsPerSample { bits: u16 },
    #[error("channel mask {mask:#x} gives no channels")]
    NoChannels { mask: u32 },
    #[error("{channels} channels; at most 8")]
    Channels { channels: u32 },
    #[error("frames of 2^{bits} samples; 2^6 to 2^13 decode")]
    FrameLength { bits: i32 },
    #[error("{max} subframes of {min} samples per frame")]
    Subframes { max: usize, min: usize },
    #[error("sample rate {rate} is outside 8000 to 192000")]
    SampleRate { rate: u32 },
    #[error("blocks of {block_len} samples have no scale factor bands")]
    NoBands { block_len: usize },
    #[error("block_align {block_align} gives no frame size field, or one over 25 bits")]
    BlockAlign { block_align: usize },
}

impl Header {
    pub fn parse(b: &[u8]) -> Result<Self, ConfigError> {
        if b.len() != 18 {
            return Err(ConfigError::HeaderLength { len: b.len() });
        }
        let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        Ok(Header {
            bits_per_sample: u16_at(0),
            channel_mask: u32::from_le_bytes([b[2], b[3], b[4], b[5]]),
            reserved: b[6..14].try_into().expect("8 bytes"),
            decode_flags: DecodeFlags(u16_at(14)),
            advanced: u16_at(16),
        })
    }

    pub fn to_bytes(&self) -> [u8; 18] {
        let mut b = [0; 18];
        b[0..2].copy_from_slice(&self.bits_per_sample.to_le_bytes());
        b[2..6].copy_from_slice(&self.channel_mask.to_le_bytes());
        b[6..14].copy_from_slice(&self.reserved);
        b[14..16].copy_from_slice(&self.decode_flags.0.to_le_bytes());
        b[16..18].copy_from_slice(&self.advanced.to_le_bytes());
        b
    }
}

/// Smallest and largest block sizes as powers of two.
const BLOCK_MIN_BITS: u32 = 6;
const BLOCK_MAX_BITS: i32 = 13;
/// Scale factor bands at most.
const MAX_BANDS: usize = 29;

/// Stream parameters every part of the bitstream depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub header: Header,
    pub channels: usize,
    /// Samples per channel of a frame.
    pub frame_len: usize,
    /// Width of the frame length prefix and of the packet's count of bits
    /// that end the previous frame.
    pub log2_frame_size: u32,
    pub max_subframes: usize,
    pub min_subframe_len: usize,
    /// Width of a subframe length code.
    pub subframe_len_bits: u32,
    /// A bit says whether a subframe is of the full frame length.
    pub max_subframe_len_bit: bool,
    /// Scale factor band edges per block size index (frame_len >> index),
    /// from 0 to the block size.
    pub bands: Vec<Vec<usize>>,
    /// For block size index `i`, band `b` and saved scale factors of
    /// block size index `x`: the band of the saved factors to take.
    pub resample: Vec<Vec<Vec<usize>>>,
    /// Bytes per packet.
    pub block_align: usize,
}

impl Config {
    pub fn new(header: Header, block_align: usize, sample_rate: u32) -> Result<Self, ConfigError> {
        if !(1..=32).contains(&header.bits_per_sample) {
            return Err(ConfigError::BitsPerSample {
                bits: header.bits_per_sample,
            });
        }
        if !(8000..=192_000).contains(&sample_rate) {
            return Err(ConfigError::SampleRate { rate: sample_rate });
        }
        if block_align == 0 {
            return Err(ConfigError::BlockAlign { block_align });
        }
        let channels = header.channel_mask.count_ones();
        if channels == 0 {
            return Err(ConfigError::NoChannels {
                mask: header.channel_mask,
            });
        }
        if channels > 8 {
            return Err(ConfigError::Channels { channels });
        }
        let log2_frame_size = block_align.ilog2() + 4;
        if log2_frame_size > 25 {
            return Err(ConfigError::BlockAlign { block_align });
        }
        let base = match sample_rate {
            ..=16000 => 9,
            16001..=22050 => 10,
            22051..=48000 => 11,
            48001..=96000 => 12,
            _ => 13,
        };
        let bits = base + header.decode_flags.frame_len_shift();
        if !(BLOCK_MIN_BITS as i32..=BLOCK_MAX_BITS).contains(&bits) {
            return Err(ConfigError::FrameLength { bits });
        }
        let frame_len = 1usize << bits;
        let log2_max = header.decode_flags.log2_max_subframes();
        let max_subframes = 1usize << log2_max;
        let min_subframe_len = frame_len / max_subframes;
        if max_subframes > 32 || min_subframe_len < 1 << BLOCK_MIN_BITS {
            return Err(ConfigError::Subframes {
                max: max_subframes,
                min: min_subframe_len,
            });
        }
        let sizes = log2_max as usize + 1;
        let bands: Vec<Vec<usize>> = (0..sizes)
            .map(|i| -> Result<Vec<usize>, ConfigError> {
                let len = frame_len >> i;
                let mut edges = vec![0];
                for &f in t::CRITICAL_FREQ.iter().take(MAX_BANDS - 1) {
                    let offset = ((len * 2 * usize::from(f)) / sample_rate as usize + 2) & !3;
                    if offset > *edges.last().expect("starts with 0") {
                        edges.push(offset);
                    }
                    if offset >= len {
                        break;
                    }
                }
                // The last edge is the block size; a block without bands
                // past the first edge has none.
                if edges.len() == 1 {
                    return Err(ConfigError::NoBands { block_len: len });
                }
                *edges.last_mut().expect("starts with 0") = len;
                Ok(edges)
            })
            .collect::<Result<_, _>>()?;
        let resample = (0..sizes)
            .map(|i| {
                (0..sizes)
                    .map(|x| {
                        (0..bands[i].len() - 1)
                            .map(|b| {
                                let centre = ((bands[i][b] + bands[i][b + 1] - 1) << i) >> 1;
                                let mut v = 0;
                                while v + 1 < bands[x].len() && bands[x][v + 1] << x < centre {
                                    v += 1;
                                }
                                v
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect();
        Ok(Config {
            header,
            channels: channels as usize,
            frame_len,
            log2_frame_size,
            max_subframes,
            min_subframe_len,
            subframe_len_bits: if log2_max == 0 {
                1
            } else {
                log2_max.ilog2() + 1
            },
            max_subframe_len_bit: max_subframes == 16 || max_subframes == 4,
            bands,
            resample,
            block_align,
        })
    }

    /// Block size index of a subframe length: frame_len >> index.
    pub fn size_index(&self, len: usize) -> usize {
        (self.frame_len / len).ilog2() as usize
    }

    /// The base of the quantizer step: 90 * bits_per_sample / 16.
    pub fn quant_base(&self) -> i32 {
        (90 * i32::from(self.header.bits_per_sample)) >> 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stream header of the SDK 11 samples at 44.1 kHz stereo.
    const SAMPLE_HEADER: [u8; 18] = [0x18, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xe0, 0, 0, 0];

    #[test]
    fn sample_stream_parameters() {
        let h = Header::parse(&SAMPLE_HEADER).unwrap();
        assert_eq!(h.to_bytes(), SAMPLE_HEADER);
        let c = Config::new(h, 5945, 44100).unwrap();
        assert_eq!((c.channels, c.frame_len, c.log2_frame_size), (2, 2048, 16));
        assert_eq!((c.max_subframes, c.min_subframe_len), (16, 128));
        assert_eq!((c.subframe_len_bits, c.max_subframe_len_bit), (3, true));
        assert!(c.header.decode_flags.len_prefix() && c.header.decode_flags.drc());
        assert_eq!(c.quant_base(), 135);
        assert_eq!(c.bands.len(), 5);
        assert_eq!(
            c.bands[4],
            // (128 * 2 * f / 44100 + 2) & !3 over the critical
            // frequencies, worked by hand.
            [0, 4, 8, 12, 16, 20, 24, 32, 36, 44, 56, 68, 88, 120, 128]
        );
        assert!(
            c.bands
                .iter()
                .enumerate()
                .all(|(i, b)| *b.last().unwrap() == 2048 >> i)
        );
    }
}
