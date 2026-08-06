//! The configuration system: translation and subscription of external configuration sources into
//! an ADP-typed model.
//!
//! This crate turns configuration sources into `SalukiConfiguration`:
//!
//! - the typed Datadog source (`DatadogConfiguration`), whose supported keys the generated `drive`
//!   feeds to `DatadogTranslator` (a `DatadogConfigWitness`) one key at a time, and
//! - the Saluki-schema-only source (`SalukiOnly`), whose values seed the fields the Datadog schema
//!   does not cover.
//!
//! [`ConfigurationSystem`] is the entry point: it translates a raw source map into the initial
//! configuration and, when the map streams updates, keeps that configuration current. This is the
//! only ADP production crate that bridges the source configuration to the model; it constructs no
//! components and does not depend on `saluki-components`.

// `saluki_only`'s transport test builds a `json!` literal holding every Saluki-only key, and that literal
// outgrew the default macro recursion limit as keys were added. Raise the ceiling so adding a key stays a
// one-line change.
#![recursion_limit = "512"]

mod env_provider;
mod loaded;
mod saluki_env_overlay;
mod saluki_only;
mod system;
mod translators;

pub use env_provider::EnvironmentProvider;
pub use loaded::{EnvPrecedence, LoadedConfiguration};
pub use system::{ConfigurationSystem, Error};
