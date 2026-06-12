use ordered_float::NotNan;
use snafu::Snafu;

// We limit grants to the largest byte count that fits precisely in both `f64` and `usize`. Floating point precision is
// the limiting factor on 64-bit platforms, while pointer width is the limiting factor on 32-bit platforms.
const MAX_PRECISE_F64_INTEGER_BYTES: u64 = 1u64 << f64::MANTISSA_DIGITS;
const MAX_GRANT_BYTES: usize = if usize::BITS >= f64::MANTISSA_DIGITS {
    MAX_PRECISE_F64_INTEGER_BYTES as usize
} else {
    usize::MAX
};

#[derive(Debug, Snafu)]
pub enum GrantError {
    #[snafu(display("Slop factor must be at least 0.0 and less than 1.0."))]
    InvalidSlopFactor,

    #[snafu(display("Initial limit must be less than or equal to 9PiB (2^53 bytes)."))]
    InitialLimitTooHigh,
}

/// A memory grant.
///
/// Grants define a number of bytes which have been granted for use, with an accompanying "slop factor."
///
/// ## Slop factor
///
/// There can be numerous sources of "slop" in terms of memory allocations, and limiting the memory usage of a process.
/// Not all components can effectively track their true firm/hard limit, memory allocators have fragmentation that may
/// be reduced over time but never fully eliminated, and so on.
///
/// In order to protect against this issue, we utilize a slop factor when calculating the effective limits that we
/// should verify memory bounds again. For example, if we seek to use no more than 64 MB of memory from the OS
/// perspective (RSS), then intuitively we know that we might only be able to allocate 55-60 MB of memory before
/// allocator fragmentation causes us to reach 64 MB RSS.
///
/// By specifying a slop factor, we can provide ourselves breathing room to ensure that we don't try to allocate every
/// last byte of the given global limit, inevitably leading to _exceeding_ that limit and potentially causing
/// out-of-memory crashes, etc.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryGrant {
    initial_limit_bytes: usize,
    slop_factor: NotNan<f64>,
    effective_limit_bytes: usize,
}

impl MemoryGrant {
    /// Creates a new memory grant based on the given effective limit.
    ///
    /// This grant will have a slop factor of 0.0 to indicate that the effective limit is already inclusive of any
    /// necessary slop factor.
    ///
    /// If the effective limit is greater than the maximum precise grant size, then an error is returned. This cap is
    /// 9007199254740992 bytes (2^53 bytes, or roughly 9 petabytes) on 64-bit platforms and `usize::MAX` on 32-bit
    /// platforms.
    pub fn effective(effective_limit_bytes: usize) -> Result<Self, GrantError> {
        Self::with_slop_factor(effective_limit_bytes, 0.0)
    }

    /// Creates a new memory grant based on the given initial limit and slop factor.
    ///
    /// The slop factor accounts for the percentage of the initial limit that can effectively be used. For example, a
    /// slop factor of 0.1 would indicate that only 90% of the initial limit should be used, and a slop factor of 0.25
    /// would indicate that only 75% of the initial limit should be used, and so on.
    ///
    /// If the slop factor isn't valid (must be 0.0 <= `slop_factor` < 1.0), then an error is returned. If the initial
    /// limit is greater than the maximum precise grant size, then an error is returned.
    pub fn with_slop_factor(initial_limit_bytes: usize, slop_factor: f64) -> Result<Self, GrantError> {
        let slop_factor = if !(0.0..1.0).contains(&slop_factor) {
            return Err(GrantError::InvalidSlopFactor);
        } else {
            NotNan::new(slop_factor).map_err(|_| GrantError::InvalidSlopFactor)?
        };

        if initial_limit_bytes > MAX_GRANT_BYTES {
            return Err(GrantError::InitialLimitTooHigh);
        }

        let effective_limit_bytes = (initial_limit_bytes as f64 * (1.0 - slop_factor.into_inner())) as usize;
        Ok(Self {
            initial_limit_bytes,
            slop_factor,
            effective_limit_bytes,
        })
    }

    /// Initial number of bytes granted.
    ///
    /// This value is purely informational, and shouldn't be used to calculating the memory available for use. For that
    /// value, see [`effective_limit_bytes`][Self::effective_limit_bytes].
    pub fn initial_limit_bytes(&self) -> usize {
        self.initial_limit_bytes
    }

    /// The slop factor for the initial limit.
    ///
    /// This value is purely informational.
    pub fn slop_factor(&self) -> f64 {
        self.slop_factor.into_inner()
    }

    /// Effective number of bytes granted.
    ///
    /// This is the value which should be followed for memory usage purposes, as it accounts for the configured slop
    /// factor.
    pub fn effective_limit_bytes(&self) -> usize {
        self.effective_limit_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryGrant, MAX_GRANT_BYTES};

    #[test]
    fn effective() {
        assert!(MemoryGrant::effective(1).is_ok());
        assert!(MemoryGrant::effective(MAX_GRANT_BYTES).is_ok());
        if MAX_GRANT_BYTES < usize::MAX {
            assert!(MemoryGrant::effective(MAX_GRANT_BYTES + 1).is_err());
        }
    }

    #[test]
    fn slop_factor() {
        assert!(MemoryGrant::with_slop_factor(1, 0.1).is_ok());
        assert!(MemoryGrant::with_slop_factor(1, 0.9).is_ok());
        assert!(MemoryGrant::with_slop_factor(1, f64::NAN).is_err());
        assert!(MemoryGrant::with_slop_factor(1, -0.1).is_err());
        assert!(MemoryGrant::with_slop_factor(1, 1.001).is_err());
        assert!(MemoryGrant::with_slop_factor(MAX_GRANT_BYTES, 0.25).is_ok());
        if MAX_GRANT_BYTES < usize::MAX {
            assert!(MemoryGrant::with_slop_factor(MAX_GRANT_BYTES + 1, 0.25).is_err());
        }
    }
}
