//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

mod datadog;
pub use self::datadog::{
    DatadogEventsServiceChecksConfiguration, DatadogMetricsConfiguration, DatadogStatusFlareConfiguration,
};
mod prometheus;
pub use self::prometheus::PrometheusConfiguration;
