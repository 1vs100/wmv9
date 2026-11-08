//! Rate control: a quantizer per picture that keeps the stream within a
//! leaky bucket drained at the target bit rate.
//!
//! Picture sizes follow `bits = scale * step^-exponent` per picture kind.
//! The I picture scale is that of the last I picture, the P picture scale
//! a running mean. A picture takes the finest quantizer whose predicted
//! size, followed by the bucket's length of P pictures at the same
//! quantizer, spends no more than the target over the stream so far, and
//! which the bucket holds. Bits left unspent count for later pictures up
//! to the size of the bucket. A picture whose size shows the choice was
//! wrong is coded again.

use std::num::NonZeroU32;

use crate::asf::{Hns, Millis};
use crate::wmv3::{PqIndex, QuantizerMode, pquant};

/// Frames per second as a fraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRate {
    num: u64,
    den: u64,
}

impl FrameRate {
    const MAX_TERM: u64 = 1_000_000;
    const MAX_PER_SECOND: u64 = 240;

    /// `num / den` frames per second, 1 to 240, with terms up to 1000000.
    pub fn new(num: u64, den: u64) -> Option<Self> {
        let ok = (1..=Self::MAX_TERM).contains(&num)
            && (1..=Self::MAX_TERM).contains(&den)
            && num >= den
            && num <= Self::MAX_PER_SECOND * den;
        ok.then_some(FrameRate { num, den })
    }

    /// Display time of frame `i`, rounded to the nearest ms.
    pub fn ms(self, i: u64) -> Millis {
        let t = (u128::from(i) * 1000 * u128::from(self.den) + u128::from(self.num) / 2)
            / u128::from(self.num);
        Millis(u64::try_from(t).expect("bounded terms keep times within 64 bits"))
    }

    /// Frame duration in 100 ns units, rounded.
    pub fn frame_time(self) -> Hns {
        Hns((10_000_000 * self.den + self.num / 2) / self.num)
    }

    /// Frames in `den` seconds.
    pub fn num(self) -> u64 {
        self.num
    }

    pub fn den(self) -> u64 {
        self.den
    }

    fn per_second(self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

/// As `num/den`, the form FFmpeg's options take.
impl std::fmt::Display for FrameRate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

/// What the stream keeps to: a bit rate of the coded pictures, without
/// the container's overhead, and a bucket of `buffer` at that rate. A
/// decoder that receives the pictures at the rate and waits `buffer`
/// before it plays has every picture in time, unless a picture exceeds
/// the bucket even at PQINDEX 31 (`Encoder::buffer_overflows`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateTarget {
    bits_per_second: NonZeroU32,
    frame_rate: FrameRate,
    buffer: Millis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RateError {
    #[error("a buffer of {buffer:?} is shorter than one frame time of {frame:?}")]
    BufferTooShort { buffer: Millis, frame: Hns },
}

impl RateTarget {
    /// The buffer holds at least one frame time.
    pub fn new(
        bits_per_second: NonZeroU32,
        frame_rate: FrameRate,
        buffer: Millis,
    ) -> Result<Self, RateError> {
        let frame = frame_rate.frame_time();
        if buffer.0 * 10_000 < frame.0 {
            return Err(RateError::BufferTooShort { buffer, frame });
        }
        Ok(RateTarget {
            bits_per_second,
            frame_rate,
            buffer,
        })
    }
}

/// Picture kinds with their own size model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Intra,
    Inter,
}

impl Kind {
    /// How fast the size falls with the quantizer step, measured on the
    /// hvideo15_1 sample over PQINDEX 2 to 31. I pictures carry DC and
    /// header bits that the step changes less.
    fn exponent(self) -> f64 {
        match self {
            Kind::Intra => 0.7,
            Kind::Inter => 1.1,
        }
    }
}

/// P picture size over I picture size at the same quantizer, for a kind
/// without a measurement: a quarter at PQINDEX 8 on hvideo15_1.
const INTER_PER_INTRA: f64 = 0.25;
/// Quantizer of the first picture, before any size is known: the middle
/// of the uniform quantizer range.
const FIRST: u8 = 8;
/// Weight of the newest P picture in the P size model. A P picture costs
/// less after a finely quantized reference and more after a coarse one,
/// so single sizes swing more than the content does.
const INTER_WEIGHT: f64 = 0.3;

/// Size of a coded picture in bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Bits(pub(super) usize);

/// One coding of a picture.
pub(super) struct Attempt {
    /// The kind the quantizer was chosen for.
    pub(super) expected: Kind,
    /// The kind the picture came out as: an I picture at a scene cut.
    pub(super) kind: Kind,
    pub(super) pq: PqIndex,
    pub(super) bits: Bits,
}

#[derive(Clone)]
pub(super) struct RateControl {
    mode: QuantizerMode,
    frame_bits: f64,
    bucket_bits: f64,
    /// Pictures in the bucket's length, at least one: the choice evens
    /// out the bits spent beyond the target over this many, and the bucket
    /// holds what it spends ahead.
    horizon: f64,
    /// Bits in the bucket after the last picture.
    level: f64,
    /// Bits spent beyond the target so far, at least minus the bucket.
    balance: f64,
    intra_scale: Option<f64>,
    inter_scale: Option<f64>,
    /// Pictures the bucket did not hold even at PQINDEX 31.
    overflows: u64,
}

impl RateControl {
    pub(super) fn new(target: &RateTarget, mode: QuantizerMode) -> Self {
        let rate = f64::from(target.bits_per_second.get());
        let fps = target.frame_rate.per_second();
        RateControl {
            mode,
            frame_bits: rate / fps,
            bucket_bits: rate * target.buffer.0 as f64 / 1000.0,
            horizon: (fps * target.buffer.0 as f64 / 1000.0).max(1.0),
            level: 0.0,
            balance: 0.0,
            intra_scale: None,
            inter_scale: None,
            overflows: 0,
        }
    }

