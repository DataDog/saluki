//! Forwarder implementations.
mod cluster_agent;
pub use self::cluster_agent::ClusterAgentForwarderConfiguration;

mod datadog;
pub use self::datadog::DatadogForwarderConfiguration;
pub use crate::common::datadog::routing::{
    MetricsRoutingTargets, RoutingTarget, RoutingTargetCatalog, RoutingTargetId, RoutingTargetKind, RoutingTargetSet,
};

mod otlp;
pub use self::otlp::OtlpForwarderConfiguration;
