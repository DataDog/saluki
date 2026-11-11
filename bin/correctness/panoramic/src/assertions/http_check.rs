use std::time::Duration;

use reqwest::ClientBuilder;
use saluki_error::GenericError;
use tokio::time::Instant;
use tracing::trace;

use crate::actions::docker_exec;
use crate::assertions::{Assertion, AssertionContext, AssertionResult, AssertionStatus, Attempts};
use crate::config::{HttpBodyMatcher, HttpStatusMatcher};
use crate::core::probe::{probe, AttemptLimits, ProbeError, POLL_INTERVAL};

/// Assertion that probes an HTTP/HTTPS endpoint and checks the response status code, and optionally its body.
///
/// - Supports both `http://` and `https://` schemes. For HTTPS, callers may opt into skipping
///   certificate verification via `insecure_skip_verify` to allow self-signed certs.
/// - The status code matcher supports either "must equal" or "must not equal" semantics, so tests
///   can assert that a route is registered (for example, status code is anything other than 404)
///   without having to know the exact status the endpoint would return.
/// - The optional body matcher requires the response body to equal an exact value. When it is set,
///   a response passes only if both the status and the body match.
pub struct HttpCheckAssertion {
    endpoint: String,
    status: HttpStatusMatcher,
    body: Option<HttpBodyMatcher>,
    insecure_skip_verify: bool,
    timeout: Duration,
}

