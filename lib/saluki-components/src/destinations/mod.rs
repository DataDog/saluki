//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

pub mod datadog;
pub use self::datadog::{DatadogEventsConfiguration, DatadogMetricsConfiguration, DatadogServiceChecksConfiguration, DogStatsDInternalStatisticsConfiguration};

mod prometheus;
pub use self::prometheus::PrometheusConfiguration;