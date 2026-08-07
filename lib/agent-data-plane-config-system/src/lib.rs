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
//!
//! Sources are layered as `SourceTree`s, which keep each value together with the provenance of that
//! value. That is what lets a configuration producer's own defaults be layered over local
//! configuration without shadowing it, and what lets translation resolve a setting whose meaning
//! depends on whether it was set explicitly.

mod env_provider;
mod loaded;
mod saluki_env_overlay;
mod saluki_only;
mod source;
mod system;
mod translators;

pub use env_provider::EnvironmentProvider;
pub use loaded::{EnvPrecedence, LoadedConfiguration};
pub use system::{ConfigurationSystem, Error};
