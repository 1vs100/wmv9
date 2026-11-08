//! MSB-first bit reader and writer.

/// Input ended inside a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("bitstream ended at bit {bit}: {needed} more bits needed")]
pub struct OutOfBits {
    pub bit: usize,
    pub needed: usize,
}

#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// Returns the next `n` (at most 32) bits without consuming them.
    pub fn peek(&self, n: u32) -> Result<u32, OutOfBits> {
        debug_assert!(n <= 32);
        if n as usize > self.remaining() {
            return Err(OutOfBits {
                bit: self.pos,
                needed: n as usize - self.remaining(),
            });
        }
        let mut v: u64 = 0;
        let first = self.pos / 8;
        let last = (self.pos + n as usize).div_ceil(8);
        for &b in &self.data[first..last] {
            v = v << 8 | u64::from(b);
        }
        let total = (last - first) * 8;
        let shift = total - (self.pos % 8) - n as usize;
        Ok(((v >> shift) & ((1u64 << n) - 1)) as u32)
    }

    pub fn read(&mut self, n: u32) -> Result<u32, OutOfBits> {
        let v = self.peek(n)?;
        self.pos += n as usize;
        Ok(v)
    }

    pub fn skip(&mut self, n: u32) -> Result<(), OutOfBits> {
        self.read(n).map(|_| ())
    }

    pub fn bit(&mut self) -> Result<bool, OutOfBits> {
        Ok(self.read(1)? == 1)
    }

    /// Remaining bytes from the one holding the current bit, and the bit
    /// offset into that byte. Consumes the rest of the input.
    pub fn rest_bits(&mut self) -> (&'a [u8], u8) {
        let out = (&self.data[self.pos / 8..], (self.pos % 8) as u8);
        self.pos = self.data.len() * 8;
        out
    }

    /// Remaining whole bytes, if the reader is byte aligned.
    pub fn rest_bytes(&mut self) -> Option<&'a [u8]> {
        if !self.pos.is_multiple_of(8) {
            return None;
        }
        let out = &self.data[self.pos / 8..];
        self.pos = self.data.len() * 8;
        Some(out)
    }
}

#[derive(Debug, Clone, Default)]
pub struct BitWriter {
    out: Vec<u8>,
    /// Bits used in the last byte of `out`, 0 when aligned.
    used: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bit_len(&self) -> usize {
        if self.used == 0 {
            self.out.len() * 8
        } else {
            (self.out.len() - 1) * 8 + self.used as usize
        }
    }

    /// Writes the low `n` bits of `v`, most significant first.
    pub fn write(&mut self, n: u32, v: u32) {
        debug_assert!(n <= 32);
        debug_assert!(n == 32 || v >> n == 0, "value {v:#x} wider than {n} bits");
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }

    pub fn bit(&mut self, b: bool) {
        if self.used == 0 {
            self.out.push(0);
        }
        if b {
            *self.out.last_mut().expect("byte pushed") |= 0x80 >> self.used;
        }
        self.used = (self.used + 1) % 8;
    }

    /// Appends whole bytes; the writer must be byte aligned.
    pub fn bytes(&mut self, b: &[u8]) {
        assert_eq!(self.used, 0, "unaligned byte write");
        self.out.extend_from_slice(b);
    }

    pub fn is_aligned(&self) -> bool {
        self.used == 0
    }

    /// Returns the bytes written; a partial last byte is zero filled.
    pub fn finish(self) -> Vec<u8> {
        self.out
    }
}
