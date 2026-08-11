//! Sketch storage.

use datadog_protos::sketches::Store as ProtoStore;

use super::error::ProtoConversionError;

mod collapsing_highest;
pub use self::collapsing_highest::CollapsingHighestDenseStore;

mod collapsing_lowest;
pub use self::collapsing_lowest::CollapsingLowestDenseStore;

mod dense;
pub use self::dense::DenseStore;

mod sparse;
pub use self::sparse::SparseStore;

/// Storage for sketch observations.
///
/// Stores manage holding the counts of mapped values, such that they contain a list of bins and the number of
/// observations currently counted in each bin.
pub trait Store: Clone + Send + Sync {
    /// Adds a count to the bin at the given index.
    fn add(&mut self, index: i32, count: u64);

    /// Returns the total count across all bins.
    fn total_count(&self) -> u64;

    /// Returns the minimum index with a non-zero count, or `None` if empty.
    fn min_index(&self) -> Option<i32>;

    /// Returns the maximum index with a non-zero count, or `None` if empty.
    fn max_index(&self) -> Option<i32>;

    /// Returns the index of the bin containing the given rank.
    ///
    /// The rank is 0-indexed, so rank 0 is the first observation.
    fn key_at_rank(&self, rank: u64) -> Option<i32>;

    /// Merges another store into this one.
    fn merge(&mut self, other: &Self);

    /// Returns `true` if the store is empty.
    fn is_empty(&self) -> bool;

    /// Clears all bins from the store.
    fn clear(&mut self);

    /// Populates this store from a protobuf `Store`.
    fn merge_from_proto(&mut self, proto: &ProtoStore) -> Result<(), ProtoConversionError>;

    /// Converts this store to a protobuf `Store`.
    fn to_proto(&self) -> ProtoStore;
}

/// Validates and converts a protobuf `f64` count to `u64`.
///
/// # Errors
///
/// If the count is negative, or has a fractional part, an error is returned.
pub(crate) fn validate_proto_count(index: i32, count: f64) -> Result<u64, ProtoConversionError> {
    if count < 0.0 {
        return Err(ProtoConversionError::NegativeBinCount { index, count });
    }
    if count.fract() != 0.0 {
        return Err(ProtoConversionError::NonIntegerBinCount { index, count });
    }
    Ok(count as u64)
}

