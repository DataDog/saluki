//! Converts the Agent's config-stream wire types (`datadog_protos::agent`) into `saluki_config`'s
//! dynamic [`ConfigSetting`] values.
//!
//! These conversions live here, rather than in `agent-data-plane`, so that library tests (in
//! particular corpus replay tests) can build the exact `ConfigSnapshot`/`ConfigUpdate` events the
//! Agent would send and check what they turn into, without depending on the binary crate.

use datadog_protos::agent::{config_event, ConfigEvent, ConfigSetting as AgentConfigSetting, ConfigSnapshot};
use prost_types::value::Kind;
use saluki_config::dynamic::{ConfigSetting, ConfigUpdate, Provenance};
use serde_json::{Map, Value};
use tracing::error;

/// Sources that indicate the Agent supplied the value rather than an operator.
pub const AGENT_DEFAULT_SOURCE: &str = "default";
/// A value that was not set by the user nor does the schema define default value for.
pub const AGENT_DECLARED_ONLY_SOURCE: &str = "schema";
/// The sources that mark a setting as an Agent-supplied default rather than an explicit value.
pub const AGENT_UNSET_SOURCES: [&str; 2] = [AGENT_DEFAULT_SOURCE, AGENT_DECLARED_ONLY_SOURCE];

/// Converts a setting from the Agent's RPC wire protocol to our `ConfigSetting` type.
pub fn setting_to_config_setting(setting: &AgentConfigSetting) -> ConfigSetting {
    let provenance = if AGENT_UNSET_SOURCES.contains(&setting.source.as_str()) {
        Provenance::Default
    } else {
        Provenance::Explicit
    };

    ConfigSetting::new(
        setting.key.clone(),
        proto_value_to_serde_value(&setting.value),
        provenance,
    )
}

/// Converts a `ConfigSnapshot` into the settings it carries.
pub fn snapshot_to_settings(snapshot: &ConfigSnapshot) -> Vec<ConfigSetting> {
    snapshot.settings.iter().map(setting_to_config_setting).collect()
}

/// Converts one event from the Agent's config stream into the update it carries.
///
/// A snapshot event becomes [`ConfigUpdate::Snapshot`] and an update event becomes
/// [`ConfigUpdate::Partial`]. Returns `None` for an update event that carries no setting, and logs an
/// error and returns `None` for an event with no data at all.
pub fn config_event_to_update(event: ConfigEvent) -> Option<ConfigUpdate> {
    match event.event {
        Some(config_event::Event::Snapshot(snapshot)) => Some(ConfigUpdate::Snapshot(snapshot_to_settings(&snapshot))),
        Some(config_event::Event::Update(update)) => update
            .setting
            .as_ref()
            .map(|setting| ConfigUpdate::Partial(setting_to_config_setting(setting))),
        None => {
            error!("Received a configuration update event with no data.");
            None
        }
    }
}

/// Recursively converts a `google::protobuf::Value` into a `serde_json::Value`.
pub fn proto_value_to_serde_value(proto_val: &Option<prost_types::Value>) -> Value {
    let Some(kind) = proto_val.as_ref().and_then(|v| v.kind.as_ref()) else {
        return Value::Null;
    };

    match kind {
        Kind::NullValue(_) => Value::Null,
        Kind::NumberValue(n) => {
            if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                Value::from(*n as i64)
            } else {
                Value::from(*n)
            }
        }
        Kind::StringValue(s) => Value::String(s.clone()),
        Kind::BoolValue(b) => Value::Bool(*b),
        Kind::StructValue(s) => {
            let json_map: Map<String, Value> = s
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), proto_value_to_serde_value(&Some(v.clone()))))
                .collect();
            Value::Object(json_map)
        }
        Kind::ListValue(l) => {
            let json_list: Vec<Value> = l
                .values
                .iter()
                .map(|v| proto_value_to_serde_value(&Some(v.clone())))
                .collect();
            Value::Array(json_list)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_setting(source: &str, key: &str, value: &str) -> AgentConfigSetting {
        AgentConfigSetting {
            source: source.to_string(),
            key: key.to_string(),
            value: Some(prost_types::Value {
                kind: Some(Kind::StringValue(value.to_string())),
            }),
        }
    }

    #[test]
    fn an_agent_default_is_marked_as_a_default() {
        let setting = setting_to_config_setting(&agent_setting(
            AGENT_DEFAULT_SOURCE,
            "dd_url",
            "https://app.datadoghq.com",
        ));

        assert_eq!(setting.key, "dd_url");
        assert_eq!(setting.value, Value::from("https://app.datadoghq.com"));
        assert_eq!(setting.provenance, Provenance::Default);
    }

    #[test]
    fn a_schema_setting_is_marked_as_a_default() {
        let setting = setting_to_config_setting(&AgentConfigSetting {
            source: "schema".to_string(),
            key: "api_key".to_string(),
            value: None,
        });

        assert_eq!(setting.value, Value::Null);
        assert_eq!(setting.provenance, Provenance::Default);
    }

    #[test]
    fn null_values_are_preserved_with_their_provenance() {
        for source in [AGENT_DEFAULT_SOURCE, "schema", "file", "remote-config"] {
            let setting = setting_to_config_setting(&AgentConfigSetting {
                source: source.to_string(),
                key: "api_key".to_string(),
                value: Some(prost_types::Value {
                    kind: Some(Kind::NullValue(0)),
                }),
            });

            let expected_provenance = if [AGENT_DEFAULT_SOURCE, "schema"].contains(&source) {
                Provenance::Default
            } else {
                Provenance::Explicit
            };
            assert_eq!(setting.value, Value::Null);
            assert_eq!(setting.provenance, expected_provenance, "source {source}");
        }
    }

    #[test]
    fn an_empty_string_value_is_kept_with_its_provenance() {
        // An empty string is still a value; provenance comes from its source, not its content.
        for (source, provenance) in [
            ("file", Provenance::Explicit),
            ("default", Provenance::Default),
            ("schema", Provenance::Default),
        ] {
            let setting = setting_to_config_setting(&agent_setting(source, "site", ""));

            assert_eq!(setting.value, Value::from(""));
            assert_eq!(setting.provenance, provenance);
        }
    }

    #[test]
    fn operator_supplied_sources_are_marked_as_explicit() {
        // Unknown sources are treated as explicit inputs rather than defaults.
        for source in [
            "file",
            "environment-variable",
            "remote-config",
            "cli",
            "source-from-the-future",
        ] {
            let setting = setting_to_config_setting(&agent_setting(source, "dd_url", "https://app.datadoghq.eu"));

            assert_eq!(
                setting.provenance,
                Provenance::Explicit,
                "source {source} should be explicit"
            );
        }
    }

    #[test]
    fn snapshot_settings_keep_order_values_and_provenance() {
        let snapshot = ConfigSnapshot {
            origin: "core-agent".to_string(),
            sequence_id: 1,
            settings: vec![
                agent_setting("file", "site", "datadoghq.eu"),
                agent_setting("default", "dd_url", "https://app.datadoghq.com"),
                agent_setting(AGENT_DECLARED_ONLY_SOURCE, "api_key", ""),
            ],
        };

        let settings = snapshot_to_settings(&snapshot);

        assert_eq!(
            settings,
            vec![
                ConfigSetting::explicit("site", Value::from("datadoghq.eu")),
                ConfigSetting::new("dd_url", Value::from("https://app.datadoghq.com"), Provenance::Default),
                ConfigSetting::new("api_key", Value::from(""), Provenance::Default),
            ]
        );
    }
}
