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
    /// Normalizes a trace tag using the production implementation.
    pub use crate::common::otlp::traces::normalize::normalize_tag;
    /// Normalizes a trace tag value using the production implementation.
    pub use crate::common::otlp::traces::normalize::normalize_tag_value;
}
