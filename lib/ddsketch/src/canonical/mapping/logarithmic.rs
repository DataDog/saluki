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

        Ok(Self { gamma, multiplier })
    }
}

impl IndexMapping for LogarithmicMapping {
    fn index(&self, value: f64) -> i32 {
        let index = value.ln() * self.multiplier;
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
        (index as f64 / self.multiplier).exp()
    }

    fn relative_accuracy(&self) -> f64 {
        (self.gamma - 1.0) / (self.gamma + 1.0)
    }

    fn min_indexable_value(&self) -> f64 {
        f64::MIN_POSITIVE.max(self.gamma.powf(i32::MIN as f64 + 1.0))
    }

    fn max_indexable_value(&self) -> f64 {
        self.gamma.powf(i32::MAX as f64 - 1.0).min(f64::MAX / self.gamma)
    }

    fn gamma(&self) -> f64 {
        self.gamma
    }

    fn index_offset(&self) -> f64 {
        0.0
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

        // Check indexOffset is 0.0 (LogarithmicMapping doesn't use offset)
        if proto.indexOffset != 0.0 {
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
        proto.indexOffset = 0.0;
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

    #[test]
    fn test_new_valid_accuracy() {
        let mapping = LogarithmicMapping::new(0.01).unwrap();
        assert!((mapping.relative_accuracy() - 0.01).abs() < 1e-10);
    }

    #[test]
    fn test_new_invalid_accuracy_zero() {
        assert!(LogarithmicMapping::new(0.0).is_err());
    }

    #[test]
    fn test_new_invalid_accuracy_one() {
        assert!(LogarithmicMapping::new(1.0).is_err());
    }

    #[test]
    fn test_new_invalid_accuracy_negative() {
        assert!(LogarithmicMapping::new(-0.1).is_err());
    }

    #[test]
    fn test_index_value_roundtrip() {
        let mapping = LogarithmicMapping::new(0.01).unwrap();

        // For any index, the value at that index should map back to the same index
        for i in -100..100 {
            let value = mapping.value(i);
            let recovered_index = mapping.index(value);
            // Due to floating-point, we might be off by 1
            assert!(
                (recovered_index - i).abs() <= 1,
                "index {} -> value {} -> index {}",
                i,
                value,
                recovered_index
            );
        }
    }

    #[test]
    fn test_bounds_ordering() {
        let mapping = LogarithmicMapping::new(0.01).unwrap();

        for i in -100..100 {
            let lower = mapping.lower_bound(i);
            let value = mapping.value(i);

            assert!(
                lower < value,
                "lower {} should be < value {} for index {}",
                lower,
                value,
                i
            );
        }
    }

    #[test]
    fn test_gamma_calculation() {
        let mapping = LogarithmicMapping::new(0.01).unwrap();
        // gamma = (1 + 0.01) / (1 - 0.01) = 1.01 / 0.99
        let expected_gamma = 1.01 / 0.99;
        assert!((mapping.gamma() - expected_gamma).abs() < 1e-10);
    }
}
