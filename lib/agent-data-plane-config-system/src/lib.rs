//! The Configuration system: translation and subscription of external configuration sources into an
//! ADP-typed model.
//!
//! This crate turns configuration sources into `SalukiConfiguration`:
//!
//! - the typed Datadog source (`DatadogConfiguration`), whose supported keys the generated `drive`
//!   feeds to `DatadogTranslator` (a `DatadogConfigWitness`) one key at a time, and
//! - the Saluki-schema-only source (`SalukiOnly`), whose values seed the fields the Datadog schema
//!   does not cover.
//!
//! `DatadogTranslator::translate` and `SalukiOnly::seed` are the two writers; a caller runs both to
//! assemble the model. This is the only ADP production crate that bridges the source configuration
//! to the model; it constructs no components and does not depend on `saluki-components`.

mod saluki_only;
mod translators;
