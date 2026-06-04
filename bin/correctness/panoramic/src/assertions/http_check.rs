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

    /// Resolve the endpoint URL, replacing container ports with the address reachable from the
    /// probe site.
    ///
    /// Host-side probes substitute the mapped ephemeral port on `127.0.0.1`. In-container probes
    /// (currently only the Windows runtime) substitute the container's primary network IP and
    /// keep the original internal port.
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

        // Pick a probe strategy. Linux/host targets are reachable directly from the test runner,
        // so we use a real HTTP client. Windows targets are not (the container's listener is
        // bound to its own loopback / internal port), so we shell out to `curl.exe` via
        // `docker exec` inside the container.
        let probe = if ctx.target_is_windows() {
            HttpProbe::InContainerCurl {
                container_name: ctx.container_name.clone(),
            }
        } else {
            match ClientBuilder::new()
                .danger_accept_invalid_certs(self.insecure_skip_verify)
                .build()
            {
                Ok(client) => HttpProbe::HostClient { client },
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

            let response_status = probe.get_status(&endpoint).await;

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

/// HTTP probe site for `http_check` assertions.
///
/// `HostClient` uses a `reqwest` client running on the test runner; this is the standard path
/// for Linux containers (which expose listeners on mapped ephemeral host ports) and host-process
/// runtimes. `InContainerCurl` shells out to `curl.exe` via `docker exec` inside the target
/// container; this is the path for Windows containers, where the listener is reachable only
/// from inside the container itself.
enum HttpProbe {
    HostClient { client: reqwest::Client },
    InContainerCurl { container_name: String },
}

impl HttpProbe {
    /// Returns the HTTP status code observed for `endpoint`, if any.
    ///
    /// `Ok(Some(status))` is the only success path. `Ok(None)` means the request did not
    /// produce a status (for example, `curl.exe` exited non-zero); `Err` is reserved for
    /// transport-level failures the caller should log and retry.
    async fn get_status(&self, endpoint: &str) -> Result<Option<u16>, String> {
        match self {
            Self::HostClient { client } => match client.get(endpoint).send().await {
                Ok(resp) => Ok(Some(resp.status().as_u16())),
                Err(e) => Err(e.to_string()),
            },
            Self::InContainerCurl { container_name } => get_status_in_container(container_name, endpoint).await,
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
