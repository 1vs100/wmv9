//! Little-endian byte cursor and writer helpers shared by the parsers.

/// Input ended before a field could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("input truncated at offset {offset:#x}: {needed} more bytes needed")]
pub struct Truncated {
    pub offset: usize,
    pub needed: usize,
}

/// Cursor over a byte slice. `base` is the absolute file offset of
/// `buf[0]`, so errors report positions in the original file.
#[derive(Debug, Clone)]
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    base: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], base: usize) -> Self {
        Self { buf, pos: 0, base }
    }

    pub fn offset(&self) -> usize {
        self.base + self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], Truncated> {
        if n > self.remaining() {
            return Err(Truncated {
                offset: self.offset(),
                needed: n - self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Splits off the next `n` bytes as an independent reader.
    pub fn sub(&mut self, n: usize) -> Result<Reader<'a>, Truncated> {
        let base = self.offset();
        Ok(Reader::new(self.take(n)?, base))
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let out = &self.buf[self.pos..];
        self.pos = self.buf.len();
        out
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Truncated> {
        Ok(self.take(N)?.try_into().expect("take returned N bytes"))
    }

    pub fn u8(&mut self) -> Result<u8, Truncated> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Result<u16, Truncated> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, Truncated> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> Result<i32, Truncated> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, Truncated> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

pub(crate) trait PutLe {
    fn put_u8(&mut self, v: u8);
    fn put_u16(&mut self, v: u16);
    fn put_u32(&mut self, v: u32);
    fn put_i32(&mut self, v: i32);
    fn put_u64(&mut self, v: u64);
}

impl PutLe for Vec<u8> {
    fn put_u8(&mut self, v: u8) {
        self.push(v);
    }
    fn put_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn put_i32(&mut self, v: i32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_le_bytes());
    }
}
