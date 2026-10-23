use std::path::Path;
use std::time::Duration;

use reqwest::ClientBuilder;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde_json::Value;
use tokio::time::Instant;

use super::{docker_exec, Action};
use crate::assertions::{AssertionContext, AssertionResult, AssertionStatus, Attempts};
use crate::probe::{probe, ProbeError, PROBE_TIMEOUT};

const RETRY_INTERVAL: Duration = Duration::from_millis(500);

pub(super) struct CoreAgentConfigSetAction {
    pub(super) key: String,
    pub(super) value: Value,
    pub(super) endpoint: String,
    pub(super) timeout: Duration,
}

impl CoreAgentConfigSetAction {
    pub(super) fn new(key: String, value: Value, endpoint: String, timeout: Duration) -> Self {
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

    /// Prepares an HTTP request on the host or a script to send it inside the container.
    ///
    /// # Errors
    ///
    /// Returns an error if the host auth token is unavailable or HTTP client construction fails.
    async fn setter(&self, ctx: &AssertionContext, value: &str) -> Result<Setter, GenericError> {
        if ctx.is_host_process {
            let auth_token_path = ctx.core_agent_auth_token_path.as_deref().ok_or_else(|| {
                generic_error!("Core Agent auth token path is unavailable for host-process runtime config mutation.")
            })?;
            return Setter::host(&self.endpoint(), value, auth_token_path).await;
        }

        let endpoint = self.endpoint().replace("localhost", "127.0.0.1");
        let command = if ctx.target_is_windows() {
            let script = format!(
                "$tok = Get-Content -Raw 'C:\\ProgramData\\Datadog\\auth_token'; curl.exe -ks -X POST -H \"Authorization: Bearer $tok\" --data-urlencode \"value={}\" \"{}\" -o NUL -w \"%{{http_code}}\"",
                powershell_quote(value),
                powershell_quote(&endpoint)
            );
            vec![
                "pwsh".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                script,
            ]
        } else {
            let script = format!(
                "tok=$(cat /etc/datadog-agent/auth_token) && curl -ks -X POST -H \"Authorization: Bearer $tok\" --data-urlencode value={} {} -o /tmp/panoramic-core-agent-config-set -w '%{{http_code}}'",
                shell_quote(value),
                shell_quote(&endpoint)
            );
            vec!["sh".to_string(), "-c".to_string(), script]
        };
        Ok(Setter::Container {
            container_name: ctx.container_name.clone(),
            endpoint,
            command,
        })
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
        let deadline = started + self.timeout;

        if let Err(e) = validate_runtime_key(&self.key) {
            return AssertionResult::new(self.name(), started, AssertionStatus::Errored, e.to_string());
        }
        let setter = match self.setter(ctx, &runtime_value_to_string(&self.value)).await {
            Ok(setter) => setter,
            Err(e) => return AssertionResult::new(self.name(), started, AssertionStatus::Errored, format!("{e:#}")),
        };

        let mut attempts = Attempts::default();
        loop {
            if ctx.cancel_token.is_cancelled() {
                return AssertionResult::new(
                    self.name(),
                    started,
                    attempts.status(),
                    attempts.explain_cancelled("Action cancelled."),
                );
            }
            if ctx.container_exit_token.is_cancelled() {
                let message = attempts.explain_cancelled("Action cancelled because the target exited.");
                return AssertionResult::new(self.name(), started, AssertionStatus::Failed, message);
            }
            if Instant::now() > deadline {
                let summary = format!("Timed out setting Core Agent runtime config '{}'.", self.key);
                return AssertionResult::new(
                    self.name(),
                    started,
                    attempts.status(),
                    attempts.explain_expired(&summary),
                );
            }

            let limit = deadline.saturating_duration_since(Instant::now()).min(PROBE_TIMEOUT);
            match probe(&setter.what(), limit, &ctx.cancel_token, setter.set(&self.key)).await {
                Ok(SetOutcome::Set) => {
                    return AssertionResult::new(self.name(), started, AssertionStatus::Passed, self.description());
                }
                Ok(SetOutcome::Rejected(message)) => {
                    return AssertionResult::new(self.name(), started, AssertionStatus::Failed, message)
                }
                Ok(SetOutcome::Unanswered(observation)) => attempts.observed(observation),
                Ok(SetOutcome::Harness(error)) => attempts.errored(error),
                Err(error @ ProbeError::Cancelled { .. }) => {
                    return AssertionResult::new(
                        self.name(),
                        started,
                        attempts.status(),
                        attempts.explain_cancelled(error),
                    );
                }
                // Direct requests test the target; a stalled Docker exec prevents evaluation.
                Err(error @ ProbeError::TimedOut { .. }) => match setter {
                    Setter::Host { .. } => attempts.observed(error),
                    Setter::Container { .. } => attempts.errored(error),
                },
            }

            // The next iteration reports cancellation.
            ctx.cancel_token
                .run_until_cancelled(tokio::time::sleep(RETRY_INTERVAL))
                .await;
        }
    }
}

/// Where and how [`CoreAgentConfigSetAction`] sends its request.
enum Setter {
    /// An HTTP client on the host that contacts the Core Agent directly.
    Host {
        client: reqwest::Client,
        endpoint: String,
        auth_token: String,
        body: String,
    },
    /// A script that sends the request inside the target container via `docker exec`.
    Container {
        container_name: String,
        endpoint: String,
        command: Vec<String>,
    },
}

/// The outcome of one configuration request.
enum SetOutcome {
    /// The Core Agent accepted the value.
    Set,
    /// The request returned an unsuccessful status or unexpected output.
    Rejected(String),
    /// The connection or in-container command failed.
    Unanswered(String),
    /// The harness couldn't send the request.
    Harness(String),
}

impl Setter {
    /// Prepares a host request, reading the auth token once.
    async fn host(endpoint: &str, value: &str, auth_token_path: &Path) -> Result<Self, GenericError> {
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
        Ok(Self::Host {
            client,
            endpoint: endpoint.to_string(),
            auth_token: auth_token.trim().to_string(),
            body: format!("value={}", form_encode(value)),
        })
    }

