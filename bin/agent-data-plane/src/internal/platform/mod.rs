//! Platform-specific settings.

#[cfg(target_os = "linux")]
mod linux_impl;

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub use self::linux_impl::*;

#[cfg(target_os = "macos")]
mod macos_impl;

#[cfg(target_os = "macos")]
pub use self::macos_impl::*;

/// Prefix for all environment variables used by the Datadog Agent.
pub const DATADOG_AGENT_ENV_VAR_PREFIX: &str = "DD";

/// Platform-specific settings and information.
pub struct PlatformSettings;

impl PlatformSettings {
    /// Returns the path to the default Datadog Agent configuration directory.
    pub fn get_config_dir_path() -> PathBuf {
        PathBuf::from(DATADOG_AGENT_CONF_DIR)
    }

    /// Returns the path to the default Datadog Agent configuration file.
    pub fn get_config_file_path() -> PathBuf {
        Path::new(DATADOG_AGENT_CONF_DIR).join("datadog.yaml")
    }

    /// Returns the path to the default Datadog Agent authentication token.
    pub fn get_auth_token_path() -> PathBuf {
        Path::new(DATADOG_AGENT_CONF_DIR).join("auth_token")
    }

    /// Returns the prefix for all environment variables used by the Datadog Agent.
    pub const fn get_env_var_prefix() -> &'static str {
        DATADOG_AGENT_ENV_VAR_PREFIX
    }
}
