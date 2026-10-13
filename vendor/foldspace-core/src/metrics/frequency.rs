//! Count-Min sketch of how often dictionary values are referenced, keyed by value content so
//! an estimate survives the value being evicted and redefined under a new ID.
//!
//! Counters are four bits wide and saturate at 15. Like TinyLFU, every counter is halved once
//! the sketch has taken a sample of increments proportional to its width, so estimates
//! describe recent use and a value seen once before the last halving reads as unseen. As in
//! Caffeine, a value's four counters share one 64-byte block, so counting it touches one cache
//! line however large the table is.

use std::mem::size_of;

const ROWS: usize = 4;
const COUNTERS_PER_WORD: usize = 16;
// Each row owns two words of a block.
const WORDS_PER_BLOCK: usize = 2 * ROWS;
const COUNTERS_PER_ROW_PER_BLOCK: usize = 2 * COUNTERS_PER_WORD;
const COUNTER_MAX: u64 = 0xF;
// Keeps the top bit of each counter after a shift, so halving never borrows from a neighbor.
const HALVE_MASK: u64 = 0x7777_7777_7777_7777;
// Counters per row per tracked value; leaves most counters zero so unseen values rarely read
// nonzero.
const WIDTH_PER_ITEM: usize = 4;
const MIN_WIDTH: usize = COUNTERS_PER_ROW_PER_BLOCK;
const MAX_WIDTH: usize = 1 << 20;
const SAMPLE_PER_COUNTER: usize = 10;

#[derive(Clone, Debug)]
pub(super) struct FrequencySketch {
    // Blocks of `WORDS_PER_BLOCK` words, sixteen counters to a word.
    table: Box<[u64]>,
    block_mask: usize,
    increments: usize,
    sample_size: usize,
}

impl FrequencySketch {
    /// A sketch sized for a dictionary of at most `items` values, with a fixed footprint of
    /// [`Self::bytes`] whatever the input.
    pub(super) fn with_capacity(items: usize) -> Self {
        let width = items
            .saturating_mul(WIDTH_PER_ITEM)
            .clamp(MIN_WIDTH, MAX_WIDTH)
            .next_power_of_two();
        let blocks = width / COUNTERS_PER_ROW_PER_BLOCK;
        Self {
            table: vec![0; blocks * WORDS_PER_BLOCK].into_boxed_slice(),
            block_mask: blocks - 1,
            increments: 0,
            sample_size: SAMPLE_PER_COUNTER * width,
        }
    }

    pub(super) fn bytes(&self) -> usize {
        size_of::<Self>() + self.table.len() * size_of::<u64>()
    }

    /// An upper bound on the increments of `hash` since the last halving, plus the halved
    /// remainder of earlier ones.
    pub(super) fn estimate(&self, hash: u64) -> u64 {
        self.slots(hash)
            .into_iter()
            .map(|(word, shift)| (self.table[word] >> shift) & COUNTER_MAX)
            .min()
            .unwrap_or(0)
    }

    pub(super) fn increment(&mut self, hash: u64) {
        for (word, shift) in self.slots(hash) {
            if (self.table[word] >> shift) & COUNTER_MAX < COUNTER_MAX {
                self.table[word] += 1 << shift;
            }
        }
        self.increments += 1;
        if self.increments >= self.sample_size {
            self.halve();
        }
    }

    fn halve(&mut self) {
        for word in self.table.iter_mut() {
            *word = (*word >> 1) & HALVE_MASK;
        }
        self.increments /= 2;
    }

    /// The word and bit offset of each row's counter for `hash`, all within one block.
    fn slots(&self, hash: u64) -> [(usize, u32); ROWS] {
        let block = (mix(hash, 0) as usize & self.block_mask) * WORDS_PER_BLOCK;
        let counters = mix(hash, 1);
        std::array::from_fn(|row| {
            let bits = counters >> (row * 8);
            (
                block + 2 * row + (bits & 1) as usize,
                ((bits >> 1) & 0xF) as u32 * 4,
            )
        })
    }
}

// SplitMix64's finalizer over a salt derives independent indexes from one hash.
fn mix(hash: u64, salt: u64) -> u64 {
    let mut z = hash.wrapping_add((salt + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn hashes(count: u64) -> impl Iterator<Item = u64> {
        (0..count).map(|value| mix(value, 7))
    }

    #[test]
    fn estimates_never_undercount_and_collisions_only_overcount() {
        // Four values per counter forces collisions in every row.
        let mut sketch = FrequencySketch::with_capacity(64);
        let width = ((sketch.block_mask + 1) * COUNTERS_PER_ROW_PER_BLOCK) as u64;
        let mut exact = BTreeMap::new();
        for (index, hash) in hashes(width * 4).enumerate() {
            for _ in 0..index % 4 {
                sketch.increment(hash);
                *exact.entry(hash).or_insert(0) += 1;
            }
        }
        assert!(sketch.increments < sketch.sample_size, "no halving yet");
        let mut overcounted = 0;
        for hash in hashes(width * 4) {
            let exact = exact.get(&hash).copied().unwrap_or(0);
            let estimate = sketch.estimate(hash);
            assert!(estimate >= exact.min(COUNTER_MAX));
            overcounted += usize::from(estimate > exact);
        }
        assert!(overcounted > 0, "the sketch is small enough to collide");
    }

    #[test]
    fn a_sparse_sketch_estimates_unseen_values_as_zero() {
        let mut sketch = FrequencySketch::with_capacity(1000);
        for hash in hashes(1000) {
            sketch.increment(hash);
            sketch.increment(hash);
        }
        let unseen = (1000..11_000)
            .map(|value| mix(value, 7))
            .filter(|hash| sketch.estimate(*hash) > 0)
            .count();
        assert!(unseen < 100, "{unseen} of 10000 unseen values read as seen");
        assert!(hashes(1000).all(|hash| sketch.estimate(hash) >= 2));
    }

    #[test]
    fn counters_saturate() {
        let mut sketch = FrequencySketch::with_capacity(1000);
        for _ in 0..100 {
            sketch.increment(1);
        }
        assert_eq!(sketch.estimate(1), COUNTER_MAX);
    }

    #[test]
    fn a_full_sample_halves_every_counter() {
        let mut sketch = FrequencySketch::with_capacity(1000);
        for _ in 0..13 {
            sketch.increment(1);
        }
        for _ in 0..7 {
            sketch.increment(2);
        }
        assert_eq!((sketch.estimate(1), sketch.estimate(2)), (13, 7));
        sketch.increments = sketch.sample_size - 1;
        sketch.increment(1);
        assert_eq!(sketch.increments, sketch.sample_size / 2);
        assert_eq!((sketch.estimate(1), sketch.estimate(2)), (7, 3));
        assert!(sketch.table.iter().all(|word| word & !HALVE_MASK == 0));
    }

    #[test]
    fn memory_is_fixed_by_capacity_and_clamped() {
        let mut sketch = FrequencySketch::with_capacity(700);
        let bytes = sketch.bytes();
        assert_eq!(bytes, size_of::<FrequencySketch>() + ROWS * 4096 / 2);
        for hash in hashes(100_000) {
            sketch.increment(hash);
        }
        assert_eq!(sketch.bytes(), bytes);
        assert_eq!(
            FrequencySketch::with_capacity(0).bytes(),
            size_of::<FrequencySketch>() + ROWS * MIN_WIDTH / 2
        );
        assert_eq!(
            FrequencySketch::with_capacity(usize::MAX).bytes(),
            size_of::<FrequencySketch>() + ROWS * MAX_WIDTH / 2
        );
    }
}
