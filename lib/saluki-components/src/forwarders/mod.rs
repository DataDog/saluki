//! Forwarder implementations.
mod datadog;
pub use self::datadog::DatadogConfiguration;

mod otlp;
pub use self::otlp::OtlpForwarderConfiguration;
