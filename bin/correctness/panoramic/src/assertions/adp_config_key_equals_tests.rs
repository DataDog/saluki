use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber as _;

use super::{AdpConfigKeyEqualsAssertion, Assertion as _, AssertionContext, LogBuffer, TargetCommand};
use crate::actions::{execute_target_command, CommandDiagnostics};

static NEXT_TEMP_PATH_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default)]
struct TraceCapture {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl TraceCapture {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.bytes.lock().expect("trace capture lock should not be poisoned")).into_owned()
    }
}

struct TraceWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl io::Write for TraceWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .expect("trace capture lock should not be poisoned")
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TraceCapture {
    type Writer = TraceWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TraceWriter {
            bytes: Arc::clone(&self.bytes),
        }
    }
}

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

fn assertion(endpoint: &str, expected: serde_json::Value, timeout: Duration) -> AdpConfigKeyEqualsAssertion {
    AdpConfigKeyEqualsAssertion::new("feature.enabled".to_string(), expected, endpoint.to_string(), timeout)
        .expect("test endpoint should be supported")
}

#[tokio::test]
async fn source_endpoint_passes_via_real_adp_config_json_child_command() {
    let command = shell_target(
        r#"test "$1" = config && test "$2" = --json && test -z "$3" && printf '%s' '{"feature":{"enabled":"source"}}'"#,
        HashMap::new(),
    );
    let ctx = host_context(command, CancellationToken::new());

    let result = assertion(
        "https://127.0.0.1:55101/config",
        serde_json::json!("source"),
        Duration::from_secs(3),
    )
    .check(&ctx)
    .await;

    assert!(result.passed, "unexpected assertion failure: {}", result.message);
}

#[tokio::test]
async fn runtime_endpoint_passes_distinct_runtime_arguments_and_value_to_real_child() {
    let command = shell_target(
        r#"test "$1" = config && test "$2" = --json && test "$3" = --runtime && printf '%s' '{"feature":{"enabled":"runtime"}}'"#,
        HashMap::new(),
    );
    let ctx = host_context(command, CancellationToken::new());

    let result = assertion(
        "https://localhost:55101/config/internal",
        serde_json::json!("runtime"),
        Duration::from_secs(3),
    )
    .check(&ctx)
    .await;

    assert!(result.passed, "unexpected assertion failure: {}", result.message);
}

#[test]
fn canonical_endpoint_forms_are_accepted() {
    for endpoint in [
        "https://localhost:55101/config",
        "https://127.0.0.1:55101/config",
        "https://localhost:55101/config/internal",
        "https://127.0.0.1:55101/config/internal",
    ] {
        AdpConfigKeyEqualsAssertion::new(
            "feature.enabled".to_string(),
            serde_json::json!(true),
            endpoint.to_string(),
            Duration::from_secs(1),
        )
        .unwrap_or_else(|error| panic!("canonical endpoint {endpoint} was rejected: {error}"));
    }
}

