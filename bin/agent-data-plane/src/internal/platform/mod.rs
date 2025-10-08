//! Platform-specific settings.

#[cfg(target_os = "linux")]
mod linux_impl;

#[cfg(target_os = "linux")]
pub use self::linux_impl::*;

/// Prefix for all environment variables used by the Datadog Agent.
pub const DATADOG_AGENT_ENV_VAR_PREFIX: &str = "DD";
