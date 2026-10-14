//! A fast hasher for keys that are integers or already-random hashes, never caller-chosen strings.

use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// The FxHash mixing step. Sequential IDs spread well enough for SwissTable buckets.
#[derive(Clone, Copy, Default)]
pub(super) struct KeyHasher(u64);

impl KeyHasher {
    #[inline]
    fn mix(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for KeyHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.mix(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.mix(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.mix(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.mix(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.mix(i as u64);
    }

    #[inline]
    fn write_isize(&mut self, i: isize) {
        self.mix(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub(super) type KeyBuildHasher = BuildHasherDefault<KeyHasher>;
