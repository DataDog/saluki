use datadog_protos::sketches::Store as ProtoStore;

use super::{validate_proto_count, Store};
use crate::canonical::error::ProtoConversionError;

/// A dense store that collapses lowest-indexed bins when capacity is exceeded.
///
/// This store maintains a maximum number of bins. When adding a new index would exceed this limit, the lowest-indexed
/// bins are collapsed (merged into the next lowest bin), sacrificing accuracy for lower quantiles to preserve accuracy
/// for higher quantiles.
///
/// Use this store when:
/// - You need bounded memory usage
/// - Higher quantiles (for example, p95, p99) are more important than lower quantiles
/// - You're tracking latencies or other metrics where the tail matters most
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollapsingLowestDenseStore {
    /// The bin counts, stored contiguously.
    bins: Vec<u64>,

    /// The count stored in bins[0] corresponds to this index.
    offset: i32,

    /// Maximum number of bins to maintain.
    max_num_bins: usize,

    /// Total count across all bins.
    count: u64,

    /// Whether collapsing has occurred (accuracy may be compromised for low quantiles).
    is_collapsed: bool,
}

impl CollapsingLowestDenseStore {
    /// Creates an empty `CollapsingLowestDenseStore` with the given maximum number of bins.
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
    /// If true, accuracy guarantees may not hold for lower quantiles.
    pub fn is_collapsed(&self) -> bool {
        self.is_collapsed
    }

