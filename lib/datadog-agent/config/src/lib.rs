pub mod classifier;

/// Build-time generated code, produced from `core_schema.yaml` plus `schema_overlay.yaml`.
pub mod generated;

/// The translation error type recorded by the translator and surfaced by the witness driver.
pub mod translate_error;

pub use generated::{drive, DatadogConfigWitness, DatadogConfiguration};
pub use translate_error::TranslateError;
