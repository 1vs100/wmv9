//! WMA version 2 encoding with one block size, exponents coded with the
//! VLC and no bit reservoir: every packet holds one frame of `frame_len`
//! samples per channel, coded as one block.
//!
//! Packet `k` codes the MDCT of frames `k` and `k + 1` of the input
//! preceded by two frames of silence. A decoder outputs frame `k` of that
//! for packet `k`. The two frames of silence are the delay that FFmpeg's
//! decoder drops (`skip_samples` in wmadec.c), moving its timestamps past
//! them, so a file presents every packet the delay earlier than the
//! samples it codes. That Windows' decoder drops the same is assumed, not
//! verified.

use std::collections::VecDeque;
use std::f64::consts::PI;

use wma_tables as t;

use super::mdct::Mdct;
use super::params::{Channels, CoefTables, Format, FormatError, Params};
use crate::asf::mux::AudioFormat as AsfAudioFormat;
use crate::asf::{Hns, WaveFormatEx};
use crate::wmv3::bits::BitWriter;

/// FORMAT flags of the 10 bytes after WAVEFORMATEX: exponents coded with
/// the VLC; no bit reservoir, no variable block length.
const FLAGS2_EXP_VLC: u16 = 0x0001;

/// WAVE_FORMAT_WMAUDIO2.
const FORMAT_TAG: u16 = 0x0161;

/// The first exponent is coded relative to this value.
const FIRST_EXPONENT_BASE: i32 = 36;
/// Exponents the decoder's power table covers.
const EXPONENT_RANGE: std::ops::RangeInclusive<i32> = -60..=95;
/// Largest step between consecutive exponents the VLC carries.
const EXPONENT_STEP: i32 = 60;
/// How far the quantizer step follows band energy, on a logarithmic
/// scale: 0 gives every band the same step, 1 every band the same signal
/// to noise ratio. A quarter keeps music within 0.2 dB of the best overall
/// ratio while quiet bands get finer steps; equal steps make loud tones
/// exceed the escape code.
const ENERGY_WEIGHT: f64 = 0.25;
/// Exponent of a band without energy.
const SILENT_EXPONENT: i32 = -24;
/// Gains at which the escape code's level width changes; the coded size
/// falls with the gain between them.
const GAIN_RANGES: [std::ops::Range<u32>; 5] =
    [1..15, 15..32, 32..40, 40..45, 45..MAX_TOTAL_GAIN + 1];
/// A gain at which every level of a frame quantizes to zero, leaving the
/// smallest frame the format allows.
const MAX_TOTAL_GAIN: u32 = 512;

/// One coefficient code table with the decoder's derived index table.
struct CoefTable {
    codes: &'static [u32],
    bits: &'static [u8],
    /// Per level from 1: how many runs have their own code.
    levels: &'static [u16],
    /// Per level from 1: the code of run 0.
    first_code: Vec<usize>,
}

impl CoefTable {
    /// The table of channel 0, or with `second` of channel 1, of a pair.
    fn new(pair: CoefTables, second: bool) -> Self {
        let (codes, bits, levels): (&[u32], &[u8], &[u16]) = match (pair, second) {
            (CoefTables::Low, false) => (&t::COEF0_HUFFCODES, &t::COEF0_HUFFBITS, &t::LEVELS0),
            (CoefTables::Low, true) => (&t::COEF1_HUFFCODES, &t::COEF1_HUFFBITS, &t::LEVELS1),
            (CoefTables::Mid, false) => (&t::COEF2_HUFFCODES, &t::COEF2_HUFFBITS, &t::LEVELS2),
            (CoefTables::Mid, true) => (&t::COEF3_HUFFCODES, &t::COEF3_HUFFBITS, &t::LEVELS3),
            (CoefTables::High, false) => (&t::COEF4_HUFFCODES, &t::COEF4_HUFFBITS, &t::LEVELS4),
            (CoefTables::High, true) => (&t::COEF5_HUFFCODES, &t::COEF5_HUFFBITS, &t::LEVELS5),
        };
        // Codes 0 and 1 are the escape and the end of block.
        let mut next = 2;
        let first_code = levels
            .iter()
            .map(|&l| {
                let c = next;
                next += usize::from(l);
                c
            })
            .collect();
        CoefTable {
            codes,
            bits,
            levels,
            first_code,
        }
    }

