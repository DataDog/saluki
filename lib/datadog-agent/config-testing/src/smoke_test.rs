use figment::Provider;
use saluki_config::{ConfigurationLoader, GenericConfiguration};
use serde::Serialize;
use serde_json::json;

use crate::config_registry::{SalukiAnnotation, ValueType, SUPPORTED_ANNOTATIONS};

/// Test value injected for `String` keys.
pub const TEST_STRING_VALUE: &str = "http://smoke-proxy.example.com:3128";
/// Test value injected for `Bool` keys.
pub const TEST_BOOL_VALUE: bool = true;
/// Test value injected for `StringList` keys.
pub const TEST_STRING_LIST_VALUE: &[&str] = &["smoke-host-1.example.com", "smoke-host-2.example.com"];

fn test_json_value(value_type: ValueType) -> serde_json::Value {
    match value_type {
        ValueType::String => json!(TEST_STRING_VALUE),
        ValueType::Bool => json!(TEST_BOOL_VALUE),
        ValueType::StringList => json!(TEST_STRING_LIST_VALUE),
        ValueType::Integer => json!(42i64),
        ValueType::Float => json!(1.5f64),
    }
}

fn effective_test_value(annotation: &SalukiAnnotation) -> serde_json::Value {
    let v = test_json_value(annotation.value_type());
    if let Some(default_raw) = annotation.schema.default {
        if let Ok(default_val) = serde_json::from_str::<serde_json::Value>(default_raw) {
            if v == default_val {
                return match annotation.value_type() {
                    ValueType::Bool => json!(!default_val.as_bool().unwrap_or(false)),
                    _ => v,
                };
            }
        }
    }
    v
}

fn json_value_to_env_string(value: &serde_json::Value, value_type: ValueType) -> String {
    match value_type {
        ValueType::Bool => value
            .as_bool()
            .map(|b| b.to_string())
            .unwrap_or_else(|| "true".to_string()),
        ValueType::Integer => value
            .as_i64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "42".to_string()),
        ValueType::Float => value
            .as_f64()
            .map(|f| f.to_string())
            .unwrap_or_else(|| "1.5".to_string()),
        ValueType::String => value.as_str().unwrap_or(TEST_STRING_VALUE).to_string(),
        ValueType::StringList => value
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| TEST_STRING_LIST_VALUE.join(" ")),
    }
}

fn collect_unchanged_leaves(
    full: &serde_json::Value, default: &serde_json::Value, path: &str, unchanged: &mut Vec<String>,
) {
    match (full, default) {
        (serde_json::Value::Object(f), serde_json::Value::Object(d)) => {
            for (key, full_val) in f {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path, key)
                };
                let def_val = d.get(key).unwrap_or(&serde_json::Value::Null);
                collect_unchanged_leaves(full_val, def_val, &child_path, unchanged);
            }
        }
        (full_val, def_val) => {
            if full_val == def_val {
                unchanged.push(path.to_string());
            }
        }
    }
}

fn yaml_path_to_json(yaml_path: &str, value: serde_json::Value) -> serde_json::Value {
    let mut root = json!({});
    saluki_config::upsert(&mut root, yaml_path, value);
    root
}

