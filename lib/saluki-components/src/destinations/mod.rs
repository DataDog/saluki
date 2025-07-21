//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

pub mod datadog;
pub use self::datadog::{DatadogEventsConfiguration, DatadogMetricsConfiguration, DatadogServiceChecksConfiguration};

mod prometheus;
pub use self::prometheus::PrometheusConfiguration;

mod dsd_stats;
pub use self::dsd_stats::DogStatsDInternalStatisticsConfiguration;