    fn put(&self, w: &mut BitWriter, code: usize) {
        w.write(u32::from(self.bits[code]), self.codes[code]);
    }
}

/// Bits of a level in the escape code (ff_wma_total_gain_to_bits).
fn escape_level_bits(total_gain: u32) -> u32 {
    match total_gain {
        ..15 => 13,
        15..32 => 12,
        32..40 => 11,
        40..45 => 10,
        _ => 9,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error("{samples} samples do not make whole sample frames of {channels} channels")]
    PartialSampleFrame { samples: usize, channels: usize },
    #[error("no gain fits frame {frame} into {block_align} bytes")]
    FrameTooLarge { frame: u64, block_align: usize },
}

/// Coefficients of one channel of a block.
struct Spectrum {
    coefs: Vec<f64>,
    exponents: Vec<i32>,
}

pub struct Encoder {
    format: Format,
    params: Params,
    mdct: Mdct,
    window: Vec<f64>,
    tables: [CoefTable; 2],
    /// Scaled input not yet coded, per channel.
    pending: Vec<VecDeque<f64>>,
    frames: u64,
}

impl Encoder {
    pub fn new(format: Format) -> Result<Self, FormatError> {
        let params = Params::new(&format)?;
        let n = params.frame_len;
        Ok(Encoder {
            format,
            mdct: Mdct::new(n),
            window: (0..n)
                .map(|i| ((i as f64 + 0.5) * PI / (2.0 * n as f64)).sin())
                .collect(),
            tables: [
                CoefTable::new(params.coef_tables, false),
                CoefTable::new(params.coef_tables, true),
            ],
            // The delay.
            pending: vec![VecDeque::from(vec![0.0; 2 * params.frame_len]); format.channels.count()],
            frames: 0,
            params,
        })
    }

    /// Bytes of every packet.
    pub fn block_align(&self) -> usize {
        self.params.block_align
    }

    /// Samples per channel each packet codes.
    pub fn frame_len(&self) -> usize {
        self.params.frame_len
    }

    /// Samples per channel of decoder output before the first input
    /// sample.
    pub fn delay(&self) -> usize {
        2 * self.params.frame_len
    }

    /// The 10 bytes that follow WAVEFORMATEX.
    pub fn format_data(&self) -> [u8; 10] {
        let mut b = [0; 10];
        b[4..6].copy_from_slice(&FLAGS2_EXP_VLC.to_le_bytes());
        b
    }

    /// The stream's description for an ASF file. The codec name is the
    /// one the Windows Media Format SDK 11 gives WMA 9 Standard.
    pub fn asf_format(&self) -> AsfAudioFormat {
        let f = &self.format;
        let kbps = u64::from(f.byte_rate.0) * 8 / 1000;
        let channels = match f.channels {
            Channels::Mono => "mono",
            Channels::Stereo => "stereo",
        };
        AsfAudioFormat {
            wave: WaveFormatEx {
                format_tag: FORMAT_TAG,
                channels: f.channels.count() as u16,
                samples_per_sec: f.sample_rate.0,
                avg_bytes_per_sec: f.byte_rate.0,
                block_align: u16::try_from(self.params.block_align)
                    .expect("Params limits block_align to 32768"),
                bits_per_sample: 16,
                extra: self.format_data().to_vec(),
            },
            average_time_per_object: Hns(
                self.params.frame_len as u64 * 10_000_000 / u64::from(f.sample_rate.0)
            ),
            codec_name: "Windows Media Audio 9.2".into(),
            codec_description: format!(
                "{kbps} kbps, {} kHz, {channels} 1-pass CBR",
                f.sample_rate.0 / 1000
            ),
            conformance: conformance(f).into(),
        }
    }

