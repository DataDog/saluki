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
    pub use super::common::otlp::traces::normalize::normalize_scalar::is_normalized_ascii_tag_scalar;

    #[cfg(all(target_arch = "aarch64", not(miri)))]
    pub use super::common::otlp::traces::normalize::normalize_neon::is_normalized_ascii_tag_simd_neon;

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub use super::common::otlp::traces::normalize::normalize_sse2::is_normalized_ascii_tag_simd_sse2;
}