fn merge_over_base(base: &serde_json::Value, overlay: serde_json::Value) -> serde_json::Value {
    let mut merged = base.clone();
    if let (Some(base_obj), Some(overlay_obj)) = (merged.as_object_mut(), overlay.as_object()) {
        for (k, v) in overlay_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
    merged
}

fn dd_env_var_to_test_key(env_var: &str) -> &str {
    env_var.strip_prefix("DD_").unwrap_or(env_var)
}

async fn make_config_from_file<P, F>(
    file_values: serde_json::Value, key_aliases: &'static [(&'static str, &'static str)], provider_factory: F,
) -> GenericConfiguration
where
    P: Provider + Send + Sync + 'static,
    F: FnOnce() -> P,
{
    let (cfg, _) = ConfigurationLoader::for_tests_with_provider_factory(
        Some(file_values),
        None,
        false,
        key_aliases,
        provider_factory,
    )
    .await;
    cfg
}

async fn make_config_from_env<P, F>(
    base_file_values: &serde_json::Value, env_vars: &[(String, String)],
    key_aliases: &'static [(&'static str, &'static str)], provider_factory: F,
) -> GenericConfiguration
where
    P: Provider + Send + Sync + 'static,
    F: FnOnce() -> P,
{
    let (cfg, _) = ConfigurationLoader::for_tests_with_provider_factory(
        Some(base_file_values.clone()),
        Some(env_vars),
        false,
        key_aliases,
        provider_factory,
    )
    .await;
    cfg
}

/// Runs smoke tests for all annotations registered to `struct_name` against a deserialized config struct `T`.
///
/// Verifies three properties:
///
/// **Supported keys**: loading the struct with the test value set via the annotation's `yaml_path`
/// and via each of its effective env vars must all produce identical structs, and each must differ
/// from the default (empty-config) struct.
///
/// **Unsupported keys**: loading the struct with that key set must produce a struct identical to the
/// default struct.
///
/// **Full field coverage**: loading the struct with all supported keys set simultaneously must
/// produce a struct where every serialized leaf field differs from the default.
///
/// `key_aliases` and `provider_factory` configure the test config loader. Pass the same aliases and
/// remapper factory used in production config loading.
pub async fn run_config_smoke_tests<T, Factory, P, PF>(
    struct_name: &'static str, non_config_fields: &[&str], base_config: serde_json::Value, config_factory: Factory,
    key_aliases: &'static [(&'static str, &'static str)], provider_factory: PF,
) where
    T: PartialEq + Serialize,
    Factory: Fn(GenericConfiguration) -> T,
    P: Provider + Send + Sync + 'static,
    PF: Fn() -> P,
{
    let keys: Vec<&'static SalukiAnnotation> = SUPPORTED_ANNOTATIONS
        .iter()
        .copied()
        .filter(|a| a.used_by.contains(&struct_name))
        .collect();

    let default_struct =
        config_factory(make_config_from_file(base_config.clone(), key_aliases, &provider_factory).await);
    let mut failures: Vec<String> = Vec::new();

    for annotation in &keys {
        let canonical_path = annotation.yaml_path();
        let injected_value = match annotation.test_json {
            Some(raw) => serde_json::from_str(raw).expect("test_json is not valid JSON"),
            None => effective_test_value(annotation),
        };
        let reference = config_factory(
            make_config_from_file(
                merge_over_base(&base_config, yaml_path_to_json(canonical_path, injected_value.clone())),
                key_aliases,
                &provider_factory,
            )
            .await,
        );

        if reference == default_struct {
            failures.push(format!(
                "yaml_path '{}': struct did not change from its default—\
                 is the test value the same as the default, or is the key not wired up?",
                canonical_path,
            ));
            continue;
        }

        for yaml_path in annotation.additional_yaml_paths {
            let from_path = config_factory(
                make_config_from_file(
                    merge_over_base(&base_config, yaml_path_to_json(yaml_path, injected_value.clone())),
                    key_aliases,
                    &provider_factory,
                )
                .await,
            );
            if from_path != reference {
                failures.push(format!(
                    "yaml_path '{}' produced a different struct than canonical yaml_path '{}'",
                    yaml_path, canonical_path,
                ));
            }
        }

        for env_var in annotation.effective_env_vars() {
            let env_pairs = [(
                dd_env_var_to_test_key(env_var).to_string(),
                json_value_to_env_string(&injected_value, annotation.value_type()),
            )];
            let from_env =
                config_factory(make_config_from_env(&base_config, &env_pairs, key_aliases, &provider_factory).await);
            if from_env != reference {
                failures.push(format!(
                    "env var '{}' produced a different struct than yaml_path '{}'",
                    env_var, canonical_path,
                ));
            }
        }
    }

    for annotation in SUPPORTED_ANNOTATIONS
        .iter()
        .filter(|a| !a.used_by.contains(&struct_name))
    {
        for yaml_path in annotation.all_yaml_paths() {
            let with_foreign = config_factory(
                make_config_from_file(
                    merge_over_base(
                        &base_config,
                        yaml_path_to_json(yaml_path, test_json_value(annotation.value_type())),
                    ),
                    key_aliases,
                    &provider_factory,
                )
                .await,
            );
            if with_foreign != default_struct {
                failures.push(format!(
                    "yaml_path '{}' is not registered for '{}' but unexpectedly changed the struct",
                    yaml_path, struct_name,
                ));
            }
        }
    }

    let mut all_vals = base_config.clone();
    for annotation in keys {
        let val = match annotation.test_json {
            Some(raw) => serde_json::from_str(raw).expect("test_json is not valid JSON"),
            None => effective_test_value(annotation),
        };
        saluki_config::upsert(&mut all_vals, annotation.yaml_path(), val);
    }
    let all_keys_struct = config_factory(make_config_from_file(all_vals, key_aliases, &provider_factory).await);
    let full_map = serde_json::to_value(&all_keys_struct).expect("failed to serialize struct with all keys set");
    let default_map = serde_json::to_value(&default_struct).expect("failed to serialize default struct");
    let mut unchanged = Vec::new();
    collect_unchanged_leaves(&full_map, &default_map, "", &mut unchanged);
    unchanged.retain(|path| !non_config_fields.contains(&path.as_str()));
    if !unchanged.is_empty() {
        failures.push(format!(
            "{} serialized field(s) are never changed by any registered config key: [{}]\n  \
             Fix: add a SalukiAnnotation for each field and include '{}' in its used_by list.\n  \
             Fix: if a field is intentionally not config-driven (for example, injected at runtime), \
             add its serialized name to the `non_config_fields` slice in this test call.",
            unchanged.len(),
            unchanged.join(", "),
            struct_name,
        ));
    }

    if !failures.is_empty() {
        panic!(
            "config smoke tests for '{}' failed with {} error(s):\n\n{}",
            struct_name,
            failures.len(),
            failures
                .iter()
                .enumerate()
                .map(|(i, msg)| format!("  [{}] {}", i + 1, msg))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }
}
