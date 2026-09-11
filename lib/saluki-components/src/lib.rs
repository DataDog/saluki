//! Component implementations.
//!
//! This crate contains full implementations of a number of common components.

#![deny(warnings)]
#![deny(missing_docs)]

mod common;

pub mod config;
pub mod decoders;
pub mod destinations;
pub mod encoders;
pub mod forwarders;
pub mod relays;

/// Semantic attribute registry support shared by OpenTelemetry components.
pub mod semantics {
    pub use crate::common::otlp::semantics::{current_registry, reset_registry, update_registry, Registry};
}

pub mod sources;
pub mod transforms;
