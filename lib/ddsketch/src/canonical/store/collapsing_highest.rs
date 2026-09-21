use datadog_protos::sketches::Store as ProtoStore;

use super::{validate_proto_count, Store};
use crate::canonical::error::ProtoConversionError;

/// A dense store that collapses highest-indexed bins when capacity is exceeded.
///
/// This store maintains a maximum number of bins. When adding a new index would exceed this limit, the highest-indexed
/// bins are collapsed (merged into the next highest bin), sacrificing accuracy for higher quantiles to preserve
/// accuracy for lower quantiles.
///
/// Use this store when:
/// - You need bounded memory usage
/// - Lower quantiles (for example, p1, p5) are more important than higher quantiles
/// - You're tracking metrics where the minimum values matter most
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollapsingHighestDenseStore {
    /// The bin counts, stored contiguously.
    bins: Vec<u64>,

    /// The count stored in bins[0] corresponds to this index.
    offset: i32,

    /// Maximum number of bins to maintain.
    max_num_bins: usize,

    /// Total count across all bins.
    count: u64,

    /// Whether collapsing has occurred (accuracy may be compromised for high quantiles).
    is_collapsed: bool,
}

impl CollapsingHighestDenseStore {
    /// Creates an empty `CollapsingHighestDenseStore` with the given maximum number of bins.
    pub fn new(max_num_bins: usize) -> Self {
        assert!(max_num_bins >= 1, "max_num_bins must be at least 1");
        Self {
            bins: Vec::new(),
            offset: 0,
            max_num_bins,
            count: 0,
            is_collapsed: false,
        }
    }

    /// Returns `true` if this store has collapsed bins.
    ///
    /// If true, accuracy guarantees may not hold for higher quantiles.
    pub fn is_collapsed(&self) -> bool {
        self.is_collapsed
    }

    /// Ensures the store can accommodate the given index, growing and collapsing if necessary.
    ///
    /// On return, `index` is always representable: it either falls inside `[offset, offset + bins.len())`, or it sits
    /// above the window, in which case it belongs to the collapsed highest bin. [`Self::bin_index`] relies on this.
    fn grow(&mut self, index: i32) {
        if self.bins.is_empty() {
            self.bins.push(0);
            self.offset = index;
            return;
        }

        if index >= self.offset + self.bins.len() as i32 {
            // Need to append bins - but first check if we need to collapse
            let new_len = (index - self.offset + 1) as usize;

            if new_len > self.max_num_bins {
                // The index sits above the widest window the store can represent. Widen the window upwards to the cap
                // first, so the collapsed counts land on the highest index that's still representable, then let
                // `bin_index` fold `index` into the top bin.
                //
                // Widening matters for accuracy: without it the collapsed ceiling is wherever the window happened to
                // already end, which makes the result depend on insertion order. Ascending input would pin every
                // observation to the first index seen instead of spreading it across the available bins. The reference
                // implementation places the ceiling at `min_index + max_num_bins - 1`, which is what widening to the
                // cap achieves here.
                if self.bins.len() < self.max_num_bins {
                    self.bins.resize(self.max_num_bins, 0);
                }

                self.is_collapsed = true;
                return;
            }

            self.bins.resize(new_len, 0);
        } else if index < self.offset {
            // Need to prepend bins
            let new_len = self.bins.len() + (self.offset - index) as usize;

            if new_len > self.max_num_bins {
                // The window can't stretch far enough down to cover `index`, so slide its top down to the highest
                // index that keeps `index` in range, collapsing everything above that point.
                self.collapse_above(index + self.max_num_bins as i32 - 1);
            }

            // Now prepend. `collapse_above` has already freed exactly as many bins as this needs, so the full
            // distance always fits within the capacity.
            let num_prepend = (self.offset - index) as usize;
            if num_prepend > 0 {
                let mut new_bins = vec![0u64; num_prepend + self.bins.len()];
                new_bins[num_prepend..].copy_from_slice(&self.bins);
                self.bins = new_bins;
                self.offset = index;
            }
        }

        debug_assert!(
            index >= self.offset,
            "grow must leave `index` representable: index={}, offset={}, len={}",
            index,
            self.offset,
            self.bins.len()
        );
        debug_assert!(
            self.bins.len() <= self.max_num_bins,
            "grow must respect the bin cap: len={}, max_num_bins={}",
            self.bins.len(),
            self.max_num_bins
        );
    }

