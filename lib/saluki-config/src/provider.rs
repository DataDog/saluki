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

    pub fn from_yaml<P>(path: P) -> Result<Self, GenericError>
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

        Self::from_serialized(raw_yaml_value, metadata)
    }

    pub fn from_json<P>(path: P) -> Result<Self, GenericError>
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
    use std::io::Write as _;

    use figment::Figment;
    use serde_json::json;
    use tempfile::NamedTempFile;

    use super::*;

    fn write_temp_file(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("should create temp file");
        file.write_all(contents.as_bytes()).expect("should write temp file");
        file.flush().expect("should flush temp file");
        file
    }

    #[test]
    fn drop_nested_nulls_json_removes_null_leaves_recursively() {
        let mut value = json!({
            "keep": "yes",
            "drop": null,
            "nested": { "keep": 1, "drop": null },
            "list": [ { "keep": true, "drop": null } ],
        });

        drop_nested_nulls_json(&mut value);

        assert_eq!(
            value,
            json!({
                "keep": "yes",
                "nested": { "keep": 1 },
                "list": [ { "keep": true } ],
            })
        );
    }

    #[test]
    fn drop_nested_nulls_yaml_removes_null_leaves_recursively() {
        let mut value: YamlValue =
            serde_yaml::from_str("keep: kept\ndrop: null\nnested:\n  keep: 1\n  drop: null\n").unwrap();

        drop_nested_nulls_yaml(&mut value);

        let expected: YamlValue = serde_yaml::from_str("keep: kept\nnested:\n  keep: 1\n").unwrap();
        assert_eq!(value, expected);
    }

    #[test]
    fn from_yaml_loads_nested_values_and_drops_nulls() {
        let file = write_temp_file("proxy:\n  http: http://proxy.example.com\nempty:\nkept: value\n");

        let provider = ResolvedProvider::from_yaml(file.path()).expect("valid YAML should load");
        let figment = Figment::new().merge(provider);

        // A nested value keeps its nesting; nothing is copied to a flattened spelling.
        assert_eq!(
            figment.extract_inner::<String>("proxy.http").unwrap(),
            "http://proxy.example.com"
        );
        assert!(figment.find_value("proxy_http").is_err());
        // A non-null value is preserved.
        assert_eq!(figment.extract_inner::<String>("kept").unwrap(), "value");
        // A null value is dropped entirely.
        assert!(figment.find_value("empty").is_err());
    }

    #[test]
    fn from_json_loads_nested_values_and_drops_nested_nulls() {
        let file = write_temp_file(r#"{"outer": {"kept": 1, "empty": null}, "top": null}"#);

        let provider = ResolvedProvider::from_json(file.path()).expect("valid JSON should load");
        let figment = Figment::new().merge(provider);

        assert_eq!(figment.extract_inner::<i64>("outer.kept").unwrap(), 1);
        assert!(figment.find_value("outer.empty").is_err());
        assert!(figment.find_value("top").is_err());
    }

    #[test]
    fn from_yaml_empty_file_yields_empty_map() {
        let file = write_temp_file("");

        let provider = ResolvedProvider::from_yaml(file.path()).expect("empty YAML should normalize to an empty map");
        let figment = Figment::new().merge(provider);

        assert!(figment.find_value("anything").is_err());
    }

    #[test]
    fn from_yaml_returns_error_for_invalid_yaml() {
        let file = write_temp_file("foo: [unclosed");

        let result = ResolvedProvider::from_yaml(file.path());
        assert!(result.is_err(), "invalid YAML should fail to load");
    }

    #[test]
    fn from_json_returns_error_for_invalid_json() {
        let file = write_temp_file("{ not valid json ");

        let result = ResolvedProvider::from_json(file.path());
        assert!(result.is_err(), "invalid JSON should fail to load");
    }

    #[test]
    fn from_json_returns_error_for_non_object_root() {
        // A scalar root can't be represented as a configuration map, so `from_serialized` rejects it.
        let file = write_temp_file(r#""just a string""#);

        let result = ResolvedProvider::from_json(file.path());
        assert!(result.is_err(), "a non-object JSON root should fail to load");
    }
}
