//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

mod dsd_stats;
pub use self::dsd_stats::DogStatsDStatisticsConfiguration;

mod prometheus;
pub use self::prometheus::PrometheusConfiguration;
