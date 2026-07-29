use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;

use super::{AdpConfigKeyEqualsAssertion, Assertion as _, AssertionContext, LogBuffer, TargetCommand};

static NEXT_TEMP_PATH_ID: AtomicU64 = AtomicU64::new(0);

fn host_context(adp_cli_command: TargetCommand, cancel_token: CancellationToken) -> AssertionContext {
    let _ = crate::default_crypto_provider().install_default();
    AssertionContext {
        log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
        container_exit_token: CancellationToken::new(),
        cancel_token,
        port_mappings: HashMap::new(),
        container_ip: None,
        target_os: None,
        container_name: "adp-config-key-equals-test".to_string(),
        is_host_process: true,
        host_process_exit_code: None,
        docker_container_exit_code: None,
        core_agent_auth_token_path: None,
        adp_cli_command,
        core_agent_cli_command: TargetCommand::new(Vec::new()),
    }
}

fn shell_target(script: &str, env: HashMap<String, String>) -> TargetCommand {
    TargetCommand::new(vec![
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "adp-config-test-child".to_string(),
    ])
    .with_host_env(env)
}

fn temp_path(label: &str) -> PathBuf {
    let id = NEXT_TEMP_PATH_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("panoramic-{label}-{}-{id}", std::process::id()))
}

fn assertion(expected: serde_json::Value, timeout: Duration) -> AdpConfigKeyEqualsAssertion {
    AdpConfigKeyEqualsAssertion::new(
        "feature.enabled".to_string(),
        expected,
        "http://127.0.0.1:9/config".to_string(),
        timeout,
    )
}

#[tokio::test]
async fn matching_key_passes_via_real_adp_config_json_child_command() {
    let command = shell_target(
        r#"test "$1" = config && test "$2" = --json && printf '%s' '{"feature":{"enabled":true}}'"#,
        HashMap::new(),
    );
    let ctx = host_context(command, CancellationToken::new());

    let result = assertion(serde_json::json!(true), Duration::from_secs(1))
        .check(&ctx)
        .await;

    assert!(result.passed, "unexpected assertion failure: {}", result.message);
}

#[tokio::test]
async fn nonmatching_values_retry_until_the_assertion_budget_expires() {
    let attempts_path = temp_path("adp-config-attempts");
    let _ = std::fs::remove_file(&attempts_path);
    let command = shell_target(
        r#"test "$1" = config && test "$2" = --json || exit 64
printf x >> "$PANORAMIC_CONFIG_ATTEMPTS_FILE"
printf '%s' '{"feature":{"enabled":false}}'"#,
        HashMap::from([(
            "PANORAMIC_CONFIG_ATTEMPTS_FILE".to_string(),
            attempts_path.to_string_lossy().into_owned(),
        )]),
    );
    let ctx = host_context(command, CancellationToken::new());
    let started = Instant::now();

    let result = assertion(serde_json::json!(true), Duration::from_secs(3))
        .check(&ctx)
        .await;

    let elapsed = started.elapsed();
    let attempts = std::fs::read(&attempts_path).unwrap_or_default().len();
    let _ = std::fs::remove_file(&attempts_path);
    assert!(!result.passed, "nonmatching value unexpectedly passed");
    assert!(attempts >= 2, "expected at least two CLI attempts, observed {attempts}");
    assert!(
        elapsed < Duration::from_secs(4),
        "assertion timeout was not bounded: {elapsed:?}"
    );
}

#[tokio::test]
async fn cancellation_interrupts_an_in_flight_adp_config_command() {
    let started_path = temp_path("adp-config-started");
    let _ = std::fs::remove_file(&started_path);
    let command = shell_target(
        r#"test "$1" = config && test "$2" = --json || exit 64
printf x > "$PANORAMIC_CONFIG_STARTED_FILE"
sleep 5
printf '%s' '{"feature":{"enabled":true}}'"#,
        HashMap::from([(
            "PANORAMIC_CONFIG_STARTED_FILE".to_string(),
            started_path.to_string_lossy().into_owned(),
        )]),
    );
    let cancel_token = CancellationToken::new();
    let ctx = host_context(command, cancel_token.clone());
    let marker_path = started_path.clone();
    let cancel_task = tokio::spawn(async move {
        let marker_deadline = Instant::now() + Duration::from_secs(3);
        while !marker_path.exists() && Instant::now() < marker_deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let child_started = marker_path.exists();
        cancel_token.cancel();
        child_started
    });
    let started = Instant::now();

    let result = assertion(serde_json::json!(true), Duration::from_secs(10))
        .check(&ctx)
        .await;

    let child_started = cancel_task.await.expect("cancellation task should finish");
    let elapsed = started.elapsed();
    let _ = std::fs::remove_file(&started_path);
    assert!(child_started, "assertion did not invoke the configured ADP CLI child");
    assert!(!result.passed, "cancelled assertion unexpectedly passed");
    assert!(
        result.message.contains("cancelled"),
        "unexpected message: {}",
        result.message
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "cancellation was not bounded: {elapsed:?}"
    );
}

#[tokio::test]
async fn adp_config_cli_diagnostics_do_not_reveal_command_or_environment_secrets() {
    let command_secret = "adp-config-command-secret";
    let environment_secret = "adp-config-environment-secret";
    let command = TargetCommand::new(vec![format!("missing-{command_secret}")]).with_host_env(HashMap::from([(
        "PANORAMIC_CONFIG_SECRET".to_string(),
        environment_secret.to_string(),
    )]));
    let ctx = host_context(command, CancellationToken::new());

    let assertion = assertion(serde_json::json!(true), Duration::from_millis(100));
    let description = assertion.description();
    let result = assertion.check(&ctx).await;

    assert!(!result.passed, "missing child unexpectedly passed");
    for secret in [command_secret, environment_secret] {
        assert!(!description.contains(secret), "description exposed {secret}");
        assert!(
            !result.message.contains(secret),
            "result exposed {secret}: {}",
            result.message
        );
    }
}
