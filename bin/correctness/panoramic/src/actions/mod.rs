use std::path::PathBuf;
use std::time::{Duration, Instant};

use airlock::docker;
use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecResults},
};
use futures::TryStreamExt as _;
use reqwest::ClientBuilder;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::assertions::{AssertionContext, AssertionResult};
use crate::config::ActionConfig;

const DEFAULT_CORE_AGENT_CONFIG_ENDPOINT_TEMPLATE: &str = "https://localhost:55001/agent/config/{key}";

/// Trait for integration-test actions.
#[async_trait::async_trait]
pub trait Action: Send + Sync {
    /// Returns the name of this action type.
    fn name(&self) -> &'static str;

    /// Returns a human-readable description of what this action does.
    fn description(&self) -> String;

    /// Executes the action and returns a result shaped like an assertion result.
    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult;
}

/// Creates an action from its configuration.
pub fn create_action(config: &ActionConfig) -> Result<Box<dyn Action>, GenericError> {
    match config {
        ActionConfig::CoreAgentConfigSet {
            key,
            value,
            endpoint,
            timeout,
        } => Ok(Box::new(CoreAgentConfigSetAction::new(
            key.clone(),
            value.clone(),
            endpoint.clone(),
            timeout.0,
        ))),
    }
}

struct CoreAgentConfigSetAction {
    key: String,
    value: Value,
    endpoint: String,
    timeout: Duration,
}

impl CoreAgentConfigSetAction {
    fn new(key: String, value: Value, endpoint: String, timeout: Duration) -> Self {
        Self {
            key,
            value,
            endpoint,
            timeout,
        }
    }

    fn endpoint(&self) -> String {
        self.endpoint.replace("{key}", &self.key)
    }
}

#[async_trait::async_trait]
impl Action for CoreAgentConfigSetAction {
    fn name(&self) -> &'static str {
        "core_agent_config_set"
    }

    fn description(&self) -> String {
        format!("Set Core Agent runtime config '{}' to {}.", self.key, self.value)
    }

    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let value = runtime_value_to_string(&self.value);

        if let Err(e) = validate_runtime_key(&self.key) {
            return AssertionResult {
                name: self.name().to_string(),
                passed: false,
                message: e.to_string(),
                duration: started.elapsed(),
            };
        }

        let result = if ctx.is_host_process {
            set_config_from_host(
                &self.endpoint(),
                &self.key,
                &value,
                ctx.core_agent_auth_token_path.as_ref(),
                self.timeout,
                &ctx.cancel_token,
                &ctx.container_exit_token,
            )
            .await
        } else if ctx.target_is_windows() {
            set_config_in_windows_container(
                &ctx.container_name,
                &self.endpoint(),
                &self.key,
                &value,
                self.timeout,
                &ctx.cancel_token,
                &ctx.container_exit_token,
            )
            .await
        } else {
            set_config_in_linux_container(
                &ctx.container_name,
                &self.endpoint(),
                &self.key,
                &value,
                self.timeout,
                &ctx.cancel_token,
                &ctx.container_exit_token,
            )
            .await
        };

        match result {
            Ok(()) => AssertionResult {
                name: self.name().to_string(),
                passed: true,
                message: self.description(),
                duration: started.elapsed(),
            },
            Err(e) => AssertionResult {
                name: self.name().to_string(),
                passed: false,
                message: e.to_string(),
                duration: started.elapsed(),
            },
        }
    }
}

