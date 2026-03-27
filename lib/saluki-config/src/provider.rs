use std::path::Path;

use facet_value::Value;
use saluki_error::{ErrorContext as _, GenericError};

/// Loads a configuration file and returns it as a `Value`.
pub fn load_yaml<P>(path: P) -> Result<Value, GenericError>
where
    P: AsRef<Path>,
{
    let file_data = read_config_file(path.as_ref(), "YAML")?;

    let mut value: Value = facet_yaml::from_str(&file_data).with_error_context(|| {
        format!(
            "Failed to deserialize YAML configuration file '{}'.",
            path.as_ref().display()
        )
    })?;

    // Normalize the raw data: strip nested nulls and convert a bare null root to an empty object.
    drop_nested_nulls(&mut value);

    if value.is_null() {
        value = Value::from(facet_value::VObject::new());
    }

    Ok(value)
}

/// Loads a JSON configuration file and returns it as a `Value`.
pub fn load_json<P>(path: P) -> Result<Value, GenericError>
where
    P: AsRef<Path>,
{
    let file_data = read_config_file(path.as_ref(), "JSON")?;

    let mut value: Value = facet_json::from_str(&file_data).with_error_context(|| {
        format!(
            "Failed to deserialize JSON configuration file '{}'.",
            path.as_ref().display()
        )
    })?;

    // Normalize the raw data: strip nested nulls and convert a bare null root to an empty object.
    drop_nested_nulls(&mut value);

    if value.is_null() {
        value = Value::from(facet_value::VObject::new());
    }

    Ok(value)
}

fn read_config_file<P>(path: P, name: &'static str) -> Result<String, GenericError>
where
    P: AsRef<Path>,
{
    std::fs::read_to_string(path.as_ref()).with_error_context(|| {
        format!(
            "Failed to read {} configuration file '{}'.",
            name,
            path.as_ref().display()
        )
    })
}

/// Recursively removes null values from objects and arrays.
fn drop_nested_nulls(value: &mut Value) {
    if let Some(arr) = value.as_array_mut() {
        for item in arr.iter_mut() {
            drop_nested_nulls(item);
        }
    } else if let Some(obj) = value.as_object_mut() {
        let keys_to_drop: Vec<String> = obj
            .iter()
            .filter(|(_, v)| v.is_null())
            .map(|(k, _)| k.as_str().to_string())
            .collect();

        for key in keys_to_drop {
            obj.remove(&key);
        }

        for (_, v) in obj.iter_mut() {
            drop_nested_nulls(v);
        }
    }
}
