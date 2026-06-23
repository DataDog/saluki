//! Tests to verify that changes to keys in a `GenericConfiguration` reach `SalukiConfiguration`.
//!
//! Each test checks a single key by setting a non default value in a `GenericConfiguration` map
//! then running that map through `DatadogConfiguration` deserialization and witness trait
//! translation. The resulting `SalukiConfiguration` is checked against default construction to
//! ensure that it's value has been perturbed. This ensures that none of the witness trait function
//! implementations are inert.
use saluki_config_tools::ConfigurationLoader;
use serde_json::json;

use crate::system::ConfigurationSystem;

/// Create a `GenericConfiguration` from the root JSON value.
async fn generic_from(value: serde_json::Value) -> saluki_config_tools::GenericConfiguration {
    ConfigurationLoader::default()
        .add_providers([figment::providers::Serialized::defaults(value)])
        .into_generic()
        .await
        .expect("build generic configuration")
}

/// Drive deserialization and translation.
async fn translate_from(value: serde_json::Value) -> agent_data_plane_config::SalukiConfiguration {
    let config = generic_from(value).await;
    let system = ConfigurationSystem::load(config)
        .start()
        .await
        .expect("translation must succeed");
    system.saluki()
}

/// Check that the `SalukiConfiguration` object is not default constructed after deserialization
/// and translation.
async fn assert_key_propagates(yaml_path: &str, test_value: serde_json::Value) {
    let default = translate_from(json!({})).await;

    // Set up a JSON object holding the test value at our key-path.
    let mut map = json!({});
    saluki_config_tools::upsert(&mut map, yaml_path, test_value);
    let with_value = translate_from(map).await;

    assert_ne!(
        default, with_value,
        "key '{}' did not change SalukiConfiguration — \
         is the test value the same as the default, or is translation not wired?",
        yaml_path,
    );
}

include!(concat!(env!("OUT_DIR"), "/translation_smoke_tests.rs"));
