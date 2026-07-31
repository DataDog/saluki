use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use tokio_util::sync::CancellationToken;

use super::{get_config_key, AdpConfigEndpoint, AdpConfigKeyEqualsAssertion};
use crate::assertions::{Assertion as _, AssertionContext, LogBuffer, TargetCommand};

fn host_context(adp_cli_command: TargetCommand) -> AssertionContext {
    AssertionContext {
        log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
        container_exit_token: CancellationToken::new(),
        cancel_token: CancellationToken::new(),
        port_mappings: HashMap::new(),
        container_ip: None,
        target_os: None,
        container_name: "adp-config-key-equals-test".to_string(),
        is_host_process: true,
        host_process_exit_code: None,
        docker_container_exit_code: None,
        core_agent_auth_token_path: None,
        adp_cli_command,
        core_agent_cli_command: TargetCommand::new(vec!["panoramic-wrong-cli-program".to_string()]),
    }
}

#[test]
fn canonical_endpoints_select_source_or_runtime_arguments() {
    let cases: &[(&str, &[&str])] = &[
        ("https://localhost:55101/config", &["config", "--json"]),
        ("https://127.0.0.1:55101/config", &["config", "--json"]),
        (
            "https://localhost:55101/config/internal",
            &["config", "--json", "--runtime"],
        ),
        (
            "https://127.0.0.1:55101/config/internal",
            &["config", "--json", "--runtime"],
        ),
    ];

    for (endpoint, expected_args) in cases {
        let selected = AdpConfigEndpoint::parse(endpoint)
            .unwrap_or_else(|error| panic!("canonical endpoint {endpoint} was rejected: {error}"));
        assert_eq!(
            selected.command_args(),
            expected_args.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }
}

#[test]
fn unsupported_endpoints_are_rejected_without_exposing_the_configured_value() {
    let secret = "endpoint-secret-that-must-not-appear";
    let endpoints = [
        "http://localhost:55101/config".to_string(),
        "https://localhost:55101/config/".to_string(),
        format!("https://localhost:55101/config?token={secret}"),
        format!("https://user:{secret}@localhost:55101/config"),
        "https://example.invalid:55101/config".to_string(),
        "not a URL".to_string(),
    ];

    for endpoint in endpoints {
        let Err(error) = AdpConfigEndpoint::parse(&endpoint) else {
            panic!("unsupported endpoint was accepted: {endpoint}");
        };
        let message = error.to_string();

        assert!(message.contains("Unsupported ADP configuration endpoint"));
        assert!(!message.contains(&endpoint), "error exposed endpoint: {message}");
        assert!(!message.contains(secret), "error exposed endpoint secret: {message}");
    }
}

#[test]
fn nested_config_keys_are_looked_up_by_dotted_path() {
    let config = serde_json::json!({
        "feature": {
            "nested": {
                "enabled": true
            }
        }
    });

    assert_eq!(
        get_config_key(&config, "feature.nested.enabled"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(get_config_key(&config, "feature.enabled"), None);
}

#[tokio::test]
async fn successful_assertion_executes_the_selected_cli_and_parses_its_json() {
    let command = TargetCommand::new(vec![
        "sh".to_string(),
        "-c".to_string(),
        r#"test "$0" = adp-selected &&
test "$1" = config &&
test "$2" = --json &&
test "$3" = --runtime &&
printf '%s' '{"feature":{"nested":{"enabled":true}}}'"#
            .to_string(),
        "adp-selected".to_string(),
    ]);
    let ctx = host_context(command);
    let assertion = AdpConfigKeyEqualsAssertion::new(
        "feature.nested.enabled".to_string(),
        serde_json::json!(true),
        "https://127.0.0.1:55101/config/internal".to_string(),
        Duration::from_secs(5),
    )
    .expect("runtime endpoint should be supported");

    let result = assertion.check(&ctx).await;

    assert!(result.passed, "unexpected assertion failure: {}", result.message);
}