    /// Names the request for probe messages.
    fn what(&self) -> String {
        match self {
            Self::Host { endpoint, .. } => format!("POST {endpoint}"),
            Self::Container {
                container_name,
                endpoint,
                ..
            } => format!("POST {endpoint} from container '{container_name}'"),
        }
    }

    /// Sends one request to set `key`.
    async fn set(&self, key: &str) -> SetOutcome {
        match self {
            Self::Host {
                client,
                endpoint,
                auth_token,
                body,
            } => {
                let response = client
                    .post(endpoint)
                    .bearer_auth(auth_token)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(body.clone())
                    .send()
                    .await;
                match response {
                    Ok(response) if response.status().is_success() => SetOutcome::Set,
                    Ok(response) => {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        SetOutcome::Rejected(format!(
                            "Core Agent runtime config set for '{}' returned status {}: {}.",
                            key, status, body
                        ))
                    }
                    Err(error) => SetOutcome::Unanswered(format!("{:#}", GenericError::from(error))),
                }
            }
            Self::Container {
                container_name,
                command,
                ..
            } => match docker_exec(container_name, command.clone()).await {
                Ok(output) if output.succeeded() => match check_status_output(key, &output.stdout) {
                    Ok(()) => SetOutcome::Set,
                    Err(error) => SetOutcome::Rejected(error.to_string()),
                },
                Ok(output) => SetOutcome::Unanswered(format!(
                    "The in-container request exited {:?}: {}{}",
                    output.exit_code, output.stdout, output.stderr
                )),
                Err(error) => SetOutcome::Harness(format!("{error:#}")),
            },
        }
    }
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
