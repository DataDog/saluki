//! Forwarder implementations.
mod datadog;
pub use self::datadog::DatadogForwarderConfiguration;

mod otlp;
pub use self::otlp::OtlpForwarderConfiguration;
