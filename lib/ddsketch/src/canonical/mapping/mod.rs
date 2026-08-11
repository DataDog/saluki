//! Index mapping.

use datadog_protos::sketches::{index_mapping::Interpolation, IndexMapping as ProtoIndexMapping};

use super::error::ProtoConversionError;

mod fixed;
pub use self::fixed::FixedLogarithmicMapping;

mod logarithmic;
pub use self::logarithmic::LogarithmicMapping;

/// Maps values to bin indices and vice versa.
///
/// The mapping defines the relationship between floating-point values and integer bin indices, determining the relative
/// accuracy of the sketch.
pub trait IndexMapping: Clone + Send + Sync {
    /// Returns the index of the bin for the given positive value.
    ///
    /// The value must be positive. For negative values, use the index of the absolute value and store in the negative
    /// store.
    fn index(&self, value: f64) -> i32;

    /// Returns the representative value for the given index.
    ///
    /// This is typically the geometric mean of the bin's lower and upper bounds.
    fn value(&self, index: i32) -> f64;

    /// Returns the lower bound of the bin at the given index.
    fn lower_bound(&self, index: i32) -> f64;

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

    /// Returns the index offset used by this mapping.
    ///
    /// The index offset shifts all bin indices by a constant value.
    fn index_offset(&self) -> f64;

    /// Returns the interpolation mode used by this mapping.
    ///
    /// The interpolation mode determines how the logarithm is approximated.
    fn interpolation(&self) -> Interpolation;

    /// Validates that a protobuf `IndexMapping` is compatible with this mapping.
    ///
    /// # Errors
    ///
    /// If the given protobuf mapping parameters don't match this mapping's configuration, an error describing the
    /// mismatch is returned.
    fn validate_proto_mapping(&self, proto: &ProtoIndexMapping) -> Result<(), ProtoConversionError>;

    /// Converts this mapping to a protobuf `IndexMapping`.
    fn to_proto(&self) -> ProtoIndexMapping;
}

#[cfg(test)]
pub(crate) mod conformance {
    use super::IndexMapping;

    /// Asserts the shared [`IndexMapping`] contract that every mapping implementation must satisfy.
    ///
    /// Rather than hand-duplicate the index/value round-trip, bound-ordering, and protobuf self-validation checks
    /// across each sibling mapping's tests, both implementations call this helper. Constructor-specific behavior
    /// (accuracy-bound validation, zero-sizedness, cross-implementation agreement) is still tested inline.
    ///
    /// `expected_relative_accuracy` is the accuracy the mapping is configured for; it's checked both directly and for
    /// internal consistency with the mapping's gamma (`alpha = (gamma - 1) / (gamma + 1)`).
    #[track_caller]
    pub(crate) fn assert_index_mapping_conformance<M: IndexMapping>(mapping: &M, expected_relative_accuracy: f64) {
        assert!(
            (mapping.relative_accuracy() - expected_relative_accuracy).abs() < 1e-10,
            "relative accuracy {} should match expected {}",
            mapping.relative_accuracy(),
            expected_relative_accuracy
        );

        let gamma = mapping.gamma();
        let derived_accuracy = (gamma - 1.0) / (gamma + 1.0);
        assert!(
            (mapping.relative_accuracy() - derived_accuracy).abs() < 1e-10,
            "relative accuracy {} should be consistent with gamma {} (derived {})",
            mapping.relative_accuracy(),
            gamma,
            derived_accuracy
        );

        for i in -100..100 {
            // The representative value must sit strictly above the bin's lower bound.
            let lower = mapping.lower_bound(i);
            let value = mapping.value(i);
            assert!(
                lower < value,
                "lower bound {} should be < value {} at index {}",
                lower,
                value,
                i
            );

            // Mapping an index to its value and back must recover the index (within one bin, due to floating-point).
            let recovered = mapping.index(value);
            assert!(
                (recovered - i).abs() <= 1,
                "index {} -> value {} -> index {} should round-trip within one bin",
                i,
                value,
                recovered
            );
        }

        // A mapping must accept its own protobuf representation.
        assert!(
            mapping.validate_proto_mapping(&mapping.to_proto()).is_ok(),
            "mapping should validate its own protobuf representation"
        );
    }
}
