use super::*;
use crate::wma::pro::config::{DecodeFlags, Header};

/// A small deterministic generator (xorshift).
struct Rng(u64);

/// Width of a fill count.
fn fill_width(count: u32) -> u32 {
    if count == 0 { 0 } else { count.ilog2() + 1 }
}

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// 0 to n - 1.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// True with probability 1 / n.
    fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    fn bits(&mut self, n: usize) -> Vec<bool> {
        (0..n).map(|_| self.one_in(2)).collect()
    }
}

fn config(mask: u32, flags: u16, block_align: usize, rate: u32) -> Config {
    let header = Header {
        bits_per_sample: 16,
        channel_mask: mask,
        reserved: [0; 8],
        decode_flags: DecodeFlags(flags),
        advanced: 0,
    };
    Config::new(header, block_align, rate).unwrap()
}

/// Mono, stereo as in the samples, 3 and 6 channels, 1 and 2 subframes,
/// and frames without a length prefix.
fn configs() -> Vec<Config> {
    vec![
        config(0x4, 0xe0, 70_000, 44100),
        config(0x3, 0xe0, 70_000, 44100),
        config(0x7, 0x60, 70_000, 22050),
        config(0x3f, 0x58, 70_000, 48000),
        config(0x3, 0x48, 70_000, 96000),
        config(0x3, 0x40, 70_000, 32000),
        config(0x3, 0x10, 70_000, 44100),
    ]
}

/// A random partition of the frame into allowed subframe lengths.
fn partition(cfg: &Config, rng: &mut Rng) -> Vec<usize> {
    let sizes = cfg.header.decode_flags.log2_max_subframes() as usize + 1;
    let mut rest = cfg.frame_len;
    let mut out = Vec::new();
    while rest > 0 {
        let fits: Vec<usize> = (0..sizes)
            .map(|k| cfg.frame_len >> k)
            .filter(|&l| l <= rest)
            .collect();
        let len = fits[rng.below(fits.len())];
        out.push(len);
        rest -= len;
    }
    out
}

fn magnitude(rng: &mut Rng) -> i32 {
    match rng.below(20) {
        0 => 1 + rng.below(1 << 20) as i32,
        1..=3 => 1 + rng.below(200) as i32,
        _ => 1 + rng.below(15) as i32,
    }
}

