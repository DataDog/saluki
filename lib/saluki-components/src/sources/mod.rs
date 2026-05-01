//! Source implementations.

/// APM receiver source and shared sampling-rate state.
pub mod apm;
pub use self::apm::ApmReceiverConfiguration;

mod dogstatsd;
pub use self::dogstatsd::DogStatsDConfiguration;

mod internal_metrics;
pub use self::internal_metrics::InternalMetricsConfiguration;

mod heartbeat;
pub use self::heartbeat::HeartbeatConfiguration;

mod otlp;
pub use self::otlp::OtlpConfiguration;
