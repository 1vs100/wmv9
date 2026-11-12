//! Huffman codes given as lengths in code order, the form of FFmpeg's
//! tables: each symbol takes the next free code of its length.

use crate::int_map::IntMap;
use crate::wmv3::bits::{BitReader, BitWriter, OutOfBits};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Code {
    bits: u32,
    len: u32,
}

/// A code table over symbols of type i32.
pub(crate) struct Vlc {
    codes: IntMap<i32, Code>,
    symbols: IntMap<Code, i32>,
    max_len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VlcError {
    #[error(transparent)]
    OutOfBits(#[from] OutOfBits),
    #[error("no code matches the bits at {position}")]
    NoCode { position: usize },
}

impl Vlc {
    /// Codes for `entries` of (length, symbol) in code order.
    pub(crate) fn new(entries: impl IntoIterator<Item = (u8, i32)>) -> Self {
        let mut codes = IntMap::default();
        let mut symbols = IntMap::default();
        let mut next: u64 = 0;
        let mut max_len = 0;
        for (len, sym) in entries {
            let len = u32::from(len);
            let code = Code {
                bits: (next >> (32 - len)) as u32,
                len,
            };
            codes.insert(sym, code);
            symbols.insert(code, sym);
            next += 1 << (32 - len);
            max_len = max_len.max(len);
        }
        assert!(next <= 1 << 32, "overdetermined code");
        Vlc {
            codes,
            symbols,
            max_len,
        }
    }

    /// Whether `sym` has a code.
    pub(crate) fn has(&self, sym: i32) -> bool {
        self.codes.contains_key(&sym)
    }

    /// Writes the code of `sym`, which must have one.
    pub(crate) fn put(&self, w: &mut BitWriter, sym: i32) {
        let c = self.codes[&sym];
        w.write(c.len, c.bits);
    }

    pub(crate) fn get(&self, r: &mut BitReader) -> Result<i32, VlcError> {
        let position = r.position();
        let mut code = Code { bits: 0, len: 0 };
        while code.len < self.max_len {
            code.bits = code.bits << 1 | u32::from(r.bit()?);
            code.len += 1;
            if let Some(&sym) = self.symbols.get(&code) {
                return Ok(sym);
            }
        }
        Err(VlcError::NoCode { position })
    }
}

/// The tables of wmaprodata.h with the symbol offsets wmaprodec.c gives
/// them.
pub(crate) struct Tables {
    /// Scale factor differences, -60 to 60.
    pub(crate) scale: Vlc,
    /// Index into SCALE_RL_RUN and SCALE_RL_LEVEL; 0 escape, 1 end.
    pub(crate) scale_rl: Vlc,
    /// Index into COEFn_RUN and COEFn_LEVEL; 0 escape, 1 end.
    pub(crate) coef: [Vlc; 2],
    /// Four values of 0 to 15 as nibbles, -1 escape to pairs.
    pub(crate) vec4: Vlc,
    /// Two values of 0 to 15 as nibbles, -1 escape to single values.
    pub(crate) vec2: Vlc,
    /// One value of 0 to 100; 100 escapes to a large value.
    pub(crate) vec1: Vlc,
}

impl Tables {
    pub(crate) fn new() -> Self {
        use wma_tables::pro as t;
        let pairs = |table: &[[u8; 2]], offset: i32| {
            Vlc::new(table.iter().map(move |e| (e[1], i32::from(e[0]) + offset)))
        };
        Tables {
            scale: pairs(&t::SCALE_TABLE, -60),
            scale_rl: pairs(&t::SCALE_RL_TABLE, 0),
            coef: [
                Vlc::new(
                    t::COEF0_LENS
                        .iter()
                        .zip(t::COEF0_SYMS)
                        .map(|(&l, s)| (l, i32::from(s))),
                ),
                pairs(&t::COEF1_TABLE, 0),
            ],
            vec4: Vlc::new(
                t::VEC4_LENS
                    .iter()
                    .zip(t::VEC4_SYMS)
                    .map(|(&l, s)| (l, i32::from(s) - 1)),
            ),
            vec2: pairs(&t::VEC2_TABLE, -1),
            vec1: pairs(&t::VEC1_TABLE, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_follow_the_lengths_in_order() {
        let v = Vlc::new([(1, 10), (2, 20), (3, 30), (3, 40)]);
        let mut w = BitWriter::new();
        for s in [40, 10, 30, 20] {
            v.put(&mut w, s);
        }
        // 111 0 110 10, then padding.
        assert_eq!(w.finish(), [0b1110_1101, 0b0000_0000]);
        let bytes = [0b1110_1101, 0];
        let mut r = BitReader::new(&bytes);
        let got: Vec<i32> = (0..4).map(|_| v.get(&mut r).unwrap()).collect();
        assert_eq!(got, [40, 10, 30, 20]);
    }

    #[test]
    fn every_table_is_complete() {
        let t = Tables::new();
        for (name, v) in [
            ("scale", &t.scale),
            ("scale_rl", &t.scale_rl),
            ("coef0", &t.coef[0]),
            ("coef1", &t.coef[1]),
            ("vec4", &t.vec4),
            ("vec2", &t.vec2),
            ("vec1", &t.vec1),
        ] {
            // A prefix code fills the code space when sum 2^-len is 1.
            let kraft: f64 = v.codes.values().map(|c| 0.5f64.powi(c.len as i32)).sum();
            assert!((kraft - 1.0).abs() < 1e-12, "{name}: {kraft}");
        }
    }
}