/// A random frame the writer must take and the reader give back.
fn random_frame(codec: &Codec, rng: &mut Rng) -> Frame {
    let cfg = &codec.cfg;
    let n = cfg.channels;
    let first = partition(cfg, rng);
    let tiling = Tiling(
        (0..n)
            .map(|_| {
                if rng.one_in(2) {
                    first.clone()
                } else {
                    partition(cfg, rng)
                }
            })
            .collect(),
    );
    let shared = tiling.0.iter().all(|t| *t == tiling.0[0]);
    let same_tiling = cfg.max_subframes == 1 || shared && rng.one_in(2);
    let mut state = new_state(n);
    let mut subframes = Vec::new();
    while let Some(shape) = next_subframe(&tiling, &mut state) {
        let len = shape.len;
        let num_bands = codec.num_bands(len);
        let count = shape.channels.len();
        let fill = match rng.below(8) {
            0 => {
                let count = 1 + rng.below(3);
                Some(Fill::Short(rng.bits(count)))
            }
            1 => {
                let bits = 1 + rng.below(40);
                let width = (fill_width(bits as u32 - 1) + rng.below(2) as u32).min(15) as u8;
                Some(Fill::Long {
                    width,
                    bits: rng.bits(bits),
                })
            }
            _ => None,
        };
        let mut groups = Vec::new();
        if n > 1 {
            let mut grouped = vec![false; count];
            let mut remaining = count;
            while remaining > 0 {
                let members: Vec<Position> = if remaining > 2 {
                    let free: Vec<usize> = (0..count).filter(|&p| !grouped[p]).collect();
                    let mut m: Vec<usize> =
                        free.iter().copied().filter(|_| rng.one_in(2)).collect();
                    if m.is_empty() {
                        m.push(free[rng.below(free.len())]);
                    }
                    m.into_iter().map(Position).collect()
                } else {
                    (0..count).filter(|&p| !grouped[p]).map(Position).collect()
                };
                for p in &members {
                    grouped[p.0] = true;
                }
                let k = members.len();
                let transform = match k {
                    2 if rng.one_in(2) => Transform::PairNone,
                    2 => Transform::PairMidSide,
                    k if k > 2 => match rng.below(3) {
                        0 => Transform::None,
                        1 => Transform::Default,
                        _ => Transform::Matrix {
                            rotations: (0..k * (k - 1) / 2).map(|_| rng.below(64) as u8).collect(),
                            signs: rng.bits(k),
                        },
                    },
                    _ => Transform::Single,
                };
                let bands = (transform.active() && rng.one_in(3)).then(|| rng.bits(num_bands));
                remaining -= k;
                groups.push(Group {
                    members,
                    transform,
                    bands,
                });
            }
        }
        let transmit: Vec<bool> = (0..count).map(|_| !rng.one_in(4)).collect();
        let quant = transmit.iter().any(|&t| t).then(|| Quant {
            vector_counts: rng.one_in(4),
            step: cfg.quant_base() + rng.below(401) as i32 - 200,
            modifier_len: (count > 1).then(|| rng.below(8) as u8),
        });
        let mut channels = Vec::new();
        for (i, c) in shape.channels.iter().enumerate() {
            let st = &mut state[c.0];
            let mut ch = SubframeChannel {
                transmit: transmit[i],
                vector_count: None,
                modifier: None,
                scale: None,
                coefficients: None,
            };
            if let Some(q) = &quant {
                if q.vector_counts {
                    ch.vector_count = Some(4 * rng.below(len / 4 + 1));
                }
                if let Some(ml) = q.modifier_len
                    && rng.one_in(2)
                {
                    ch.modifier = Some(if ml == 0 {
                        0
                    } else {
                        rng.below(1 << ml) as u32
                    });
                }
                ch.scale = Some(if !st.coded {
                    ScaleFactors::Dpcm {
                        step: 1 + rng.below(4) as u8,
                        deltas: (0..num_bands).map(|_| rng.below(121) as i32 - 60).collect(),
                    }
                } else if st.next > 0 && rng.one_in(3) {
                    ScaleFactors::Kept
                } else {
                    let mut changes = Vec::new();
                    let mut band = 0;
                    while band < num_bands && !rng.one_in(4) {
                        band += rng.below((num_bands - band).min(MAX_ESCAPED_SKIP + 1));
                        let m = 1 + rng.below(MAX_ESCAPED_SCALE as usize) as i32;
                        changes.push(ScaleChange {
                            band,
                            delta: if rng.one_in(2) { m } else { -m },
                        });
                        band += 1;
                    }
                    ScaleFactors::Changed(changes)
                });
                st.coded = true;
            }
            if ch.transmit {
                let values = (0..len)
                    .map(|_| {
                        if rng.one_in(3) {
                            let m = magnitude(rng);
                            if rng.one_in(2) { m } else { -m }
                        } else {
                            0
                        }
                    })
                    .collect();
                ch.coefficients = Some(Coefficients {
                    table: if rng.one_in(2) {
                        CoefTable::First
                    } else {
                        CoefTable::Second
                    },
                    values,
                });
            }
            channels.push(ch);
        }
        for c in &shape.channels {
            state[c.0].next += 1;
        }
        subframes.push(Subframe {
            channel_indexes: shape.channels,
            len,
            fill,
            groups,
            channels,
            quant,
        });
    }
    let width = (cfg.frame_len * 2).ilog2();
    let trim = rng.one_in(3).then(|| {
        let mut value = || (!rng.one_in(2)).then(|| rng.below(1 << width) as u32);
        Trim {
            start: value(),
            end: value(),
        }
    });
    let postproc = match (n, rng.below(4)) {
        (1, _) | (_, 0 | 1) => PostProc::Absent,
        (_, 2) => PostProc::Flag,
        _ => PostProc::Matrix((0..n * n).map(|_| rng.below(16) as u8).collect()),
    };
    Frame {
        tiling,
        same_tiling,
        postproc,
        drc_gain: cfg.header.decode_flags.drc().then(|| rng.below(256) as u8),
        trim,
        subframes,
    }
}

