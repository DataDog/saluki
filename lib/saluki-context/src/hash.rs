use std::hash::{self, BuildHasher};

pub struct NoopU64Hasher(u64);

impl NoopU64Hasher {
    pub fn new() -> Self {
        Self(0)
    }
}

impl Clone for NoopU64Hasher {
    fn clone(&self) -> Self {
        Self(0)
    }
}

impl hash::Hasher for NoopU64Hasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }

    fn write(&mut self, _: &[u8]) {
        panic!("NoopU64Hasher is only valid for hashing `u64` values");
    }
}

impl BuildHasher for NoopU64Hasher {
    type Hasher = NoopU64Hasher;

    fn build_hasher(&self) -> Self::Hasher {
        Self(0)
    }
}