    pub(super) fn overflows(&self) -> u64 {
        self.overflows
    }

    /// Effective quantizer step: twice PQUANT, three times with the
    /// non-uniform quantizer's dead zone.
    fn step(&self, pq: PqIndex) -> f64 {
        let pquant = f64::from(pquant(pq, self.mode).get());
        let uniform = match self.mode {
            QuantizerMode::Implicit => pq.get() < 9,
            // With Explicit the encoder sets PQUANTIZER.
            QuantizerMode::Explicit | QuantizerMode::Uniform => true,
            QuantizerMode::NonUniform => false,
        };
        if uniform { 2.0 * pquant } else { 3.0 * pquant }
    }

    fn scale(&self, kind: Kind) -> Option<f64> {
        match kind {
            Kind::Intra => self.intra_scale,
            Kind::Inter => self.inter_scale,
        }
    }

    /// The model scale a picture of `bits` coded at `pq` shows.
    fn measure(&self, kind: Kind, pq: PqIndex, bits: Bits) -> f64 {
        bits.0 as f64 * self.step(pq).powf(kind.exponent())
    }

    /// Predicted size of a picture, from its own kind or, without one,
    /// from the other.
    fn predict(&self, kind: Kind, pq: PqIndex) -> Option<f64> {
        let s = self.step(pq);
        let own = |k: Kind| self.scale(k).map(|c| c * s.powf(-k.exponent()));
        own(kind).or_else(|| match kind {
            Kind::Intra => own(Kind::Inter).map(|b| b / INTER_PER_INTRA),
            Kind::Inter => own(Kind::Intra).map(|b| b * INTER_PER_INTRA),
        })
    }

    fn room(&self) -> f64 {
        self.bucket_bits - self.level
    }

    /// The quantizer for a picture of `kind`.
    pub(super) fn choose(&self, kind: Kind) -> PqIndex {
        if self.intra_scale.is_none() && self.inter_scale.is_none() {
            return PqIndex::new(FIRST).expect("in range");
        }
        let all = || (1..=31).filter_map(PqIndex::new);
        let held = |q: &PqIndex| self.predict(kind, *q).is_some_and(|b| b <= self.room());
        let on_target = |q: &PqIndex| {
            let (Some(this), Some(next)) = (self.predict(kind, *q), self.predict(Kind::Inter, *q))
            else {
                return false;
            };
            self.balance + this + (self.horizon - 1.0) * next - self.horizon * self.frame_bits
                <= 0.0
        };
        let coarsest = PqIndex::new(31).expect("in range");
        all().find(|q| held(q) && on_target(q)).unwrap_or(coarsest)
    }