#[test]
fn random_frames_write_and_read_back() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for cfg in configs() {
        let codec = Codec::new(&cfg);
        for i in 0..200 {
            let frame = random_frame(&codec, &mut rng);
            let trailer = rng.one_in(2);
            let bits = codec
                .write(&frame, trailer)
                .unwrap_or_else(|e| panic!("{} channels, frame {i}: {e}", cfg.channels));
            let (again, t) = codec
                .read(&bits)
                .unwrap_or_else(|e| panic!("{} channels, frame {i}: {e}", cfg.channels));
            assert_eq!(again, frame, "{} channels, frame {i}", cfg.channels);
            assert_eq!(t, trailer);
        }
    }
}

/// A frame of one full-length subframe per channel, coded, with `values`.
fn simple_frame(cfg: &Config, values: Vec<i32>) -> Frame {
    let len = cfg.frame_len;
    let codec = Codec::new(cfg);
    let num_bands = codec.num_bands(len);
    Frame {
        tiling: Tiling(vec![vec![len]; cfg.channels]),
        same_tiling: true,
        postproc: PostProc::Absent,
        drc_gain: cfg.header.decode_flags.drc().then_some(0),
        trim: None,
        subframes: vec![Subframe {
            channel_indexes: (0..cfg.channels).map(ChannelIndex).collect(),
            len,
            fill: None,
            groups: if cfg.channels > 1 {
                vec![Group {
                    members: (0..cfg.channels).map(Position).collect(),
                    transform: Transform::PairMidSide,
                    bands: None,
                }]
            } else {
                Vec::new()
            },
            channels: (0..cfg.channels)
                .map(|_| SubframeChannel {
                    transmit: true,
                    vector_count: None,
                    modifier: None,
                    scale: Some(ScaleFactors::Dpcm {
                        step: 2,
                        deltas: vec![0; num_bands],
                    }),
                    coefficients: Some(Coefficients {
                        table: CoefTable::First,
                        values: values.clone(),
                    }),
                })
                .collect(),
            quant: Some(Quant {
                vector_counts: false,
                step: 100,
                modifier_len: (cfg.channels > 1).then_some(0),
            }),
        }],
    }
}

#[test]
fn models_the_reader_would_not_give_back_are_refused() {
    let cfg = config(0x3, 0xe0, 70_000, 44100);
    let codec = Codec::new(&cfg);
    let base = simple_frame(&cfg, vec![0; 2048]);
    assert!(codec.write(&base, true).is_ok());

    let mut kept_first = base.clone();
    kept_first.subframes[0].channels[0].scale = Some(ScaleFactors::Kept);
    assert_eq!(
        codec.write(&kept_first, true),
        Err(WriteError::Subframe {
            subframe: 0,
            what: SubframeError::ScaleOrder
        })
    );

    let mut long_fill = base.clone();
    long_fill.subframes[0].fill = Some(Fill::Short(vec![true; 4]));
    assert_eq!(
        codec.write(&long_fill, true),
        Err(WriteError::Subframe {
            subframe: 0,
            what: SubframeError::Fill
        })
    );

    let mut not_shared = base.clone();
    not_shared.tiling.0[1] = vec![1024, 1024];
    assert_eq!(
        codec.write(&not_shared, true),
        Err(WriteError::TilingNotShared)
    );

    let mut silent = base.clone();
    for ch in &mut silent.subframes[0].channels {
        ch.transmit = false;
        ch.coefficients = None;
    }
    assert_eq!(
        codec.write(&silent, true),
        Err(WriteError::Subframe {
            subframe: 0,
            what: SubframeError::Quant
        })
    );
}

/// With 100-byte packets the length prefix has 10 bits.
#[test]
fn frames_longer_than_the_length_prefix_holds_are_refused() {
    let cfg = config(0x3, 0xe0, 100, 44100);
    let codec = Codec::new(&cfg);
    let frame = simple_frame(&cfg, (0..2048).map(|i| i % 7 - 3).collect());
    assert!(matches!(
        codec.write(&frame, true),
        Err(WriteError::FrameTooLong { max: 1023, .. })
    ));
}
