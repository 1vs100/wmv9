//! Prefix code tables built from (code, length) pairs indexed by symbol.

use std::collections::HashMap;

use super::bits::{BitReader, BitWriter};
use super::{CodeTable, FrameError};

#[derive(Debug)]
pub struct Vlc {
    name: CodeTable,
    decode: HashMap<(u8, u32), u16>,
    encode: Vec<Option<(u32, u8)>>,
    max_len: u8,
}

impl Vlc {
    /// Entries with length 0 are unused symbols.
    pub fn new<C: Copy + Into<u32>>(name: CodeTable, codes: &[C], lens: &[u8]) -> Self {
        assert_eq!(codes.len(), lens.len(), "{name:?}: table shape");
        let mut decode = HashMap::new();
        let mut encode = Vec::with_capacity(codes.len());
        for (sym, (&code, &len)) in codes.iter().zip(lens).enumerate() {
            if len == 0 {
                encode.push(None);
                continue;
            }
            let code = code.into();
            let prev = decode.insert((len, code), sym as u16);
            assert!(prev.is_none(), "{name:?}: duplicate code for symbol {sym}");
            encode.push(Some((code, len)));
        }
        let max_len = lens.iter().copied().max().unwrap_or(0);
        Vlc {
            name,
            decode,
            encode,
            max_len,
        }
    }

    /// Table stored as `[code, length]` pairs indexed by symbol.
    pub fn from_pairs(name: CodeTable, pairs: &[[u32; 2]]) -> Self {
        let codes: Vec<u32> = pairs.iter().map(|p| p[0]).collect();
        let lens: Vec<u8> = pairs.iter().map(|p| p[1] as u8).collect();
        Vlc::new(name, &codes, &lens)
    }

    pub fn read(&self, r: &mut BitReader) -> Result<u16, FrameError> {
        for len in 1..=self.max_len {
            let code = r.peek(len.into())?;
            if let Some(&sym) = self.decode.get(&(len, code)) {
                r.skip(len.into())?;
                return Ok(sym);
            }
        }
        Err(FrameError::InvalidCode {
            table: self.name,
            bit: r.position(),
        })
    }

    pub fn write(&self, w: &mut BitWriter, sym: u16) -> Result<(), FrameError> {
        let (code, len) = self.encode.get(usize::from(sym)).copied().flatten().ok_or(
            FrameError::UncodableSymbol {
                table: self.name,
                symbol: sym,
            },
        )?;
        w.write(len.into(), code);
        Ok(())
    }
}