/// Generates the shared [`Store`] trait conformance suite for a concrete store type.
///
/// Every `Store` implementation shares the same observable contract for adding counts, reporting
/// totals/min/max, ranking, merging, clearing, and round-tripping through protobuf. Rather than hand-duplicate that
/// suite across each sibling implementation, each implementation's `tests` module invokes this macro with its
/// concrete type. Implementation-specific behavior (collapsing, private-field layout, the dense-only bin iterator)
/// is still covered by inline tests in the respective module.
///
/// The store type must implement `Default`. The collapsing stores default to 2048 bins, which is far larger than
/// any index range these cases use, so no collapsing occurs and the shared assertions hold for every
/// implementation.
#[cfg(test)]
macro_rules! store_conformance_tests {
    ($store:ty) => {
        #[test]
        fn add_single_value_sets_count_and_bounds() {
            let mut store = <$store>::default();
            store.add(5, 1);

            assert_eq!(store.total_count(), 1);
            assert_eq!(store.min_index(), Some(5));
            assert_eq!(store.max_index(), Some(5));
        }

        #[test]
        fn add_repeated_at_same_index_accumulates() {
            let mut store = <$store>::default();
            store.add(5, 3);
            store.add(5, 2);

            assert_eq!(store.total_count(), 5);
            assert_eq!(store.min_index(), Some(5));
            assert_eq!(store.max_index(), Some(5));
        }

        #[test]
        fn add_distinct_indices_tracks_distribution_and_bounds() {
            let mut store = <$store>::default();
            store.add(5, 1);
            store.add(10, 2);
            store.add(3, 3);

            assert_eq!(store.total_count(), 6);
            assert_eq!(store.min_index(), Some(3));
            assert_eq!(store.max_index(), Some(10));
            assert_eq!(
                $crate::canonical::store::conformance::distribution(&store),
                [(3, 3), (5, 1), (10, 2)].into_iter().collect()
            );
        }

        #[test]
        fn add_with_zero_count_is_a_noop() {
            let mut store = <$store>::default();
            store.add(5, 0);

            assert!(store.is_empty());
            assert_eq!(store.total_count(), 0);
            assert_eq!(store.min_index(), None);
        }

        #[test]
        fn key_at_rank_maps_each_rank_to_its_index() {
            let mut store = <$store>::default();
            store.add(5, 3);
            store.add(10, 2);

            assert_eq!(store.key_at_rank(0), Some(5));
            assert_eq!(store.key_at_rank(2), Some(5));
            assert_eq!(store.key_at_rank(3), Some(10));
            assert_eq!(store.key_at_rank(4), Some(10));
            assert_eq!(store.key_at_rank(5), None);
        }

        #[test]
        fn empty_store_reports_no_min_max_or_rank() {
            let store = <$store>::default();

            assert!(store.is_empty());
            assert_eq!(store.total_count(), 0);
            assert_eq!(store.min_index(), None);
            assert_eq!(store.max_index(), None);
            assert_eq!(store.key_at_rank(0), None);
        }

        #[test]
        fn merge_combines_distributions() {
            let mut store1 = <$store>::default();
            store1.add(5, 2);
            store1.add(10, 1);

            let mut store2 = <$store>::default();
            store2.add(5, 1);
            store2.add(15, 3);

            store1.merge(&store2);

            assert_eq!(store1.total_count(), 7);
            assert_eq!(store1.min_index(), Some(5));
            assert_eq!(store1.max_index(), Some(15));
            assert_eq!(
                $crate::canonical::store::conformance::distribution(&store1),
                [(5, 3), (10, 1), (15, 3)].into_iter().collect()
            );
        }

        #[test]
        fn merge_from_empty_store_is_a_noop() {
            let mut store = <$store>::default();
            store.add(5, 2);

            let empty = <$store>::default();
            store.merge(&empty);

            assert_eq!(store.total_count(), 2);
            assert_eq!(
                $crate::canonical::store::conformance::distribution(&store),
                [(5, 2)].into_iter().collect()
            );
        }

        #[test]
        fn clear_resets_to_empty() {
            let mut store = <$store>::default();
            store.add(5, 2);
            store.add(10, 1);

            store.clear();

            assert!(store.is_empty());
            assert_eq!(store.total_count(), 0);
            assert_eq!(store.min_index(), None);
        }

        #[test]
        fn handles_negative_indices() {
            let mut store = <$store>::default();
            store.add(-5, 1);
            store.add(5, 1);

            assert_eq!(store.total_count(), 2);
            assert_eq!(store.min_index(), Some(-5));
            assert_eq!(store.max_index(), Some(5));
            assert_eq!(
                $crate::canonical::store::conformance::distribution(&store),
                [(-5, 1), (5, 1)].into_iter().collect()
            );
        }

        #[test]
        fn handles_widely_scattered_indices() {
            let mut store = <$store>::default();
            store.add(-1000, 1);
            store.add(0, 2);
            store.add(1000, 3);

            assert_eq!(store.total_count(), 6);
            assert_eq!(store.min_index(), Some(-1000));
            assert_eq!(store.max_index(), Some(1000));
            assert_eq!(
                $crate::canonical::store::conformance::distribution(&store),
                [(-1000, 1), (0, 2), (1000, 3)].into_iter().collect()
            );
        }

        #[test]
        fn proto_round_trip_preserves_distribution() {
            let mut store = <$store>::default();
            store.add(-3, 4);
            store.add(5, 2);
            store.add(10, 1);

            let proto = store.to_proto();
            let mut restored = <$store>::default();
            restored
                .merge_from_proto(&proto)
                .expect("round-tripped proto must be valid");

            assert_eq!(restored.total_count(), store.total_count());
            assert_eq!(
                $crate::canonical::store::conformance::distribution(&restored),
                $crate::canonical::store::conformance::distribution(&store)
            );
        }
    };
}

#[cfg(test)]
pub(crate) use store_conformance_tests;

#[cfg(test)]
pub(crate) mod conformance {
    use std::collections::BTreeMap;

    use super::Store;

    /// Reconstructs the full `index -> count` distribution of a store using only the public [`Store`] trait API.
    ///
    /// This walks every rank in `[0, total_count)` and tallies the index each rank maps to. Doing this through the
    /// trait (rather than each implementation's private fields) lets the shared conformance suite assert exact bin
    /// distributions for every `Store`, including after merges and protobuf round-trips.
    pub(crate) fn distribution<S: Store>(store: &S) -> BTreeMap<i32, u64> {
        let mut dist = BTreeMap::new();
        for rank in 0..store.total_count() {
            let index = store
                .key_at_rank(rank)
                .expect("every rank below total_count must map to an index");
            *dist.entry(index).or_insert(0) += 1;
        }
        dist
    }
}
