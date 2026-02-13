//! Component implementations.
//!
//! This crate contains full implementations of a number of common components.

#![deny(warnings)]
#![deny(missing_docs)]

mod common;
pub mod decoders;
pub mod destinations;
pub mod encoders;
pub mod forwarders;
pub mod relays;
pub mod sources;
pub mod transforms;

/// Bench-only exports for local performance measurement.
#[cfg(feature = "bench")]
pub mod bench {
    use stringtheory::MetaString;

    use crate::common::otlp::traces::normalize::normalize_scalar as normalize_scalar_impl;
    #[cfg(any(target_arch = "x86", target_arch = "x86_64", all(target_arch = "aarch64", not(miri))))]
    use crate::common::otlp::traces::normalize::normalize_simd as normalize_simd_impl;

    /// Scalar normalization entrypoint for benchmarks.
    pub mod normalize_scalar {
        use stringtheory::MetaString;

        use super::normalize_scalar_impl;

        /// Normalizes a trace tag using the scalar implementation only.
        pub fn normalize(value: &str) -> MetaString {
            normalize_scalar_impl::normalize(value, true)
        }
    }

    /// SIMD normalization entrypoint for benchmarks.
    pub fn normalize_simd(value: &str) -> MetaString {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64", all(target_arch = "aarch64", not(miri))))]
        {
            return normalize_simd_impl(value, true);
        }

        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", all(target_arch = "aarch64", not(miri)),)))]
        {
            normalize_scalar_impl::normalize(value, true)
        }
    }
}
