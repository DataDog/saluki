//! Store implementations for DDSketch bins.
//!
//! A store manages the bin counts for either positive or negative values.
//! Different implementations provide different memory/accuracy trade-offs.

mod collapsing_highest;
mod collapsing_lowest;
mod dense;
mod sparse;

pub use collapsing_highest::CollapsingHighestDenseStore;
pub use collapsing_lowest::CollapsingLowestDenseStore;
pub use dense::DenseStore;
pub use sparse::SparseStore;

/// A store for DDSketch bins.
///
/// Stores manage the bin counts for either positive or negative values.
/// Different implementations provide different memory/accuracy trade-offs:
///
/// - [`DenseStore`]: Contiguous array storage, grows unbounded. Best for data with
///   a bounded range of values.
/// - [`SparseStore`]: Hash map storage, efficient for scattered indices but no collapsing.
/// - [`CollapsingLowestDenseStore`]: Dense storage with a maximum bin limit. When the limit
///   is exceeded, lowest-indexed bins are collapsed. Best when higher quantiles (e.g., p99)
///   are more important.
/// - [`CollapsingHighestDenseStore`]: Dense storage with a maximum bin limit. When the limit
///   is exceeded, highest-indexed bins are collapsed. Best when lower quantiles (e.g., p1)
///   are more important.
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

    /// Returns whether the store is empty.
    fn is_empty(&self) -> bool;

    /// Clears all bins from the store.
    fn clear(&mut self);
}

/// A provider for creating new store instances.
///
/// This trait allows the DDSketch to create stores without knowing the concrete type.
pub trait StoreProvider: Clone + Send + Sync {
    /// The type of store this provider creates.
    type Store: Store;

    /// Creates a new empty store.
    fn new_store(&self) -> Self::Store;
}

/// A provider that creates stores using their `Default` implementation.
#[derive(Clone, Debug, Default)]
pub struct DefaultStoreProvider<S>(std::marker::PhantomData<S>);

impl<S> DefaultStoreProvider<S> {
    /// Creates a new default store provider.
    pub fn new() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<S: Store + Default> StoreProvider for DefaultStoreProvider<S> {
    type Store = S;

    fn new_store(&self) -> Self::Store {
        S::default()
    }
}
