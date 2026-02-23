//! Tests for configuration extraction when the same key appears from multiple sources.
//!
//! When config is merged from e.g. config stream snapshot and environment variables, duplicate
//! keys can occur. `as_typed_allow_duplicate_keys` tolerates them by taking the last value.
//! When both a canonical key and a deprecated alias map to the same struct field,
//! `merged_config_as_json_map` allows callers to collapse the alias before deserializing.

use saluki_config::dynamic::ConfigUpdate;
use saluki_config::ConfigurationLoader;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct TestConfig {
    #[serde(rename = "forwarder_retry_queue_payloads_max_size", default)]
    retry_queue_max_size: u64,
}

/// Same field as TestConfig but with a deprecated alias; both keys in the map would cause "duplicate field".
#[derive(Debug, Deserialize)]
struct TestConfigWithAlias {
    #[serde(
        rename = "forwarder_retry_queue_payloads_max_size",
        alias = "forwarder_retry_queue_max_size",
        default
    )]
    retry_queue_max_size: u64,
}

#[tokio::test]
async fn as_typed_allow_duplicate_keys_uses_last_value_when_same_key_from_static_and_dynamic() {
    let static_values = json!({ "forwarder_retry_queue_payloads_max_size": 100 });
    let (config, maybe_sender) = ConfigurationLoader::for_tests(Some(static_values), None, true).await;
    let sender = maybe_sender.expect("dynamic config enabled, sender should be Some");

    let snapshot = json!({ "forwarder_retry_queue_payloads_max_size": 200 });
    sender
        .send(ConfigUpdate::Snapshot(snapshot))
        .await
        .expect("send snapshot");

    config.ready().await;

    let extracted: TestConfig = config
        .as_typed_allow_duplicate_keys()
        .expect("should not error on duplicate key");
    assert_eq!(
        extracted.retry_queue_max_size, 200,
        "last value (from dynamic snapshot) should win"
    );
}

#[tokio::test]
async fn merged_config_as_json_map_allows_collapsing_canonical_and_alias_keys() {
    let static_values = json!({
        "forwarder_retry_queue_payloads_max_size": 200,
        "forwarder_retry_queue_max_size": 100,
    });
    let (config, _) = ConfigurationLoader::for_tests(Some(static_values), None, false).await;

    config.ready().await;

    let mut map = config
        .merged_config_as_json_map()
        .expect("merged config should extract as object");

    // Collapse alias so only one key maps to the field (canonical wins).
    if map.contains_key("forwarder_retry_queue_payloads_max_size") {
        map.remove("forwarder_retry_queue_max_size");
    }

    let extracted: TestConfigWithAlias =
        serde_json::from_value(serde_json::Value::Object(map)).expect("deserialize after collapse");

    assert_eq!(
        extracted.retry_queue_max_size, 200,
        "canonical key value should be used after collapsing alias"
    );
}
