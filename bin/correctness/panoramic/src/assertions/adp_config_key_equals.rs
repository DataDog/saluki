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
use tracing::trace;

use crate::assertions::{Assertion, AssertionContext, AssertionResult};

const DEFAULT_ADP_CONFIG_ENDPOINT: &str = "https://localhost:55101/config";

/// Assertion that polls ADP's `/config` endpoint until one key equals the expected value.
pub struct AdpConfigKeyEqualsAssertion {
    key: String,
    expected: Value,
    endpoint: String,
    timeout: Duration,
}

impl AdpConfigKeyEqualsAssertion {
    pub fn new(key: String, expected: Value, endpoint: String, timeout: Duration) -> Self {
        Self {
            key,
            expected,
            endpoint,
            timeout,
        }
    }

    fn resolve_endpoint(&self, ctx: &AssertionContext) -> String {
        let mut endpoint = self.endpoint.clone();

        for (internal_spec, host_port) in &ctx.port_mappings {
            if let Some(internal_port) = internal_spec.split('/').next() {
                let replacement = if ctx.target_is_windows() {
                    let host = ctx.container_ip.as_deref().unwrap_or("127.0.0.1");
                    format!("{}:{}", host, internal_port)
                } else {
                    format!("127.0.0.1:{}", host_port)
                };
                endpoint = endpoint
                    .replace(&format!("localhost:{}", internal_port), &replacement)
                    .replace(&format!("127.0.0.1:{}", internal_port), &replacement);
            }
        }

        endpoint
    }
}

#[async_trait::async_trait]
impl Assertion for AdpConfigKeyEqualsAssertion {
    fn name(&self) -> &'static str {
        "adp_config_key_equals"
    }

    fn description(&self) -> String {
        format!("ADP /config key '{}' equals {}.", self.key, self.expected)
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let deadline = started + self.timeout;
        let endpoint = self.resolve_endpoint(ctx);
        let probe = match ConfigProbe::new(ctx).await {
            Ok(probe) => probe,
            Err(e) => {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: e.to_string(),
                    duration: started.elapsed(),
                };
            }
        };
        let mut last_observed = None;

        loop {
            if Instant::now() > deadline {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!(
                        "{} did not happen within {:?}. Last observed value: {}.",
                        self.description(),
                        self.timeout,
                        last_observed
                            .as_ref()
                            .map(Value::to_string)
                            .unwrap_or_else(|| "<missing>".to_string())
                    ),
                    duration: started.elapsed(),
                };
            }

            if ctx.cancel_token.is_cancelled() || ctx.container_exit_token.is_cancelled() {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: "Assertion cancelled because container exited.".to_string(),
                    duration: started.elapsed(),
                };
            }

            match probe.fetch_config(&endpoint).await {
                Ok(config) => {
                    let actual = get_config_key(&config, &self.key).cloned();
                    if actual.as_ref() == Some(&self.expected) {
                        return AssertionResult {
                            name: self.name().to_string(),
                            passed: true,
                            message: format!("ADP /config key '{}' equals {}.", self.key, self.expected),
                            duration: started.elapsed(),
                        };
                    }
                    last_observed = actual;
                    trace!(key = %self.key, endpoint = %endpoint, "ADP config value did not match, retrying...");
                }
                Err(e) => {
                    trace!(endpoint = %endpoint, error = %e, "Failed to fetch ADP config, retrying...");
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

enum ConfigProbe {
    HostClient { client: reqwest::Client },
    InContainerCurl { container_name: String },
}

impl ConfigProbe {
    async fn new(ctx: &AssertionContext) -> Result<Self, GenericError> {
        if ctx.target_is_windows() {
            Ok(Self::InContainerCurl {
                container_name: ctx.container_name.clone(),
            })
        } else {
            let client = ClientBuilder::new()
                .danger_accept_invalid_certs(true)
                .build()
                .error_context("Failed to build HTTP client.")?;
            Ok(Self::HostClient { client })
        }
    }

    async fn fetch_config(&self, endpoint: &str) -> Result<Value, GenericError> {
        let body = match self {
            Self::HostClient { client } => {
                let resp = client
                    .get(endpoint)
                    .send()
                    .await
                    .error_context("Failed to send request.")?;
                let status = resp.status();
                let body = resp.text().await.error_context("Failed to read response body.")?;
                if !status.is_success() {
                    return Err(generic_error!("ADP /config returned status {}: {}", status, body));
                }
                body
            }
            Self::InContainerCurl { container_name } => fetch_config_in_container(container_name, endpoint).await?,
        };

        serde_json::from_str(&body).error_context("Failed to parse ADP /config JSON.")
    }
}

async fn fetch_config_in_container(container_name: &str, endpoint: &str) -> Result<String, GenericError> {
    let docker = docker::connect().error_context("Failed to connect to Docker.")?;
    let endpoint = endpoint.replace("localhost", "127.0.0.1");
    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions::<String> {
                cmd: Some(vec!["curl.exe".to_string(), "-ks".to_string(), endpoint]),
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

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let StartExecResults::Attached { mut output, .. } = result {
        while let Some(chunk) = output.try_next().await.error_context("Failed to read exec output.")? {
            match chunk {
                LogOutput::StdOut { message } => stdout.push_str(&String::from_utf8_lossy(&message)),
                LogOutput::StdErr { message } => stderr.push_str(&String::from_utf8_lossy(&message)),
                _ => {}
            }
        }
    }

    let inspect = docker
        .inspect_exec(&exec_id)
        .await
        .error_context("Failed to inspect exec.")?;
    if inspect.exit_code != Some(0) {
        return Err(generic_error!("curl.exe exited {:?}: {}", inspect.exit_code, stderr));
    }

    Ok(stdout)
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
