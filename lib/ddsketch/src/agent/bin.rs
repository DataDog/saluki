//! Sketch bin representation.

const MAX_BIN_WIDTH: u32 = u32::MAX;

/// A sketch bin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct Bin {
    /// The bin index.
    pub(crate) k: i16,

    /// The number of observations within the bin.
    pub(crate) n: u32,
}

impl Bin {
    /// Returns the key of the bin.
    pub fn key(&self) -> i32 {
        self.k as i32
    }

    /// Returns the number of observations within the bin.
    pub fn count(&self) -> u32 {
        self.n
    }

    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn increment(&mut self, n: u64) -> u64 {
        let next = n + u64::from(self.n);
        if next > u64::from(MAX_BIN_WIDTH) {
            self.n = MAX_BIN_WIDTH;
            return next - u64::from(MAX_BIN_WIDTH);
        }

        // SAFETY: We already know `next` is less than or equal to `MAX_BIN_WIDTH` if we got here, and `MAX_BIN_WIDTH`
        // is u32, so next can't possibly be larger than a u32.
        self.n = next as u32;
        0
    }
}
