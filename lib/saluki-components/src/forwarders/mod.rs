//! Forwarder implementations.
mod cluster_agent;
pub use self::cluster_agent::ClusterAgentForwarderConfiguration;

mod datadog;
pub use self::datadog::DatadogForwarderConfiguration;
pub use crate::common::datadog::io::API_KEY_VALIDATION_PURPOSE_TAG;

mod otlp;
pub use self::otlp::OtlpForwarderConfiguration;
