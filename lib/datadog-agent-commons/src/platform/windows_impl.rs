use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use windows_registry::LOCAL_MACHINE;

// TODO: Add Windows-specific tests for registry-backed path resolution once we have Windows CI coverage.

/// Default configuration directory for the Datadog Agent.
///
/// The Datadog Agent Windows installer stores the effective configuration root in
/// `HKLM\\SOFTWARE\\Datadog\\Datadog Agent\\ConfigRoot`. This constant is the fallback
/// used when the registry value is unavailable.
pub const DATADOG_AGENT_CONF_DIR: &str = r"C:\ProgramData\Datadog";

/// Default log directory for the Datadog Agent.
///
/// The effective log directory is derived from the registry-backed configuration root at runtime.
/// This constant is the fallback used when the registry value is unavailable.
pub const DATADOG_AGENT_LOG_DIR: &str = r"C:\ProgramData\Datadog\logs";

/// Default local syslog URI for the Datadog Agent.
///
/// The core Agent does not support syslog logging on Windows.
pub const DATADOG_AGENT_DEFAULT_SYSLOG_URI: &str = "";

const DATADOG_AGENT_REGISTRY_SUBKEY: &str = r"SOFTWARE\Datadog\Datadog Agent";
const DATADOG_AGENT_CONFIG_ROOT_VALUE: &str = "ConfigRoot";

static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Returns the path to the default Datadog Agent configuration directory.
pub fn get_config_dir_path() -> &'static Path {
    CONFIG_DIR
        .get_or_init(|| read_config_root_from_registry().unwrap_or_else(|| PathBuf::from(DATADOG_AGENT_CONF_DIR)))
        .as_path()
}

/// Returns the path to the default Datadog Agent log directory.
pub fn get_log_dir_path() -> &'static Path {
    LOG_DIR.get_or_init(|| get_config_dir_path().join("logs")).as_path()
}

fn read_config_root_from_registry() -> Option<PathBuf> {
    LOCAL_MACHINE
        .open(DATADOG_AGENT_REGISTRY_SUBKEY)
        .and_then(|key| key.get_string(DATADOG_AGENT_CONFIG_ROOT_VALUE))
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
