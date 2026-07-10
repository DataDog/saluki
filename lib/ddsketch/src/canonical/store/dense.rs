use datadog_protos::sketches::Store as ProtoStore;

use super::{validate_proto_count, Store};
use crate::canonical::error::ProtoConversionError;

/// A dense store using contiguous array storage.
///
/// This store grows unbounded to accommodate any range of indices. It's memory-efficient when the indices are clustered
/// together, but can use significant memory if indices are widely scattered.
///
/// Use this store when:
/// - You have a bounded range of input values
/// - Memory usage isn't a concern
/// - You need the fastest possible insertion performance
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DenseStore {
    /// The bin counts, stored contiguously.
    bins: Vec<u64>,

    /// The count stored in bins[0] corresponds to this index.
    offset: i32,

    /// Total count across all bins.
    count: u64,
}

impl DenseStore {
    /// Creates an empty `DenseStore`.
    pub fn new() -> Self {
        Self {
            bins: Vec::new(),
            offset: 0,
            count: 0,
        }
    }

    /// Ensures the store can accommodate the given index, growing if necessary.
    fn grow(&mut self, index: i32) {
        if self.bins.is_empty() {
            self.bins.push(0);
            self.offset = index;
            return;
        }

        if index < self.offset {
            // Need to prepend bins
            let num_prepend = (self.offset - index) as usize;
            let mut new_bins = vec![0u64; num_prepend + self.bins.len()];
            new_bins[num_prepend..].copy_from_slice(&self.bins);
            self.bins = new_bins;
            self.offset = index;
        } else if index >= self.offset + self.bins.len() as i32 {
            // Need to append bins
            let new_len = (index - self.offset + 1) as usize;
            self.bins.resize(new_len, 0);
        }
    }

    /// Returns the index into the bins array for the given logical index.
    #[inline]
    fn bin_index(&self, index: i32) -> usize {
        (index - self.offset) as usize
    }

    /// Iterates through all non-zero bins in the store, yielding `(index,count)` pairs.
    pub fn iter_non_zero_bins(&self) -> impl Iterator<Item = (i32, u64)> + '_ {
        self.bins.iter().enumerate().filter_map(|(i, &count)| {
            if count == 0 {
                None
            } else {
                Some((self.offset + i as i32, count))
            }
        })
    }
}

impl Store for DenseStore {
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

        // Grow to accommodate the other store's range
        if let (Some(other_min), Some(other_max)) = (other.min_index(), other.max_index()) {
            self.grow(other_min);
            self.grow(other_max);

            // Add all non-zero bins from the other store
            for (i, &count) in other.bins.iter().enumerate() {
                if count > 0 {
                    let index = other.offset + i as i32;
                    let bin_idx = self.bin_index(index);
                    self.bins[bin_idx] = self.bins[bin_idx].saturating_add(count);
                }
            }
        }

        self.count = self.count.saturating_add(other.count);
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn clear(&mut self) {
        self.bins.clear();
        self.offset = 0;
        self.count = 0;
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

impl Default for DenseStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shared `Store` trait conformance suite (add/rank/merge/clear/proto round-trip/etc.).
    crate::canonical::store::store_conformance_tests!(DenseStore);

    // `iter_non_zero_bins` is a `DenseStore`-specific accessor, not part of the `Store` trait, so it's covered here
    // rather than in the shared conformance suite.
    #[test]
    fn iter_non_zero_bins_is_empty_for_empty_store() {
        let store = DenseStore::new();

        assert_eq!(store.iter_non_zero_bins().collect::<Vec<_>>(), Vec::new());
    }

    #[test]
    fn iter_non_zero_bins_skips_zero_counts_and_applies_offset() {
        let mut store = DenseStore::new();
        store.add(10, 3);
        store.add(12, 5);

        assert_eq!(store.iter_non_zero_bins().collect::<Vec<_>>(), vec![(10, 3), (12, 5)]);
    }
}
