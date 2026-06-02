use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use windows_registry::{Key, LOCAL_MACHINE};

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
    read_config_root_from_registry_key(
        LOCAL_MACHINE,
        DATADOG_AGENT_REGISTRY_SUBKEY,
        DATADOG_AGENT_CONFIG_ROOT_VALUE,
    )
}

fn read_config_root_from_registry_key(root: &Key, subkey: &str, value_name: &str) -> Option<PathBuf> {
    root.open(subkey)
        .and_then(|key| key.get_string(value_name))
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use windows_registry::CURRENT_USER;

    use super::read_config_root_from_registry_key;

    const TEST_VALUE_NAME: &str = "ConfigRoot";

    struct TestRegistryKey {
        subkey: String,
    }

    impl TestRegistryKey {
        fn create() -> Self {
            let subkey = format!(
                r"SOFTWARE\Datadog\SalukiPlatformTests\{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            );
            CURRENT_USER
                .create(&subkey)
                .expect("temporary registry key should be created");

            Self { subkey }
        }

        fn set_string_value(&self, name: &str, value: &str) {
            let key = CURRENT_USER
                .create(&self.subkey)
                .expect("temporary registry key should open for writes");
            key.set_string(name, value)
                .expect("registry string value should be written");
        }
    }

    impl Drop for TestRegistryKey {
        fn drop(&mut self) {
            let _ = CURRENT_USER.remove_tree(&self.subkey);
        }
    }

    #[test]
    fn read_config_root_from_registry_key_reads_config_root_path() {
        let key = TestRegistryKey::create();
        let expected = r"C:\ProgramData\Datadog Test 🐕";
        key.set_string_value(TEST_VALUE_NAME, expected);

        let actual =
            read_config_root_from_registry_key(CURRENT_USER, &key.subkey, TEST_VALUE_NAME).expect("value should exist");

        assert_eq!(actual, PathBuf::from(expected));
    }

    #[test]
    fn read_config_root_from_registry_key_returns_none_for_missing_and_empty_values() {
        let key = TestRegistryKey::create();

        assert!(read_config_root_from_registry_key(CURRENT_USER, &key.subkey, TEST_VALUE_NAME).is_none());

        key.set_string_value(TEST_VALUE_NAME, "");

        assert!(read_config_root_from_registry_key(CURRENT_USER, &key.subkey, TEST_VALUE_NAME).is_none());
    }
}
