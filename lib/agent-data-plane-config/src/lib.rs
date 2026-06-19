//! # Configuration model for Saluki components and ADP.
//!
//! [`SalukiConfiguration`] is the ADP-native runtime configuration model: the typed output of
//! configuration translation, and the single model that all ADP runtime code consumes. It
//! adds control-level configuration ([`ControlConfiguration`]) to the component groups, which embed
//! the leaf structs from `saluki-component-config` directly.
//!
//! [`SalukiOnlyConfiguration`] is the Saluki-schema-only source input. Its [`seed`] produces
//! a base `SalukiConfiguration` (defaults plus Saluki-only values) that the Datadog `drive` later
//! overlays its disjoint schema fields onto.
//!
//! ## Workspace dependency boundaries
//!
//! Depends on `saluki-component-config` (embeds its leaf structs). Must not depend on the Datadog
//! source model (`datadog-agent-config`), the raw config map (`saluki-config-tools`), the config
//! system (`agent-data-plane-config-system`), or component implementations (`saluki-components`). It
//! is separate from `saluki-component-config` so components see only their own slice, and separate
//! from the config-system so the model provably cannot import source mechanics.
//!
//! [`seed`]: SalukiOnlyConfiguration::seed

#![deny(missing_docs)]

pub mod control;
pub mod dogstatsd;
pub mod model;
pub mod saluki_only;

pub use self::control::ControlConfiguration;
pub use self::model::{ComponentConfiguration, SalukiConfiguration};
pub use self::saluki_only::SalukiOnlyConfiguration;
