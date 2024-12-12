use std::path::Path;

use figment::{
    providers::Serialized,
    value::{Dict, Map},
    Error, Metadata, Profile, Provider,
};
use saluki_error::{ErrorContext as _, GenericError};
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;

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

        drop_nested_nulls_yaml(&mut raw_yaml_value);

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

        drop_nested_nulls_json(&mut raw_json_value);

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
