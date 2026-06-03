use std::time::{Duration, Instant};

use airlock::docker;
use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecResults},
};
use futures::TryStreamExt as _;
use reqwest::ClientBuilder;
use tracing::trace;

use crate::assertions::{Assertion, AssertionContext, AssertionResult};
use crate::config::HttpStatusMatcher;

/// Assertion that probes an HTTP/HTTPS endpoint and checks the response status code.
///
/// - Supports both `http://` and `https://` schemes. For HTTPS, callers may opt into skipping
///   certificate verification via `insecure_skip_verify` to allow self-signed certs.
/// - The status code matcher supports either "must equal" or "must not equal" semantics, so tests
///   can assert that a route is registered (for example, status code is anything other than 404)
///   without having to know the exact status the endpoint would return.
pub struct HttpCheckAssertion {
    endpoint: String,
    status: HttpStatusMatcher,
    insecure_skip_verify: bool,
    timeout: Duration,
}

impl HttpCheckAssertion {
    pub fn new(endpoint: String, status: HttpStatusMatcher, insecure_skip_verify: bool, timeout: Duration) -> Self {
        Self {
            endpoint,
            status,
            insecure_skip_verify,
            timeout,
        }
    }

    /// Resolve the endpoint URL, replacing container ports with the address reachable from the test runner.
    fn resolve_endpoint(&self, ctx: &AssertionContext) -> String {
        let mut endpoint = self.endpoint.clone();

        for (internal_spec, host_port) in &ctx.port_mappings {
            if let Some(internal_port) = internal_spec.split('/').next() {
                let replacement = match ctx.container_ip.as_deref() {
                    Some(container_ip) => format!("{}:{}", container_ip, internal_port),
                    None => format!("127.0.0.1:{}", host_port),
                };
                endpoint = endpoint
                    .replace(&format!("localhost:{}", internal_port), &replacement)
                    .replace(&format!("127.0.0.1:{}", internal_port), &replacement);
            }
        }

        endpoint
    }

    fn status_matches(&self, actual: u16) -> bool {
        match self.status {
            HttpStatusMatcher::Equal(expected) => actual == expected,
            HttpStatusMatcher::NotEqual(forbidden) => actual != forbidden,
        }
    }
}

#[async_trait::async_trait]
impl Assertion for HttpCheckAssertion {
    fn name(&self) -> &'static str {
        "http_check"
    }

    fn description(&self) -> String {
        match &self.status {
            HttpStatusMatcher::Equal(code) => {
                format!("HTTP endpoint '{}' returns status {}.", self.endpoint, code)
            }
            HttpStatusMatcher::NotEqual(code) => {
                format!(
                    "HTTP endpoint '{}' returns any status other than {}.",
                    self.endpoint, code
                )
            }
        }
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let deadline = started + self.timeout;

        let endpoint = self.resolve_endpoint(ctx);

        let client = if ctx.use_container_exec_for_network_checks {
            None
        } else {
            match ClientBuilder::new()
                .danger_accept_invalid_certs(self.insecure_skip_verify)
                .build()
            {
                Ok(client) => Some(client),
                Err(e) => {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: format!("Failed to build HTTP client: {}.", e),
                        duration: started.elapsed(),
                    };
                }
            }
        };

        loop {
            if Instant::now() > deadline {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!("{} did not happen within {:?}.", self.description(), self.timeout),
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

            let response_status = if ctx.use_container_exec_for_network_checks {
                get_status_in_container(&ctx.container_name, &endpoint).await
            } else if let Some(client) = client.as_ref() {
                match client.get(&endpoint).send().await {
                    Ok(resp) => Ok(Some(resp.status().as_u16())),
                    Err(e) => Err(e.to_string()),
                }
            } else {
                Ok(None)
            };

            match response_status {
                Ok(Some(actual)) => {
                    if self.status_matches(actual) {
                        return AssertionResult {
                            name: self.name().to_string(),
                            passed: true,
                            message: format!("HTTP endpoint '{}' returned status {}.", endpoint, actual),
                            duration: started.elapsed(),
                        };
                    }
                    trace!(
                        endpoint = %endpoint,
                        actual = actual,
                        "HTTP check returned non-matching status, retrying..."
                    );
                }
                Ok(None) => {
                    trace!(endpoint = %endpoint, "HTTP check produced no status, retrying...");
                }
                Err(e) => {
                    trace!(
                        endpoint = %endpoint,
                        error = %e,
                        "HTTP check request failed, retrying..."
                    );
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

async fn get_status_in_container(container_name: &str, endpoint: &str) -> Result<Option<u16>, String> {
    let docker = docker::connect().map_err(|e| format!("Failed to connect to Docker: {}", e))?;
    let endpoint = endpoint.replace("localhost", "127.0.0.1");
    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions::<String> {
                cmd: Some(vec![
                    "curl.exe".to_string(),
                    "-k".to_string(),
                    "-s".to_string(),
                    "-o".to_string(),
                    "NUL".to_string(),
                    "-w".to_string(),
                    "%{http_code}".to_string(),
                    endpoint,
                ]),
                attach_stdout: Some(true),
                attach_stderr: Some(false),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("Failed to create exec: {}", e))?;
    let exec_id = exec.id.clone();
    let result = docker
        .start_exec(&exec_id, None)
        .await
        .map_err(|e| format!("Failed to start exec: {}", e))?;

    let mut stdout = String::new();
    if let StartExecResults::Attached { mut output, .. } = result {
        while let Some(chunk) = output
            .try_next()
            .await
            .map_err(|e| format!("Failed to read exec output: {}", e))?
        {
            if let LogOutput::StdOut { message } = chunk {
                stdout.push_str(&String::from_utf8_lossy(&message));
            }
        }
    }

    let inspect = docker
        .inspect_exec(&exec_id)
        .await
        .map_err(|e| format!("Failed to inspect exec: {}", e))?;
    if inspect.exit_code != Some(0) {
        return Ok(None);
    }

    Ok(stdout.trim().parse::<u16>().ok().filter(|status| *status != 0))
}
