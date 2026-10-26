use std::time::Duration;

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde_json::Value;
use tokio::time::Instant;
use tracing::trace;

use crate::{
    actions::{execute_target_command, CommandDiagnostics, CommandError},
    assertions::{Assertion, AssertionContext, AssertionResult, AssertionStatus, Attempts},
    probe::{POLL_INTERVAL, PROBE_TIMEOUT},
};

const DEFAULT_ADP_CONFIG_ENDPOINT: &str = "https://localhost:55101/config";
const ADP_CONFIG_CLI_DIAGNOSTIC_LABEL: &str = "ADP configuration CLI command";

#[derive(Clone, Copy)]
enum AdpConfigEndpoint {
    Source,
    Runtime,
}

impl AdpConfigEndpoint {
    fn parse(configured_endpoint: &str) -> Result<Self, GenericError> {
        match configured_endpoint {
            "https://localhost:55101/config" | "https://127.0.0.1:55101/config" => Ok(Self::Source),
            "https://localhost:55101/config/runtime" | "https://127.0.0.1:55101/config/runtime" => Ok(Self::Runtime),
            _ => Err(unsupported_endpoint_error()),
        }
    }

    fn command_args(self) -> Vec<String> {
        match self {
            Self::Source => vec!["config".to_string(), "--json".to_string()],
            Self::Runtime => vec!["config".to_string(), "--json".to_string(), "--runtime".to_string()],
        }
    }
}

fn unsupported_endpoint_error() -> GenericError {
    generic_error!(
        "Unsupported ADP configuration endpoint. Expected exactly `https://localhost:55101/config`, \
         `https://127.0.0.1:55101/config`, or the corresponding `/config/runtime` URL."
    )
}

/// Assertion that polls the selected ADP configuration view until one key equals the expected value.
pub struct AdpConfigKeyEqualsAssertion {
    key: String,
    expected: Value,
    endpoint: AdpConfigEndpoint,
    timeout: Duration,
}

impl AdpConfigKeyEqualsAssertion {
    pub fn new(key: String, expected: Value, endpoint: String, timeout: Duration) -> Result<Self, GenericError> {
        Ok(Self {
            key,
            expected,
            endpoint: AdpConfigEndpoint::parse(&endpoint)?,
            timeout,
        })
    }

    /// Reads the configuration with the ADP CLI, limited to `timeout`.
    ///
    /// # Errors
    ///
    /// Returns the command's error, or [`CommandError::Failed`] if its output isn't JSON.
    async fn fetch_config(&self, ctx: &AssertionContext, timeout: Duration) -> Result<Value, CommandError> {
        let args = self.endpoint.command_args();
        let command = ctx.adp_cli_command.with_args(&args);
        let diagnostics = CommandDiagnostics::redacted_without_child_output(ADP_CONFIG_CLI_DIAGNOSTIC_LABEL);
        let stdout =
            execute_target_command(ctx, &command, &diagnostics, timeout, ctx.adp_cli_command.host_env()).await?;

        serde_json::from_str(&stdout)
            .error_context("Failed to parse ADP config CLI JSON.")
            .map_err(CommandError::Failed)
    }

    fn timed_out_result(
        &self, started: Instant, attempts: &Attempts, last_observed: Option<&Value>,
    ) -> AssertionResult {
        let summary = format!(
            "{} did not happen within {:?}. Last observed value: {}.",
            self.description(),
            self.timeout,
            last_observed
                .map(Value::to_string)
                .unwrap_or_else(|| "<missing>".to_string())
        );
        AssertionResult::new(
            self.name(),
            started,
            attempts.status(),
            attempts.explain_expired(&summary),
        )
    }

    /// Reports test cancellation or target exit.
    fn cancelled_result(&self, started: Instant, attempts: &Attempts, ctx: &AssertionContext) -> AssertionResult {
        if ctx.cancel_token.is_cancelled() {
            let message = attempts.explain_cancelled("Assertion cancelled.");
            AssertionResult::new(self.name(), started, attempts.status(), message)
        } else {
            let message = attempts.explain_cancelled("Assertion cancelled because container exited.");
            AssertionResult::new(self.name(), started, AssertionStatus::Failed, message)
        }
    }
}

#[async_trait::async_trait]
impl Assertion for AdpConfigKeyEqualsAssertion {
    fn name(&self) -> &'static str {
        "adp_config_key_equals"
    }

    fn description(&self) -> String {
        format!("ADP config key '{}' equals {}.", self.key, self.expected)
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let deadline = started + self.timeout;
        let mut last_observed = None;
        let mut attempts = Attempts::default();

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.timed_out_result(started, &attempts, last_observed.as_ref());
            }

            if ctx.cancel_token.is_cancelled() || ctx.container_exit_token.is_cancelled() {
                return self.cancelled_result(started, &attempts, ctx);
            }

            let fetched = self.fetch_config(ctx, remaining.min(PROBE_TIMEOUT)).await;
            // Treat interruption as cancellation, not as evidence about the configuration.
            if ctx.cancel_token.is_cancelled() || ctx.container_exit_token.is_cancelled() {
                return self.cancelled_result(started, &attempts, ctx);
            }
            match fetched {
                Ok(config) => {
                    let actual = get_config_key(&config, &self.key).cloned();
                    if actual.as_ref() == Some(&self.expected) {
                        let message = format!("ADP config key '{}' equals {}.", self.key, self.expected);
                        return AssertionResult::new(self.name(), started, AssertionStatus::Passed, message);
                    }
                    attempts.observed(format_args!(
                        "the key was {}.",
                        actual
                            .as_ref()
                            .map(Value::to_string)
                            .unwrap_or_else(|| "<missing>".to_string())
                    ));
                    last_observed = actual;
                    trace!(key = %self.key, "ADP config value did not match, retrying...");
                }
                Err(e @ CommandError::Harness(_)) => {
                    trace!(key = %self.key, error = %e, "Failed to run the ADP config CLI, retrying...");
                    attempts.errored(format_args!("{e:#}"));
                }
                Err(e @ CommandError::Failed(_)) => {
                    trace!(key = %self.key, error = %e, "Failed to read ADP config with its CLI, retrying...");
                    attempts.observed(format_args!("{e:#}"));
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.timed_out_result(started, &attempts, last_observed.as_ref());
            }
            let retry_delay = remaining.min(POLL_INTERVAL);
            tokio::select! {
                _ = ctx.cancel_token.cancelled() => return self.cancelled_result(started, &attempts, ctx),
                _ = ctx.container_exit_token.cancelled() => return self.cancelled_result(started, &attempts, ctx),
                _ = tokio::time::sleep(retry_delay) => {}
            }
        }
    }
}

fn get_config_key<'a>(config: &'a Value, key: &str) -> Option<&'a Value> {
    let mut current = config;
    for part in key.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

/// Returns the default ADP `/config` endpoint.
pub fn default_adp_config_endpoint() -> String {
    DEFAULT_ADP_CONFIG_ENDPOINT.to_string()
}

#[cfg(all(test, unix))]
#[path = "adp_config_key_equals_tests.rs"]
mod tests;