async fn set_config_from_host(
    endpoint: &str, key: &str, value: &str, auth_token_path: Option<&PathBuf>, timeout: Duration,
    cancel_token: &CancellationToken, exit_token: &CancellationToken,
) -> Result<(), GenericError> {
    let auth_token_path = auth_token_path.ok_or_else(|| {
        generic_error!("Core Agent auth token path is unavailable for host-process runtime config mutation.")
    })?;
    let auth_token = tokio::fs::read_to_string(auth_token_path)
        .await
        .with_error_context(|| {
            format!(
                "Failed to read Core Agent auth token at '{}'.",
                auth_token_path.display()
            )
        })?;
    let client = ClientBuilder::new()
        .danger_accept_invalid_certs(true)
        .build()
        .error_context("Failed to build HTTP client.")?;
    let deadline = Instant::now() + timeout;

    loop {
        if cancel_token.is_cancelled() || exit_token.is_cancelled() {
            return Err(generic_error!("Action cancelled because the target exited."));
        }
        if Instant::now() > deadline {
            return Err(generic_error!("Timed out setting Core Agent runtime config '{}'.", key));
        }

        match client
            .post(endpoint)
            .bearer_auth(auth_token.trim())
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("value={}", form_encode(value)))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(generic_error!(
                    "Core Agent runtime config set for '{}' returned status {}: {}.",
                    key,
                    status,
                    body
                ));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

async fn set_config_in_linux_container(
    container_name: &str, endpoint: &str, key: &str, value: &str, timeout: Duration, cancel_token: &CancellationToken,
    exit_token: &CancellationToken,
) -> Result<(), GenericError> {
    let endpoint = endpoint.replace("localhost", "127.0.0.1");
    let script = format!(
        "tok=$(cat /etc/datadog-agent/auth_token) && curl -ks -X POST -H \"Authorization: Bearer $tok\" --data-urlencode value={} {} -o /tmp/panoramic-core-agent-config-set -w '%{{http_code}}'",
        shell_quote(value),
        shell_quote(&endpoint)
    );
    let deadline = Instant::now() + timeout;
    loop {
        if cancel_token.is_cancelled() || exit_token.is_cancelled() {
            return Err(generic_error!("Action cancelled because the target exited."));
        }
        if Instant::now() > deadline {
            return Err(generic_error!("Timed out setting Core Agent runtime config '{}'.", key));
        }
        match exec_collect(container_name, vec!["sh".to_string(), "-c".to_string(), script.clone()]).await {
            Ok(output) => return check_status_output(key, &output),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

async fn set_config_in_windows_container(
    container_name: &str, endpoint: &str, key: &str, value: &str, timeout: Duration, cancel_token: &CancellationToken,
    exit_token: &CancellationToken,
) -> Result<(), GenericError> {
    let endpoint = endpoint.replace("localhost", "127.0.0.1");
    let command = format!(
        "$tok = Get-Content -Raw 'C:\\ProgramData\\Datadog\\auth_token'; curl.exe -ks -X POST -H \"Authorization: Bearer $tok\" --data-urlencode \"value={}\" \"{}\" -o NUL -w \"%{{http_code}}\"",
        powershell_quote(value),
        powershell_quote(&endpoint)
    );
    let deadline = Instant::now() + timeout;
    loop {
        if cancel_token.is_cancelled() || exit_token.is_cancelled() {
            return Err(generic_error!("Action cancelled because the target exited."));
        }
        if Instant::now() > deadline {
            return Err(generic_error!("Timed out setting Core Agent runtime config '{}'.", key));
        }
        match exec_collect(
            container_name,
            vec![
                "pwsh".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                command.clone(),
            ],
        )
        .await
        {
            Ok(output) => return check_status_output(key, &output),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

async fn exec_collect(container_name: &str, cmd: Vec<String>) -> Result<String, GenericError> {
    let docker = docker::connect().error_context("Failed to connect to Docker.")?;
    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions::<String> {
                cmd: Some(cmd),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .error_context("Failed to create exec.")?;
    let exec_id = exec.id.clone();
    let result = docker
        .start_exec(&exec_id, None)
        .await
        .error_context("Failed to start exec.")?;

    let mut output_text = String::new();
    if let StartExecResults::Attached { mut output, .. } = result {
        while let Some(chunk) = output.try_next().await.error_context("Failed to read exec output.")? {
            match chunk {
                LogOutput::StdOut { message } | LogOutput::StdErr { message } => {
                    output_text.push_str(&String::from_utf8_lossy(&message));
                }
                _ => {}
            }
        }
    }

    let inspect = docker
        .inspect_exec(&exec_id)
        .await
        .error_context("Failed to inspect exec.")?;
    if inspect.exit_code != Some(0) {
        return Err(generic_error!("exec exited {:?}: {}", inspect.exit_code, output_text));
    }

    Ok(output_text)
}

fn check_status_output(key: &str, output: &str) -> Result<(), GenericError> {
    let status = output.trim();
    if status == "200" {
        Ok(())
    } else {
        Err(generic_error!(
            "Core Agent runtime config set for '{}' returned non-success status/output: {}.",
            key,
            output
        ))
    }
}

fn runtime_value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}

fn validate_runtime_key(key: &str) -> Result<(), GenericError> {
    if key.is_empty() {
        return Err(generic_error!("Core Agent runtime config key must not be empty."));
    }
    if key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        Ok(())
    } else {
        Err(generic_error!(
            "Core Agent runtime config key '{}' contains unsupported characters.",
            key
        ))
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn powershell_quote(value: &str) -> String {
    value.replace('`', "``").replace('"', "`\"")
}

fn form_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => encoded.push(byte as char),
            b' ' => encoded.push('+'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Returns the default Core Agent runtime config endpoint template.
pub fn default_core_agent_config_endpoint_template() -> String {
    DEFAULT_CORE_AGENT_CONFIG_ENDPOINT_TEMPLATE.to_string()
}
