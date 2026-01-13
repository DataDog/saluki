use num_traits::{PrimInt, Signed, Unsigned};

// Pre-generated sketch parameters.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/config.rs"));
}

pub use self::generated::*;

/// A value that can be used as the key in a sketch bin.
pub trait AsBinKey: PrimInt + Signed + std::fmt::Debug {
    /// The multiplicative identity of the key type.
    const ONE: Self;

    /// The additive identity of the key type.
    const ZERO: Self;

    /// The minimum value of the key type.
    const MIN: Self;

    /// The maximum value of the key type.
    const MAX: Self;

    /// Returns `true` if the key is zero.
    fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    /// Converts `i32` into `Self`, losslessly.
    fn from_i32(value: i32) -> Self;

    /// Converts `Self` into `i32`, losslessly.
    fn into_i32(self) -> i32;

    /// Collects all keys between `self` and `end` into a vector.
    fn keys_between(self, end: Self) -> Vec<Self>;
}

impl AsBinKey for i16 {
    const ONE: Self = 1;
    const ZERO: Self = 0;
    const MIN: Self = Self::MIN;
    const MAX: Self = Self::MAX;

    fn from_i32(value: i32) -> Self {
        value as Self
    }

    fn into_i32(self) -> i32 {
        self as i32
    }

    fn keys_between(self, end: Self) -> Vec<Self> {
        (self..=end).collect()
    }
}

/// A value that can be used as the count in a sketch bin.
pub trait AsBinCount: PrimInt + Unsigned + std::fmt::Debug {
    /// The multiplicative identity of the count type.
    const ONE: Self;

    /// The additive identity of the count type.
    const ZERO: Self;

    /// The maximum value of the count type.
    const MAX: Self;

    /// Converts `Self` into `f64`, losslessly.
    fn to_f64(self) -> f64;

    /// Converts `Self` into `u64`, losslessly.
    fn to_u64(self) -> u64;

    /// Returns `true` if the count is zero.
    fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    /// Adds `other` to `self`, returning the sum and any overflow.
    ///
    /// Overflow is considered from the bitwidth of `Self`.
    fn add_with_overflow(self, other: u64) -> (Self, u64);

    /// Divides `other` by the bitwidth of `Self`, returning the quotient and remainder.
    ///
    /// This can be used to calculate the number of times that `Self::MAX` is contained within `other`, as well as the
    /// remainder.
    fn div_rem_overflow(other: u64) -> (u64, Self);
}

impl AsBinCount for u16 {
    const ONE: Self = 1;
    const ZERO: Self = 0;
    const MAX: Self = Self::MAX;

    fn to_f64(self) -> f64 {
        f64::from(self)
    }

    fn to_u64(self) -> u64 {
        self as u64
    }

    fn add_with_overflow(self, other: u64) -> (Self, u64) {
        let self_max_u64 = Self::MAX as u64;
        let self_u64 = self as u64;

        // We promote `self` to `u64` and then try to sum it with `other`.
        //
        // We first check if we overflowed `u64` when summing, and if not, then we check if the result would overflow `Self`,
        // and then we return the relevant values depending on if there was any overflow or not.
        match self_u64.overflowing_add(other) {
            // We didn't overflow `u64` when summing, so figure out if we overflowed `Self` or not.
            (sum, false) => {
                if sum > self_max_u64 {
                    // We overflowed `Self`, so return the maximum value and the overflow amount.
                    (Self::MAX, sum - self_max_u64)
                } else {
                    // We didn't overflow `Self`, so return the sum and no overflow.
                    (sum as Self, 0)
                }
            }
            // We did overflow `u64`, which we treat the same as if we overflowed `Self`.
            (sum, true) => (Self::MAX, sum - self_max_u64),
        }
    }

    fn div_rem_overflow(other: u64) -> (u64, Self) {
        let self_max_u64 = Self::MAX as u64;
        if other < self_max_u64 {
            // `other` doesn't overflow `Self`, so all we have is a remainder.
            (0, other as Self)
        } else {
            // `other` overflows `Self`, so we have a quotient and a remainder.
            let quotient = other / self_max_u64;
            let remainder = (other % self_max_u64) as Self;
            (quotient, remainder)
        }
    }
}