#[test]
fn unsupported_endpoints_are_rejected_without_exposing_the_configured_value() {
    let endpoint_secret = "endpoint-secret";
    let unsupported = [
        "http://localhost:55101/config".to_string(),
        "https://localhost:55101/config/".to_string(),
        format!("https://localhost:55101/config?token={endpoint_secret}"),
        format!("https://user:{endpoint_secret}@localhost:55101/config"),
        format!("https://localhost:55101/config#{endpoint_secret}"),
        "https://example.invalid:55101/config".to_string(),
        "https://localhost:55102/config".to_string(),
        "not a URL".to_string(),
    ];

    for endpoint in unsupported {
        let error = match AdpConfigKeyEqualsAssertion::new(
            "feature.enabled".to_string(),
            serde_json::json!(true),
            endpoint,
            Duration::from_secs(1),
        ) {
            Ok(_) => panic!("unsupported endpoint should be rejected"),
            Err(error) => error,
        };
        let message = error.to_string();

        assert!(
            message.contains("localhost:55101")
                && message.contains("127.0.0.1:55101")
                && message.contains("/config")
                && message.contains("/config/internal"),
            "endpoint error was not actionable: {message}"
        );
        assert!(
            !message.contains(endpoint_secret),
            "endpoint error exposed configured secret: {message}"
        );
    }
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

    let result = assertion(
        "https://localhost:55101/config",
        serde_json::json!(true),
        Duration::from_secs(3),
    )
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
async fn hung_config_invocation_consumes_the_remaining_assertion_budget_without_retrying() {
    let attempts_path = temp_path("adp-config-hung-attempts");
    let _ = std::fs::remove_file(&attempts_path);
    let command = shell_target(
        r#"test "$1" = config && test "$2" = --json || exit 64
printf x >> "$PANORAMIC_CONFIG_ATTEMPTS_FILE"
exec sleep 30"#,
        HashMap::from([(
            "PANORAMIC_CONFIG_ATTEMPTS_FILE".to_string(),
            attempts_path.to_string_lossy().into_owned(),
        )]),
    );
    let ctx = host_context(command, CancellationToken::new());
    let started = Instant::now();

    let result = assertion(
        "https://localhost:55101/config",
        serde_json::json!(true),
        Duration::from_millis(11_500),
    )
    .check(&ctx)
    .await;

    let elapsed = started.elapsed();
    let attempts = std::fs::read(&attempts_path).unwrap_or_default().len();
    let _ = std::fs::remove_file(&attempts_path);
    assert!(!result.passed, "hung child unexpectedly passed");
    assert_eq!(attempts, 1, "hung CLI invocation was retried");
    assert!(
        elapsed < Duration::from_secs(13),
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
exec sleep 5"#,
        HashMap::from([(
            "PANORAMIC_CONFIG_STARTED_FILE".to_string(),
            started_path.to_string_lossy().into_owned(),
        )]),
    );
    let cancel_token = CancellationToken::new();
    let ctx = host_context(command, cancel_token.clone());
    let mut assertion_task = tokio::spawn(async move {
        assertion(
            "https://localhost:55101/config",
            serde_json::json!(true),
            Duration::from_secs(10),
        )
        .check(&ctx)
        .await
    });

    let child_started = tokio::time::timeout(Duration::from_secs(30), async {
        while !started_path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if !child_started {
        cancel_token.cancel();
        if tokio::time::timeout(Duration::from_secs(2), &mut assertion_task)
            .await
            .is_err()
        {
            assertion_task.abort();
            let _ = assertion_task.await;
        }
        let _ = std::fs::remove_file(&started_path);
        panic!("assertion did not invoke the configured ADP CLI child within 30 seconds");
    }

    let started = Instant::now();
    cancel_token.cancel();
    let result = match tokio::time::timeout(Duration::from_secs(2), &mut assertion_task).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            let _ = std::fs::remove_file(&started_path);
            panic!("assertion task failed: {error}");
        }
        Err(_) => {
            assertion_task.abort();
            let _ = assertion_task.await;
            let _ = std::fs::remove_file(&started_path);
            panic!("cancellation did not interrupt the ADP CLI child within 2 seconds");
        }
    };

    let elapsed = started.elapsed();
    let _ = std::fs::remove_file(&started_path);
    assert!(!result.passed, "cancelled assertion unexpectedly passed");
    assert!(
        result.message.contains("cancelled"),
        "unexpected message: {}",
        result.message
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "cancellation was not bounded: {elapsed:?}"
    );
}

#[tokio::test]
async fn nonzero_adp_config_cli_output_is_absent_from_description_result_and_trace() {
    let command_secret = "adp-config-command-output-secret";
    let environment_secret = "adp-config-environment-output-secret";
    let config_secret = "adp-config-child-config-secret";
    let command = TargetCommand::new(vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "printf '%s\\n%s\\n{config_secret}\\n' \"$0\" \"$PANORAMIC_CONFIG_SECRET\"; \
             printf '%s\\n%s\\n{config_secret}\\n' \"$0\" \"$PANORAMIC_CONFIG_SECRET\" >&2; exit 7"
        ),
        command_secret.to_string(),
    ])
    .with_host_env(HashMap::from([(
        "PANORAMIC_CONFIG_SECRET".to_string(),
        environment_secret.to_string(),
    )]));
    let ctx = host_context(command, CancellationToken::new());
    let assertion = assertion(
        "https://localhost:55101/config",
        serde_json::json!(true),
        Duration::from_secs(2),
    );
    let description = assertion.description();
    let trace_capture = TraceCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .without_time()
        .with_writer(trace_capture.clone())
        .finish();

    let result = assertion.check(&ctx).with_subscriber(subscriber).await;
    let trace = trace_capture.contents();

    assert!(!result.passed, "nonzero child unexpectedly passed");
    assert!(
        trace.contains(
            "Failed to read ADP config with its CLI, retrying... key=feature.enabled \
             error=ADP configuration CLI command exited exit status: 7."
        ),
        "assertion did not trace the fixed-label child failure: {trace}"
    );
    for secret in [command_secret, environment_secret, config_secret] {
        assert!(!description.contains(secret), "description exposed {secret}");
        assert!(
            !result.message.contains(secret),
            "result exposed {secret}: {}",
            result.message
        );
        assert!(!trace.contains(secret), "trace exposed {secret}: {trace}");
    }
}

#[tokio::test]
async fn suppressed_command_diagnostics_return_a_fixed_label_without_child_output() {
    let stdout_secret = "suppressed-command-stdout-secret";
    let stderr_secret = "suppressed-command-stderr-secret";
    let command = shell_target(
        &format!("printf '{stdout_secret}'; printf '{stderr_secret}' >&2; exit 9"),
        HashMap::new(),
    );
    let ctx = host_context(command.clone(), CancellationToken::new());
    let command = command.with_args(&["config".to_string(), "--json".to_string()]);
    let diagnostics = CommandDiagnostics::redacted_without_child_output("fixed safe command label");

    let error = execute_target_command(
        &ctx,
        &command,
        &diagnostics,
        Duration::from_secs(5),
        ctx.adp_cli_command.host_env(),
    )
    .await
    .expect_err("nonzero command should fail");
    let message = error.to_string();

    assert!(
        message.contains("fixed safe command label"),
        "unexpected diagnostic: {message}"
    );
    assert!(!message.contains(stdout_secret), "diagnostic exposed stdout: {message}");
    assert!(!message.contains(stderr_secret), "diagnostic exposed stderr: {message}");
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

    let assertion = assertion(
        "https://localhost:55101/config",
        serde_json::json!(true),
        Duration::from_millis(100),
    );
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
