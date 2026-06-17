//! Configuration system facade: loading, translation, lifecycle, and runtime updates.
//!
//! This is the only ADP production crate permitted to use `saluki-config-tools`
//! raw-map APIs. All raw config access is confined here.
//!
//! # In scope
//!
//! - `ConfigurationSystem` and its staged no-escape lifecycle:
//!   `ConfigurationSystem::load()` -> `LoadedConfigurationSystem` ->
//!   `loaded.start_runtime()` -> `StartedConfigurationSystem`. The `Loaded -> Started`
//!   transition consumes by value so the raw map cannot escape.
//!
//! - Local source loading: wiring `KEY_ALIASES` and `DatadogRemapper` from
//!   `datadog-agent-config` into the `saluki-config-tools` loader. Sources load once,
//!   inside this crate.
//!
//! - Bootstrap parsing: exposing typed `BootstrapConfiguration` slices from the loaded
//!   snapshot, before runtime authority exists.
//!
//! - `Translator`: the witness consumer that implements `DatadogConfigConsumer`. Holds
//!   the in-progress `SalukiConfiguration` as its accumulator. Delegates to per-domain
//!   subsystem translators (forwarder, dogstatsd, metrics, etc.).
//!
//! - Seed-then-drive translation: `Translator::new(saluki_only.seed())` followed by
//!   `drive(&datadog, &mut translator)` followed by `translator.finish()`.
//!
//! - `ConfigUpdateRouter`: retranslates on inbound source updates, diffs old vs new
//!   native slices, and sends changed slices through `ScopedConfig<T>` handles.
//!
//! - `DynamicConfigHandles`: the bundle of per-slice `ScopedConfig<T>` returned by
//!   `started.dynamic_handles()`.
//!
//! - Typed Datadog Agent attachment: `RemoteAgentClientConfiguration` (pure config),
//!   `RemoteAgentClient` (connect), `DatadogAgentConnection` (retained capability),
//!   `Attachments` (per-consumer typed capabilities).
//!
//! - Config views: producing `SourceConfigView` and `InternalConfigView` for the
//!   `/config` endpoints. Live-on-request from the retained snapshot.
//!
//! - Overlay/classifier validation.
//!
//! # Out of scope
//!
//! - Component implementations. This crate must not depend on `saluki-components`.
//!   The translator writes into component-native structs from
//!   `saluki-component-config`, but never instantiates or runs a component.
//!
//! - The ADP-native model definitions themselves (`SalukiConfiguration`,
//!   `ControlConfiguration`, etc.). Those live in `agent-data-plane-config`.
//!   This crate depends on that model and populates it.
//!
//! - The leaf component-native config struct definitions. Those live in
//!   `saluki-component-config`. This crate writes into them via the translator.
//!
//! - The Datadog source model definitions (`DatadogConfiguration`, witness trait,
//!   `drive()`). Those live in `datadog-agent-config`. This crate implements the
//!   trait and calls the driver.
//!
//! - Binary startup, topology assembly, CLI handlers, internal services. Those live
//!   in `bin/agent-data-plane` and consume this crate's typed outputs.
//!
//! # Dependency rule
//!
//! Depends on `datadog-agent-config`, `agent-data-plane-config`,
//! `saluki-component-config`, and `saluki-config-tools`. Must not depend on
//! `saluki-components` or `bin/agent-data-plane`.