    /// Takes interleaved samples and returns the packets they complete.
    pub fn push(&mut self, pcm: &[i16]) -> Result<Vec<Vec<u8>>, EncodeError> {
        let channels = self.format.channels.count();
        if !pcm.len().is_multiple_of(channels) {
            return Err(EncodeError::PartialSampleFrame {
                samples: pcm.len(),
                channels,
            });
        }
        // The decoder's output scale: full scale is 1.0, multiplied by
        // 2 * 32768 / frame_len as FFmpeg's encoder does before the MDCT.
        let scale = 2.0 / self.params.frame_len as f64;
        for frame in pcm.chunks(channels) {
            for (ch, &s) in frame.iter().enumerate() {
                self.pending[ch].push_back(f64::from(s) * scale);
            }
        }
        let mut out = Vec::new();
        while self.pending[0].len() >= 2 * self.params.frame_len {
            out.push(self.packet()?);
        }
        Ok(out)
    }

    /// Codes what is left, completed with silence, up to the packet that
    /// completes the last input frame.
    pub fn finish(mut self) -> Result<Vec<Vec<u8>>, EncodeError> {
        let n = self.params.frame_len;
        let mut out = Vec::new();
        let len = self.pending[0].len().div_ceil(n) * n + n;
        for p in &mut self.pending {
            p.resize(len, 0.0);
        }
        while self.pending[0].len() >= 2 * n {
            out.push(self.packet()?);
        }
        Ok(out)
    }

    /// Codes the first pending frame and drops it.
    fn packet(&mut self) -> Result<Vec<u8>, EncodeError> {
        let spectra = self.spectra();
        let gain = self
            .smallest_gain(&spectra)
            .ok_or(EncodeError::FrameTooLarge {
                frame: self.frames,
                block_align: self.params.block_align,
            })?;
        let mut bytes = self
            .block(&spectra, gain)
            .expect("the search returns a gain whose levels fit")
            .finish();
        bytes.resize(self.params.block_align, 0);
        for p in &mut self.pending {
            p.drain(..self.params.frame_len);
        }
        self.frames += 1;
        Ok(bytes)
    }

    /// Spectra of the first pending frame, mid and side for stereo.
    fn spectra(&self) -> Vec<Spectrum> {
        let n = self.params.frame_len;
        let mut spectra: Vec<Vec<f64>> = self
            .pending
            .iter()
            .map(|p| {
                let block: Vec<f64> = (0..2 * n)
                    .map(|i| {
                        let w = if i < n {
                            self.window[i]
                        } else {
                            self.window[2 * n - 1 - i]
                        };
                        p[i] * w
                    })
                    .collect();
                // The decoder's inverse transform inverts the negated
                // MDCT (measured with FFmpeg's decoder).
                self.mdct.forward(&block).into_iter().map(|c| -c).collect()
            })
            .collect();
        if self.format.channels == Channels::Stereo {
            let (l, r) = spectra.split_at_mut(1);
            for (a, b) in l[0].iter_mut().zip(r[0].iter_mut()) {
                let (m, s) = ((*a + *b) * 0.5, (*a - *b) * 0.5);
                *a = m;
                *b = s;
            }
        }
        spectra
            .into_iter()
            .map(|coefs| Spectrum {
                exponents: self.exponents(&coefs),
                coefs,
            })
            .collect()
    }

