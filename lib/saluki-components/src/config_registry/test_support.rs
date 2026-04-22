use saluki_config::{ConfigurationLoader, GenericConfiguration};
use serde_json::json;

use super::{ConfigKey, ValueType, ALL_KEYS};
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

/// Runs smoke tests for all keys in `keys` against a deserialized config struct `T`.
///
/// Verifies two properties for every key in the global registry:
///
/// **Supported keys** (those in `keys`): loading the struct with the test value set via the
/// key's `yaml_path` and via each of its declared `env_vars` must all produce identical structs,
/// and each must differ from the default (empty-config) struct.
///
/// **Unsupported keys** (all other keys in `ALL_KEYS`): loading the struct with that key set must
/// produce a struct identical to the default struct — i.e., the struct is unaffected.
///
/// `config_factory` converts a raw `GenericConfiguration` into the typed struct under test.
pub async fn run_config_smoke_tests<T, Factory>(
    struct_name: &'static str, keys: &[&'static ConfigKey], config_factory: Factory,
) where
    T: PartialEq + std::fmt::Debug,
    Factory: Fn(GenericConfiguration) -> T,
{
    // Registry consistency: all passed keys declare struct_name as consumer.
    for key in keys {
        assert!(
            key.used_by.contains(&struct_name),
            "key '{}' is in the test set for '{}' but does not declare it in used_by",
            key.yaml_path,
            struct_name,
        );
    }

    // Registry consistency: all keys in ALL_KEYS for this struct are present in the test set.
    let local_paths: std::collections::HashSet<&str> = keys.iter().map(|k| k.yaml_path).collect();
    for key in ALL_KEYS.iter().filter(|k| k.used_by.contains(&struct_name)) {
        assert!(
            local_paths.contains(key.yaml_path),
            "key '{}' is registered for '{}' in ALL_KEYS but is missing from the test set",
            key.yaml_path,
            struct_name,
        );
    }

    let default_struct = config_factory(make_config_from_file(json!({})).await);

    // Supported keys: all sources must produce the same struct, and it must differ from default.
    for key in keys {
        let from_yaml = config_factory(
            make_config_from_file(yaml_path_to_json(key.yaml_path, test_json_value(key.value_type))).await,
        );

        assert_ne!(
            from_yaml, default_struct,
            "key '{}' via yaml_path did not change the struct from its default — \
             is the test value the same as the default, or is the key not wired up?",
            key.yaml_path,
        );

        for env_var in key.env_vars {
            let env_pairs = [(
                dd_env_var_to_test_key(env_var).to_string(),
                test_env_string(key.value_type),
            )];
            let from_env = config_factory(make_config_from_env(&env_pairs).await);
            assert_eq!(
                from_env, from_yaml,
                "key '{}' via env var '{}' produced a different struct than via yaml_path",
                key.yaml_path, env_var,
            );
        }
    }

    // Unsupported keys: setting them must not change the struct.
    for key in ALL_KEYS.iter().filter(|k| !k.used_by.contains(&struct_name)) {
        let with_foreign = config_factory(
            make_config_from_file(yaml_path_to_json(key.yaml_path, test_json_value(key.value_type))).await,
        );
        assert_eq!(
            with_foreign, default_struct,
            "key '{}' (not registered for '{}') unexpectedly changed the struct",
            key.yaml_path, struct_name,
        );
    }
}
