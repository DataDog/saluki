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