    /// Collapses every bin above `new_ceiling` into the bin at `new_ceiling`, making it the new highest bin.
    ///
    /// Taking the new highest index rather than a number of bins to shift by is what keeps this correct when
    /// `new_ceiling` lands below the window entirely: that case folds the whole store into a single bin, instead of
    /// clamping the shift and leaving the window short of where it needs to be.
    fn collapse_above(&mut self, new_ceiling: i32) {
        if self.bins.is_empty() {
            return;
        }

        self.is_collapsed = true;

        // How many bins, counting up from the bottom of the window, sit at or below the new ceiling.
        let keep = (new_ceiling as i64 - self.offset as i64 + 1).max(0) as usize;

        if keep == 0 {
            // Every bin sits above the new highest index, so the whole store folds into one bin.
            let collapsed_count: u64 = self.bins.iter().sum();
            self.bins.clear();
            self.bins.push(collapsed_count);
            self.offset = new_ceiling;
        } else if keep < self.bins.len() {
            let collapsed_count: u64 = self.bins[keep..].iter().sum();
            self.bins[keep - 1] = self.bins[keep - 1].saturating_add(collapsed_count);
            self.bins.truncate(keep);
        }
    }

    /// Returns the index into the bins array for the given logical index.
    ///
    /// Indices above the window map to the highest bin, which is where collapsed counts accumulate. [`Self::grow`]
    /// guarantees no index arrives here from below the window, and that the store holds at least one bin.
    #[inline]
    fn bin_index(&self, index: i32) -> usize {
        if index >= self.offset + self.bins.len() as i32 {
            // Index is above our range, map to highest bin
            return self.bins.len() - 1;
        }

        debug_assert!(index >= self.offset, "grow should have made `index` representable");

        // Clamping keeps the bins consistent with `count` even if that invariant is ever broken: the observation
        // loses accuracy instead of being dropped outright.
        (index.max(self.offset) - self.offset) as usize
    }
}

impl Store for CollapsingHighestDenseStore {
    fn add(&mut self, index: i32, count: u64) {
        if count == 0 {
            return;
        }

        self.grow(index);

        let bin_idx = self.bin_index(index);
        self.bins[bin_idx] = self.bins[bin_idx].saturating_add(count);
        self.count = self.count.saturating_add(count);
    }

    fn total_count(&self) -> u64 {
        self.count
    }

    fn min_index(&self) -> Option<i32> {
        if self.bins.is_empty() {
            return None;
        }

        for (i, &count) in self.bins.iter().enumerate() {
            if count > 0 {
                return Some(self.offset + i as i32);
            }
        }
        None
    }

    fn max_index(&self) -> Option<i32> {
        if self.bins.is_empty() {
            return None;
        }

        for (i, &count) in self.bins.iter().enumerate().rev() {
            if count > 0 {
                return Some(self.offset + i as i32);
            }
        }
        None
    }

    fn key_at_rank(&self, rank: u64) -> Option<i32> {
        if rank >= self.count {
            return None;
        }

        let mut cumulative = 0u64;
        for (i, &count) in self.bins.iter().enumerate() {
            cumulative += count;
            if cumulative > rank {
                return Some(self.offset + i as i32);
            }
        }
        None
    }

