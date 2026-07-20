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

mod loaded;
mod saluki_env_overlay;
mod saluki_only;
mod system;
mod translators;

pub use loaded::{EnvPrecedence, LoadedConfiguration};
pub use system::{ConfigurationSystem, Error};
