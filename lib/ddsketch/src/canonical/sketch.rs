//! Canonical DDSketch implementation.

use super::mapping::{IndexMapping, LogarithmicMapping};
use super::store::{CollapsingLowestDenseStore, Store};
use crate::common::float_eq;

/// A fast and fully-mergeable quantile sketch with relative-error guarantees.
///
/// This implementation supports most of the capabilities of the various official DDSketch implementations, such as:
///
/// - support for tracking negative and positive values
/// - multiple store types (sparse, dense, collapsing)
/// - configurable index interpolation schemes (only logarithmic currently supported)
///
/// Defaults to using a "low collapsing" dense store with a logarithmic index mapping. This works well for tracking
/// values like time durations/latencies where the tail latencies (higher percentiles) matter most.
///
/// # Example
///
/// ```
/// use ddsketch::canonical::DDSketch;
///
/// let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
/// sketch.add(1.0);
/// sketch.add(2.0);
/// sketch.add(3.0);
///
/// let median = sketch.quantile(0.5).unwrap();
/// ```
#[derive(Clone, Debug)]
pub struct DDSketch<M: IndexMapping = LogarithmicMapping, S: Store = CollapsingLowestDenseStore> {
    /// The index mapping for this sketch.
    mapping: M,

    /// Store for positive values.
    positive_store: S,

    /// Store for negative values.
    negative_store: S,

    /// Count of values that map to zero.
    zero_count: u64,

    /// Total count of all values.
    count: u64,

    /// Sum of all values.
    sum: f64,

    /// Minimum value seen.
    min: f64,

    /// Maximum value seen.
    max: f64,
}

impl DDSketch<LogarithmicMapping, CollapsingLowestDenseStore> {
    /// Creates a new `DDSketch` with the given relative accuracy.
    ///
    /// Defaults to logarithmic mapping and the "low collapsing" dense store, with a maximum of 2048 bins per store.
    ///
    /// # Errors
    ///
    /// If the relative accuracy is not between `0` and `1`, an error is returned.
    pub fn with_relative_accuracy(relative_accuracy: f64) -> Result<Self, &'static str> {
        let mapping = LogarithmicMapping::new(relative_accuracy)?;
        Ok(Self::new(
            mapping,
            CollapsingLowestDenseStore::default(),
            CollapsingLowestDenseStore::default(),
        ))
    }
}

impl<M: IndexMapping, S: Store> DDSketch<M, S> {
    /// Creates a new DDSketch with the given mapping and stores.
    pub fn new(mapping: M, positive_store: S, negative_store: S) -> Self {
        Self {
            mapping,
            positive_store,
            negative_store,
            zero_count: 0,
            count: 0,
            sum: 0.0,
            min: f64::MAX,
            max: f64::MIN,
        }
    }

    /// Adds a single value to the sketch.
    pub fn add(&mut self, value: f64) {
        self.add_n(value, 1);
    }

    /// Adds a value to the sketch with the given count.
    ///
    /// This is useful for weighted values or pre-aggregated data.
    pub fn add_n(&mut self, value: f64, n: u64) {
        if n == 0 {
            return;
        }

        self.count += n;
        self.sum += value * n as f64;

        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }

        if value > self.mapping.min_indexable_value() {
            let index = self.mapping.index(value);
            self.positive_store.add(index, n);
        } else if value < -self.mapping.min_indexable_value() {
            let index = self.mapping.index(-value);
            self.negative_store.add(index, n);
        } else {
            self.zero_count += n;
        }
    }

    /// Returns the approximate value at the given quantile.
    ///
    /// The quantile should be in the range of [0, 1]. If it is outside of this range, it will clamped to either 0 or 1.
    ///
    /// Returns `None` if the sketch is empty, otherwise returns the approximate value.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }

        if q <= 0.0 {
            return Some(self.min);
        }

        if q >= 1.0 {
            return Some(self.max);
        }

        let rank = (q * (self.count - 1) as f64).round() as u64;

        let negative_count = self.negative_store.total_count();
        let total_negative_and_zero = negative_count + self.zero_count;

        if rank < negative_count {
            // We need to reverse the rank since negative values are stored with positive indices
            let reverse_rank = negative_count - rank - 1;
            if let Some(index) = self.negative_store.key_at_rank(reverse_rank) {
                return Some(-self.mapping.value(index));
            }
        } else if rank < total_negative_and_zero {
            return Some(0.0);
        } else {
            let positive_rank = rank - total_negative_and_zero;
            if let Some(index) = self.positive_store.key_at_rank(positive_rank) {
                return Some(self.mapping.value(index));
            }
        }

        unreachable!("rank out of bounds on non-empty sketch")
    }

    /// Merges another sketch into this one.
    ///
    /// The other sketch must use the same mapping type.
    pub fn merge(&mut self, other: &Self)
    where
        M: PartialEq,
    {
        if other.count == 0 {
            return;
        }

        self.count += other.count;
        self.sum += other.sum;

        if other.min < self.min {
            self.min = other.min;
        }
        if other.max > self.max {
            self.max = other.max;
        }

        self.positive_store.merge(&other.positive_store);
        self.negative_store.merge(&other.negative_store);
        self.zero_count += other.zero_count;
    }

    /// Returns whether the sketch is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns the total number of values added to the sketch.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Returns the sum of all values, or `None` if the sketch is empty.
    pub fn sum(&self) -> Option<f64> {
        if self.is_empty() {
            None
        } else {
            Some(self.sum)
        }
    }

    /// Returns the minimum value, or `None` if the sketch is empty.
    pub fn min(&self) -> Option<f64> {
        if self.is_empty() {
            None
        } else {
            Some(self.min)
        }
    }

    /// Returns the maximum value, or `None` if the sketch is empty.
    pub fn max(&self) -> Option<f64> {
        if self.is_empty() {
            None
        } else {
            Some(self.max)
        }
    }

    /// Returns the average of all values, or `None` if the sketch is empty.
    pub fn avg(&self) -> Option<f64> {
        if self.is_empty() {
            None
        } else {
            Some(self.sum / self.count as f64)
        }
    }

    /// Clears the sketch, removing all values.
    pub fn clear(&mut self) {
        self.positive_store.clear();
        self.negative_store.clear();
        self.zero_count = 0;
        self.count = 0;
        self.sum = 0.0;
        self.min = f64::MAX;
        self.max = f64::MIN;
    }

    /// Returns a reference to the index mapping.
    pub fn mapping(&self) -> &M {
        &self.mapping
    }

    /// Returns a reference to the positive value store.
    pub fn positive_store(&self) -> &S {
        &self.positive_store
    }

    /// Returns a reference to the negative value store.
    pub fn negative_store(&self) -> &S {
        &self.negative_store
    }

    /// Returns the count of values mapped to zero.
    pub fn zero_count(&self) -> u64 {
        self.zero_count
    }

    /// Returns the relative accuracy of this sketch.
    pub fn relative_accuracy(&self) -> f64 {
        self.mapping.relative_accuracy()
    }
}

