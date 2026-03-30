use std::path::Path;

use figment::{
    providers::Serialized,
    value::{Dict, Map},
    Error, Metadata, Profile, Provider,
};
use saluki_error::{ErrorContext as _, GenericError};
use serde::Serialize;
use serde_json::{Map as JsonMap, Value as JsonValue};
use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};

pub struct ResolvedProvider {
    data: Map<Profile, Dict>,
    metadata: Metadata,
}

impl ResolvedProvider {
    fn from_serialized<T: Serialize>(data: T, metadata: Metadata) -> Result<Self, GenericError> {
        let provider = Serialized::defaults(data);
        let data = provider.data().error_context(
            "Failed to deserialize configuration data or data in configuration file is not a map/object.",
        )?;

        Ok(Self { data, metadata })
    }

    pub fn from_yaml<P>(path: P, key_aliases: &[(&str, &str)]) -> Result<Self, GenericError>
    where
        P: AsRef<Path>,
    {
        let (file_data, metadata) = read_serialized_config_file(path.as_ref(), "YAML")?;

        let mut raw_yaml_value: YamlValue = serde_yaml::from_str(&file_data).with_error_context(|| {
            format!(
                "Failed to deserialize YAML configuration file '{}'.",
                path.as_ref().display()
            )
        })?;

        // Normalize the raw YAML data we got back.
        //
        // If the file is empty, we'll get a null value which we just normalize as an empty map to make `Serialized` happy.
        drop_nested_nulls_yaml(&mut raw_yaml_value);

        if raw_yaml_value.is_null() {
            raw_yaml_value = YamlValue::Mapping(YamlMapping::new());
        }

        apply_key_aliases_yaml(&mut raw_yaml_value, key_aliases);

        Self::from_serialized(raw_yaml_value, metadata)
    }

    pub fn from_json<P>(path: P, key_aliases: &[(&str, &str)]) -> Result<Self, GenericError>
    where
        P: AsRef<Path>,
    {
        let (file_data, metadata) = read_serialized_config_file(path.as_ref(), "JSON")?;

        let mut raw_json_value: JsonValue = serde_json::from_str(&file_data).with_error_context(|| {
            format!(
                "Failed to deserialize JSON configuration file '{}'.",
                path.as_ref().display()
            )
        })?;

        // Normalize the raw JSON data we got back.
        //
        // If the file is empty, we'll get a null value which we just normalize as an empty map to make `Serialized` happy.
        drop_nested_nulls_json(&mut raw_json_value);

        if raw_json_value.is_null() {
            raw_json_value = JsonValue::Object(JsonMap::new());
        }

        apply_key_aliases(&mut raw_json_value, key_aliases);

        Self::from_serialized(raw_json_value, metadata)
    }
}

impl Provider for ResolvedProvider {
    fn metadata(&self) -> Metadata {
        self.metadata.clone()
    }

    fn data(&self) -> Result<Map<Profile, Dict>, Error> {
        Ok(self.data.clone())
    }
}

fn read_serialized_config_file<P>(path: P, name: &'static str) -> Result<(String, Metadata), GenericError>
where
    P: AsRef<Path>,
{
    let file_data = std::fs::read_to_string(path.as_ref()).with_error_context(|| {
        format!(
            "Failed to read {} configuration file '{}'.",
            name,
            path.as_ref().display()
        )
    })?;

    let metadata = Metadata::from(format!("{} file", name), path.as_ref());

    Ok((file_data, metadata))
}

/// Adds a flat top-level key for each alias entry whose nested path exists in `value`.
///
/// For each `(from_path, to_key)` pair: if `from_path` (dot-separated) resolves to a value,
/// that value is written under `to_key` at the top level — but only if `to_key` is not already
/// present. This lets both `proxy: http: <url>` (YAML-nested) and `proxy_http: <url>` (flat)
/// produce the same canonical Figment key without dropping either representation.
fn apply_key_aliases(value: &mut JsonValue, aliases: &[(&str, &str)]) {
    for &(from_path, to_key) in aliases {
        if let Some(nested_val) = get_nested(value, from_path).cloned() {
            if let Some(obj) = value.as_object_mut() {
                obj.entry(to_key.to_string()).or_insert(nested_val);
            }
        }
    }
}

fn get_nested<'a>(value: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    path.split('.').try_fold(value, |v, key| v.get(key))
}

fn apply_key_aliases_yaml(value: &mut YamlValue, aliases: &[(&str, &str)]) {
    for &(from_path, to_key) in aliases {
        if let Some(nested_val) = get_nested_yaml(value, from_path).cloned() {
            if let YamlValue::Mapping(mapping) = value {
                let key = YamlValue::String(to_key.to_string());
                mapping.entry(key).or_insert(nested_val);
            }
        }
    }
}

fn get_nested_yaml<'a>(value: &'a YamlValue, path: &str) -> Option<&'a YamlValue> {
    path.split('.').try_fold(value, |v, key| {
        if let YamlValue::Mapping(m) = v {
            m.get(key)
        } else {
            None
        }
    })
}

