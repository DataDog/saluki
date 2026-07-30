//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

mod dsd_stats;
pub use self::dsd_stats::{DogStatsDStatisticsConfiguration, DogStatsDStatsAPIHandler};

mod dsd_debug_log;
pub use self::dsd_debug_log::DogStatsDDebugLogConfiguration;

mod dogstatsd_client_telemetry;
pub use self::dogstatsd_client_telemetry::DogStatsDClientTelemetryConfiguration;

#[cfg(test)]
mod dogstatsd_client_telemetry_tests;

mod prometheus;
pub use self::prometheus::{PrometheusConfiguration, PrometheusPayloadProvider};