    fn merge(&mut self, other: &Self) {
        if other.bins.is_empty() {
            return;
        }

        if other.is_collapsed {
            self.is_collapsed = true;
        }

        // Process each bin from the other store
        for (i, &count) in other.bins.iter().enumerate() {
            if count > 0 {
                let index = other.offset + i as i32;
                self.add(index, count);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn clear(&mut self) {
        self.bins.clear();
        self.offset = 0;
        self.count = 0;
        self.is_collapsed = false;
    }

    fn merge_from_proto(&mut self, proto: &ProtoStore) -> Result<(), ProtoConversionError> {
        for (&index, &count) in &proto.binCounts {
            let count = validate_proto_count(index, count)?;
            if count > 0 {
                self.add(index, count);
            }
        }

        let offset = proto.contiguousBinIndexOffset;
        for (i, &count) in proto.contiguousBinCounts.iter().enumerate() {
            let index = offset + i as i32;
            let count = validate_proto_count(index, count)?;
            if count > 0 {
                self.add(index, count);
            }
        }

        Ok(())
    }

    fn to_proto(&self) -> ProtoStore {
        let mut proto = ProtoStore::new();

        if self.bins.is_empty() {
            return proto;
        }

        // Use contiguous encoding for dense store
        proto.contiguousBinIndexOffset = self.offset;
        proto.contiguousBinCounts = self.bins.iter().map(|&c| c as f64).collect();

        proto
    }
}

impl Default for CollapsingHighestDenseStore {
    /// Creates a collapsing highest dense store with a default of 2048 bins.
    fn default() -> Self {
        Self::new(2048)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shared `Store` trait conformance suite. The default 2048-bin capacity is far larger than the small index
    // ranges these cases use, so no collapsing occurs and the standard `Store` contract holds.
    crate::canonical::store::store_conformance_tests!(CollapsingHighestDenseStore);

    #[test]
    fn within_limit_does_not_collapse() {
        let mut store = CollapsingHighestDenseStore::new(10);
        for i in 0..10 {
            store.add(i, 1);
        }

        assert_eq!(store.total_count(), 10);
        assert!(!store.is_collapsed());
        assert_eq!(store.bins.len(), 10);
    }

    #[test]
    fn collapse_on_low_index() {
        let mut store = CollapsingHighestDenseStore::new(5);

        // Add bins 5-9
        for i in 5..10 {
            store.add(i, 1);
        }
        assert!(!store.is_collapsed());

        // Adding index 0 should trigger collapse of highest bins
        store.add(0, 1);

        assert!(store.is_collapsed());
        assert_eq!(store.total_count(), 6);
        assert!(store.bins.len() <= 5);
    }

    #[test]
    fn collapse_on_high_index() {
        let mut store = CollapsingHighestDenseStore::new(5);

        // Add bins 0-4
        for i in 0..5 {
            store.add(i, 1);
        }
        assert!(!store.is_collapsed());

        // Adding index 10 should trigger collapse since it would need more than 5 bins
        store.add(10, 1);

        assert!(store.is_collapsed());
        assert_eq!(store.total_count(), 6);
    }

    #[test]
    fn key_at_rank_after_collapse() {
        let mut store = CollapsingHighestDenseStore::new(3);

        store.add(0, 1);
        store.add(1, 1);
        store.add(2, 1);
        // Adding a lower index should trigger collapse
        store.add(-1, 1);

        assert!(store.is_collapsed());
        assert_eq!(store.total_count(), 4);

        // All counts should still be accounted for
        assert!(store.key_at_rank(0).is_some());
        assert!(store.key_at_rank(3).is_some());
        assert!(store.key_at_rank(4).is_none());
    }

    #[test]
    fn merge_respects_collapse() {
        let mut store1 = CollapsingHighestDenseStore::new(5);
        store1.add(0, 1);

        let mut store2 = CollapsingHighestDenseStore::new(5);
        for i in 0..10 {
            store2.add(i, 1);
        }

        assert!(store2.is_collapsed());

        store1.merge(&store2);

        assert!(store1.is_collapsed());
    }
}
