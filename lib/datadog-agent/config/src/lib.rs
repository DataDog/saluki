pub mod classifier;

/// Generated typed deserializer for the supported Datadog Agent configuration surface.
///
/// Generated at build time from `core_schema.yaml` plus `schema_overlay.yaml`. Contains only keys
/// inventoried as `support: full` or `support: partial`. Mostly unused until the configuration
/// translator consumes it.
pub mod datadog_configuration;

pub use datadog_configuration::DatadogConfiguration;

/// Datadog source-language normalization metadata: key aliases and the env-var remapper.
pub mod source_normalization;

/// The translation error type recorded by the translator and surfaced by the witness driver.
pub mod translate_error;

/// Generated witness trait and fallible driver over the supported Datadog configuration keys.
pub mod witness;

pub use source_normalization::{DatadogRemapper, KEY_ALIASES};
pub use translate_error::TranslateError;
pub use witness::{drive, DatadogConfigConsumer};
