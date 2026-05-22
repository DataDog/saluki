//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

mod dsd_stats;
pub use self::dsd_stats::{DogStatsDStatisticsConfiguration, DogStatsDStatsCollector};

mod dsd_debug_log;
pub use self::dsd_debug_log::DogStatsDDebugLogConfiguration;

mod prometheus;
pub use self::prometheus::{PrometheusConfiguration, PrometheusPayloadProvider};
