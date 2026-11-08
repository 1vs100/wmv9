//! Stream parameters a WMA version 2 decoder derives from the format
//! header, as ff_wma_init computes them for a stream with one block size.

use wma_tables as t;

/// Channel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    Mono,
    Stereo,
}

impl Channels {
    pub fn count(self) -> usize {
        match self {
            Channels::Mono => 1,
            Channels::Stereo => 2,
        }
    }
}

/// Samples per second per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SampleRate(pub u32);

/// Bytes per second of a coded stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteRate(pub u32);

/// What the WAVEFORMATEX of a stream declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub sample_rate: SampleRate,
    pub channels: Channels,
    /// nAvgBytesPerSec. Decoders take the bit rate as eight times this
    /// and choose tables from it, so it is the exact rate of the stream.
    pub byte_rate: ByteRate,
}

/// Sample rates of the WMA version 2 rate tables.
const SAMPLE_RATES: std::ops::RangeInclusive<u32> = 8000..=48000;
/// 24 kbps, the lowest rate the encoder is tested at.
const MIN_BYTE_RATE: ByteRate = ByteRate(3000);
/// Largest packet FFmpeg's encoder and decoder accept
/// (MAX_CODED_SUPERFRAME_SIZE).
const MAX_BLOCK_ALIGN: u64 = 32768;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    #[error("sample rate {} is outside 8000 to 48000", rate.0)]
    SampleRate { rate: SampleRate },
    #[error("{} bytes per second is outside {} to {}", rate.0, min.0, max.0)]
    ByteRate {
        rate: ByteRate,
        min: ByteRate,
        max: ByteRate,
    },
}

/// Bands of the coefficients above the noise coding start frequency, which
/// a stream with noise coding flags one by one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoiseBands {
    pub(crate) count: usize,
}

/// Pair of coefficient code tables, for channel 0 and channel 1, by the
/// rate range ff_wma_init selects it for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoefTables {
    /// Tables 0 and 1.
    Low,
    /// Tables 2 and 3.
    Mid,
    /// Tables 4 and 5.
    High,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Params {
    pub(crate) frame_len_bits: u32,
    pub(crate) frame_len: usize,
    /// Sizes of the exponent bands; they cover the whole block.
    pub(crate) exponent_bands: Vec<usize>,
    /// Coefficients transmitted per block; the rest are zero.
    pub(crate) coefs_end: usize,
    pub(crate) noise: Option<NoiseBands>,
    pub(crate) coef_tables: CoefTables,
    /// Bytes of every packet, each holding one frame.
    pub(crate) block_align: usize,
}

impl Params {
    pub(crate) fn new(f: &Format) -> Result<Self, FormatError> {
        let sample_rate = f.sample_rate.0;
        if !SAMPLE_RATES.contains(&sample_rate) {
            return Err(FormatError::SampleRate {
                rate: f.sample_rate,
            });
        }
        let frame_len_bits = match sample_rate {
            ..=16000 => 9,
            16001..=22050 => 10,
            _ => 11,
        };
        let frame_len = 1usize << frame_len_bits;
        // block_align = byte_rate * frame_len / sample_rate.
        let max = ByteRate(
            u32::try_from(((MAX_BLOCK_ALIGN + 1) * u64::from(sample_rate) - 1) / frame_len as u64)
                .expect("below 2^32 for every sample rate"),
        );
        if f.byte_rate < MIN_BYTE_RATE || f.byte_rate > max {
            return Err(FormatError::ByteRate {
                rate: f.byte_rate,
                min: MIN_BYTE_RATE,
                max,
            });
        }
        let channels = f.channels.count() as u32;
        let bit_rate = u64::from(f.byte_rate.0) * 8;

        // ff_wma_init keeps these in single precision and compares them
        // with double constants, so the comparisons are made in double.
        let bps = bit_rate as f32 / (channels * sample_rate) as f32;
        let bps1 = if channels == 2 {
            (f64::from(bps) * 1.6) as f32
        } else {
            bps
        };
        let (bps, bps1) = (f64::from(bps), f64::from(bps1));
        let normalized = match sample_rate {
            44100.. => 44100,
            22050.. => 22050,
            16000.. => 16000,
            11025.. => 11025,
            _ => 8000,
        };
        let high_freq_factor = match normalized {
            44100 if bps1 >= 0.61 => None,
            44100 => Some(0.4),
            22050 if bps1 >= 1.16 => None,
            22050 if bps1 >= 0.72 => Some(0.7),
            22050 => Some(0.6),
            16000 if bps > 0.5 => Some(0.5),
            16000 => Some(0.3),
            11025 => Some(0.7),
            _ if bps <= 0.625 => Some(0.5),
            _ if bps > 0.75 => None,
            _ => Some(0.65),
        };

        let exponent_bands = exponent_bands(frame_len, sample_rate);
        let coefs_end = frame_len - frame_len * 9 / 100;
        let noise = high_freq_factor.map(|factor: f64| {
            let nyquist = (f64::from(sample_rate) * 0.5) as f32;
            let high_freq = (f64::from(nyquist) * factor) as f32;
            let start =
                (f64::from(frame_len as f32 * 2.0 * high_freq / sample_rate as f32) + 0.5) as usize;
            let mut pos = 0;
            let mut count = 0;
            for &size in &exponent_bands {
                let (lo, hi) = (pos.max(start), (pos + size).min(coefs_end));
                pos += size;
                if hi > lo {
                    count += 1;
                }
            }
            NoiseBands { count }
        });
        let coef_tables = match () {
            _ if sample_rate >= 32000 && bps1 < 0.72 => CoefTables::Low,
            _ if sample_rate >= 32000 && bps1 < 1.16 => CoefTables::Mid,
            _ => CoefTables::High,
        };
        let block_align = (bit_rate * frame_len as u64 / (u64::from(sample_rate) * 8)) as usize;
        Ok(Params {
            frame_len_bits,
            frame_len,
            exponent_bands,
            coefs_end,
            noise,
            coef_tables,
            block_align,
        })
    }
}