impl HttpCheckAssertion {
    pub fn new(
        endpoint: String, status: HttpStatusMatcher, body: Option<HttpBodyMatcher>, insecure_skip_verify: bool,
        timeout: Duration,
    ) -> Self {
        Self {
            endpoint,
            status,
            body,
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

    fn body_matches(&self, actual: &str) -> bool {
        match &self.body {
            None => true,
            Some(HttpBodyMatcher::Equal(expected)) => actual == expected,
        }
    }
}

#[async_trait::async_trait]
impl Assertion for HttpCheckAssertion {
    fn name(&self) -> &'static str {
        "http_check"
    }

    fn description(&self) -> String {
        let status = match &self.status {
            HttpStatusMatcher::Equal(code) => format!("status {code}"),
            HttpStatusMatcher::NotEqual(code) => format!("any status other than {code}"),
        };
        match &self.body {
            None => format!("HTTP endpoint '{}' returns {}.", self.endpoint, status),
            Some(HttpBodyMatcher::Equal(body)) => {
                format!(
                    "HTTP endpoint '{}' returns {} with body {:?}.",
                    self.endpoint, status, body
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

            let limits = AttemptLimits::new(deadline.saturating_duration_since(Instant::now()));
            match probe(
                &what,
                limits.attempt,
                &ctx.cancel_token,
                http_probe.get(&endpoint, self.body.is_some(), &limits),
            )
            .await
            {
                Ok(Answer::Response { status, body }) if self.status_matches(status) && self.body_matches(&body) => {
                    let message = format!("HTTP endpoint '{}' returned status {}.", endpoint, status);
                    return AssertionResult::new(self.name(), started, AssertionStatus::Passed, message);
                }
                Ok(Answer::Response { status, body }) if self.status_matches(status) => {
                    trace!(endpoint = %endpoint, status, body, "HTTP check returned non-matching body, retrying...");
                    attempts.observed(format_args!("status {status} with body {body:?}"));
                }
                Ok(Answer::Response { status, .. }) => {
                    trace!(endpoint = %endpoint, status, "HTTP check returned non-matching status, retrying...");
                    attempts.observed(format_args!("status {status}"));
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
    /// The endpoint answered with this status and body.
    ///
    /// `body` is empty unless the caller asked for it.
    Response { status: u16, body: String },
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

    /// Sends one GET request to `endpoint`, reading the response body when `read_body` is set.
    async fn get(&self, endpoint: &str, read_body: bool, limits: &AttemptLimits) -> Answer {
        match self {
            Self::HostClient { client } => {
                let resp = match client.get(endpoint).send().await {
                    Ok(resp) => resp,
                    Err(e) if e.is_builder() => return Answer::HarnessError(format!("{:#}", GenericError::from(e))),
                    Err(e) => return Answer::TargetError(format!("{:#}", GenericError::from(e))),
                };
                let status = resp.status().as_u16();
                if !read_body {
                    return Answer::Response {
                        status,
                        body: String::new(),
                    };
                }
                match resp.text().await {
                    Ok(body) => Answer::Response { status, body },
                    Err(e) => Answer::TargetError(format!("{:#}", GenericError::from(e))),
                }
            }
            Self::InContainerCurl {
                container_name,
                insecure_skip_verify,
            } => {
                let cmd = curl_command(endpoint, *insecure_skip_verify, read_body, limits);
                get_in_container(container_name, cmd).await
            }
        }
    }
}

/// Returns a `curl.exe` command that sends a GET request to `endpoint` and prints the status on the last line of its output.
///
/// With `read_body`, the response body precedes the status; otherwise it is discarded.
fn curl_command(endpoint: &str, insecure_skip_verify: bool, read_body: bool, limits: &AttemptLimits) -> Vec<String> {
    let endpoint = endpoint.replace("localhost", "127.0.0.1");
    let mut cmd = vec!["curl.exe".to_string()];
    if insecure_skip_verify {
        cmd.push("-k".to_string());
    }
    cmd.extend(["--max-time".to_string(), limits.curl_max_time(), "-s".to_string()]);
    if !read_body {
        cmd.extend(["-o".to_string(), "NUL".to_string()]);
    }
    // curl expands `\n` in the write-out format, which keeps the status on its own line after the body.
    cmd.extend(["-w".to_string(), "\\n%{http_code}".to_string(), endpoint]);
    cmd
}

async fn get_in_container(container_name: &str, cmd: Vec<String>) -> Answer {
    let output = match docker_exec(container_name, cmd).await {
        Ok(output) => output,
        Err(e) => return Answer::HarnessError(format!("{e:#}")),
    };
    if !output.succeeded() {
        return Answer::TargetError(format!("curl.exe exited {:?}.", output.exit_code));
    }
    parse_curl_output(&output.stdout)
}

/// Splits the output of a [`curl_command`] into the response body and the status on its last line.
fn parse_curl_output(stdout: &str) -> Answer {
    let (body, status) = stdout.rsplit_once('\n').unwrap_or(("", stdout));
    match status.trim().parse::<u16>() {
        Ok(status) if status != 0 => Answer::Response {
            status,
            body: body.to_string(),
        },
        _ => Answer::TargetError(format!("curl.exe reported no status: '{}'.", stdout.trim())),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use airlock::driver::ContainerOs;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    use super::{curl_command, parse_curl_output, Answer, HttpCheckAssertion};
    use crate::actions::test_docker::{curl_max_time, Exec, FakeDocker};
    use crate::assertions::test_support::host_context;
    use crate::assertions::{Assertion as _, AssertionResult, AssertionStatus};
    use crate::config::{HttpBodyMatcher, HttpStatusMatcher};
    use crate::core::probe::AttemptLimits;
    use crate::intake::test_server::install_crypto_provider;

    /// Checks a Windows target for `timeout`, with its execs served by `docker`.
    async fn check_windows_target(docker: &FakeDocker, timeout: Duration) -> AssertionResult {
        let mut ctx = host_context(CancellationToken::new());
        ctx.is_host_process = false;
        ctx.target_os = Some(ContainerOs::Windows);
        ctx.container_name = "windows-target".to_string();
        let assertion = HttpCheckAssertion::new(
            "https://localhost:5001/health".to_string(),
            HttpStatusMatcher::Equal(200),
            None,
            true,
            timeout,
        );

        docker.scope(assertion.check(&ctx)).await
    }

    #[test]
    fn curl_gets_the_request_limit_of_the_attempt() {
        for remaining in [Duration::from_secs(60), Duration::from_secs(3)] {
            let limits = AttemptLimits::new(remaining);

            let cmd = curl_command("https://localhost:5001/health", true, false, &limits);

            let max_time = cmd
                .iter()
                .position(|arg| arg == "--max-time")
                .and_then(|index| cmd.get(index + 1))
                .unwrap_or_else(|| panic!("curl should bound itself: {cmd:?}"));
            assert_eq!(*max_time, limits.curl_max_time(), "{remaining:?}");
            assert_eq!(cmd.last().map(String::as_str), Some("https://127.0.0.1:5001/health"));
        }
    }

    /// Serves `body` with status 200 to every connection, and returns the URL of the server.
    async fn serve_body(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("should bind a listener");
        let addr = listener.local_addr().expect("listener should have an address");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0; 1024];
                let _ = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}/")
    }

    /// Checks `endpoint` from the host for a status of 200 and `body`.
    async fn check_host_body(endpoint: String, body: Option<&str>) -> AssertionResult {
        let assertion = HttpCheckAssertion::new(
            endpoint,
            HttpStatusMatcher::Equal(200),
            body.map(|body| HttpBodyMatcher::Equal(body.to_string())),
            false,
            Duration::from_secs(1),
        );
        install_crypto_provider();
        assertion.check(&host_context(CancellationToken::new())).await
    }

    #[tokio::test]
    async fn a_matching_body_passes_the_check() {
        let endpoint = serve_body("OK\n").await;

        let result = check_host_body(endpoint, Some("OK\n")).await;

        assert_eq!(result.status, AssertionStatus::Passed, "{}", result.message);
    }

    #[tokio::test]
    async fn a_different_body_fails_the_check() {
        let endpoint = serve_body("not OK").await;

        let unchecked = check_host_body(endpoint.clone(), None).await;
        let checked = check_host_body(endpoint, Some("OK\n")).await;

        assert_eq!(unchecked.status, AssertionStatus::Passed, "{}", unchecked.message);
        assert_eq!(checked.status, AssertionStatus::Failed, "{}", checked.message);
        assert!(checked.message.contains(r#"body "not OK""#), "{}", checked.message);
    }

    #[test]
    fn curl_keeps_the_body_only_when_asked() {
        let limits = AttemptLimits::new(Duration::from_secs(10));

        let discarding = curl_command("http://localhost:8126/services", false, false, &limits);
        let reading = curl_command("http://localhost:8126/services", false, true, &limits);

        assert!(discarding.iter().any(|arg| arg == "NUL"), "{discarding:?}");
        assert!(!reading.iter().any(|arg| arg == "NUL"), "{reading:?}");
    }

    #[test]
    fn curl_output_splits_into_body_and_status() {
        let cases = [
            ("\n200", 200, ""),
            ("OK\n\n200", 200, "OK\n"),
            ("line one\nline two\n404", 404, "line one\nline two"),
        ];
        for (stdout, expected_status, expected_body) in cases {
            match parse_curl_output(stdout) {
                Answer::Response { status, body } => {
                    assert_eq!((status, body.as_str()), (expected_status, expected_body), "{stdout:?}");
                }
                _ => panic!("{stdout:?} should parse as a response"),
            }
        }
        for stdout in ["000", "\n000", ""] {
            assert!(
                matches!(parse_curl_output(stdout), Answer::TargetError(_)),
                "{stdout:?} should report no status"
            );
        }
    }

    #[tokio::test]
    async fn a_target_that_never_answers_curl_fails_the_check() {
        // When curl shared the attempt's limit, Docker overhead let the attempt time out first, so a target that never
        // answered left only harness errors and the step errored instead of failing.
        //
        // The first exec is curl waiting out its `--max-time` for a target that accepted the connection but never
        // answered. Later execs never finish, so they add no observation of the target: the step fails only if the
        // first attempt observed curl's own timeout. A 3 s step gets a short attempt; a 10.5 s step gets a full one.
        let check = |timeout| async move {
            let docker = FakeDocker::start(Duration::from_millis(50), |index, command| match index {
                0 => Exec::Exits {
                    duration: curl_max_time(command),
                    stdout: "000".to_string(),
                    exit_code: 28,
                },
                _ => Exec::Stalled,
            })
            .await;
            let result = check_windows_target(&docker, timeout).await;
            (result, docker.launched())
        };

        let ((short, short_launched), (full, full_launched)) =
            tokio::join!(check(Duration::from_secs(3)), check(Duration::from_millis(10_500)));

        assert_eq!(
            [short.status, full.status],
            [AssertionStatus::Failed; 2],
            "3s: {}\n10.5s: {}",
            short.message,
            full.message
        );
        for launched in [short_launched, full_launched] {
            assert_eq!(launched.len(), 2, "{launched:?}");
            assert_eq!(launched[0][0], "curl.exe");
        }
    }

    #[tokio::test]
    async fn a_docker_exec_that_never_finishes_errors_the_check() {
        // Neither a stalled start nor unfinished output establishes a product observation. Cover both on Windows too;
        // Running was previously used only by Unix-gated config tests, breaking Windows builds under deny(warnings).
        for (phase, execution) in [("start", Exec::Stalled), ("output", Exec::Running)] {
            let docker = FakeDocker::start(Duration::ZERO, move |_, _| execution.clone()).await;

            let result = check_windows_target(&docker, Duration::from_secs(1)).await;

            assert_eq!(result.status, AssertionStatus::Errored, "{phase}: {}", result.message);
            assert!(result.message.contains("Timed out"), "{phase}: {}", result.message);
            assert_eq!(docker.launched().len(), 1, "{phase}");
        }
    }
}
