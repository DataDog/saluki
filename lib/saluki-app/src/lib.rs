//! High-level application primitives.
//!
//! This crate provides common primitives necessary for bootstrapping an application prior to running, such as
//! initializing logging, metrics, and memory management.
#![deny(warnings)]
#![deny(missing_docs)]

pub mod logging;
pub mod memory;
pub mod metrics;
pub mod tls;

/// Common imports.
pub mod prelude {
    pub use super::{
        logging::{fatal_and_exit, initialize_logging},
        memory::{initialize_allocator_telemetry, initialize_memory_bounds, MemoryBoundsConfiguration},
        metrics::initialize_metrics,
        tls::initialize_tls,
    };
}
