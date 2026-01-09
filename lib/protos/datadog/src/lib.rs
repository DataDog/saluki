//! Datadog Agent-specific Protocol Buffers definitions.
//!
//! This crate contains generated code based on the Protocol Buffers definitions used by the Datadog Agent to
//! communicate with the Datadog Platform, specifically for shipping metrics and traces.
#![deny(warnings)]
#![allow(dead_code)]
#![allow(clippy::enum_variant_names)]
#![allow(clippy::doc_overindented_list_items)]

pub(crate) mod serde;

mod payload_builder_include {
    include!(concat!(env!("OUT_DIR"), "/payload_builder/mod.rs"));
}

mod payload_include {
    include!(concat!(env!("OUT_DIR"), "/payload_protos/mod.rs"));
}

mod trace_include {
    include!(concat!(env!("OUT_DIR"), "/trace_protos/mod.rs"));
}

mod agent_include {
    include!(concat!(env!("OUT_DIR"), "/api.mod.rs"));
}

/// Payload-related definitions.
///
/// This includes definitions for building series, sketch, and event payloads to send to the Datadog platform.
pub mod payload {
    /// Payload definitions.
    ///
    /// Appropriate for use cases when owned values are required, such as when deserializing payloads or when a
    /// full-blown builder approach is not required or is too burdensome.
    pub mod definitions {
        pub use crate::payload_include::agent_payload::events_payload::*;
        pub use crate::payload_include::agent_payload::metric_payload::*;
        pub use crate::payload_include::agent_payload::sketch_payload::{sketch::*, Sketch};
        pub use crate::payload_include::agent_payload::*;
    }

    /// Builder definitions.
    ///
    /// Appropriate for serialization-only use cases where efficiency/performance is a priority. Allows for
    /// incrementally encoding payloads by writing out fields/messages in a "piecemeal" fashion.
    pub mod builder {
        pub use crate::payload_builder_include::datadog::agentpayload::*;
    }
}

/// Trace-related definitions.
///
/// This includes definitions for building trace, and trace-related, payloads to send to the Datadog platform.
pub mod traces {
    /// Payload definitions.
    ///
    /// Appropriate for use cases when owned values are required, such as when deserializing payloads or when a
    /// full-blown builder approach is not required or is too burdensome.
    pub mod definitions {
        pub use crate::trace_include::agent_payload::*;
        pub use crate::trace_include::span::{attribute_any_value::*, attribute_array_value::*, *};
        pub use crate::trace_include::stats::*;
        pub use crate::trace_include::tracer_payload::*;
    }
}

/// Agent-related definitions.
///
/// This includes gRPC client stubs, and request/response definitions, for interacting with the Datadog Agent's
/// insecure/secure gRPC services.
pub mod agent {
    pub use super::agent_include::datadog::api::v1::agent_client::AgentClient;
    pub use super::agent_include::datadog::api::v1::agent_secure_client::AgentSecureClient;
    pub use super::agent_include::datadog::autodiscovery::*;
    pub use super::agent_include::datadog::model::v1::*;
    pub use super::agent_include::datadog::remoteagent::*;
    pub use super::agent_include::datadog::workloadmeta::*;
}
