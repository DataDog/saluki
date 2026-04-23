use saluki_config::{ConfigurationLoader, GenericConfiguration};
use serde::Serialize;
use serde_json::json;

use super::{SalukiAnnotation, ValueType, ALL_ANNOTATIONS};
use crate::config::{DatadogRemapper, KEY_ALIASES};

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

fn test_env_string(value_type: ValueType) -> String {
    match value_type {
        ValueType::String => TEST_STRING_VALUE.to_string(),
        ValueType::Bool => "true".to_string(),
        ValueType::StringList => TEST_STRING_LIST_VALUE.join(" "),
        ValueType::Integer => "42".to_string(),
        ValueType::Float => "1.5".to_string(),
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

fn dd_env_var_to_test_key(env_var: &str) -> &str {
    env_var.strip_prefix("DD_").unwrap_or(env_var)
}

async fn make_config_from_file(file_values: serde_json::Value) -> GenericConfiguration {
    let (cfg, _) = ConfigurationLoader::for_tests_with_provider_factory(
        Some(file_values),
        None,
        false,
        KEY_ALIASES,
        DatadogRemapper::new,
    )
    .await;
    cfg
}

async fn make_config_from_env(env_vars: &[(String, String)]) -> GenericConfiguration {
    let (cfg, _) = ConfigurationLoader::for_tests_with_provider_factory(
        None,
        Some(env_vars),
        false,
        KEY_ALIASES,
        DatadogRemapper::new,
    )
    .await;
    cfg
}

/// Runs smoke tests for all annotations registered to `struct_name` against a deserialized config struct `T`.
///
/// Annotations are discovered automatically from [`ALL_ANNOTATIONS`] by filtering on `used_by` —
/// there is no need to pass a list of keys explicitly. Register an annotation for a struct by
/// adding its name (from [`crate::config_registry::structs`]) to the annotation's `used_by` field.
///
/// Verifies three properties:
///
/// **Supported keys** (those registered to `struct_name`): loading the struct with the test value
/// set via the annotation's `yaml_path` and via each of its effective env vars must all produce
/// identical structs, and each must differ from the default (empty-config) struct.
///
/// **Unsupported keys** (all other annotations in `ALL_ANNOTATIONS`): loading the struct with
/// that key set must produce a struct identical to the default struct — i.e., the struct is
/// unaffected.
///
/// **Full field coverage**: loading the struct with all supported keys set simultaneously must
/// produce a struct where every serialized leaf field differs from the default. This catches fields
/// that exist in the struct but have no registered annotation exercising them. Fields that are
/// intentionally not configuration-driven (e.g. populated from runtime state) can be excluded by
/// passing their serialized paths in `non_config_fields` (e.g. `&["workload_provider"]`).
///
/// `config_factory` converts a raw `GenericConfiguration` into the typed struct under test.
pub async fn run_config_smoke_tests<T, Factory>(
    struct_name: &'static str, non_config_fields: &[&str], config_factory: Factory,
) where
    T: PartialEq + std::fmt::Debug + Serialize,
    Factory: Fn(GenericConfiguration) -> T,
{
    let keys: Vec<&'static SalukiAnnotation> = ALL_ANNOTATIONS
        .iter()
        .copied()
        .filter(|a| a.used_by.contains(&struct_name))
        .collect();

    let default_struct = config_factory(make_config_from_file(json!({})).await);
    let mut failures: Vec<String> = Vec::new();

    // Supported keys: the canonical yaml_path and every additional yaml_path and env var is
    // loaded independently; all must produce the same struct, and each must differ from the default.
    for annotation in &keys {
        let canonical_path = annotation.yaml_path();
        let reference = config_factory(
            make_config_from_file(yaml_path_to_json(
                canonical_path,
                test_json_value(annotation.value_type()),
            ))
            .await,
        );

        if reference == default_struct {
            failures.push(format!(
                "yaml_path '{}': struct did not change from its default — \
                 is the test value the same as the default, or is the key not wired up?",
                canonical_path,
            ));
            // Reference is invalid; skip comparisons for this key to avoid cascading noise.
            continue;
        }

        for yaml_path in annotation.additional_yaml_paths {
            let from_path = config_factory(
                make_config_from_file(yaml_path_to_json(yaml_path, test_json_value(annotation.value_type()))).await,
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
                test_env_string(annotation.value_type()),
            )];
            let from_env = config_factory(make_config_from_env(&env_pairs).await);
            if from_env != reference {
                failures.push(format!(
                    "env var '{}' produced a different struct than yaml_path '{}'",
                    env_var, canonical_path,
                ));
            }
        }
    }

    // Unsupported keys: setting any of their yaml_paths must not change the struct.
    for annotation in ALL_ANNOTATIONS.iter().filter(|a| !a.used_by.contains(&struct_name)) {
        for yaml_path in annotation.all_yaml_paths() {
            let with_foreign = config_factory(
                make_config_from_file(yaml_path_to_json(yaml_path, test_json_value(annotation.value_type()))).await,
            );
            if with_foreign != default_struct {
                failures.push(format!(
                    "yaml_path '{}' is not registered for '{}' but unexpectedly changed the struct",
                    yaml_path, struct_name,
                ));
            }
        }
    }

    // Full coverage: load all supported keys at once and verify every serialized leaf field
    // differs from the default. Catches struct fields that exist and are populated from config
    // but have no registered annotation exercising them.
    let mut all_vals = json!({});
    for annotation in keys {
        saluki_config::upsert(
            &mut all_vals,
            annotation.yaml_path(),
            test_json_value(annotation.value_type()),
        );
    }
    let all_keys_struct = config_factory(make_config_from_file(all_vals).await);
    let full_map = serde_json::to_value(&all_keys_struct).expect("failed to serialize struct with all keys set");
    let default_map = serde_json::to_value(&default_struct).expect("failed to serialize default struct");
    let mut unchanged = Vec::new();
    collect_unchanged_leaves(&full_map, &default_map, "", &mut unchanged);
    unchanged.retain(|path| !non_config_fields.contains(&path.as_str()));
    if !unchanged.is_empty() {
        failures.push(format!(
            "{} serialized field(s) are never changed by any registered config key: [{}]\n  \
             Fix: add a SalukiAnnotation for each field and include '{}' in its used_by list.\n  \
             Fix: if a field is intentionally not config-driven (e.g. injected at runtime), \
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
