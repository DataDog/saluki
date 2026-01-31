//! Canonical DDSketch implementation.

use datadog_protos::sketches::DDSketch as ProtoDDSketch;

use super::error::ProtoConversionError;
use super::mapping::{IndexMapping, LogarithmicMapping};
use super::store::{CollapsingLowestDenseStore, Store};

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
    /// Creates a new `DDSketch` with the given mapping and stores.
    pub fn new(mapping: M, positive_store: S, negative_store: S) -> Self {
        Self {
            mapping,
            positive_store,
            negative_store,
            zero_count: 0,
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
    /// The quantile must be in the range of [0, 1].
    ///
    /// Returns `None` if the sketch is empty, or if the quantile is out of bounds. Otherwise, returns the approximate
    /// value.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.is_empty() {
            return None;
        }

        if !(0.0..=1.0).contains(&q) {
            return None;
        }

        let rank = (q * (self.count() - 1) as f64).round_ties_even() as u64;

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
        if other.is_empty() {
            return;
        }

        self.positive_store.merge(&other.positive_store);
        self.negative_store.merge(&other.negative_store);
        self.zero_count += other.zero_count;
    }

    /// Returns `true` if the sketch is empty.
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// Returns the total number of values added to the sketch.
    pub fn count(&self) -> u64 {
        self.negative_store().total_count() + self.positive_store().total_count() + self.zero_count
    }

    /// Clears the sketch, removing all values.
    pub fn clear(&mut self) {
        self.positive_store.clear();
        self.negative_store.clear();
        self.zero_count = 0;
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

    /// Creates a `DDSketch` from a protobuf `DDSketch` message.
    ///
    /// This validates that the protobuf's index mapping is compatible with
    /// the mapping type `M`, then populates the stores with the bin data.
    ///
    /// # Arguments
    ///
    /// * `proto` - The protobuf `DDSketch` message to convert from
    /// * `mapping` - The mapping instance to use (must be compatible with proto's mapping)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The protobuf is missing a mapping
    /// - The mapping parameters don't match the provided mapping
    /// - Any bin counts are negative or non-integer
    /// - The zero count is negative or non-integer
    ///
    /// # Note
    ///
    /// The protobuf `DDSketch` does not include `sum`, `min`, `max`, or `count` fields.
    /// These are computed or set to defaults:
    /// - `count`: sum of all bin counts plus zero_count
    /// - `sum`, `min`, `max`: set to sentinel defaults (cannot be recovered from proto)
    pub fn from_proto(proto: &ProtoDDSketch, mapping: M) -> Result<Self, ProtoConversionError>
    where
        S: Default,
    {
        // Validate the mapping
        let proto_mapping = proto.mapping.as_ref().ok_or(ProtoConversionError::MissingMapping)?;
        mapping.validate_proto_mapping(proto_mapping)?;

        // Validate and convert zero count
        let zero_count = if proto.zeroCount < 0.0 {
            return Err(ProtoConversionError::NegativeZeroCount { count: proto.zeroCount });
        } else if proto.zeroCount.fract() != 0.0 {
            return Err(ProtoConversionError::NonIntegerZeroCount { count: proto.zeroCount });
        } else {
            proto.zeroCount as u64
        };

        let mut positive_store = S::default();
        if let Some(proto_positive) = proto.positiveValues.as_ref() {
            positive_store.merge_from_proto(proto_positive)?;
        }

        let mut negative_store = S::default();
        if let Some(proto_negative) = proto.negativeValues.as_ref() {
            negative_store.merge_from_proto(proto_negative)?;
        }

        Ok(Self {
            mapping,
            positive_store,
            negative_store,
            zero_count,
        })
    }

    /// Converts this `DDSketch` to a protobuf `DDSketch` message.
    ///
    /// # Note
    ///
    /// The protobuf `DDSketch` does not include `sum`, `min`, `max`, or `count` fields.
    /// This information is lost in the conversion.
    pub fn to_proto(&self) -> ProtoDDSketch {
        let mut proto = ProtoDDSketch::new();

        proto.set_mapping(self.mapping.to_proto());

        if !self.positive_store().is_empty() {
            proto.set_positiveValues(self.positive_store.to_proto());
        }

        if !self.negative_store().is_empty() {
            proto.set_negativeValues(self.negative_store.to_proto());
        }

        proto.set_zeroCount(self.zero_count as f64);

        proto
    }
}

impl<M: IndexMapping + PartialEq, S: Store + PartialEq> PartialEq for DDSketch<M, S> {
    fn eq(&self, other: &Self) -> bool {
        self.mapping == other.mapping
            && self.positive_store == other.positive_store
            && self.negative_store == other.negative_store
            && self.zero_count == other.zero_count
    }
}

impl<M: IndexMapping + PartialEq, S: Store + PartialEq> Eq for DDSketch<M, S> {}

