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

mod sketch_include {
    include!(concat!(env!("OUT_DIR"), "/sketch_protos/mod.rs"));
}

mod trace_piecemeal_include {
    include!(concat!(env!("OUT_DIR"), "/trace_piecemeal/mod.rs"));
}

mod checks_include {
    include!(concat!(env!("OUT_DIR"), "/checks.mod.rs"));
}

/// Metrics-related definitions.
pub mod metrics {
    pub use super::include::agent_payload::metric_payload::*;
    pub use super::include::agent_payload::sketch_payload::{sketch::*, Sketch};
    pub use super::include::agent_payload::*;

    /// Metrics V3 API-related definitions.
    pub mod v3 {
        pub use super::super::include::intake_v3::*;
    }
}

/// Event-related definitions.
pub mod events {
    pub use super::include::agent_payload::events_payload::*;
    pub use super::include::agent_payload::EventsPayload;
}

/// Trace-related definitions.
pub mod traces {
    pub use super::trace_include::agent_payload::*;
    pub use super::trace_include::classic_span::{attribute_any_value::*, attribute_array_value::*, *};
    pub use super::trace_include::classic_tracer_payload::*;
    pub use super::trace_include::stats::*;

    /// Piecemeal-generated builder types for incremental trace encoding.
    pub mod builders {
        pub use super::super::trace_piecemeal_include::datadog::trace::*;
    }

    /// Efficient Trace Payload (ETP) types: the string-indexed trace payload format used by the v0.2 trace intake.
    ///
    /// The `idx` module name mirrors the Agent's proto package (`datadog.trace.idx`); prefer the term "ETP" when
    /// referring to this format in prose and in our own (non-generated) code.
    pub mod idx {
        pub use super::super::trace_include::idx_span::{any_value, *};
        pub use super::super::trace_include::idx_tracer_payload::*;
    }
}

/// Agent definitions.
pub mod agent {
    pub use super::agent_include::datadog::api::v1::agent_client::AgentClient;
    pub use super::agent_include::datadog::api::v1::agent_secure_client::AgentSecureClient;
    pub use super::agent_include::datadog::autodiscovery::*;
    pub use super::agent_include::datadog::model::v1::*;
    pub use super::agent_include::datadog::remoteagent::v1::remote_agent_client::RemoteAgentClient;
    pub use super::agent_include::datadog::remoteagent::*;
    pub use super::agent_include::datadog::workloadmeta::*;
}

/// DDSketch definitions from (sketches-go).
pub mod sketches {
    pub use super::sketch_include::ddsketch::*;
}

/// Checks definitions.
pub mod checks {
    pub use super::checks_include::datadog::checks::v1::*;
}

#[cfg(test)]
mod tests {
    use protobuf::Message as _;

    use super::traces::{self, AgentPayload};

    #[test]
    fn agent_payload_round_trips_classic_and_indexed_tracer_payloads() {
        let mut classic_payload = traces::TracerPayload::new();
        classic_payload.containerID = "classic-container".to_string();

        let mut indexed_payload = traces::idx::TracerPayload::new();
        indexed_payload.strings = vec!["".to_string(), "indexed-container".to_string()];
        indexed_payload.containerIDRef = 1;

        let mut payload = AgentPayload::new();
        payload.tracerPayloads.push(classic_payload);
        payload.idxTracerPayloads.push(indexed_payload);

        let encoded = payload.write_to_bytes().expect("payload should encode");
        let decoded = AgentPayload::parse_from_bytes(&encoded).expect("payload should decode");

        let indexed_payloads: &[traces::idx::TracerPayload] = decoded.idxTracerPayloads();
        assert_eq!(decoded.tracerPayloads()[0].containerID(), "classic-container");
        assert_eq!(indexed_payloads[0].containerIDRef(), 1);
        assert_eq!(indexed_payloads[0].strings(), &["", "indexed-container"]);
    }
}
