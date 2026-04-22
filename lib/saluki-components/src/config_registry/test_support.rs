use saluki_config::{ConfigurationLoader, GenericConfiguration};
use serde_json::json;

use super::{ConfigKey, ValueType, ALL_KEYS};
use crate::config::{DatadogRemapper, KEY_ALIASES};

/// Canonical test value used for `String` and `Integer`-like config keys.
pub const TEST_STRING_VALUE: &str = "http://smoke-proxy.example.com:3128";

/// Canonical test value used for `Bool` config keys.
pub const TEST_BOOL_VALUE: bool = true;

/// Canonical test value used for `StringList` config keys.
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
/// Verifies:
/// - every key in `keys` declares `struct_name` in its `used_by` set
/// - every key in `ALL_KEYS` for `struct_name` is present in `keys`
/// - for each key, the test value round-trips through the yaml_path file source and every declared env var
///
/// `config_factory` converts a raw `GenericConfiguration` into the typed struct under test.
/// `verifier` receives the deserialized struct and the key being tested, and returns `true` if the
/// expected test value was captured.
pub async fn run_config_smoke_tests<T, Factory, Verifier>(
    struct_name: &'static str, keys: &[&'static ConfigKey], config_factory: Factory, verifier: Verifier,
) where
    Factory: Fn(GenericConfiguration) -> T,
    Verifier: Fn(&T, &ConfigKey) -> bool,
{
    for key in keys {
        assert!(
            key.used_by.contains(&struct_name),
            "key '{}' is in the test set for '{}' but does not declare it in used_by",
            key.yaml_path,
            struct_name,
        );
    }

    let local_paths: std::collections::HashSet<&str> = keys.iter().map(|k| k.yaml_path).collect();
    for key in ALL_KEYS.iter().filter(|k| k.used_by.contains(&struct_name)) {
        assert!(
            local_paths.contains(key.yaml_path),
            "key '{}' is registered for '{}' in ALL_KEYS but is missing from the test set",
            key.yaml_path,
            struct_name,
        );
    }

    for key in keys {
        let file_values = yaml_path_to_json(key.yaml_path, test_json_value(key.value_type));
        let typed = config_factory(make_config_from_file(file_values).await);
        assert!(
            verifier(&typed, key),
            "key '{}' was not captured from yaml_path source",
            key.yaml_path,
        );

        for env_var in key.env_vars {
            let env_pairs = [(
                dd_env_var_to_test_key(env_var).to_string(),
                test_env_string(key.value_type),
            )];
            let typed = config_factory(make_config_from_env(&env_pairs).await);
            assert!(
                verifier(&typed, key),
                "key '{}' was not captured from env var '{}'",
                key.yaml_path,
                env_var,
            );
        }
    }
}
