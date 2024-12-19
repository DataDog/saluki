//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

mod datadog;
pub use self::datadog::{
    new_remote_agent_server, DatadogEventsServiceChecksConfiguration, DatadogMetricsConfiguration,
    DatadogStatusFlareConfiguration,
};
mod prometheus;
pub use self::prometheus::PrometheusConfiguration;
