use std::path::Path;

use facet_value::Value;
use saluki_error::{ErrorContext as _, GenericError};

/// Loads a YAML configuration file and returns it as a `Value`.
///
/// If `key_aliases` is non-empty, each `(nested_path, flat_key)` pair is applied after loading:
/// when `nested_path` (dot-separated) resolves to a value, that value is also written under
/// `flat_key` at the top level — but only if `flat_key` is not already explicitly set.
pub fn load_yaml<P>(path: P, key_aliases: &[(&str, &str)]) -> Result<Value, GenericError>
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

    apply_key_aliases(&mut value, key_aliases);

    Ok(value)
}

/// Loads a JSON configuration file and returns it as a `Value`.
///
/// If `key_aliases` is non-empty, each `(nested_path, flat_key)` pair is applied after loading:
/// when `nested_path` (dot-separated) resolves to a value, that value is also written under
/// `flat_key` at the top level — but only if `flat_key` is not already explicitly set.
pub fn load_json<P>(path: P, key_aliases: &[(&str, &str)]) -> Result<Value, GenericError>
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

    apply_key_aliases(&mut value, key_aliases);

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

/// Adds a flat top-level key for each alias entry whose nested path exists in `value`.
///
/// For each `(from_path, to_key)` pair: if `from_path` (dot-separated) resolves to a value,
/// that value is written under `to_key` at the top level — but only if `to_key` is not already
/// present. This lets both `proxy: http: <url>` (YAML-nested) and `proxy_http: <url>` (flat)
/// produce the same canonical key without dropping either representation.
fn apply_key_aliases(value: &mut Value, aliases: &[(&str, &str)]) {
    for &(from_path, to_key) in aliases {
        if let Some(nested_val) = get_nested(value, from_path).cloned() {
            if let Some(obj) = value.as_object_mut() {
                if !obj.contains_key(to_key) {
                    obj.insert(to_key, nested_val);
                }
            }
        }
    }
}

fn get_nested<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |v, key| v.as_object()?.get(key))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn json_value(s: &str) -> Value {
        facet_json::from_str(s).unwrap()
    }

    fn yaml_value(s: &str) -> Value {
        facet_yaml::from_str(s).unwrap()
    }

    #[test]
    fn alias_nested_path_to_flat_key() {
        let mut value = json_value(r#"{ "proxy": { "http": "http://proxy.example.com" } }"#);
        apply_key_aliases(&mut value, &[("proxy.http", "proxy_http")]);
        assert_eq!(
            value.as_object().unwrap().get("proxy_http").and_then(|v| v.as_string()).map(|s| s.as_str()),
            Some("http://proxy.example.com")
        );
    }

    #[test]
    fn alias_does_not_overwrite_existing_flat_key() {
        let mut value = json_value(r#"{ "proxy": { "http": "from-nested" }, "proxy_http": "from-flat" }"#);
        apply_key_aliases(&mut value, &[("proxy.http", "proxy_http")]);
        assert_eq!(
            value.as_object().unwrap().get("proxy_http").and_then(|v| v.as_string()).map(|s| s.as_str()),
            Some("from-flat")
        );
    }

    #[test]
    fn alias_missing_nested_path_adds_nothing() {
        let mut value = json_value(r#"{ "other_key": "value" }"#);
        apply_key_aliases(&mut value, &[("proxy.http", "proxy_http")]);
        assert!(value.as_object().unwrap().get("proxy_http").is_none());
    }

    #[test]
    fn alias_multiple_entries() {
        let mut value = json_value(r#"{ "proxy": { "http": "http://h", "https": "http://s" } }"#);
        apply_key_aliases(
            &mut value,
            &[("proxy.http", "proxy_http"), ("proxy.https", "proxy_https")],
        );
        assert_eq!(
            value.as_object().unwrap().get("proxy_http").and_then(|v| v.as_string()).map(|s| s.as_str()),
            Some("http://h")
        );
        assert_eq!(
            value.as_object().unwrap().get("proxy_https").and_then(|v| v.as_string()).map(|s| s.as_str()),
            Some("http://s")
        );
    }

    #[test]
    fn get_nested_single_level() {
        let value = json_value(r#"{ "key": "val" }"#);
        let nested = get_nested(&value, "key");
        assert!(nested.is_some());
        assert_eq!(nested.unwrap().as_string().map(|s| s.as_str()), Some("val"));
    }

    #[test]
    fn get_nested_multi_level() {
        let value = json_value(r#"{ "a": { "b": { "c": 42 } } }"#);
        let nested = get_nested(&value, "a.b.c");
        assert!(nested.is_some());
    }

    #[test]
    fn get_nested_missing_returns_none() {
        let value = json_value(r#"{ "a": { "b": "val" } }"#);
        assert!(get_nested(&value, "a.x").is_none());
        assert!(get_nested(&value, "z").is_none());
    }

    #[test]
    fn yaml_alias_nested_path_to_flat_key() {
        let mut value = yaml_value("proxy:\n  http: http://proxy.example.com");
        apply_key_aliases(&mut value, &[("proxy.http", "proxy_http")]);
        assert_eq!(
            value.as_object().unwrap().get("proxy_http").and_then(|v| v.as_string()).map(|s| s.as_str()),
            Some("http://proxy.example.com")
        );
    }

    #[test]
    fn yaml_alias_does_not_overwrite_existing_flat_key() {
        let mut value = yaml_value("proxy:\n  http: from-nested\nproxy_http: from-flat");
        apply_key_aliases(&mut value, &[("proxy.http", "proxy_http")]);
        assert_eq!(
            value.as_object().unwrap().get("proxy_http").and_then(|v| v.as_string()).map(|s| s.as_str()),
            Some("from-flat")
        );
    }

    #[test]
    fn yaml_alias_missing_nested_path_adds_nothing() {
        let mut value = yaml_value("other_key: value");
        apply_key_aliases(&mut value, &[("proxy.http", "proxy_http")]);
        assert!(value.as_object().unwrap().get("proxy_http").is_none());
    }

    #[test]
    fn yaml_get_nested_multi_level() {
        let value = yaml_value("a:\n  b:\n    c: 42");
        assert!(get_nested(&value, "a.b.c").is_some());
    }

    #[test]
    fn yaml_get_nested_missing_returns_none() {
        let value = yaml_value("a:\n  b: val");
        assert!(get_nested(&value, "a.x").is_none());
        assert!(get_nested(&value, "z").is_none());
    }
}
