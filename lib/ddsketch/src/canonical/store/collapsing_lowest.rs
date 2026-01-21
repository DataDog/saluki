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
/// - Higher quantiles (e.g., p95, p99) are more important than lower quantiles
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
                // We need to collapse the new low indices into the current lowest
                // Don't actually add the bins, just record that we collapsed
                self.is_collapsed = true;
                // The index is below our range, so when we add the count,
                // we'll add it to the lowest bin (bins[0])
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
                // Need to collapse lowest bins to make room for higher indices
                let bins_to_collapse = new_len - self.max_num_bins;
                self.collapse_lowest(bins_to_collapse);
            }

            // Now append
            let target_len = ((index - self.offset + 1) as usize).min(self.max_num_bins);
            if target_len > self.bins.len() {
                self.bins.resize(target_len, 0);
            }
        }
    }

    /// Collapses the lowest `n` bins into the bin at index `n`.
    fn collapse_lowest(&mut self, n: usize) {
        if n == 0 || self.bins.is_empty() {
            return;
        }

        self.is_collapsed = true;

        let n = n.min(self.bins.len() - 1);
        if n == 0 {
            return;
        }

        // Sum up the bins to collapse
        let collapsed_count: u64 = self.bins[..n].iter().sum();

        // Add to the bin that will become the new lowest
        self.bins[n] = self.bins[n].saturating_add(collapsed_count);

        // Remove the collapsed bins
        self.bins.drain(..n);
        self.offset += n as i32;
    }

    /// Returns the index into the bins array for the given logical index.
    ///
    /// If the index is below our range, it is mapped to the lowest bin. If the index is above our range, `None` is
    /// returned.
    #[inline]
    fn bin_index(&self, index: i32) -> Option<usize> {
        if index < self.offset {
            // Index is below our range, map to lowest bin
            Some(0)
        } else {
            let idx = (index - self.offset) as usize;
            if idx < self.bins.len() {
                Some(idx)
            } else {
                None
            }
        }
    }
}

impl Store for CollapsingLowestDenseStore {
    fn add(&mut self, index: i32, count: u64) {
        if count == 0 {
            return;
        }

        self.grow(index);

        if let Some(bin_idx) = self.bin_index(index) {
            self.bins[bin_idx] = self.bins[bin_idx].saturating_add(count);
        }
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

    #[test]
    fn test_within_limit() {
        let mut store = CollapsingLowestDenseStore::new(10);
        for i in 0..10 {
            store.add(i, 1);
        }

        assert_eq!(store.total_count(), 10);
        assert!(!store.is_collapsed());
        assert_eq!(store.bins.len(), 10);
    }

    #[test]
    fn test_collapse_on_high_index() {
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
    fn test_collapse_on_low_index() {
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
    fn test_key_at_rank_after_collapse() {
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
    fn test_merge_respects_collapse() {
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
