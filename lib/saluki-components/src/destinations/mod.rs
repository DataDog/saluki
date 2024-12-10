//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

mod datadog;
pub use self::datadog::{DatadogEventsServiceChecksConfiguration, DatadogMetricsConfiguration};
mod prometheus;
pub use self::prometheus::PrometheusConfiguration;
