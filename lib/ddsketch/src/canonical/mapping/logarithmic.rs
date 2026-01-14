//! Logarithmic index mapping implementation.

use super::IndexMapping;

/// Logarithmic index mapping for DDSketch.
///
/// Maps values to indices using: `index = ceil(log(value) / log(gamma))`
/// where `gamma = (1 + alpha) / (1 - alpha)` and `alpha` is the relative accuracy.
///
/// This mapping provides the guaranteed relative error bound for quantile queries.
#[derive(Clone, Debug, PartialEq)]
pub struct LogarithmicMapping {
    /// The base of the logarithm, determines bin widths.
    gamma: f64,
    /// Precomputed 1/ln(gamma) for performance.
    multiplier: f64,
    /// The relative accuracy guarantee.
    relative_accuracy: f64,
    /// Minimum value that can be indexed.
    min_indexable_value: f64,
    /// Maximum value that can be indexed.
    max_indexable_value: f64,
}

impl LogarithmicMapping {
    /// Creates a new logarithmic mapping with the given relative accuracy.
    ///
    /// # Arguments
    ///
    /// * `relative_accuracy` - The relative accuracy guarantee, must be in (0, 1).
    ///
    /// # Errors
    ///
    /// Returns an error if the relative accuracy is not in the valid range (0, 1).
    ///
    /// # Example
    ///
    /// ```
    /// use ddsketch::canonical::LogarithmicMapping;
    ///
    /// // Create a mapping with 1% relative accuracy
    /// let mapping = LogarithmicMapping::new(0.01).unwrap();
    /// ```
    pub fn new(relative_accuracy: f64) -> Result<Self, &'static str> {
        if relative_accuracy <= 0.0 || relative_accuracy >= 1.0 {
            return Err("relative accuracy must be between 0 and 1 (exclusive)");
        }

        // gamma = (1 + alpha) / (1 - alpha)
        let gamma = (1.0 + relative_accuracy) / (1.0 - relative_accuracy);
        Self::with_gamma(gamma, relative_accuracy)
    }

    /// Creates a new logarithmic mapping with the given gamma value.
    ///
    /// # Arguments
    ///
    /// * `gamma` - The base of the logarithm, must be > 1.
    /// * `relative_accuracy` - The relative accuracy this gamma provides.
    ///
    /// # Errors
    ///
    /// Returns an error if gamma is not greater than 1.
    pub fn with_gamma(gamma: f64, relative_accuracy: f64) -> Result<Self, &'static str> {
        if gamma <= 1.0 {
            return Err("gamma must be greater than 1");
        }

        let gamma_ln = gamma.ln();
        let multiplier = 1.0 / gamma_ln;

        // Calculate the indexable range.
        // The minimum indexable value is constrained by the smallest positive f64.
        // The maximum indexable value is constrained by i32 index overflow.
        let min_indexable_value = f64::MIN_POSITIVE.max(gamma.powf(i32::MIN as f64 + 1.0));
        let max_indexable_value = gamma.powf(i32::MAX as f64 - 1.0).min(f64::MAX / gamma);

        Ok(Self {
            gamma,
            multiplier,
            relative_accuracy,
            min_indexable_value,
            max_indexable_value,
        })
    }

    /// Returns the gamma value used for this mapping.
    pub fn get_gamma(&self) -> f64 {
        self.gamma
    }
}

impl IndexMapping for LogarithmicMapping {
    fn index(&self, value: f64) -> i32 {
        // index = ceil(log_gamma(value)) = ceil(ln(value) / ln(gamma))
        (value.ln() * self.multiplier).ceil() as i32
    }

    fn value(&self, index: i32) -> f64 {
        // Return the geometric mean of the bin's bounds: gamma^(index - 0.5)
        self.gamma.powf(index as f64 - 0.5)
    }

    fn lower_bound(&self, index: i32) -> f64 {
        // lower_bound = gamma^(index - 1)
        self.gamma.powf((index - 1) as f64)
    }

    fn upper_bound(&self, index: i32) -> f64 {
        // upper_bound = gamma^index
        self.gamma.powf(index as f64)
    }

    fn relative_accuracy(&self) -> f64 {
        self.relative_accuracy
    }

    fn min_indexable_value(&self) -> f64 {
        self.min_indexable_value
    }

    fn max_indexable_value(&self) -> f64 {
        self.max_indexable_value
    }

    fn gamma(&self) -> f64 {
        self.gamma
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
            let upper = mapping.upper_bound(i);
            let value = mapping.value(i);

            assert!(
                lower < value,
                "lower {} should be < value {} for index {}",
                lower,
                value,
                i
            );
            assert!(
                value < upper,
                "value {} should be < upper {} for index {}",
                value,
                upper,
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
