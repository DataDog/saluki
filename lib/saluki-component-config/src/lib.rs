//! # Saluki component configuration model
//!
//! ## Responsibilities
//!
//! Model the configuration of Saluki components. This is a leaf crate, depended on from above by
//! the configuration system.
//!
//! Component-native config structs are the resolved, source-agnostic data types that a
//! component receives as its runtime configuration. They carry no Datadog key names, no
//! source-language `serde`, no aliases, and no remapping logic.
//!
//! ## Workspace Dependency Boundaries
//!
//! This is a leaf config crate: it depends on no other configuration crate and must not depend on
//! the Datadog source model, the ADP native model, or the raw config map (`saluki-config-tools`).
