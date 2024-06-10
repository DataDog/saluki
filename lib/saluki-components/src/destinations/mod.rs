//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

mod datadog_metrics;
pub use self::datadog_metrics::DatadogMetricsConfiguration;

mod prometheus;
pub use self::prometheus::PrometheusConfiguration;
