//! # Saluki component configuration model
//!
//! Leaf crate for component-native configuration structs and the dynamic config handle.
//!
//! Component-native config structs are the resolved, source-agnostic data types that a component
//! receives as its runtime configuration. They carry no Datadog key names, no source-language
//! `serde`, no aliases, and no remapping logic. Structs here may derive `Serialize` for the
//! `/config/internal` view, but must not carry source-deserialization.
//!
//! [`DynamicConfig<T>`] is the fixed-or-live runtime handle that wraps a component config value. A
//! never-dynamic component takes a plain `T`; a dynamic-capable component takes `DynamicConfig<T>`.
//! Whether the handle is currently `Fixed` or `Live` is a deployment detail hidden from the
//! component.
//!
//! ## Workspace dependency boundaries
//!
//! This is a leaf config crate: it depends on no other configuration crate and must not depend on
//! the Datadog source model (`datadog-agent-config`), the ADP native model
//! (`agent-data-plane-config`), the config system (`agent-data-plane-config-system`), the raw config
//! map (`saluki-config-tools`), or component implementations (`saluki-components`).
//!
//! ## Scope
//!
//! [`SourceConfig`] is the component-native config struct for the DogStatsD source.
// TODO: remaining component structs land here as their owning components are migrated.

#![deny(missing_docs)]

pub mod dogstatsd;
pub mod dynamic;

pub use self::dogstatsd::{EnablePayloadsConfiguration, OriginEnrichmentConfiguration, SourceConfig};
pub use self::dynamic::DynamicConfig;