    /// The quantizer to code a picture again at: a coarser one when the
    /// bucket does not hold it, and on a first attempt that came out as
    /// another kind or as the first of its kind, the choice its size
    /// gives. The choice is made again at most once and otherwise only
    /// becomes coarser, so the attempts end.
    pub(super) fn retry(&self, a: &Attempt, first_attempt: bool) -> Option<PqIndex> {
        if !self.holds(a.bits) {
            // This picture's own size predicts its size at other
            // quantizers.
            let scale = self.measure(a.kind, a.pq, a.bits);
            return (a.pq.get() + 1..=31)
                .filter_map(PqIndex::new)
                .find(|q| scale * self.step(*q).powf(-a.kind.exponent()) <= self.room())
                .or(PqIndex::new(31).filter(|&last| last != a.pq));
        }
        let unexpected = a.kind != a.expected || self.scale(a.kind).is_none();
        if first_attempt && unexpected {
            let mut trial = self.clone();
            trial.absorb(a);
            let again = trial.choose(a.kind);
            return (again != a.pq).then_some(again);
        }
        None
    }

    /// Whether a picture of `bits` fits the bucket.
    fn holds(&self, bits: Bits) -> bool {
        bits.0 as f64 <= self.room()
    }

    /// Adds the picture to the bucket, counting it when the bucket does
    /// not hold it, and to the models, and drains one picture time.
    pub(super) fn commit(&mut self, a: &Attempt) {
        if !self.holds(a.bits) {
            self.overflows += 1;
        }
        self.absorb(a);
        let over = a.bits.0 as f64 - self.frame_bits;
        self.level = (self.level + over).max(0.0);
        self.balance = (self.balance + over).max(-self.bucket_bits);
    }

