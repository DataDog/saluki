use std::time::{Duration, Instant};

use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde_json::Value;
use tracing::trace;

use crate::{
    actions::{execute_target_command, CommandDiagnostics},
    assertions::{Assertion, AssertionContext, AssertionResult},
};

const DEFAULT_ADP_CONFIG_ENDPOINT: &str = "https://localhost:55101/config";
const ADP_CONFIG_CLI_DIAGNOSTIC_LABEL: &str = "ADP configuration CLI command";
const CONFIG_POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_CONFIG_COMMAND_DURATION: Duration = Duration::from_secs(10);

#[derive(Clone, Copy)]
enum AdpConfigEndpoint {
    Source,
    Runtime,
}

impl AdpConfigEndpoint {
    fn parse(configured_endpoint: &str) -> Result<Self, GenericError> {
        let endpoint = reqwest::Url::parse(configured_endpoint).map_err(|_| unsupported_endpoint_error())?;
        if endpoint.scheme() != "https"
            || !endpoint.has_host()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(unsupported_endpoint_error());
        }

        match endpoint.path() {
            "/config" => Ok(Self::Source),
            "/config/internal" => Ok(Self::Runtime),
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
        "Unsupported ADP configuration endpoint. Expected an HTTPS URL with path exactly `/config` or \
         `/config/internal`, without credentials, a query, or a fragment."
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

    async fn fetch_config(&self, ctx: &AssertionContext, timeout: Duration) -> Result<Value, GenericError> {
        let args = self.endpoint.command_args();
        let command = ctx.adp_cli_command.with_args(&args);
        let diagnostics = CommandDiagnostics::Redacted(ADP_CONFIG_CLI_DIAGNOSTIC_LABEL);
        let stdout =
            execute_target_command(ctx, &command, &diagnostics, timeout, ctx.adp_cli_command.host_env()).await?;

        serde_json::from_str(&stdout).error_context("Failed to parse ADP config CLI JSON.")
    }

    fn timed_out_result(&self, started: Instant, last_observed: Option<&Value>) -> AssertionResult {
        AssertionResult {
            name: self.name().to_string(),
            passed: false,
            message: format!(
                "{} did not happen within {:?}. Last observed value: {}.",
                self.description(),
                self.timeout,
                last_observed
                    .map(Value::to_string)
                    .unwrap_or_else(|| "<missing>".to_string())
            ),
            duration: started.elapsed(),
        }
    }

    fn cancelled_result(&self, started: Instant) -> AssertionResult {
        AssertionResult {
            name: self.name().to_string(),
            passed: false,
            message: "Assertion cancelled because container exited.".to_string(),
            duration: started.elapsed(),
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

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.timed_out_result(started, last_observed.as_ref());
            }

            if ctx.cancel_token.is_cancelled() || ctx.container_exit_token.is_cancelled() {
                return self.cancelled_result(started);
            }

            let invocation_timeout = remaining.min(MAX_CONFIG_COMMAND_DURATION);
            match self.fetch_config(ctx, invocation_timeout).await {
                Ok(config) => {
                    let actual = get_config_key(&config, &self.key).cloned();
                    if actual.as_ref() == Some(&self.expected) {
                        return AssertionResult {
                            name: self.name().to_string(),
                            passed: true,
                            message: format!("ADP config key '{}' equals {}.", self.key, self.expected),
                            duration: started.elapsed(),
                        };
                    }
                    last_observed = actual;
                    trace!(key = %self.key, "ADP config value did not match, retrying...");
                }
                Err(e) => {
                    trace!(key = %self.key, error = %e, "Failed to read ADP config with its CLI, retrying...");
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.timed_out_result(started, last_observed.as_ref());
            }
            let retry_delay = remaining.min(CONFIG_POLL_INTERVAL);
            tokio::select! {
                _ = ctx.cancel_token.cancelled() => return self.cancelled_result(started),
                _ = ctx.container_exit_token.cancelled() => return self.cancelled_result(started),
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
