pub mod classifier;

/// Build-time generated code, produced from `core_schema.yaml` plus `schema_overlay.yaml`.
pub mod generated;

pub use generated::DatadogConfiguration;
