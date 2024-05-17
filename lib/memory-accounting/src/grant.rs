use ordered_float::NotNan;
use snafu::Snafu;

// We limit grants to a size of 2^53 (~9PB) because at numbers bigger than that, we end up with floating point loss when
// we do scaling calculations. Since we are not going to support grants that large -- and if we ever have to, well,
// then, I'll eat my synpatic implant or whatever makes sense to eat 40 years from now -- we just limit them like this
// for now.
const MAX_GRANT_BYTES: usize = 2usize.pow(f64::MANTISSA_DIGITS);

#[derive(Debug, Snafu)]
pub enum GrantError {
    #[snafu(display("Slop factor must be between 0.0 and 1.0 inclusive."))]
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
/// should verify memory bounds again. For example, if we seek to use no more than 64MB of memory from the OS
/// perspective (RSS), then intuitively we know that we might only be able to allocate 55-60MB of memory before
/// allocator fragmentation causes us to reach 64MB RSS.
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
    /// If the effective limit is greater than 9007199254740992 bytes (2^53 bytes, or roughly 9 petabytes), then `None`
    /// is returned. This is a hardcoded limit.
    pub fn effective(effective_limit_bytes: usize) -> Result<Self, GrantError> {
        Self::with_slop_factor(effective_limit_bytes, 0.0)
    }

    /// Creates a new memory grant based on the given initial limit and slop factor.
    ///
    /// The slop factor accounts for the percentage of the initial limit that can effectively be used. For example, a
    /// slop factor of 0.1 would indicate that only 90% of the initial limit should be used, and a slop factor of 0.25
    /// would indicate that only 75% of the initial limit should be used, and so on.
    ///
    /// If the slop factor is not valid (must be 0.0 < slop_factor <= 1.0), then `None` is returned.  If the effective
    /// limit is greater than 9007199254740992 bytes (2^53 bytes, or roughly 9 petabytes), then `None` is returned. This
    /// is a hardcoded limit.
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
    /// This value is purely informational, and should not be used to calculating the memory available for use. For that
    /// value, see [`effective_limit_bytes`].
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
    use super::MemoryGrant;

    #[test]
    fn effective() {
        assert!(MemoryGrant::effective(1).is_ok());
        assert!(MemoryGrant::effective(2usize.pow(f64::MANTISSA_DIGITS)).is_ok());
        assert!(MemoryGrant::effective(2usize.pow(f64::MANTISSA_DIGITS) + 1).is_err());
    }

    #[test]
    fn slop_factor() {
        assert!(MemoryGrant::with_slop_factor(1, 0.1).is_ok());
        assert!(MemoryGrant::with_slop_factor(1, 0.9).is_ok());
        assert!(MemoryGrant::with_slop_factor(1, f64::NAN).is_err());
        assert!(MemoryGrant::with_slop_factor(1, -0.1).is_err());
        assert!(MemoryGrant::with_slop_factor(1, 1.001).is_err());
        assert!(MemoryGrant::with_slop_factor(2usize.pow(f64::MANTISSA_DIGITS), 0.25).is_ok());
        assert!(MemoryGrant::with_slop_factor(2usize.pow(f64::MANTISSA_DIGITS) + 1, 0.25).is_err());
    }
}
