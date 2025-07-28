//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

mod datadog;
pub use self::datadog::DogStatsDStatisticsConfiguration;

mod prometheus;
pub use self::prometheus::PrometheusConfiguration;