    /// The smallest gain at which the frame fits the packet: the first
    /// range of `GAIN_RANGES` that has one, and in it the smallest by
    /// bisection.
    fn smallest_gain(&self, spectra: &[Spectrum]) -> Option<u32> {
        let budget = self.params.block_align * 8;
        let fits = |gain: u32| {
            self.block(spectra, gain)
                .is_some_and(|w| w.bit_len() <= budget)
        };
        GAIN_RANGES.iter().find_map(|r| {
            let (mut lo, mut hi) = (r.start, r.end - 1);
            if !fits(hi) {
                return None;
            }
            while lo < hi {
                let mid = (lo + hi) / 2;
                if fits(mid) {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            Some(hi)
        })
    }

    /// Exponents from the energy of each band, within what the VLC and
    /// the decoder's table carry.
    fn exponents(&self, coefs: &[f64]) -> Vec<i32> {
        let mut out = Vec::with_capacity(self.params.exponent_bands.len());
        let mut last = FIRST_EXPONENT_BASE;
        let mut pos = 0;
        for &size in &self.params.exponent_bands {
            let band = &coefs[pos..pos + size];
            pos += size;
            let rms = (band.iter().map(|c| c * c).sum::<f64>() / size as f64).sqrt();
            let wanted = if rms > 0.0 {
                (16.0 * rms.log10() * ENERGY_WEIGHT).round() as i32
            } else {
                SILENT_EXPONENT
            };
            let e = wanted
                .max(SILENT_EXPONENT)
                .clamp(last - EXPONENT_STEP, last + EXPONENT_STEP)
                .clamp(*EXPONENT_RANGE.start(), *EXPONENT_RANGE.end());
            out.push(e);
            last = e;
        }
        out
    }

    /// The coded block at `total_gain`, `None` when a level exceeds what
    /// the escape code carries.
    fn block(&self, spectra: &[Spectrum], total_gain: u32) -> Option<BitWriter> {
        let p = &self.params;
        let mut w = BitWriter::new();
        if self.format.channels == Channels::Stereo {
            w.bit(true);
        }
        for _ in spectra {
            w.bit(true);
        }
        let mut v = total_gain - 1;
        while v >= 127 {
            w.write(7, 127);
            v -= 127;
        }
        w.write(7, v);
        if let Some(noise) = &p.noise {
            // Every high band is coded, none replaced by noise.
            for _ in spectra {
                for _ in 0..noise.count {
                    w.bit(false);
                }
            }
        }
        for s in spectra {
            let mut last = FIRST_EXPONENT_BASE;
            for &e in &s.exponents {
                let code = usize::try_from(e - last + EXPONENT_STEP).expect("steps are clamped");
                w.write(
                    u32::from(t::AAC_SCALEFACTOR_BITS[code]),
                    t::AAC_SCALEFACTOR_CODE[code],
                );
                last = e;
            }
        }
        let norm = 2.0 / p.frame_len as f64;
        let escape_bits = escape_level_bits(total_gain);
        for (ch, s) in spectra.iter().enumerate() {
            let table =
                &self.tables[usize::from(ch == 1 && self.format.channels == Channels::Stereo)];
            let max_exponent = s
                .exponents
                .iter()
                .map(|&e| exponent_value(e))
                .fold(f64::MIN, f64::max);
            let mult = 10f64.powf(f64::from(total_gain) * 0.05) / max_exponent * norm;
            let mut run = 0usize;
            let mut pos = 0;
            for (&size, &e) in p.exponent_bands.iter().zip(&s.exponents) {
                let step = exponent_value(e) * mult;
                for i in pos..(pos + size).min(p.coefs_end) {
                    let level = (s.coefs[i] / step).round();
                    if level == 0.0 {
                        run += 1;
                        continue;
                    }
                    let magnitude = level.abs() as u32;
                    if magnitude >= 1 << escape_bits {
                        return None;
                    }
                    let m = magnitude as usize;
                    let own = m <= table.levels.len() && run < usize::from(table.levels[m - 1]);
                    if own {
                        table.put(&mut w, table.first_code[m - 1] + run);
                    } else {
                        table.put(&mut w, 0);
                        w.write(escape_bits, magnitude);
                        w.write(p.frame_len_bits, run as u32);
                    }
                    w.bit(level > 0.0);
                    run = 0;
                }
                pos += size;
            }
            if run > 0 {
                table.put(&mut w, 1);
            }
        }
        Some(w)
    }
}

/// Device conformance template of WMA 9 Standard by rate, after
/// Microsoft's documentation of the templates; no sample shows one.
fn conformance(f: &Format) -> &'static str {
    let bps = u64::from(f.byte_rate.0) * 8;
    match () {
        _ if bps <= 64_000 && f.sample_rate.0 <= 44100 => "L1",
        _ if bps <= 160_000 && f.sample_rate.0 <= 48000 => "L2",
        _ => "L3",
    }
}

/// The amplitude an exponent stands for.
fn exponent_value(e: i32) -> f64 {
    10f64.powf(f64::from(e) / 16.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wma::{ByteRate, SampleRate};

    #[test]
    fn asf_description_of_a_64_kbps_stereo_stream() {
        let e = Encoder::new(Format {
            sample_rate: SampleRate(44100),
            channels: Channels::Stereo,
            byte_rate: ByteRate(8000),
        })
        .unwrap();
        let f = e.asf_format();
        assert_eq!(
            f.wave,
            WaveFormatEx {
                format_tag: 0x0161,
                channels: 2,
                samples_per_sec: 44100,
                avg_bytes_per_sec: 8000,
                block_align: 371,
                bits_per_sample: 16,
                extra: vec![0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
            }
        );
        assert_eq!(f.average_time_per_object, Hns(464_399));
        assert_eq!(f.codec_description, "64 kbps, 44 kHz, stereo 1-pass CBR");
        assert_eq!(f.conformance, "L1");
    }

    /// Quiet tones with noise at rates high enough for low gains, which
    /// give levels too large for the narrower escape codes of higher gains.
    #[test]
    fn search_finds_the_smallest_gain_that_fits() {
        let fits = |e: &Encoder, spectra: &[Spectrum], gain| {
            e.block(spectra, gain)
                .is_some_and(|w| w.bit_len() <= e.params.block_align * 8)
        };
        let mut seed = 7u32;
        let mut checked = 0;
        for byte_rate in [12000, 20000, 40000] {
            let mut e = Encoder::new(Format {
                sample_rate: SampleRate(8000),
                channels: Channels::Mono,
                byte_rate: ByteRate(byte_rate),
            })
            .unwrap();
            let n = e.frame_len();
            for freq in [0.003, 0.05, 0.3, 1.2] {
                let pcm = (0..3 * n).map(|i| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = f64::from(seed >> 16) / 65536.0 - 0.5;
                    (i as f64 * freq).sin() * 1000.0 + noise * 6.0
                });
                let scale = 2.0 / n as f64;
                e.pending[0].extend(pcm.map(|s| s.round() * scale));
                while e.pending[0].len() >= 2 * n {
                    let spectra = e.spectra();
                    let scan = (1..=MAX_TOTAL_GAIN).find(|&g| fits(&e, &spectra, g));
                    assert_eq!(
                        e.smallest_gain(&spectra),
                        scan,
                        "{byte_rate} B/s frame {checked}"
                    );
                    checked += 1;
                    for p in &mut e.pending {
                        p.drain(..n);
                    }
                }
            }
        }
        assert!(checked > 20);
    }

    #[test]
    fn partial_sample_frames_are_refused() {
        let mut e = Encoder::new(Format {
            sample_rate: SampleRate(44100),
            channels: Channels::Stereo,
            byte_rate: ByteRate(8000),
        })
        .unwrap();
        assert_eq!(
            e.push(&[1, 2, 3]),
            Err(EncodeError::PartialSampleFrame {
                samples: 3,
                channels: 2
            })
        );
    }
}
