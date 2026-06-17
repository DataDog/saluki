//! Leaf crate for component-native configuration structs and dynamic config handles.
//!
//! # In scope
//!
//! - Component-native config structs (`DatadogForwarderConfig`, `DogStatsDConfig`,
//!   `OtlpConfig`, etc.). These are resolved, source-agnostic data types that a
//!   component receives as its runtime configuration. They carry no Datadog key names,
//!   no source-language serde attributes, no aliases, no remapping, and no fixups.
//!
//! - `ScopedConfig<T>`, the fixed-or-live runtime handle that wraps a component config
//!   value. A never-dynamic component takes a plain `T`; a dynamic-capable component
//!   takes `ScopedConfig<T>`. Whether the handle is currently `Fixed` or `Live` is a
//!   deployment detail hidden from the component.
//!
//! - Supporting leaf types used inside the config structs (endpoint descriptors, TLS
//!   config, retry config, duration wrappers, etc.) when those types are shared across
//!   multiple component configs and do not belong to a single component.
//!
//! # Out of scope
//!
//! - Datadog source models, key names, overlay metadata, or anything from
//!   `datadog-agent-config`. This crate must never depend on or re-export Datadog
//!   source-language types.
//!
//! - The ADP-native target model (`SalukiConfiguration`, `ControlConfiguration`,
//!   `ComponentConfiguration`, group wrappers). Those live in `agent-data-plane-config`,
//!   which embeds these leaf structs.
//!
//! - Raw configuration maps, loaders, or any type from `saluki-config-tools`.
//!   Components do not load or parse raw config; the translation system does that.
//!
//! - Component implementations. Those stay in `saluki-components`, which depends on
//!   this crate (not the other way around).
//!
//! - Source-language `Deserialize` impls or `from_configuration` constructors. The
//!   translator in `agent-data-plane-config-system` owns source-to-native mapping.
//!   Structs here may derive `Serialize` for the `/config/internal` view, but must
//!   not carry source-deserialization.
//!
//! - Translation logic, seed logic, or witness trait implementations.
//!
//! # Dependency rule
//!
//! This is a leaf crate. It must not depend on `datadog-agent-config`,
//! `agent-data-plane-config`, `agent-data-plane-config-system`,
//! `saluki-config-tools`, or `saluki-components`.
