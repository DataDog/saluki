//! OpenTelemetry Protocol Buffers definitions.
//!
//! This crate contains generated code based on the Protocol Buffers definitions used by
//! OpenTelemetry.
#![deny(warnings)]
#![allow(clippy::enum_variant_names)]
#![allow(clippy::doc_lazy_continuation)]
mod include {
    include!(concat!(env!("OUT_DIR"), "/mod.rs"));
}

pub use self::include::opentelemetry::proto::*;
