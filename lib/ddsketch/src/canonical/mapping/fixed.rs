//! Fixed logarithmic index mapping with hardcoded 1% relative accuracy.

use datadog_protos::sketches::{index_mapping::Interpolation, IndexMapping as ProtoIndexMapping};

use super::IndexMapping;
use crate::canonical::error::ProtoConversionError;
use crate::common::float_eq;

/// A zero-sized logarithmic index mapping with hardcoded 1% relative accuracy.
///
/// This mapping is functionally identical to `LogarithmicMapping::new(0.01)` but uses compile-time constants instead of
/// storing gamma and multiplier as fields. This makes the type zero-sized, saving 16 bytes per DDSketch instance.
///
/// Use this mapping when you know all your sketches will use 1% relative accuracy (the common default).
///
/// # Example
///
/// ```
/// use ddsketch::canonical::mapping::FixedLogarithmicMapping;
/// use ddsketch::canonical::DDSketch;
/// use ddsketch::canonical::store::CollapsingLowestDenseStore;
///
/// // Create a sketch with the fixed mapping (saves 16 bytes vs LogarithmicMapping)
/// let sketch: DDSketch<FixedLogarithmicMapping, CollapsingLowestDenseStore> = DDSketch::default();
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FixedLogarithmicMapping;

impl FixedLogarithmicMapping {
    // For alpha = 0.01 (1% relative accuracy):
    // gamma = (1 + alpha) / (1 - alpha) = 1.01 / 0.99
    const GAMMA: f64 = 1.02020202020202;

    // multiplier = 1 / ln(gamma)
    const MULTIPLIER: f64 = 49.99833328888678;

    // The relative accuracy this mapping provides
    const RELATIVE_ACCURACY: f64 = 0.01;

    /// Creates a new `FixedLogarithmicMapping`.
    ///
    /// This is a no-op since the type is zero-sized, but provided for API consistency.
    #[inline]
    pub const fn new() -> Self {
        Self
    }
}

impl IndexMapping for FixedLogarithmicMapping {
    #[inline]
    fn index(&self, value: f64) -> i32 {
        let index = value.ln() * Self::MULTIPLIER;
        if index >= 0.0 {
            index as i32
        } else {
            (index as i32) - 1
        }
    }

    #[inline]
    fn value(&self, index: i32) -> f64 {
        self.lower_bound(index) * (1.0 + self.relative_accuracy())
    }

    #[inline]
    fn lower_bound(&self, index: i32) -> f64 {
        (index as f64 / Self::MULTIPLIER).exp()
    }

    #[inline]
    fn relative_accuracy(&self) -> f64 {
        Self::RELATIVE_ACCURACY
    }

    #[inline]
    fn min_indexable_value(&self) -> f64 {
        f64::MIN_POSITIVE.max(Self::GAMMA.powf(i32::MIN as f64 + 1.0))
    }

    #[inline]
    fn max_indexable_value(&self) -> f64 {
        Self::GAMMA.powf(i32::MAX as f64 - 1.0).min(f64::MAX / Self::GAMMA)
    }

    #[inline]
    fn gamma(&self) -> f64 {
        Self::GAMMA
    }

    #[inline]
    fn index_offset(&self) -> f64 {
        0.0
    }

    #[inline]
    fn interpolation(&self) -> Interpolation {
        Interpolation::NONE
    }

    fn validate_proto_mapping(&self, proto: &ProtoIndexMapping) -> Result<(), ProtoConversionError> {
        // Check gamma matches (with floating-point tolerance)
        if !float_eq(proto.gamma, Self::GAMMA) {
            return Err(ProtoConversionError::GammaMismatch {
                expected: Self::GAMMA,
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
        proto.gamma = Self::GAMMA;
        proto.indexOffset = 0.0;
        proto.interpolation = protobuf::EnumOrUnknown::new(Interpolation::NONE);
        proto
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::mapping::LogarithmicMapping;

    #[test]
    fn test_zero_sized() {
        assert_eq!(std::mem::size_of::<FixedLogarithmicMapping>(), 0);
    }

    #[test]
    fn test_matches_logarithmic_mapping() {
        let fixed = FixedLogarithmicMapping::new();
        let dynamic = LogarithmicMapping::new(0.01).unwrap();

        // Verify constants match
        assert!(
            (fixed.gamma() - dynamic.gamma()).abs() < 1e-10,
            "gamma mismatch: {} vs {}",
            fixed.gamma(),
            dynamic.gamma()
        );
        assert!(
            (fixed.relative_accuracy() - dynamic.relative_accuracy()).abs() < 1e-10,
            "relative_accuracy mismatch: {} vs {}",
            fixed.relative_accuracy(),
            dynamic.relative_accuracy()
        );

        // Verify index calculations match for various values
        for &value in &[0.001, 0.1, 1.0, 10.0, 100.0, 1000.0, 1_000_000.0] {
            let fixed_idx = fixed.index(value);
            let dynamic_idx = dynamic.index(value);
            assert_eq!(
                fixed_idx, dynamic_idx,
                "index mismatch for value {}: {} vs {}",
                value, fixed_idx, dynamic_idx
            );
        }

        // Verify value calculations match for various indices
        for idx in -100..100 {
            let fixed_val = fixed.value(idx);
            let dynamic_val = dynamic.value(idx);
            assert!(
                (fixed_val - dynamic_val).abs() < 1e-10 || (fixed_val / dynamic_val - 1.0).abs() < 1e-10,
                "value mismatch for index {}: {} vs {}",
                idx,
                fixed_val,
                dynamic_val
            );
        }
    }

    #[test]
    fn test_index_value_roundtrip() {
        let mapping = FixedLogarithmicMapping::new();

        for i in -100..100 {
            let value = mapping.value(i);
            let recovered_index = mapping.index(value);
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
    fn test_proto_roundtrip() {
        let mapping = FixedLogarithmicMapping::new();
        let proto = mapping.to_proto();

        assert!(mapping.validate_proto_mapping(&proto).is_ok());
    }
}
