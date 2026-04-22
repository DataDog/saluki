//! Datadog Agent configuration registry entries.

/// Proxy configuration keys.
pub mod proxy;

use std::sync::LazyLock;

use super::ConfigKey;

/// All recognized Datadog Agent configuration keys across every sub-system.
///
/// Iterate over this at runtime to identify unknown or unsupported keys in a loaded
/// configuration. As new config structs are registered, their sub-module's `ALL` slice is
/// appended here.
pub static ALL_KEYS: LazyLock<Vec<&'static ConfigKey>> = LazyLock::new(|| {
    let mut keys = Vec::new();
    keys.extend_from_slice(proxy::ALL);
    keys
});
