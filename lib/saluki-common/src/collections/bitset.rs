use smallvec::SmallVec;

/// A dense, contiguous bitset.
///
/// `ContiguousBitSet` is designed for tracking set membership of dense, contiguous values, such as the indexes of
/// values in a vector. It is able to hold up to 128 values (indices 0–127) inline with no heap allocation. It is _not_
/// suitable for sparse values (values that are far apart in value) as the size of the underlying storage will tied to
/// the largest value in the set: roughly `((max_value / 64) + 1) * 8` bytes.
///
/// All operations are O(1) with the exception of `set` when setting a bit that extends beyond the current capacity,
/// which will require a heap allocation.
#[derive(Clone, Debug, Default)]
pub struct ContiguousBitSet {
    words: SmallVec<[u64; 2]>,
}

impl ContiguousBitSet {
    /// Creates a new, empty bitset with no bits set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of bits that are set.
    pub fn len(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Returns `false` if no bits are set.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// Sets the bit at `index`.
    pub fn set(&mut self, index: usize) {
        let word = index / 64;
        let bit = index % 64;

        // Grow if needed.
        while self.words.len() <= word {
            self.words.push(0);
        }

        self.words[word] |= 1 << bit;
    }

    /// Clears the bit at `index`.
    ///
    /// If `index` is beyond the current capacity, this is a no-op.
    pub fn clear(&mut self, index: usize) {
        let word = index / 64;
        let bit = index % 64;

        if let Some(w) = self.words.get_mut(word) {
            *w &= !(1 << bit);
        }
    }

    /// Returns `true` if the bit at `index` is set.
    pub fn is_set(&self, index: usize) -> bool {
        let word = index / 64;
        let bit = index % 64;
        self.words.get(word).is_some_and(|w| w & (1 << bit) != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bitset() {
        let bs = ContiguousBitSet::new();
        assert!(bs.is_empty());
        assert!(!bs.is_set(0));
        assert!(!bs.is_set(127));
        assert_eq!(bs.len(), 0);
    }

    #[test]
    fn set_and_check() {
        let mut bs = ContiguousBitSet::new();
        bs.set(0);
        bs.set(63);
        bs.set(64);
        bs.set(127);

        assert!(bs.is_set(0));
        assert!(bs.is_set(63));
        assert!(bs.is_set(64));
        assert!(bs.is_set(127));
        assert!(!bs.is_set(1));
        assert!(!bs.is_set(65));
        assert!(!bs.is_empty());
        assert_eq!(bs.len(), 4);
    }

    #[test]
    fn clear_bit() {
        let mut bs = ContiguousBitSet::new();
        bs.set(10);
        assert!(bs.is_set(10));

        bs.clear(10);
        assert!(!bs.is_set(10));
        assert!(bs.is_empty());
    }

    #[test]
    fn clear_beyond_capacity_is_noop() {
        let mut bs = ContiguousBitSet::new();
        bs.clear(999); // Should not panic or allocate.
        assert!(bs.is_empty());
    }

    #[test]
    fn grows_beyond_inline() {
        let mut bs = ContiguousBitSet::new();
        bs.set(200);

        assert!(bs.is_set(200));
        assert!(!bs.is_set(199));
        assert!(!bs.is_empty());
        assert_eq!(bs.len(), 1);
    }

    #[test]
    fn len_across_words() {
        let mut bs = ContiguousBitSet::new();
        for i in (0..192).step_by(3) {
            bs.set(i);
        }
        assert_eq!(bs.len(), 64);
    }

    #[test]
    fn clone_independence() {
        let mut bs = ContiguousBitSet::new();
        bs.set(5);
        let mut cloned = bs.clone();
        cloned.set(10);

        assert!(bs.is_set(5));
        assert!(!bs.is_set(10));
        assert!(cloned.is_set(5));
        assert!(cloned.is_set(10));
    }
}