    /// Adds the picture to the size models.
    fn absorb(&mut self, a: &Attempt) {
        let scale = self.measure(a.kind, a.pq, a.bits);
        match a.kind {
            Kind::Intra if a.expected == Kind::Inter => {
                // A scene cut: the P pictures after it code the new content.
                self.intra_scale = Some(scale);
                self.inter_scale = None;
            }
            Kind::Intra => self.intra_scale = Some(scale),
            Kind::Inter => {
                self.inter_scale = Some(match self.inter_scale {
                    Some(old) => old + (scale - old) * INTER_WEIGHT,
                    None => scale,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pictures whose size follows the quantizer with other exponents than
    /// the model's, with an I picture every 5 seconds, and content that
    /// gets four times harder from second 10 to 20: the bucket never
    /// overflows, each 10 seconds of the same content keep to the target
    /// within 10%, and the whole within 5%.
    #[test]
    fn rate_follows_the_target_through_changes_of_content() {
        const RATE: f64 = 300_000.0;
        let target = RateTarget::new(
            NonZeroU32::new(RATE as u32).unwrap(),
            FrameRate::new(30, 1).unwrap(),
            Millis(3000),
        )
        .unwrap();
        let mut rc = RateControl::new(&target, QuantizerMode::Implicit);
        let size = |kind: Kind, pq: PqIndex, hard: f64, rc: &RateControl| {
            let s = rc.step(pq);
            let bits = match kind {
                Kind::Intra => 400_000.0 * s.powf(-0.6),
                Kind::Inter => 150_000.0 * s.powf(-1.3),
            };
            Bits((bits * hard) as usize)
        };
        let mut seconds = vec![0usize; 30];
        for t in 0..900 {
            let hard = if (300..600).contains(&t) { 4.0 } else { 1.0 };
            let kind = if t % 150 == 0 {
                Kind::Intra
            } else {
                Kind::Inter
            };
            let mut pq = rc.choose(kind);
            let mut bits = size(kind, pq, hard, &rc);
            let mut first_attempt = true;
            while let Some(again) = rc.retry(
                &Attempt {
                    expected: kind,
                    kind,
                    pq,
                    bits,
                },
                first_attempt,
            ) {
                pq = again;
                bits = size(kind, pq, hard, &rc);
                first_attempt = false;
            }
            assert!(rc.holds(bits), "picture {t} of {bits:?} overflows");
            rc.commit(&Attempt {
                expected: kind,
                kind,
                pq,
                bits,
            });
            seconds[t / 30] += bits.0;
        }
        for (segment, bits) in seconds.chunks(10).enumerate() {
            let rate = bits.iter().sum::<usize>() as f64 / 10.0;
            assert!(
                (rate - RATE).abs() < RATE * 0.1,
                "seconds {}0 to {}9: {rate} bits per second",
                segment,
                segment
            );
        }
        let average = seconds.iter().sum::<usize>() as f64 / 30.0;
        assert!(
            (average - RATE).abs() < RATE * 0.05,
            "{average} bits per second"
        );
    }

    /// P pictures that cost more after a coarser reference and less after
    /// a finer one, as coded pictures do: the quantizer holds steady
    /// instead of swinging between fine and coarse.
    #[test]
    fn quantizer_holds_steady_when_size_follows_the_reference() {
        let target = RateTarget::new(
            NonZeroU32::new(300_000).unwrap(),
            FrameRate::new(30, 1).unwrap(),
            Millis(3000),
        )
        .unwrap();
        let mut rc = RateControl::new(&target, QuantizerMode::Implicit);
        let mut reference_step: Option<f64> = None;
        let mut quantizers = Vec::new();
        for t in 0..300 {
            let kind = if t == 0 { Kind::Intra } else { Kind::Inter };
            let mut pq = rc.choose(kind);
            let size = |pq: PqIndex| {
                let s = rc.step(pq);
                let bits = match (kind, reference_step) {
                    (Kind::Inter, Some(r)) => 100_000.0 * s.powf(-1.1) * (r / s).powf(1.5),
                    _ => 400_000.0 * s.powf(-0.7),
                };
                Bits(bits as usize)
            };
            let mut bits = size(pq);
            let mut first_attempt = true;
            while let Some(again) = rc.retry(
                &Attempt {
                    expected: kind,
                    kind,
                    pq,
                    bits,
                },
                first_attempt,
            ) {
                pq = again;
                bits = size(pq);
                first_attempt = false;
            }
            rc.commit(&Attempt {
                expected: kind,
                kind,
                pq,
                bits,
            });
            reference_step = Some(rc.step(pq));
            quantizers.push(pq.get());
        }
        // After two seconds the quantizer stays within adjacent indexes,
        // alternating between them to meet the rate.
        let steady = &quantizers[60..];
        let (lo, hi) = (steady.iter().min().unwrap(), steady.iter().max().unwrap());
        assert!(hi - lo <= 1, "PQINDEX from {lo} to {hi}: {quantizers:?}");
    }

    /// A picture the bucket holds, of the kind its quantizer was chosen
    /// for, is kept. One it does not hold is coded again more coarsely, at
    /// a quantizer its size predicts to fit, and at PQINDEX 31 it is kept.
    #[test]
    fn pictures_the_bucket_cannot_hold_are_coded_more_coarsely() {
        let target = RateTarget::new(
            NonZeroU32::new(300_000).unwrap(),
            FrameRate::new(30, 1).unwrap(),
            Millis(1000),
        )
        .unwrap();
        let mut rc = RateControl::new(&target, QuantizerMode::Implicit);
        let pq = |v| PqIndex::new(v).unwrap();
        // 300000 bits of bucket; 100000 bits of I picture leave 90000 in it
        // after one picture time.
        rc.commit(&Attempt {
            expected: Kind::Intra,
            kind: Kind::Intra,
            pq: pq(8),
            bits: Bits(100_000),
        });
        rc.commit(&Attempt {
            expected: Kind::Inter,
            kind: Kind::Inter,
            pq: pq(8),
            bits: Bits(10_000),
        });
        assert_eq!(rc.room(), 210_000.0);
        assert_eq!(
            rc.retry(
                &Attempt {
                    expected: Kind::Inter,
                    kind: Kind::Inter,
                    pq: pq(8),
                    bits: Bits(200_000)
                },
                false
            ),
            None
        );
        let again = rc
            .retry(
                &Attempt {
                    expected: Kind::Inter,
                    kind: Kind::Inter,
                    pq: pq(8),
                    bits: Bits(250_000),
                },
                false,
            )
            .expect("coded again");
        assert!(again > pq(8));
        let predicted = 250_000.0 * (rc.step(pq(8)) / rc.step(again)).powf(1.1);
        assert!(
            predicted <= rc.room(),
            "{predicted} bits predicted at {again:?}"
        );
        assert_eq!(
            rc.retry(
                &Attempt {
                    expected: Kind::Inter,
                    kind: Kind::Inter,
                    pq: pq(31),
                    bits: Bits(250_000)
                },
                false
            ),
            None
        );
    }

    fn target_1s() -> RateTarget {
        RateTarget::new(
            NonZeroU32::new(300_000).unwrap(),
            FrameRate::new(30, 1).unwrap(),
            Millis(1000),
        )
        .unwrap()
    }

    /// Codes `count` pictures of `kind` whose size is `scale * step^-e`
    /// with the model's own exponent, keeping the bucket the decoder sees.
    /// Returns the overflow retries.
    fn run(
        rc: &mut RateControl,
        decoder_level: &mut f64,
        kind: Kind,
        scale: f64,
        count: usize,
    ) -> usize {
        let mut retries = 0;
        for t in 0..count {
            let size =
                |rc: &RateControl, pq| Bits((scale * rc.step(pq).powf(-kind.exponent())) as usize);
            let mut pq = rc.choose(kind);
            let mut first_attempt = true;
            loop {
                let a = Attempt {
                    expected: kind,
                    kind,
                    pq,
                    bits: size(rc, pq),
                };
                match rc.retry(&a, first_attempt) {
                    Some(again) => {
                        retries += usize::from(!rc.holds(a.bits));
                        pq = again;
                    }
                    None => {
                        *decoder_level += a.bits.0 as f64;
                        assert!(
                            *decoder_level <= rc.bucket_bits,
                            "picture {t}: {decoder_level} bits in the bucket"
                        );
                        *decoder_level = (*decoder_level - rc.frame_bits).max(0.0);
                        rc.commit(&a);
                        break;
                    }
                }
                first_attempt = false;
            }
        }
        retries
    }

    /// Ten seconds of pictures far below the target, then hard ones: the
    /// unspent bits are spent within the bucket, and the hard pictures
    /// rarely need a second coding.
    #[test]
    fn a_burst_after_easy_content_stays_within_the_bucket() {
        let mut rc = RateControl::new(&target_1s(), QuantizerMode::Implicit);
        let mut level = 0.0;
        run(&mut rc, &mut level, Kind::Inter, 1_000.0, 300);
        let retries = run(&mut rc, &mut level, Kind::Inter, 1_000_000.0, 300);
        assert!(retries <= 5, "{retries} pictures coded again");
    }

    /// With unspent bits to use, the first choice for a large I picture
    /// already fits the bucket.
    #[test]
    fn a_first_choice_fits_the_bucket() {
        let mut rc = RateControl::new(&target_1s(), QuantizerMode::Implicit);
        let mut level = 0.0;
        run(&mut rc, &mut level, Kind::Inter, 1_000.0, 300);
        let i_scale = 4_000_000.0;
        rc.intra_scale = Some(i_scale);
        let pq = rc.choose(Kind::Intra);
        let bits = i_scale * rc.step(pq).powf(-Kind::Intra.exponent());
        assert!(
            bits <= rc.room(),
            "{bits} bits at {pq:?}, room {}",
            rc.room()
        );
    }

    #[test]
    fn buffers_shorter_than_a_frame_are_refused() {
        let rate = NonZeroU32::new(300_000).unwrap();
        let fps = FrameRate::new(30, 1).unwrap();
        assert_eq!(
            RateTarget::new(rate, fps, Millis(33)),
            Err(RateError::BufferTooShort {
                buffer: Millis(33),
                frame: Hns(333_333)
            })
        );
        assert!(RateTarget::new(rate, fps, Millis(34)).is_ok());
    }

    #[test]
    fn pictures_over_the_bucket_at_pqindex_31_are_counted() {
        let mut rc = RateControl::new(&target_1s(), QuantizerMode::Implicit);
        let a = Attempt {
            expected: Kind::Intra,
            kind: Kind::Intra,
            pq: PqIndex::new(31).unwrap(),
            bits: Bits(400_000),
        };
        assert_eq!(rc.retry(&a, false), None);
        rc.commit(&a);
        assert_eq!(rc.overflows(), 1);
    }
}
