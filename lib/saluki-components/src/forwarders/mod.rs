//! Forwarder implementations.
pub use crate::common::datadog::endpoints::{queue_endpoint, MetricsRecipient, MetricsRecipientRoute};

/// Delivery hosts: the Datadog forwarder as a host of recipients that change while it runs.
pub mod delivery {
    pub use crate::common::datadog::config::ForwarderHttpProtocol;
    pub use crate::common::datadog::delivery::*;
}

mod cluster_agent;
pub use self::cluster_agent::ClusterAgentForwarderConfiguration;

mod datadog;
pub use self::datadog::DatadogForwarderConfiguration;

mod otlp;
pub use self::otlp::OtlpForwarderConfiguration;
