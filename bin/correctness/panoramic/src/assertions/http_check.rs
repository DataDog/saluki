use std::time::Duration;

use reqwest::ClientBuilder;
use saluki_error::GenericError;
use tokio::time::Instant;
use tracing::trace;

use crate::actions::docker_exec;
use crate::assertions::{Assertion, AssertionContext, AssertionResult, AssertionStatus, Attempts};
use crate::config::HttpStatusMatcher;
use crate::probe::{probe, ProbeError, POLL_INTERVAL, PROBE_TIMEOUT};

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
        let endpoint = self.resolve_endpoint(ctx);

        // Pick a probe strategy. Linux/host targets are reachable directly from the test runner,
        // so we use a real HTTP client. Windows targets are not (the container's listener is
        // bound to its own loopback / internal port), so we shell out to `curl.exe` via
        // `docker exec` inside the container.
        let http_probe = if ctx.target_is_windows() {
            HttpProbe::InContainerCurl {
                container_name: ctx.container_name.clone(),
                insecure_skip_verify: self.insecure_skip_verify,
            }
        } else {
            match ClientBuilder::new()
                .danger_accept_invalid_certs(self.insecure_skip_verify)
                .build()
            {
                Ok(client) => HttpProbe::HostClient { client },
                Err(e) => {
                    return AssertionResult::new(
                        self.name(),
                        started,
                        AssertionStatus::Errored,
                        format!("Failed to build HTTP client: {}.", e),
                    );
                }
            }
        };
        let what = http_probe.what(&endpoint);
        // Building the client can be slow on a busy machine, so the budget starts after it.
        let deadline = Instant::now() + self.timeout;
        let mut attempts = Attempts::default();

        loop {
            if ctx.cancel_token.is_cancelled() {
                return AssertionResult::new(
                    self.name(),
                    started,
                    attempts.status(),
                    attempts.explain_cancelled("Assertion cancelled."),
                );
            }
            if ctx.container_exit_token.is_cancelled() {
                let message = attempts.explain_cancelled("Assertion cancelled because container exited.");
                return AssertionResult::new(self.name(), started, AssertionStatus::Failed, message);
            }

            if Instant::now() > deadline {
                let summary = format!("{} did not happen within {:?}.", self.description(), self.timeout);
                return AssertionResult::new(
                    self.name(),
                    started,
                    attempts.status(),
                    attempts.explain_expired(&summary),
                );
            }

            let limit = deadline.saturating_duration_since(Instant::now()).min(PROBE_TIMEOUT);
            match probe(&what, limit, &ctx.cancel_token, http_probe.get_status(&endpoint)).await {
                Ok(Answer::Status(actual)) if self.status_matches(actual) => {
                    let message = format!("HTTP endpoint '{}' returned status {}.", endpoint, actual);
                    return AssertionResult::new(self.name(), started, AssertionStatus::Passed, message);
                }
                Ok(Answer::Status(actual)) => {
                    trace!(endpoint = %endpoint, actual, "HTTP check returned non-matching status, retrying...");
                    attempts.observed(format_args!("status {actual}"));
                }
                Ok(Answer::TargetError(error)) => {
                    trace!(endpoint = %endpoint, error = %error, "HTTP check request failed, retrying...");
                    attempts.observed(error);
                }
                Ok(Answer::HarnessError(error)) => {
                    trace!(endpoint = %endpoint, error = %error, "HTTP check could not send its request, retrying...");
                    attempts.errored(error);
                }
                Err(error @ ProbeError::Cancelled { .. }) => {
                    return AssertionResult::new(
                        self.name(),
                        started,
                        attempts.status(),
                        attempts.explain_cancelled(error),
                    );
                }
                // Direct requests test the target; a stalled Docker exec prevents evaluation.
                Err(error @ ProbeError::TimedOut { .. }) => match http_probe {
                    HttpProbe::HostClient { .. } => attempts.observed(error),
                    HttpProbe::InContainerCurl { .. } => attempts.errored(error),
                },
            }

            // The next iteration reports cancellation.
            ctx.cancel_token
                .run_until_cancelled(tokio::time::sleep(POLL_INTERVAL))
                .await;
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
    HostClient {
        client: reqwest::Client,
    },
    InContainerCurl {
        container_name: String,
        insecure_skip_verify: bool,
    },
}

/// The outcome of one HTTP request.
enum Answer {
    /// The endpoint answered with this status.
    Status(u16),
    /// The request failed, for example through a refused connection or `curl.exe` error.
    TargetError(String),
    /// The harness couldn't send the request.
    HarnessError(String),
}

impl HttpProbe {
    /// Names the request to `endpoint` for probe messages.
    fn what(&self, endpoint: &str) -> String {
        match self {
            Self::HostClient { .. } => format!("GET {endpoint}"),
            Self::InContainerCurl { container_name, .. } => {
                format!("GET {endpoint} with curl.exe in container '{container_name}'")
            }
        }
    }

    /// Sends one request to `endpoint`.
    async fn get_status(&self, endpoint: &str) -> Answer {
        match self {
            Self::HostClient { client } => match client.get(endpoint).send().await {
                Ok(resp) => Answer::Status(resp.status().as_u16()),
                Err(e) if e.is_builder() => Answer::HarnessError(format!("{:#}", GenericError::from(e))),
                Err(e) => Answer::TargetError(format!("{:#}", GenericError::from(e))),
            },
            Self::InContainerCurl {
                container_name,
                insecure_skip_verify,
            } => get_status_in_container(container_name, endpoint, *insecure_skip_verify).await,
        }
    }
}

async fn get_status_in_container(container_name: &str, endpoint: &str, insecure_skip_verify: bool) -> Answer {
    let endpoint = endpoint.replace("localhost", "127.0.0.1");
    let mut cmd = vec!["curl.exe".to_string()];
    if insecure_skip_verify {
        cmd.push("-k".to_string());
    }
    cmd.extend([
        // Abandoning a probe doesn't stop its exec, so curl bounds itself to keep retries from piling up.
        "--max-time".to_string(),
        PROBE_TIMEOUT.as_secs().to_string(),
        "-s".to_string(),
        "-o".to_string(),
        "NUL".to_string(),
        "-w".to_string(),
        "%{http_code}".to_string(),
        endpoint,
    ]);

    let output = match docker_exec(container_name, cmd).await {
        Ok(output) => output,
        Err(e) => return Answer::HarnessError(format!("{e:#}")),
    };
    if !output.succeeded() {
        return Answer::TargetError(format!("curl.exe exited {:?}.", output.exit_code));
    }
    match output.stdout.trim().parse::<u16>() {
        Ok(status) if status != 0 => Answer::Status(status),
        _ => Answer::TargetError(format!("curl.exe reported no status: '{}'.", output.stdout.trim())),
    }
}
