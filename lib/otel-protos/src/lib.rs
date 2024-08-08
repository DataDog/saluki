//! OpenTelemetry Protocol Buffers definitions.
//!
//! This crate contains generated code based on the Protocol Buffers definitions used by
//! OpenTelemetry.
#![deny(warnings)]
#![allow(clippy::enum_variant_names)]
mod include {
    include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
}

mod api {
    include!(concat!(env!("OUT_DIR"), "/api.mod.rs"));
}

pub use self::include::logs;
pub use self::include::metrics;
pub use self::include::trace;

pub mod collector {
    pub use super::api::opentelemetry::proto::collector::*;
}
