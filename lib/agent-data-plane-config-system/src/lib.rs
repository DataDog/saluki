//! Configuration system facade: loading, authority resolution, translation, validation, config
//! views, and runtime updates.
//!
//! This is the only ADP production crate permitted to use the raw-map APIs of `saluki-config`; all
//! raw configuration access is confined here. It wires the Datadog source-normalization metadata
//! (`KEY_ALIASES` and the remapper) from `datadog-agent-config` into the loader, drives the witness
//! translation, and produces a `SalukiConfiguration`.
//!
//! It does not construct components and does not depend on `saluki-components`.
