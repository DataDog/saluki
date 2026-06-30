//! ADP-native configuration model: the typed target of configuration translation.
//!
//! This crate owns the domain-shaped model types that translation produces and that ADP runtime
//! code consumes: `SalukiConfiguration { control, shared, domains }`, `ControlConfiguration`,
//! `SharedConfiguration`, `DomainConfiguration` and its per-domain structs, `BootstrapConfiguration`,
//! `SalukiOnlyConfiguration`, the source-authority enums, and `DynamicConfig<T>`.
//!
//! It does not embed component config structs (those stay in `saluki-components`, built from this
//! model). It depends on neither the raw configuration map nor the Datadog source model, so a
//! consumer can depend on it without inheriting either.