/// Band sizes from the critical frequencies, in multiples of four
/// coefficients.
fn exponent_bands(block_len: usize, sample_rate: u32) -> Vec<usize> {
    let b = sample_rate as usize;
    let mut bands = Vec::new();
    let mut lpos = 0;
    for &a in &t::WMA_CRITICAL_FREQS {
        let pos = (((block_len * 2 * usize::from(a)) + (b << 1)) / (4 * b) * 4).min(block_len);
        if pos > lpos {
            bands.push(pos - lpos);
        }
        if pos >= block_len {
            break;
        }
        lpos = pos;
    }
    bands
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(sample_rate: u32, channels: Channels, byte_rate: u32) -> Format {
        Format {
            sample_rate: SampleRate(sample_rate),
            channels,
            byte_rate: ByteRate(byte_rate),
        }
    }

    fn params(sample_rate: u32, channels: Channels, byte_rate: u32) -> Params {
        Params::new(&format(sample_rate, channels, byte_rate)).unwrap()
    }

    #[test]
    fn stereo_44100_at_64_kbps() {
        let p = params(44100, Channels::Stereo, 8000);
        assert_eq!(p.frame_len, 2048);
        assert_eq!(p.exponent_bands.iter().sum::<usize>(), 2048);
        assert_eq!(p.coefs_end, 1864);
        // bps1 = 64000 / 88200 * 1.6 = 1.161: no noise coding, table 2.
        assert_eq!(p.noise, None);
        assert_eq!(p.coef_tables, CoefTables::High);
        assert_eq!(p.block_align, 371);
    }

    struct Expected {
        sample_rate: u32,
        channels: Channels,
        byte_rate: u32,
        noise: bool,
        tables: CoefTables,
    }

    fn check(cases: &[Expected]) {
        for e in cases {
            let p = params(e.sample_rate, e.channels, e.byte_rate);
            assert_eq!(
                (p.noise.is_some(), p.coef_tables),
                (e.noise, e.tables),
                "{} Hz {:?} {} B/s",
                e.sample_rate,
                e.channels,
                e.byte_rate
            );
        }
    }

    #[test]
    fn rates_select_noise_coding_and_tables() {
        check(&[
            // bps = 24000 / 22050 = 1.09: between 0.72 and 1.16.
            Expected {
                sample_rate: 22050,
                channels: Channels::Mono,
                byte_rate: 3000,
                noise: true,
                tables: CoefTables::High,
            },
            // bps1 = 32000 / 88200 * 1.6 = 0.58.
            Expected {
                sample_rate: 44100,
                channels: Channels::Stereo,
                byte_rate: 4000,
                noise: true,
                tables: CoefTables::Low,
            },
            // bps = 1.0, normalized to 22050 Hz.
            Expected {
                sample_rate: 32000,
                channels: Channels::Mono,
                byte_rate: 4000,
                noise: true,
                tables: CoefTables::Mid,
            },
            // bps = 3.0.
            Expected {
                sample_rate: 8000,
                channels: Channels::Mono,
                byte_rate: 3000,
                noise: false,
                tables: CoefTables::High,
            },
        ]);
    }

    #[test]
    fn rate_thresholds_compare_as_ffmpeg_does() {
        // bps1 is 1.15999997 in single precision: below the double 1.16 that
        // ff_wma_init compares with, equal to 1.16 in single precision.
        // Expected values from ff_wma_init's arithmetic compiled in C.
        let mono = |sample_rate, byte_rate, noise, tables| Expected {
            sample_rate,
            channels: Channels::Mono,
            byte_rate,
            noise,
            tables,
        };
        check(&[
            mono(25000, 3625, true, CoefTables::High),
            mono(24000, 3480, true, CoefTables::High),
            mono(32000, 4640, true, CoefTables::Mid),
            mono(48000, 6960, false, CoefTables::Mid),
        ]);
    }

    #[test]
    fn rates_outside_the_tables_are_refused() {
        let refused =
            |sample_rate, byte_rate| Params::new(&format(sample_rate, Channels::Mono, byte_rate));
        assert_eq!(
            refused(7999, 8000),
            Err(FormatError::SampleRate {
                rate: SampleRate(7999)
            })
        );
        // 32768 bytes per 512 samples at 8000 Hz.
        let limits = |rate| FormatError::ByteRate {
            rate: ByteRate(rate),
            min: ByteRate(3000),
            max: ByteRate(512_015),
        };
        assert_eq!(refused(8000, 2999), Err(limits(2999)));
        assert_eq!(refused(8000, 512_016), Err(limits(512_016)));
        assert_eq!(params(8000, Channels::Mono, 512_015).block_align, 32768);
    }
}