fn drop_nested_nulls_yaml(value: &mut YamlValue) {
    match value {
        YamlValue::Sequence(items) => {
            for item in items {
                drop_nested_nulls_yaml(item);
            }
        }
        YamlValue::Mapping(mapping) => {
            let mut to_drop = Vec::new();

            for (entry_key, entry_value) in mapping.iter_mut() {
                if entry_value.is_null() {
                    to_drop.push(entry_key.clone());
                } else {
                    drop_nested_nulls_yaml(entry_value);
                }
            }

            for key in to_drop {
                mapping.remove(key);
            }
        }

        // This isn't a type we need to interact with so just ignore it.
        _ => {}
    }
}

fn drop_nested_nulls_json(value: &mut JsonValue) {
    match value {
        JsonValue::Array(items) => {
            for item in items {
                drop_nested_nulls_json(item);
            }
        }
        JsonValue::Object(mapping) => {
            let mut to_drop = Vec::new();

            for (entry_key, entry_value) in mapping.iter_mut() {
                if entry_value.is_null() {
                    to_drop.push(entry_key.clone());
                } else {
                    drop_nested_nulls_json(entry_value);
                }
            }

            for key in to_drop {
                mapping.remove(&key);
            }
        }

        // This isn't a type we need to interact with so just ignore it.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn alias_nested_path_to_flat_key() {
        let mut value = json!({ "proxy": { "http": "http://proxy.example.com" } });
        apply_key_aliases(&mut value, &[("proxy.http", "proxy_http")]);
        assert_eq!(value["proxy_http"], "http://proxy.example.com");
    }

    #[test]
    fn alias_does_not_overwrite_existing_flat_key() {
        let mut value = json!({ "proxy": { "http": "from-nested" }, "proxy_http": "from-flat" });
        apply_key_aliases(&mut value, &[("proxy.http", "proxy_http")]);
        assert_eq!(value["proxy_http"], "from-flat");
    }

    #[test]
    fn alias_missing_nested_path_adds_nothing() {
        let mut value = json!({ "other_key": "value" });
        apply_key_aliases(&mut value, &[("proxy.http", "proxy_http")]);
        assert!(value.get("proxy_http").is_none());
    }

    #[test]
    fn alias_multiple_entries() {
        let mut value = json!({ "proxy": { "http": "http://h", "https": "http://s" } });
        apply_key_aliases(
            &mut value,
            &[("proxy.http", "proxy_http"), ("proxy.https", "proxy_https")],
        );
        assert_eq!(value["proxy_http"], "http://h");
        assert_eq!(value["proxy_https"], "http://s");
    }

    #[test]
    fn get_nested_single_level() {
        let value = json!({ "key": "val" });
        assert_eq!(get_nested(&value, "key"), Some(&json!("val")));
    }

    #[test]
    fn get_nested_multi_level() {
        let value = json!({ "a": { "b": { "c": 42 } } });
        assert_eq!(get_nested(&value, "a.b.c"), Some(&json!(42)));
    }

    #[test]
    fn get_nested_missing_returns_none() {
        let value = json!({ "a": { "b": "val" } });
        assert!(get_nested(&value, "a.x").is_none());
        assert!(get_nested(&value, "z").is_none());
    }

    #[test]
    fn yaml_alias_nested_path_to_flat_key() {
        let mut value: YamlValue = serde_yaml::from_str("proxy:\n  http: http://proxy.example.com").unwrap();
        apply_key_aliases_yaml(&mut value, &[("proxy.http", "proxy_http")]);
        assert_eq!(value["proxy_http"].as_str(), Some("http://proxy.example.com"));
    }

    #[test]
    fn yaml_alias_does_not_overwrite_existing_flat_key() {
        let mut value: YamlValue =
            serde_yaml::from_str("proxy:\n  http: from-nested\nproxy_http: from-flat").unwrap();
        apply_key_aliases_yaml(&mut value, &[("proxy.http", "proxy_http")]);
        assert_eq!(value["proxy_http"].as_str(), Some("from-flat"));
    }

    #[test]
    fn yaml_alias_missing_nested_path_adds_nothing() {
        let mut value: YamlValue = serde_yaml::from_str("other_key: value").unwrap();
        apply_key_aliases_yaml(&mut value, &[("proxy.http", "proxy_http")]);
        assert!(value.get("proxy_http").is_none());
    }

    #[test]
    fn yaml_get_nested_multi_level() {
        let value: YamlValue = serde_yaml::from_str("a:\n  b:\n    c: 42").unwrap();
        assert_eq!(get_nested_yaml(&value, "a.b.c").and_then(|v| v.as_i64()), Some(42));
    }

    #[test]
    fn yaml_get_nested_missing_returns_none() {
        let value: YamlValue = serde_yaml::from_str("a:\n  b: val").unwrap();
        assert!(get_nested_yaml(&value, "a.x").is_none());
        assert!(get_nested_yaml(&value, "z").is_none());
    }
}
