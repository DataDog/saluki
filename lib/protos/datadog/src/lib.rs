//! Datadog Agent-specific Protocol Buffers definitions.
//!
//! This crate contains generated code based on the Protocol Buffers definitions used by the Datadog Agent to
//! communicate with the Datadog Platform, specifically for shipping metrics and traces.
#![deny(warnings)]
#![allow(dead_code)]
#![allow(clippy::enum_variant_names)]
#![allow(clippy::doc_overindented_list_items)]

pub(crate) mod serde;

mod include {
    include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
}

mod trace_include {
    include!(concat!(env!("OUT_DIR"), "/trace_protos/mod.rs"));
}

mod agent_include {
    include!(concat!(env!("OUT_DIR"), "/api.mod.rs"));
}

/// Metrics-related definitions.
pub mod metrics {
    pub use super::include::agent_payload::metric_payload::*;
    pub use super::include::agent_payload::sketch_payload::{sketch::*, Sketch};
    pub use super::include::agent_payload::*;
}

/// Event-related definitions.
pub mod events {
    pub use super::include::agent_payload::events_payload::*;
    pub use super::include::agent_payload::EventsPayload;
}

/// Trace-related definitions.
pub mod traces {
    pub use super::trace_include::agent_payload::*;
    pub use super::trace_include::span::{attribute_any_value::*, attribute_array_value::*, *};
    pub use super::trace_include::stats::*;
    pub use super::trace_include::tracer_payload::*;
}

/// Agent definitions.
pub mod agent {
    pub use super::agent_include::datadog::api::v1::agent_client::AgentClient;
    pub use super::agent_include::datadog::api::v1::agent_secure_client::AgentSecureClient;
    pub use super::agent_include::datadog::autodiscovery::*;
    pub use super::agent_include::datadog::model::v1::*;
    pub use super::agent_include::datadog::remoteagent::*;
    pub use super::agent_include::datadog::workloadmeta::*;
}
