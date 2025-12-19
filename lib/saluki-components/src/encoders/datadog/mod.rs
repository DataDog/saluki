mod events;
pub use self::events::DatadogEventsConfiguration;

mod service_checks;
pub use self::service_checks::DatadogServiceChecksConfiguration;

mod metrics;
pub use self::metrics::DatadogMetricsConfiguration;

mod logs;
pub use self::logs::DatadogLogsConfiguration;

mod traces;
pub use self::traces::DatadogTraceConfiguration;
