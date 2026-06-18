//! ADP-native configuration model: the typed output of configuration translation.
//!
//! # In scope
//!
//! - `SalukiConfiguration { control, components }`, the complete ADP-native runtime
//!   configuration after translation. This is the single output model that all ADP
//!   runtime code consumes.
//!
//! - `ControlConfiguration`: pipeline gates and topology-shaping decisions. Read only
//!   by config-system and the topology builder, never by components.
//!
//! - `ComponentConfiguration` and its per-domain group wrappers (`ForwarderConfigs`,
//!   `DogStatsDConfigs`, `MetricsConfigs`, etc.). Each group embeds
//!   `saluki-component-config` leaf structs directly -- no duplicate model, no extra
//!   conversion layer.
//!
//! - `BootstrapConfiguration { datadog: DatadogBootstrap, saluki: SalukiBootstrap }`:
//!   the small typed pre-runtime slice. Its struct definition is the allowlist.
//!
//! - `SalukiOnlyConfiguration` and its per-subsystem sub-structs: the parsed
//!   Saluki-schema-only input (`SALUKI_*` / `saluki.yaml`). Owns the `seed()` method
//!   that produces a base `SalukiConfiguration` with defaults and Saluki-only values.
//!
//! - Authority enums (local vs stream).
//!
//! - Typed view models (`ConfigViews`, `SourceConfigView`, `InternalConfigView`).
//!
//! # Out of scope
//!
//! - Datadog source models (`DatadogConfiguration`), the witness trait
//!   (`DatadogConfigConsumer`), the witness driver (`drive`), `KEY_ALIASES`,
//!   `DatadogRemapper`. Those are Datadog source-language concerns and live in
//!   `datadog-agent-config`.
//!
//! - Raw config maps (`GenericConfiguration`), loaders (`ConfigurationLoader`), merge
//!   logic, watch plumbing, or any type from `saluki-config-tools`. This crate
//!   describes the translated output, not the translation machinery.
//!
//! - The config-system facade (`ConfigurationSystem`, lifecycle objects, translator,
//!   router). Those live in `agent-data-plane-config-system`.
//!
//! - Component implementations. Components consume the leaf structs from
//!   `saluki-component-config`, never the group wrappers or `SalukiConfiguration`
//!   directly.
//!
//! - Translation logic, witness implementations, or `consume_<key>` mappings.
//!
//! # Dependency rule
//!
//! Depends on `saluki-component-config` (embeds its leaf structs). Must not depend on
//! `datadog-agent-config`, `saluki-config-tools`, `agent-data-plane-config-system`, or
//! `saluki-components`.

#![deny(missing_docs)]

pub mod authority;
pub mod bootstrap;
pub mod control;
pub mod model;
pub mod saluki_only;
pub mod views;

pub use self::authority::RuntimeAuthority;
pub use self::bootstrap::{
    AgentIpcBootstrap, BootstrapConfiguration, DatadogBootstrap, LocalApiBootstrap, LoggingBootstrap, SalukiBootstrap,
    TelemetryBootstrap,
};
pub use self::control::{ControlConfiguration, OtlpControl, OtlpProxyControl, PipelineGate};
pub use self::model::{
    ChecksConfigs, ComponentConfiguration, DogStatsDConfigs, EventsConfigs, ForwarderConfigs, LogsConfigs,
    MetricsConfigs, OtlpConfigs, SalukiConfiguration, ServiceChecksConfigs, TracesConfigs, WorkloadConfigs,
};
pub use self::saluki_only::SalukiOnlyConfiguration;
pub use self::views::{ConfigViews, InternalConfigView, SourceConfigView};