impl<M: IndexMapping, S: Store> PartialEq for DDSketch<M, S> {
    fn eq(&self, other: &Self) -> bool {
        self.count == other.count
            && self.zero_count == other.zero_count
            && float_eq(self.min, other.min)
            && float_eq(self.max, other.max)
            && float_eq(self.sum, other.sum)
    }
}

impl<M: IndexMapping, S: Store> Eq for DDSketch<M, S> {}

impl<M: IndexMapping + Default, S: Store + Default> Default for DDSketch<M, S> {
    fn default() -> Self {
        Self::new(M::default(), S::default(), S::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_sketch() {
        let sketch = DDSketch::with_relative_accuracy(0.01).unwrap();

        assert!(sketch.is_empty());
        assert_eq!(sketch.count(), 0);
        assert_eq!(sketch.min(), None);
        assert_eq!(sketch.max(), None);
        assert_eq!(sketch.sum(), None);
        assert_eq!(sketch.quantile(0.5), None);
    }

    #[test]
    fn test_single_value() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch.add(42.0);

        assert!(!sketch.is_empty());
        assert_eq!(sketch.count(), 1);
        assert_eq!(sketch.min(), Some(42.0));
        assert_eq!(sketch.max(), Some(42.0));
        assert_eq!(sketch.sum(), Some(42.0));
    }

    #[test]
    fn test_multiple_values() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        for i in 1..=100 {
            sketch.add(i as f64);
        }

        assert_eq!(sketch.count(), 100);
        assert_eq!(sketch.min(), Some(1.0));
        assert_eq!(sketch.max(), Some(100.0));

        // Median should be close to 50
        let median = sketch.quantile(0.5).unwrap();
        assert!((median - 50.0).abs() < 5.0, "median {} not close to 50", median);
    }

    #[test]
    fn test_negative_values() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch.add(-10.0);
        sketch.add(-5.0);
        sketch.add(5.0);
        sketch.add(10.0);

        assert_eq!(sketch.count(), 4);
        assert_eq!(sketch.min(), Some(-10.0));
        assert_eq!(sketch.max(), Some(10.0));
    }

    #[test]
    fn test_zero_values() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch.add(0.0);
        sketch.add(0.0);
        sketch.add(1.0);

        assert_eq!(sketch.count(), 3);
        assert_eq!(sketch.zero_count(), 2);
    }

    #[test]
    fn test_merge() {
        let mut sketch1 = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch1.add(1.0);
        sketch1.add(2.0);

        let mut sketch2 = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch2.add(3.0);
        sketch2.add(4.0);

        sketch1.merge(&sketch2);

        assert_eq!(sketch1.count(), 4);
        assert_eq!(sketch1.min(), Some(1.0));
        assert_eq!(sketch1.max(), Some(4.0));
    }

    #[test]
    fn test_clear() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch.add(1.0);
        sketch.add(2.0);

        sketch.clear();

        assert!(sketch.is_empty());
        assert_eq!(sketch.count(), 0);
    }

    #[test]
    fn test_quantile_bounds() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        for i in 1..=100 {
            sketch.add(i as f64);
        }

        // q=0 should return min
        assert_eq!(sketch.quantile(0.0), Some(1.0));

        // q=1 should return max
        assert_eq!(sketch.quantile(1.0), Some(100.0));
    }

    #[test]
    fn test_add_n() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch.add_n(10.0, 5);

        assert_eq!(sketch.count(), 5);
        assert_eq!(sketch.sum(), Some(50.0));
    }

    #[test]
    fn test_relative_accuracy_guarantee() {
        let accuracy = 0.01; // 1%
        let mut sketch = DDSketch::with_relative_accuracy(accuracy).unwrap();

        // Add values from 1 to 1000
        for i in 1..=1000 {
            sketch.add(i as f64);
        }

        // Check various quantiles
        for q in [0.5, 0.9, 0.95, 0.99] {
            let estimated = sketch.quantile(q).unwrap();
            let expected = q * 1000.0;

            let relative_error = (estimated - expected).abs() / expected;
            assert!(
                relative_error <= accuracy * 2.0, // Allow some slack due to discrete bins
                "quantile {} estimated {} expected {} error {}",
                q,
                estimated,
                expected,
                relative_error
            );
        }
    }
}
