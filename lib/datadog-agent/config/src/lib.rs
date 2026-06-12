pub mod classifier;

/// Generated typed deserializer for the supported Datadog Agent configuration surface.
///
/// Generated at build time from `core_schema.yaml` plus `schema_overlay.yaml`. Contains only keys
/// inventoried as `support: full` or `support: partial`. Mostly unused until the configuration
/// translator consumes it.
pub mod datadog_configuration;

pub use datadog_configuration::DatadogConfiguration;
