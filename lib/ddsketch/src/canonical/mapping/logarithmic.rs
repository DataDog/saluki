use datadog_protos::sketches::{index_mapping::Interpolation, IndexMapping as ProtoIndexMapping};

use super::IndexMapping;
use crate::canonical::error::ProtoConversionError;
use crate::common::float_eq;

/// Logarithmic index mapping.
///
/// Maps values to indices using: `index = ceil(log(value) / log(gamma))` where `gamma = (1 + alpha) / (1 - alpha)` and
/// `alpha` is the relative accuracy.
#[derive(Clone, Debug, PartialEq)]
pub struct LogarithmicMapping {
    /// The base of the logarithm, determines bin widths.
    gamma: f64,

    /// Constant shift applied to all computed indices.
    index_offset: f64,

    /// Precomputed 1/ln(gamma) for performance.
    multiplier: f64,
}

impl LogarithmicMapping {
    /// Creates a new `LogarithmicMapping` with the given relative accuracy.
    ///
    /// The relative accuracy must be between `0` and `1` (inclusive).
    ///
    /// # Errors
    ///
    /// If the relative accuracy is out of bounds, an error is returned.
    ///
    /// # Example
    ///
    /// ```
    /// use ddsketch::canonical::mapping::LogarithmicMapping;
    ///
    /// // Create a mapping with 1% relative accuracy
    /// let mapping = LogarithmicMapping::new(0.01).unwrap();
    /// ```
    pub fn new(relative_accuracy: f64) -> Result<Self, &'static str> {
        if relative_accuracy <= 0.0 || relative_accuracy >= 1.0 {
            return Err("relative accuracy must be between 0 and 1 (exclusive)");
        }

        let gamma = (1.0 + relative_accuracy) / (1.0 - relative_accuracy);
        if gamma <= 1.0 {
            return Err("gamma must be greater than 1");
        }

        let multiplier = 1.0 / gamma.ln();

        Ok(Self {
            gamma,
            index_offset: 0.0,
            multiplier,
        })
    }

    /// Creates a new `LogarithmicMapping` from an explicit logarithmic base.
    ///
    /// This constructor is useful when the caller already knows the desired `gamma`
    /// and wants to build the mapping directly instead of deriving it from relative
    /// accuracy.
    ///
    /// `gamma` must be greater than `1.0`, otherwise the logarithmic mapping is
    /// invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if `gamma <= 1.0`.
    pub fn new_with_gamma(gamma: f64) -> Result<Self, &'static str> {
        Self::new_with_gamma_and_offset(gamma, 0.0)
    }

    /// Creates a new `LogarithmicMapping` from an explicit logarithmic base and index offset.
    ///
    /// `gamma` must be greater than `1.0`, otherwise the logarithmic mapping is
    /// invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if `gamma <= 1.0`.
    pub fn new_with_gamma_and_offset(gamma: f64, index_offset: f64) -> Result<Self, &'static str> {
        if gamma <= 1.0 {
            return Err("gamma must be greater than 1");
        }
        let multiplier = 1.0 / gamma.ln();
        Ok(Self {
            gamma,
            index_offset,
            multiplier,
        })
    }
}

impl IndexMapping for LogarithmicMapping {
    fn index(&self, value: f64) -> i32 {
        let index = value.ln() * self.multiplier + self.index_offset;
        if index >= 0.0 {
            index as i32
        } else {
            (index as i32) - 1
        }
    }

    fn value(&self, index: i32) -> f64 {
        self.lower_bound(index) * (1.0 + self.relative_accuracy())
    }

    fn lower_bound(&self, index: i32) -> f64 {
        ((index as f64 - self.index_offset) / self.multiplier).exp()
    }

    fn relative_accuracy(&self) -> f64 {
        (self.gamma - 1.0) / (self.gamma + 1.0)
    }

    fn min_indexable_value(&self) -> f64 {
        f64::MIN_POSITIVE.max((((i32::MIN as f64) - self.index_offset) / self.multiplier + 1.0).exp())
    }

    fn max_indexable_value(&self) -> f64 {
        ((((i32::MAX as f64) - self.index_offset) / self.multiplier - 1.0).exp()).min(f64::MAX / self.gamma)
    }

    fn gamma(&self) -> f64 {
        self.gamma
    }

    fn index_offset(&self) -> f64 {
        self.index_offset
    }

    fn interpolation(&self) -> Interpolation {
        Interpolation::NONE
    }

    fn validate_proto_mapping(&self, proto: &ProtoIndexMapping) -> Result<(), ProtoConversionError> {
        // Check gamma matches (with floating-point tolerance)
        if !float_eq(proto.gamma, self.gamma) {
            return Err(ProtoConversionError::GammaMismatch {
                expected: self.gamma,
                actual: proto.gamma,
            });
        }

        // Check indexOffset matches (with floating-point tolerance)
        if !float_eq(proto.indexOffset, self.index_offset) {
            return Err(ProtoConversionError::NonZeroIndexOffset {
                actual: proto.indexOffset,
            });
        }

        // Check interpolation is NONE (LogarithmicMapping uses exact log)
        let interpolation = proto.interpolation.enum_value_or_default();
        if interpolation != Interpolation::NONE {
            return Err(ProtoConversionError::UnsupportedInterpolation {
                actual: interpolation as i32,
            });
        }

        Ok(())
    }

    fn to_proto(&self) -> ProtoIndexMapping {
        let mut proto = ProtoIndexMapping::new();
        proto.gamma = self.gamma;
        proto.indexOffset = self.index_offset;
        proto.interpolation = protobuf::EnumOrUnknown::new(Interpolation::NONE);
        proto
    }
}

impl Default for LogarithmicMapping {
    /// Creates a logarithmic mapping with 1% relative accuracy (the common default).
    fn default() -> Self {
        Self::new(0.01).expect("0.01 is a valid relative accuracy")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shared `IndexMapping` conformance suite (round-trip, bound ordering, gamma/accuracy consistency, proto
    // self-validation).
    #[test]
    fn conforms_to_index_mapping_contract() {
        let mapping = LogarithmicMapping::new(0.01).unwrap();
        crate::canonical::mapping::conformance::assert_index_mapping_conformance(&mapping, 0.01);
    }

    // Constructor accuracy-bound validation is specific to the runtime-configured `LogarithmicMapping`.
    #[test]
    fn new_rejects_accuracy_of_zero() {
        assert!(LogarithmicMapping::new(0.0).is_err());
    }

    #[test]
    fn new_rejects_accuracy_of_one() {
        assert!(LogarithmicMapping::new(1.0).is_err());
    }

    #[test]
    fn new_rejects_negative_accuracy() {
        assert!(LogarithmicMapping::new(-0.1).is_err());
    }

    #[test]
    fn gamma_is_derived_from_accuracy() {
        let mapping = LogarithmicMapping::new(0.01).unwrap();
        // gamma = (1 + 0.01) / (1 - 0.01) = 1.01 / 0.99
        let expected_gamma = 1.01 / 0.99;
        assert!((mapping.gamma() - expected_gamma).abs() < 1e-10);
    }
}
