//! OpenTelemetry Protocol Buffers definitions.
#![deny(warnings)]
#![allow(clippy::enum_variant_names)]
#![allow(clippy::doc_overindented_list_items)]
#![allow(rustdoc::invalid_html_tags)]
mod otlp_include {
    include!(concat!(env!("OUT_DIR"), "/otlp.mod.rs"));
}

pub use otlp_include::*;
