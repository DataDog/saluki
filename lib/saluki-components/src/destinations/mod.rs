//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

pub mod datadog;
pub use self::datadog::{
    DatadogEventsConfiguration, DatadogMetricsConfiguration, DatadogServiceChecksConfiguration,
    DogStatsDStatisticsConfiguration,
};

mod prometheus;
pub use self::prometheus::PrometheusConfiguration;
