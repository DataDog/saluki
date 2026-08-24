//! Linux (and AIX) platform settings.
//!
//! Two filesystem layouts are supported, matching the Core Agent:
//!
//! - the FHS layout (the default), where configuration lives in `/etc/datadog-agent` and logs live in
//!   `/var/log/datadog`, and
//! - the "common root" layout, selected by the `DD_COMMON_ROOT` environment variable, where every directory the Agent
//!   owns is collapsed under a single root: `{root}/etc`, `{root}/logs`, `{root}/run`, and so on.
//!
//! The common root layout exists to support installs where the root filesystem is read-only and the Agent cannot
//! scatter writes across `/etc` and `/var`. The Core Agent implements it in `pkg/util/defaultpaths`
//! (`commonRootOrPath`), and reads the environment variable during package initialization rather than from its own
//! configuration, because the layout determines where the configuration file itself is found.
//!
//! We resolve it the same way, and for the same reason: the paths derived here (the configuration file, the IPC auth
//! token, and the IPC certificate) are all needed during bootstrap, before ADP has registered with the Core Agent and
//! can receive its resolved configuration. Paths that arrive over the configuration stream have already been resolved
//! by the Core Agent against its own root, and must not be transformed again.

use std::{
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// Default configuration directory for the Datadog Agent.
///
/// This is the FHS layout, used when the common root layout is not selected. It is deliberately private: callers must
/// go through [`get_config_dir_path`] so that the common root is never circumvented.
const DATADOG_AGENT_CONF_DIR: &str = "/etc/datadog-agent";

/// Default log directory for the Datadog Agent.
///
/// This is the FHS layout, used when the common root layout is not selected. It is deliberately private: callers must
/// go through [`get_log_dir_path`] so that the common root is never circumvented.
const DATADOG_AGENT_LOG_DIR: &str = "/var/log/datadog";

/// Default local syslog URI for the Datadog Agent.
const DATADOG_AGENT_DEFAULT_SYSLOG_URI: &str = "unixgram:///dev/log";

/// Environment variable that selects the common root layout.
const COMMON_ROOT_ENV_VAR: &str = "DD_COMMON_ROOT";

/// Common root used when `DD_COMMON_ROOT` is set but carries no value.
const DEFAULT_COMMON_ROOT: &str = "/opt/datadog-agent";

/// Configuration subdirectory of the common root.
const COMMON_ROOT_CONF_SUBDIR: &str = "etc";

/// Log subdirectory of the common root.
const COMMON_ROOT_LOG_SUBDIR: &str = "logs";

static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Returns the path to the default Datadog Agent configuration directory.
pub fn get_config_dir_path() -> &'static Path {
    CONFIG_DIR
        .get_or_init(|| config_dir_for(common_root().as_deref()))
        .as_path()
}

/// Returns the path to the default Datadog Agent log directory.
pub fn get_log_dir_path() -> &'static Path {
    LOG_DIR.get_or_init(|| log_dir_for(common_root().as_deref())).as_path()
}

/// Returns the default local syslog URI for the Datadog Agent.
pub const fn get_default_syslog_uri() -> &'static str {
    DATADOG_AGENT_DEFAULT_SYSLOG_URI
}

/// Reads the configured common root from the environment.
fn common_root() -> Option<PathBuf> {
    parse_common_root(env::var_os(COMMON_ROOT_ENV_VAR).as_deref())
}

/// Resolves the common root from the raw environment variable value.
///
/// An unset variable leaves the FHS layout in place. A variable that is set but empty selects the common root layout
/// rooted at [`DEFAULT_COMMON_ROOT`], which matches the Core Agent and allows the layout to be switched on without
/// having to name a location.
fn parse_common_root(value: Option<&OsStr>) -> Option<PathBuf> {
    match value {
        None => None,
        Some(value) if value.is_empty() => Some(PathBuf::from(DEFAULT_COMMON_ROOT)),
        Some(value) => Some(PathBuf::from(value)),
    }
}

/// Returns the configuration directory for the given common root, falling back to the FHS layout when unset.
fn config_dir_for(common_root: Option<&Path>) -> PathBuf {
    match common_root {
        Some(root) => root.join(COMMON_ROOT_CONF_SUBDIR),
        None => PathBuf::from(DATADOG_AGENT_CONF_DIR),
    }
}

/// Returns the log directory for the given common root, falling back to the FHS layout when unset.
fn log_dir_for(common_root: Option<&Path>) -> PathBuf {
    match common_root {
        Some(root) => root.join(COMMON_ROOT_LOG_SUBDIR),
        None => PathBuf::from(DATADOG_AGENT_LOG_DIR),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        path::{Path, PathBuf},
    };

    use super::{config_dir_for, log_dir_for, parse_common_root};

    #[test]
    fn common_root_is_unset_by_default() {
        assert_eq!(parse_common_root(None), None);
    }

    #[test]
    fn common_root_falls_back_to_the_default_root_when_set_but_empty() {
        assert_eq!(
            parse_common_root(Some(OsStr::new(""))),
            Some(PathBuf::from("/opt/datadog-agent"))
        );
    }

    #[test]
    fn common_root_uses_the_configured_value() {
        assert_eq!(
            parse_common_root(Some(OsStr::new("/mnt/datadog"))),
            Some(PathBuf::from("/mnt/datadog"))
        );
    }

    #[test]
    fn directories_use_the_fhs_layout_without_a_common_root() {
        assert_eq!(config_dir_for(None), PathBuf::from("/etc/datadog-agent"));
        assert_eq!(log_dir_for(None), PathBuf::from("/var/log/datadog"));
    }

    #[test]
    fn directories_are_derived_from_the_common_root() {
        let root = Path::new("/mnt/datadog");

        assert_eq!(config_dir_for(Some(root)), PathBuf::from("/mnt/datadog/etc"));
        assert_eq!(log_dir_for(Some(root)), PathBuf::from("/mnt/datadog/logs"));
    }
}
