use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

use tokio_util::sync::CancellationToken;

use super::{AdpConfigEndpoint, AdpConfigKeyEqualsAssertion, ADP_CONFIG_CLI_DIAGNOSTIC_LABEL};
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

fn source_assertion(key: &str, timeout: Duration) -> AdpConfigKeyEqualsAssertion {
    AdpConfigKeyEqualsAssertion::new(
        key.into(),
        serde_json::json!(true),
        "https://localhost:55101/config".into(),
        timeout,
    )
    .expect("source endpoint should be supported")
}

#[test]
fn canonical_endpoints_select_source_or_runtime_arguments() {
    let cases: &[(&str, &[&str])] = &[
        ("https://localhost:55101/config", &["config", "--json"]),
        ("https://127.0.0.1:55101/config", &["config", "--json"]),
        (
            "https://localhost:55101/config/runtime",
            &["config", "--json", "--runtime"],
        ),
        (
            "https://127.0.0.1:55101/config/runtime",
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
        format!("https://localhost:55101/{}/{}", "config", "internal"),
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
        "https://127.0.0.1:55101/config/runtime".to_string(),
        Duration::from_secs(5),
    )
    .expect("runtime endpoint should be supported");

    let result = assertion.check(&ctx).await;

    assert!(result.passed, "unexpected assertion failure: {}", result.message);
}

#[tokio::test]
async fn assertion_retries_a_nonmatching_value_and_accepts_the_next_match() {
    let counter = std::env::temp_dir().join(format!("panoramic-config-{}", rand::random::<u64>()));
    let command = TargetCommand::new(vec![
        "sh".to_string(),
        "-c".to_string(),
        r#"if test -s "$0"; then
    printf '{"feature":{"nested":{"enabled":true}}}'
else
    printf '{"feature":{"nested":{}}}'
fi
printf x >> "$0""#
            .to_string(),
        counter.to_string_lossy().into_owned(),
    ]);
    let ctx = host_context(command);
    let assertion = source_assertion("feature.nested.enabled", Duration::from_secs(20));

    let result = tokio::time::timeout(Duration::from_secs(30), assertion.check(&ctx)).await;
    let calls = std::fs::read(&counter).unwrap_or_default().len();
    let _ = std::fs::remove_file(counter);
    let result = result.expect("assertion should finish within 30 seconds");

    assert!(result.passed, "unexpected assertion failure: {}", result.message);
    assert!(calls >= 2, "expected at least two CLI calls, observed {calls}");
}

#[tokio::test]
async fn fetch_config_redacts_nonzero_child_output() {
    let stdout_secret = "config-stdout-secret";
    let stderr_secret = "config-stderr-secret";
    let command = TargetCommand::new(vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("printf '{stdout_secret}'; printf '{stderr_secret}' >&2; exit 9"),
    ]);
    let ctx = host_context(command);
    let assertion = source_assertion("feature.enabled", Duration::from_secs(10));

    let error = assertion
        .fetch_config(&ctx, Duration::from_secs(10))
        .await
        .expect_err("nonzero CLI child should fail");
    let message = error.to_string();
    assert!(message.contains(ADP_CONFIG_CLI_DIAGNOSTIC_LABEL));
    assert!(!message.contains(stdout_secret), "error exposed stdout: {message}");
    assert!(!message.contains(stderr_secret), "error exposed stderr: {message}");
}