/// Configuration parameters for `DDSketch`.
pub trait SketchParameters: Copy {
    /// The type of the key used to index bins.
    type BinKey: AsBinKey;

    /// The type of the count used to store bin counts.
    type BinCount: AsBinCount;

    // User-provided parameters.
    //
    // These form the basis of the desired behavior of the sketch: how many bins to use, the smallest value that can be resolved,
    // and the relative accuracy, and so on.

    /// The maximum number of bins to use.
    const BIN_LIMIT: usize;

    /// The maximum count that can be represented by a single bin.
    const MAX_BIN_COUNT: Self::BinCount = Self::BinCount::MAX;

    /// The minimum resolvable value that can be resolved by this sketch.
    const MINIMUM_VALUE: f64;

    /// The relative accuracy of the the quantiles reported by this sketch.
    const RELATIVE_ACCURACY: f64;

    // Generated parameters.
    //
    // These are pre-generated values used by the actual sketch logic when interpolating keys/values.

    /// The gamma parameter of the index mapping.
    ///
    /// For a given value `v`, the bin index it belongs to is roughly equal to `log(v) / log(gamma)`.
    const GAMMA_V: f64;

    /// The natural logarithm of the gamma parameter.
    ///
    /// Used purely for avoiding calculating `log(gamma)` repeatedly.
    const GAMMA_LN: f64;

    /// Minimum value representable by a sketch with these params.
    ///
    /// For a given value `v`, we generate the following keys (bin index) based on `NORM_MIN`:
    ///
    /// key(0)  = -NORM_MIN > v < NORM_MIN
    /// key(1)  = v == NORM_MIN
    /// key(-1) = v == -NORM_MIN
    const NORM_MIN: f64;

    /// Bias of the exponent, used to ensure key(x) >= 1.
    const NORM_BIAS: i32;

    /// Gets the value lower bound of the bin at the given key.
    #[inline]
    fn bin_lower_bound(k: Self::BinKey) -> f64 {
        if k < Self::BinKey::ZERO {
            return -Self::bin_lower_bound(-k);
        }

        if k == Self::BinKey::MAX {
            return f64::INFINITY;
        }

        if k.is_zero() {
            return 0.0;
        }

        Self::GAMMA_V.powf(f64::from(k.into_i32() - Self::NORM_BIAS))
    }

    /// Gets the key for the given value.
    ///
    /// The key corresponds to the bin where this value would be represented. The value returned here is such that:
    ///
    /// > γ^k <= v < γ^(k+1)
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    fn key(v: f64) -> Self::BinKey {
        if v < 0.0 {
            return -Self::key(-v);
        }

        // Figure out if we're in the zero value band (-min_value <= v <= min_value) and assign that to a fixed key of 0 if so.
        if v == 0.0 || (v > 0.0 && v < Self::NORM_MIN) {
            return Self::BinKey::ZERO;
        }

        // Calculate our key based on the interpolated value.
        //
        // SAFETY: Generation of `GAMMA_LN` must ensure that `log_gamma(f64::MAX)` cannot overflow `i32`.
        let unbiased_key = (v.ln() / Self::GAMMA_LN).round_ties_even() as i32;
        let biased_key = unbiased_key.wrapping_add(Self::NORM_BIAS);

        // SAFETY We clamp the key to the maximum value of our bin key type, which ensures we don't overflow
        // during conversion.
        let clamped_key = biased_key.clamp(1, Self::BinKey::MAX.into_i32());
        Self::BinKey::from_i32(clamped_key)
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentSketchParameters, SketchParameters};

    #[test]
    fn test_params_key_lower_bound_identity() {
        // We use the Agent-specific parameters for this just since we know they're generated in a sane way,
        // rather than copy/pasting them or hand-rolling the values and constructing a manual implementation
        // of `SketchParameters`
        let max_key = <AgentSketchParameters as SketchParameters>::BinKey::MAX;
        for key in (-max_key + 1)..max_key {
            let actual_key = AgentSketchParameters::key(AgentSketchParameters::bin_lower_bound(key));
            assert_eq!(key, actual_key);
        }
    }
}
