//! Hash maps of small integer keys the crate builds from fixed tables,
//! hashed by multiplication instead of SipHash, which costs more than the
//! lookups the decoders and encoders make per code.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

pub(crate) type IntMap<K, V> = HashMap<K, V, BuildHasherDefault<IntHasher>>;

/// FxHash: rotate, xor in the next word, multiply.
#[derive(Default)]
pub(crate) struct IntHasher(u64);

impl IntHasher {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for IntHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add(u64::from(b));
        }
    }

    fn write_u8(&mut self, v: u8) {
        self.add(u64::from(v));
    }

    fn write_u32(&mut self, v: u32) {
        self.add(u64::from(v));
    }

    fn write_i32(&mut self, v: i32) {
        self.add(u64::from(v as u32));
    }
}
