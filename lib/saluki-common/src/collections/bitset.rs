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

    /// Clears all bits, resetting the bitset to its initial empty state.
    ///
    /// All existing capacity is retained.
    pub fn clear_all(&mut self) {
        for w in &mut self.words {
            *w = 0;
        }
    }
}

impl<'a> IntoIterator for &'a ContiguousBitSet {
    type Item = usize;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        let len = self.words.len();
        Iter {
            words: &self.words,
            front: 0,
            back: len,
            front_bits: self.words.first().copied().unwrap_or(0),
            back_bits: if len > 1 { self.words[len - 1] } else { 0 },
        }
    }
}

/// Iterator over set bit indices of a [`ContiguousBitSet`].
///
/// Indices are yielded in ascending order: lowest to highest.
pub struct Iter<'a> {
    words: &'a [u64],
    front: usize,
    back: usize,
    front_bits: u64,
    back_bits: u64,
}

impl Iterator for Iter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            if self.front_bits != 0 {
                let bit = self.front_bits.trailing_zeros() as usize;
                self.front_bits &= self.front_bits - 1; // Clear lowest set bit.
                return Some(self.front * 64 + bit);
            }
            self.front += 1;
            if self.front >= self.back {
                return None;
            }
            self.front_bits = if self.front == self.back - 1 {
                // Reached back's word -- take its remaining bits.
                std::mem::take(&mut self.back_bits)
            } else {
                self.words[self.front]
            };
        }
    }
}

impl DoubleEndedIterator for Iter<'_> {
    fn next_back(&mut self) -> Option<usize> {
        loop {
            if self.back <= self.front {
                return None;
            }
            if self.back > self.front + 1 {
                // Back is at a separate word from front.
                if self.back_bits != 0 {
                    let bit = 63 - self.back_bits.leading_zeros() as usize;
                    self.back_bits &= !(1u64 << bit); // Clear highest set bit.
                    return Some((self.back - 1) * 64 + bit);
                }
                self.back -= 1;
                if self.back > self.front + 1 {
                    self.back_bits = self.words[self.back - 1];
                }
                // Otherwise back == front + 1: fall through to front_bits on next iteration.
                continue;
            }
            // back == front + 1: same word as front. Consume from front_bits (high end).
            if self.front_bits != 0 {
                let bit = 63 - self.front_bits.leading_zeros() as usize;
                self.front_bits &= !(1u64 << bit); // Clear highest set bit.
                return Some(self.front * 64 + bit);
            }
            return None;
        }
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

    #[test]
    fn iter_ascending() {
        let mut bs = ContiguousBitSet::new();
        bs.set(3);
        bs.set(0);
        bs.set(127);
        bs.set(64);

        let items: Vec<usize> = bs.into_iter().collect();
        assert_eq!(items, vec![0, 3, 64, 127]);
    }

    #[test]
    fn iter_descending() {
        let mut bs = ContiguousBitSet::new();
        bs.set(3);
        bs.set(0);
        bs.set(127);
        bs.set(64);

        let items: Vec<usize> = bs.into_iter().rev().collect();
        assert_eq!(items, vec![127, 64, 3, 0]);
    }

    #[test]
    fn iter_interleaved_single_word() {
        let mut bs = ContiguousBitSet::new();
        bs.set(1);
        bs.set(3);
        bs.set(5);
        bs.set(7);

        let mut iter = bs.into_iter();
        assert_eq!(iter.next(), Some(1));
        assert_eq!(iter.next_back(), Some(7));
        assert_eq!(iter.next(), Some(3));
        assert_eq!(iter.next_back(), Some(5));
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
    }

    #[test]
    fn iter_interleaved_multi_word() {
        let mut bs = ContiguousBitSet::new();
        bs.set(0);
        bs.set(63);
        bs.set(64);
        bs.set(127);

        let mut iter = bs.into_iter();
        assert_eq!(iter.next(), Some(0));
        assert_eq!(iter.next_back(), Some(127));
        assert_eq!(iter.next(), Some(63));
        assert_eq!(iter.next_back(), Some(64));
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
    }

    #[test]
    fn iter_empty() {
        let bs = ContiguousBitSet::new();
        let items: Vec<usize> = bs.into_iter().collect();
        assert!(items.is_empty());
        assert_eq!(bs.into_iter().next_back(), None);
    }
}
