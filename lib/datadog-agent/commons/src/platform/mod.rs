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

#[cfg(windows)]
mod windows_impl;

#[cfg(windows)]
pub use self::windows_impl::*;

/// Prefix for all environment variables used by the Datadog Agent.
pub const DATADOG_AGENT_ENV_VAR_PREFIX: &str = "DD";

/// Platform-specific settings and information.
pub struct PlatformSettings;

impl PlatformSettings {
    /// Returns the path to the default Datadog Agent configuration directory.
    pub fn get_config_dir_path() -> &'static Path {
        get_config_dir_path()
    }

    /// Returns the path to the default Datadog Agent configuration file.
    pub fn get_config_file_path() -> PathBuf {
        Self::get_config_dir_path().join("datadog.yaml")
    }

    /// Returns the path to the default Datadog Agent authentication token.
    pub fn get_auth_token_path() -> PathBuf {
        Self::get_config_dir_path().join("auth_token")
    }

    /// Returns the filename of the IPC certificate file.
    pub fn get_ipc_cert_filename() -> &'static Path {
        Path::new("ipc_cert.pem")
    }

    /// Returns the default log file path for the Agent Data Plane.
    pub fn get_default_log_file_path() -> PathBuf {
        Self::get_log_dir_path().join("agent-data-plane.log")
    }

    /// Returns the default DogStatsD debug log file path.
    pub fn get_default_dogstatsd_log_file_path() -> PathBuf {
        Self::get_log_dir_path()
            .join("dogstatsd_info")
            .join("dogstatsd-stats.log")
    }

    /// Returns the default local syslog URI used by the Datadog Agent on this platform.
    pub const fn get_default_syslog_uri() -> &'static str {
        DATADOG_AGENT_DEFAULT_SYSLOG_URI
    }

    fn get_log_dir_path() -> &'static Path {
        get_log_dir_path()
    }

    /// Returns the prefix for all environment variables used by the Datadog Agent.
    pub const fn get_env_var_prefix() -> &'static str {
        DATADOG_AGENT_ENV_VAR_PREFIX
    }
}

#[cfg(test)]
mod tests {
    use super::PlatformSettings;

    #[test]
    fn default_log_files_are_derived_from_platform_log_dir() {
        assert_eq!(
            PlatformSettings::get_default_log_file_path(),
            PlatformSettings::get_log_dir_path().join("agent-data-plane.log")
        );
        assert_eq!(
            PlatformSettings::get_default_dogstatsd_log_file_path(),
            PlatformSettings::get_log_dir_path()
                .join("dogstatsd_info")
                .join("dogstatsd-stats.log")
        );
    }
}
