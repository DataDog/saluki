//! High-level application primitives.
//!
//! This crate provides common primitives necessary for bootstrapping an application prior to running, such as
//! initializing logging, metrics, and memory management.
#![deny(warnings)]
#![deny(missing_docs)]

#[cfg(feature = "api")]
pub mod api;

#[cfg(feature = "logging")]
pub mod logging;

#[cfg(feature = "memory")]
pub mod memory;

#[cfg(feature = "metrics")]
pub mod metrics;

#[cfg(feature = "tls")]
pub mod tls;

/// Common imports.
pub mod prelude {
    #[cfg(feature = "logging")]
    pub use super::logging::{
        fatal_and_exit, get_console_server_parts, initialize_dynamic_logging, initialize_logging,
    };
    #[cfg(feature = "memory")]
    pub use super::memory::{initialize_allocator_telemetry, initialize_memory_bounds, MemoryBoundsConfiguration};
    #[cfg(feature = "metrics")]
    pub use super::metrics::initialize_metrics;
    #[cfg(feature = "tls")]
    pub use super::tls::initialize_tls;
}
