//! Forwarder implementations.
mod cluster_agent;
pub use self::cluster_agent::ClusterAgentForwarderConfiguration;

mod datadog;
pub use self::datadog::DatadogForwarderConfiguration;
pub use crate::common::datadog::config::{
    ForwarderConfiguration, ForwarderHttpProtocol, OpwMetricsConfiguration, OpwMetricsSettings,
};
pub use crate::common::datadog::endpoints::EndpointConfiguration;
pub use crate::common::datadog::proxy::ProxyConfiguration;
pub use crate::common::datadog::retry::RetryConfiguration;
pub use crate::common::datadog::routing::{
    MetricsRoutingTargets, RoutingTarget, RoutingTargetCatalog, RoutingTargetId, RoutingTargetKind, RoutingTargetSet,
};

mod otlp;
pub use self::otlp::OtlpForwarderConfiguration;
