//! Index mapping for DDSketch.
//!
//! The index mapping defines how floating-point values are mapped to integer bin indices,
//! which determines the relative accuracy guarantees of the sketch.

mod logarithmic;

pub use logarithmic::LogarithmicMapping;

/// Maps values to bin indices and vice versa.
///
/// The mapping defines the relationship between floating-point values and integer bin indices,
/// determining the relative accuracy of the sketch.
pub trait IndexMapping: Clone + Send + Sync {
    /// Returns the index of the bin for the given positive value.
    ///
    /// The value must be positive. For negative values, use the index of the absolute value
    /// and store in the negative store.
    fn index(&self, value: f64) -> i32;

    /// Returns the representative value for the given index.
    ///
    /// This is typically the geometric mean of the bin's lower and upper bounds.
    fn value(&self, index: i32) -> f64;

    /// Returns the lower bound of the bin at the given index.
    fn lower_bound(&self, index: i32) -> f64;

    /// Returns the upper bound of the bin at the given index.
    fn upper_bound(&self, index: i32) -> f64;

    /// Returns the relative accuracy of this mapping.
    ///
    /// The relative accuracy is the maximum relative error guaranteed for any quantile query.
    fn relative_accuracy(&self) -> f64;

    /// Returns the minimum positive value that can be indexed.
    fn min_indexable_value(&self) -> f64;

    /// Returns the maximum positive value that can be indexed.
    fn max_indexable_value(&self) -> f64;

    /// Returns the gamma value (base of the logarithm) for this mapping.
    fn gamma(&self) -> f64;
}
