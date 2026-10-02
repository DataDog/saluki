//! Remote Configuration consumers for this crate's components.
//!
//! Each type here subscribes to one product on a
//! [`RemoteConfigurationClient`](datadog_agent_remote_config::RemoteConfigurationClient) and is handed to the
//! components that read it.

pub use crate::common::otlp::semantics::SemanticRegistryProvider;
