//! # Configuration model for Saluki components and ADP.
//!
//! `SalukiConfiguration` is the type which configures ADP.
//!
//! ## Responsibilities
//!
//! Model all runtime configuration used by ADP in a single, hierarchical type by adding
//! control-level configuration to what is modeled in `saluki-component-config`.
//!
//! ## Workspace Dependency Boundaries
//!
//! This depends only on `saluki-component-config` and `saluki-io`; it must not depend on the
//! Datadog model or a raw config map. It is separate from `saluki-component-config` so components
//! see only their own slice, and separate from the config-system so the model provably cannot
//! import source mechanics: it does not depend on them.