impl<M: IndexMapping + Default, S: Store + Default> Default for DDSketch<M, S> {
    fn default() -> Self {
        Self::new(M::default(), S::default(), S::default())
    }
}

#[cfg(test)]
mod tests {
    use ndarray::{Array, Axis};
    use ndarray_stats::{
        interpolate::{Higher, Lower},
        QuantileExt,
    };
    use noisy_float::types::N64;
    use num_traits::ToPrimitive as _;

    use super::*;

    macro_rules! assert_rel_acc_range_eq {
        ($quantile:expr, $rel_acc:expr, $expected_lower:expr, $expected_upper:expr, $actual:expr) => {{
            let expected_lower_f64 = $expected_lower.to_f64().unwrap();
            let expected_lower_adj = if expected_lower_f64 > 0.0 {
                expected_lower_f64 * (1.0 - $rel_acc)
            } else {
                expected_lower_f64 * (1.0 + $rel_acc)
            };
            let expected_upper_f64 = $expected_upper.to_f64().unwrap();
            let expected_upper_adj = if expected_upper_f64 > 0.0 {
                expected_upper_f64 * (1.0 + $rel_acc)
            } else {
                expected_upper_f64 * (1.0 - $rel_acc)
            };
            let actual = $actual.to_f64().unwrap();

            /*
            For debugging purposes:

            println!(
                "asserting range equality for q={}, expected_lower={} (adj: {}), expected_upper={} (adj: {}), actual={}",
                $quantile,
                $expected_lower,
                expected_lower_adj,
                $expected_upper,
                expected_upper_adj,
                actual
            );
            */

            assert!(
                actual >= expected_lower_adj && actual <= expected_upper_adj,
                "mismatch at q={}: expected {} - {} ({}% relative accuracy), got {}",
                $quantile,
                expected_lower_adj,
                expected_upper_adj,
                $rel_acc * 100.0,
                actual
            );
        }};
    }

    macro_rules! assert_rel_acc_eq {
        ($quantile:expr, $rel_acc:expr, $expected:expr, $actual:expr) => {{
            assert_rel_acc_range_eq!($quantile, $rel_acc, $expected, $expected, $actual);
        }};
    }

    struct Dataset<M: IndexMapping, S: Store> {
        raw_data: Vec<N64>,
        sketch: DDSketch<M, S>,
    }

    impl<M: IndexMapping, S: Store + Default> Dataset<M, S> {
        fn new<V>(index_mapping: M, values: V) -> Self
        where
            V: Iterator<Item = f64>,
        {
            let mut raw_data = Vec::new();
            let mut sketch = DDSketch::new(index_mapping, S::default(), S::default());
            for value in values {
                raw_data.push(N64::new(value));
                sketch.add(value);
            }

            Self { raw_data, sketch }
        }

        #[track_caller]
        fn validate(self, quantiles: &[f64]) {
            let Self { mut raw_data, sketch } = self;

            // Make sure the total counts match.
            assert_eq!(raw_data.len() as u64, sketch.count());

            // Sort our raw data before comparing quantiles.
            raw_data.sort();

            let mut data = Array::from_vec(raw_data);
            let rel_acc = sketch.relative_accuracy();

            // Compare quantiles.
            for q in quantiles {
                let expected_lower = data
                    .quantile_axis_mut(Axis(0), N64::new(*q), &Lower)
                    .map(|v| v.into_scalar())
                    .ok();
                let expected_upper = data
                    .quantile_axis_mut(Axis(0), N64::new(*q), &Higher)
                    .map(|v| v.into_scalar())
                    .ok();
                let actual = sketch.quantile(*q).map(N64::new);

                match (expected_lower, expected_upper, actual) {
                    (Some(expected_lower), Some(expected_upper), Some(actual)) => {
                        // DDSketch does not do linear interpolation between the two closest values, so for example,
                        // if we have 10 values (1-5 and 10-15, let's say), with an alpha of 0.01, and we ask for
                        // q=5, you might expect to get back 7.5 -- (5 + 10) / 2 -- but DDSketch can return anywhere
                        // from 5*0.99 to 10*1.01, or 4.95 to 10.1.
                        //
                        // We capture the quantile from the raw data with different interpolation methods to
                        // calculate those wider bounds so we can validate against the actual guarantees provided by
                        // DDSketch.
                        assert_rel_acc_range_eq!(q, rel_acc, expected_lower, expected_upper, actual);
                    }
                    (None, None, None) => (),
                    _ => panic!(
                        "mismatched quantiles: expected_lower={:?}, expected_upper={:?}, actual {:?}",
                        expected_lower, expected_upper, actual
                    ),
                }
            }
        }
    }

    fn integers(start: i64, end: i64) -> impl Iterator<Item = f64> {
        (start..=end).map(|x| x as f64)
    }

