//! Defines the event type for configuration changes.

use serde_json::Value as JsonValue;

use crate::upsert;

/// An event that occurs when the configuration changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChangeEvent {
    /// The key that changed.
    pub key: String,
    /// The previous value, if any.
    pub old_value: Option<JsonValue>,
    /// The new value.
    pub new_value: Option<JsonValue>,
}

/// Whether a configuration setting was asked for, or merely defaulted.
///
/// A configuration producer generally publishes every setting it knows about, including settings
/// nobody configured, so a value on its own cannot answer "did anyone ask for this?". Consumers that
/// treat an unconfigured setting differently from a configured one need this distinction; consumers
/// that only want effective values can ignore it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provenance {
    /// The value came from a real input: a configuration file, an environment variable, remote
    /// configuration, and so on.
    Explicit,
    /// The value is the producer's own default, standing in for a setting nobody configured.
    Default,
}

/// A single configuration setting, as published by a configuration producer.
///
/// The key is in the producer's own flat, possibly-dotted form, and the value is that key's entire
/// value: a map-valued setting arrives as one setting, not one setting per entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigSetting {
    /// The key being set.
    pub key: String,
    /// The value of the key.
    pub value: JsonValue,
    /// Where the value came from.
    pub provenance: Provenance,
}

impl ConfigSetting {
    /// Creates a new `ConfigSetting`.
    pub fn new(key: impl Into<String>, value: JsonValue, provenance: Provenance) -> Self {
        Self {
            key: key.into(),
            value,
            provenance,
        }
    }

    /// Creates a new `ConfigSetting` that an input explicitly asked for.
    pub fn explicit(key: impl Into<String>, value: JsonValue) -> Self {
        Self::new(key, value, Provenance::Explicit)
    }
}

/// An update message for the dynamic configuration state, sent from the config stream to the updater task.
#[derive(Clone, Debug)]
pub enum ConfigUpdate {
    /// A complete snapshot of the configuration.
    ///
    /// The existing state should be replaced.
    Snapshot(Vec<ConfigSetting>),
    /// A partial update for a single setting.
    ///
    /// This should be merged into the existing state.
    Partial(ConfigSetting),
}

impl ConfigUpdate {
    /// Creates a snapshot update from the given settings.
    pub fn snapshot(settings: impl IntoIterator<Item = ConfigSetting>) -> Self {
        Self::Snapshot(settings.into_iter().collect())
    }
}

/// Builds the nested state tree described by `settings`.
///
/// Dotted keys expand into nested objects, and each value is inserted whole, so an object-valued
/// setting keeps entry keys that themselves contain dots (intake URLs, for example) intact.
pub fn settings_to_state(settings: &[ConfigSetting]) -> JsonValue {
    let mut state = JsonValue::Object(serde_json::Map::new());
    for setting in settings {
        upsert(&mut state, &setting.key, setting.value.clone());
    }

    state
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{settings_to_state, ConfigSetting, Provenance};

    #[test]
    fn settings_expand_dotted_keys_into_nested_objects() {
        let settings = [
            ConfigSetting::explicit("dogstatsd_port", json!(8125)),
            ConfigSetting::explicit("otlp_config.traces.enabled", json!(true)),
        ];

        assert_eq!(
            settings_to_state(&settings),
            json!({ "dogstatsd_port": 8125, "otlp_config": { "traces": { "enabled": true } } })
        );
    }

    #[test]
    fn object_valued_settings_keep_dotted_entry_keys_intact() {
        let settings = [ConfigSetting::explicit(
            "additional_endpoints",
            json!({ "https://app.datadoghq.eu": ["deadbeef"] }),
        )];

        assert_eq!(
            settings_to_state(&settings),
            json!({ "additional_endpoints": { "https://app.datadoghq.eu": ["deadbeef"] } })
        );
    }

    #[test]
    fn later_settings_win_over_earlier_ones() {
        let settings = [
            ConfigSetting::new("dd_url", json!("https://app.datadoghq.com"), Provenance::Default),
            ConfigSetting::explicit("dd_url", json!("https://app.datadoghq.eu")),
        ];

        assert_eq!(
            settings_to_state(&settings),
            json!({ "dd_url": "https://app.datadoghq.eu" })
        );
    }
}
