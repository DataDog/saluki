use std::collections::BTreeMap;

use datadog_protos::sketches::Store as ProtoStore;

use super::{validate_proto_count, Store};
use crate::canonical::error::ProtoConversionError;

/// A sparse store using a sorted map for bin storage.
///
/// This store only keeps track of non-empty bins, making it memory-efficient for data with widely scattered indices.
/// However, it doesn't support collapsing, so memory usage can grow unbounded.
///
/// Use this store when:
/// - Input values span a wide range with gaps
/// - You don't need bounded memory usage
/// - You want to avoid the overhead of dense array allocation
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SparseStore {
    /// The bin counts, keyed by index.
    bins: BTreeMap<i32, u64>,

    /// Total count across all bins.
    count: u64,
}

impl SparseStore {
    /// Creates an empty `SparseStore`.
    pub fn new() -> Self {
        Self {
            bins: BTreeMap::new(),
            count: 0,
        }
    }
}

impl Store for SparseStore {
    fn add(&mut self, index: i32, count: u64) {
        if count == 0 {
            return;
        }

        *self.bins.entry(index).or_insert(0) += count;
        self.count = self.count.saturating_add(count);
    }

    fn total_count(&self) -> u64 {
        self.count
    }

    fn min_index(&self) -> Option<i32> {
        self.bins.iter().find(|(_, &c)| c > 0).map(|(&k, _)| k)
    }

    fn max_index(&self) -> Option<i32> {
        self.bins.iter().rev().find(|(_, &c)| c > 0).map(|(&k, _)| k)
    }

    fn key_at_rank(&self, rank: u64) -> Option<i32> {
        if rank >= self.count {
            return None;
        }

        let mut cumulative = 0u64;
        for (&index, &count) in &self.bins {
            cumulative += count;
            if cumulative > rank {
                return Some(index);
            }
        }
        None
    }

    fn merge(&mut self, other: &Self) {
        for (&index, &count) in &other.bins {
            if count > 0 {
                *self.bins.entry(index).or_insert(0) += count;
            }
        }
        self.count = self.count.saturating_add(other.count);
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn clear(&mut self) {
        self.bins.clear();
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

        // Use sparse encoding for sparse store
        for (&index, &count) in &self.bins {
            if count > 0 {
                proto.binCounts.insert(index, count as f64);
            }
        }

        proto
    }
}

impl Default for SparseStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shared `Store` trait conformance suite (add/rank/merge/clear/proto round-trip/etc.).
    crate::canonical::store::store_conformance_tests!(SparseStore);

    // `SparseStore`'s defining property -- only allocating map entries for occupied bins -- isn't observable through
    // the `Store` trait, so it's asserted directly here against the private `bins` map.
    #[test]
    fn only_allocates_map_entries_for_occupied_bins() {
        let mut store = SparseStore::new();
        store.add(-1000, 1);
        store.add(0, 2);
        store.add(1000, 3);

        // Only 3 entries in the map, not the 2001 that a dense store would span.
        assert_eq!(store.bins.len(), 3);
    }
}