    #[test]
    fn test_empty_sketch() {
        let sketch = DDSketch::with_relative_accuracy(0.01).unwrap();

        assert!(sketch.is_empty());
        assert_eq!(sketch.count(), 0);
        assert_eq!(sketch.quantile(0.5), None);
    }

    #[test]
    fn test_accuracy_integers_positive_only_even_small() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(1, 10));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_positive_only_even_medium() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(1, 250));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_positive_only_even_large() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(1, 1000));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_positive_only_odd_small() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(1, 11));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_positive_only_odd_medium() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(1, 293));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_positive_only_odd_large() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(1, 1023));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_negative_only_even_small() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(-10, -1));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_negative_only_even_medium() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(-250, -1));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_negative_only_even_large() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(-1000, -1));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_negative_only_odd_small() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(-11, -1));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_negative_only_odd_medium() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(-293, -1));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
    }

    #[test]
    fn test_accuracy_integers_negative_only_odd_large() {
        let index_mapping = LogarithmicMapping::new(0.01).unwrap();
        let dataset = Dataset::<_, CollapsingLowestDenseStore>::new(index_mapping, integers(-1023, -1));
        dataset.validate(&[0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99]);
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
    #[ignore]
    fn test_quantile_bounds() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        for i in 1..=100 {
            sketch.add(i as f64);
        }

        // q=0 should return min
        let min_actual = sketch.quantile(0.0).unwrap();
        assert_rel_acc_eq!(0.0, 0.01, 1.0, min_actual);

        // q=1 should return max
        let max_actual = sketch.quantile(1.0).unwrap();
        assert_rel_acc_eq!(1.0, 0.01, 100.0, max_actual);
    }

    #[test]
    fn test_add_n() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch.add_n(10.0, 5);

        assert_eq!(sketch.count(), 5);
    }

    #[test]
    fn test_proto_roundtrip() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch.add(1.0);
        sketch.add(2.0);
        sketch.add(3.0);
        sketch.add(100.0);

        let proto = sketch.to_proto();
        let mapping = LogarithmicMapping::new(0.01).unwrap();
        let recovered: DDSketch = DDSketch::from_proto(&proto, mapping).unwrap();

        // Check bin data is preserved
        assert_eq!(sketch.count(), recovered.count());
        assert_eq!(sketch.zero_count(), recovered.zero_count());

        // Check quantiles are approximately equal
        for q in [0.25, 0.5, 0.75, 0.99] {
            let orig = sketch.quantile(q).unwrap();
            let recov = recovered.quantile(q).unwrap();
            assert!(
                (orig - recov).abs() < 0.001,
                "quantile {} mismatch: {} vs {}",
                q,
                orig,
                recov
            );
        }
    }

    #[test]
    fn test_proto_roundtrip_with_negatives() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch.add(-10.0);
        sketch.add(-5.0);
        sketch.add(0.0);
        sketch.add(5.0);
        sketch.add(10.0);

        let proto = sketch.to_proto();
        let mapping = LogarithmicMapping::new(0.01).unwrap();
        let recovered: DDSketch = DDSketch::from_proto(&proto, mapping).unwrap();

        assert_eq!(sketch.count(), recovered.count());
        assert_eq!(sketch.zero_count(), recovered.zero_count());
    }

    #[test]
    fn test_proto_roundtrip_empty() {
        let sketch = DDSketch::with_relative_accuracy(0.01).unwrap();

        let proto = sketch.to_proto();
        let mapping = LogarithmicMapping::new(0.01).unwrap();
        let recovered: DDSketch = DDSketch::from_proto(&proto, mapping).unwrap();

        assert!(recovered.is_empty());
        assert_eq!(recovered.count(), 0);
    }

    #[test]
    fn test_proto_gamma_mismatch() {
        let mut sketch = DDSketch::with_relative_accuracy(0.01).unwrap();
        sketch.add(1.0);

        let proto = sketch.to_proto();

        // Try to decode with a different relative accuracy (different gamma)
        let different_mapping = LogarithmicMapping::new(0.05).unwrap();
        let result = DDSketch::<_, CollapsingLowestDenseStore>::from_proto(&proto, different_mapping);

        assert!(result.is_err());
        match result {
            Err(crate::canonical::ProtoConversionError::GammaMismatch { .. }) => {}
            _ => panic!("Expected GammaMismatch error"),
        }
    }

    #[test]
    fn test_proto_missing_mapping() {
        use datadog_protos::sketches::DDSketch as ProtoDDSketch;

        let proto = ProtoDDSketch::new(); // No mapping set
        let mapping = LogarithmicMapping::new(0.01).unwrap();
        let result = DDSketch::<_, CollapsingLowestDenseStore>::from_proto(&proto, mapping);

        assert!(result.is_err());
        match result {
            Err(crate::canonical::ProtoConversionError::MissingMapping) => {}
            _ => panic!("Expected MissingMapping error"),
        }
    }
}