    /// Ensures the store can accommodate the given index, growing and collapsing if necessary.
    ///
    /// On return, `index` is always representable: it either falls inside `[offset, offset + bins.len())`, or it falls
    /// below `offset`, in which case it belongs to the collapsed lowest bin. [`Self::bin_index`] relies on this.
    fn grow(&mut self, index: i32) {
        if self.bins.is_empty() {
            self.bins.push(0);
            self.offset = index;
            return;
        }

        if index < self.offset {
            // Need to prepend bins - but first check if we need to collapse
            let num_prepend = (self.offset - index) as usize;
            let new_len = self.bins.len() + num_prepend;

            if new_len > self.max_num_bins {
                // The index sits below the widest window the store can represent. Widen the window downwards to the
                // cap first, so the collapsed counts land on the lowest index that's still representable, then let
                // `bin_index` fold `index` into `bins[0]`.
                //
                // Widening matters for accuracy: without it the collapsed floor is wherever the window happened to
                // already start, which makes the result depend on insertion order. Descending input would pin every
                // observation to the first index seen instead of spreading it across the available bins. The
                // reference implementation places the floor at `max_index - max_num_bins + 1`, which is what widening
                // to the cap achieves here.
                let widen_by = self.max_num_bins - self.bins.len();
                if widen_by > 0 {
                    let mut widened = vec![0u64; self.max_num_bins];
                    widened[widen_by..].copy_from_slice(&self.bins);
                    self.bins = widened;
                    self.offset -= widen_by as i32;
                }

                self.is_collapsed = true;
                return;
            }

            let mut new_bins = vec![0u64; new_len];
            new_bins[num_prepend..].copy_from_slice(&self.bins);
            self.bins = new_bins;
            self.offset = index;
        } else if index >= self.offset + self.bins.len() as i32 {
            // Need to append bins
            let new_len = (index - self.offset + 1) as usize;

            if new_len > self.max_num_bins {
                // The window can't stretch far enough to cover `index`, so slide its bottom up to the lowest index
                // that keeps `index` in range, collapsing everything below that point.
                self.collapse_below(index - self.max_num_bins as i32 + 1);
            }

            // Now append
            let target_len = ((index - self.offset + 1) as usize).min(self.max_num_bins);
            if target_len > self.bins.len() {
                self.bins.resize(target_len, 0);
            }
        }

        debug_assert!(
            index < self.offset + self.bins.len() as i32,
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

    /// Collapses every bin below `new_offset` into the bin at `new_offset`, making it the new lowest bin.
    ///
    /// Taking the new lowest index rather than a number of bins to shift by is what keeps this correct when
    /// `new_offset` lands above the window entirely: that case folds the whole store into a single bin, instead of
    /// clamping the shift and leaving the window short of where it needs to be.
    fn collapse_below(&mut self, new_offset: i32) {
        debug_assert!(
            new_offset > self.offset,
            "collapse_below only slides the window upwards"
        );

        if self.bins.is_empty() {
            return;
        }

        self.is_collapsed = true;

        let n = (new_offset - self.offset) as usize;
        if n >= self.bins.len() {
            // Every bin sits below the new lowest index, so the whole store folds into one bin.
            let collapsed_count: u64 = self.bins.iter().sum();
            self.bins.clear();
            self.bins.push(collapsed_count);
        } else {
            let collapsed_count: u64 = self.bins[..n].iter().sum();
            self.bins[n] = self.bins[n].saturating_add(collapsed_count);
            self.bins.drain(..n);
        }

        self.offset = new_offset;
    }

    /// Returns the index into the bins array for the given logical index.
    ///
    /// Indices below the window map to the lowest bin, which is where collapsed counts accumulate. [`Self::grow`]
    /// guarantees no index arrives here from above the window, and that the store holds at least one bin.
    #[inline]
    fn bin_index(&self, index: i32) -> usize {
        if index < self.offset {
            // Index is below our range, map to lowest bin
            return 0;
        }

        let idx = (index - self.offset) as usize;
        debug_assert!(idx < self.bins.len(), "grow should have made `index` representable");

        // Clamping keeps the bins consistent with `count` even if that invariant is ever broken: the observation
        // loses accuracy instead of being dropped outright.
        idx.min(self.bins.len() - 1)
    }
}

impl Store for CollapsingLowestDenseStore {
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

        // Adjust count since add() already incremented it
        // We need to subtract the other.count we added and use the saturating add
        // Actually, the adds already handle this correctly
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
        // Process sparse binCounts
        for (&index, &count) in &proto.binCounts {
            let count = validate_proto_count(index, count)?;
            if count > 0 {
                self.add(index, count);
            }
        }

        // Process contiguous bins
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

impl Default for CollapsingLowestDenseStore {
    /// Creates a collapsing lowest dense store with a default of 2048 bins.
    fn default() -> Self {
        Self::new(2048)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shared `Store` trait conformance suite. The default 2048-bin capacity is far larger than the small index
    // ranges these cases use, so no collapsing occurs and the standard `Store` contract holds.
    crate::canonical::store::store_conformance_tests!(CollapsingLowestDenseStore);

    #[test]
    fn within_limit_does_not_collapse() {
        let mut store = CollapsingLowestDenseStore::new(10);
        for i in 0..10 {
            store.add(i, 1);
        }

        assert_eq!(store.total_count(), 10);
        assert!(!store.is_collapsed());
        assert_eq!(store.bins.len(), 10);
    }

    #[test]
    fn collapse_on_high_index() {
        let mut store = CollapsingLowestDenseStore::new(5);

        // Add bins 0-4
        for i in 0..5 {
            store.add(i, 1);
        }
        assert!(!store.is_collapsed());

        // Adding index 5 should trigger collapse
        store.add(5, 1);

        assert!(store.is_collapsed());
        assert_eq!(store.total_count(), 6);
        assert!(store.bins.len() <= 5);
    }

    #[test]
    fn collapse_on_low_index() {
        let mut store = CollapsingLowestDenseStore::new(5);

        // Add bins 5-9
        for i in 5..10 {
            store.add(i, 1);
        }
        assert!(!store.is_collapsed());

        // Adding index 0 should trigger collapse since it would need 10 bins
        store.add(0, 1);

        assert!(store.is_collapsed());
        assert_eq!(store.total_count(), 6);
    }

    #[test]
    fn key_at_rank_after_collapse() {
        let mut store = CollapsingLowestDenseStore::new(3);

        store.add(0, 1);
        store.add(1, 1);
        store.add(2, 1);
        store.add(3, 1); // This should trigger collapse

        assert!(store.is_collapsed());
        assert_eq!(store.total_count(), 4);

        // All counts should still be accounted for
        assert!(store.key_at_rank(0).is_some());
        assert!(store.key_at_rank(3).is_some());
        assert!(store.key_at_rank(4).is_none());
    }

    #[test]
    fn merge_respects_collapse() {
        let mut store1 = CollapsingLowestDenseStore::new(5);
        store1.add(0, 1);

        let mut store2 = CollapsingLowestDenseStore::new(5);
        for i in 0..10 {
            store2.add(i, 1);
        }

        assert!(store2.is_collapsed());

        store1.merge(&store2);

        assert!(store1.is_collapsed());
    }
}
