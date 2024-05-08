//! Datadog Agent-specific Protocol Buffers definitions.
//!
//! This crate contains generated code based on the Protocol Buffers definitions used by the Datadog Agent to
//! communicate with the Datadog Platform, specifically for shipping metrics and traces.
#![deny(warnings)]
#![allow(clippy::enum_variant_names)]
mod include {
    include!(concat!(env!("OUT_DIR"), "/protos/mod.rs"));
}

mod agent_secure_include {
    include!(concat!(env!("OUT_DIR"), "/api.mod.rs"));
}

/// Metrics-related definitions.
pub mod metrics {
    pub use super::include::dd_metric::metric_payload::*;
    pub use super::include::dd_metric::sketch_payload::{sketch::*, Sketch};
    pub use super::include::dd_metric::*;
    pub use super::include::ddsketch_full::*;
}

/// Trace-related definitions.
pub mod traces {
    pub use super::include::dd_trace::*;
}

/// Agent "secure" definitions.
pub mod agent_secure {
    pub use super::agent_secure_include::datadog::api::v1::agent_secure_client::AgentSecureClient;
    pub use super::agent_secure_include::datadog::model::v1::*;
}
