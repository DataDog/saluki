//! Forwarder implementations.
mod cluster_agent;
pub use self::cluster_agent::ClusterAgentForwarderConfiguration;

mod datadog;
pub use self::datadog::DatadogForwarderConfiguration;

mod otlp;
pub use self::otlp::OtlpForwarderConfiguration;
