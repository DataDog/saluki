pub mod classifier;
pub mod source_adapter;
pub mod witness;

/// Generated typed deserializer for the supported Datadog Agent configuration surface.
///
/// Generated at build time from `core_schema.yaml` plus `schema_overlay.yaml`. Contains only keys
/// inventoried as `support: full` or `support: partial`. Mostly unused until the configuration
/// translator consumes it.
pub mod datadog_configuration;

pub use datadog_configuration::DatadogConfiguration;
pub use source_adapter::{DatadogRemapper, KEY_ALIASES};
pub use witness::{drive, DatadogConfigConsumer, TranslateError, TranslateResult};
