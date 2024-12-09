//! Destination implementations.

mod blackhole;
pub use self::blackhole::BlackholeConfiguration;

mod datadog_events_service_checks;
pub use self::datadog_events_service_checks::DatadogEventsServiceChecksConfiguration;

mod datadog_metrics;
pub use self::datadog_metrics::DatadogMetricsConfiguration;

mod datadog_status;
pub use self::datadog_status::DatadogStatusConfiguration;

mod prometheus;
pub use self::prometheus::PrometheusConfiguration;
