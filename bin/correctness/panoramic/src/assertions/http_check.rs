use std::time::{Duration, Instant};

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

    /// Resolve the endpoint URL, replacing container ports with mapped host ports.
    fn resolve_endpoint(&self, port_mappings: &std::collections::HashMap<String, u16>) -> String {
        let mut endpoint = self.endpoint.clone();

        for (internal_spec, host_port) in port_mappings {
            if let Some(internal_port) = internal_spec.split('/').next() {
                endpoint = endpoint
                    .replace(
                        &format!("localhost:{}", internal_port),
                        &format!("127.0.0.1:{}", host_port),
                    )
                    .replace(
                        &format!("127.0.0.1:{}", internal_port),
                        &format!("127.0.0.1:{}", host_port),
                    );
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

        let endpoint = self.resolve_endpoint(&ctx.port_mappings);

        let client = match ClientBuilder::new()
            .danger_accept_invalid_certs(self.insecure_skip_verify)
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!("Failed to build HTTP client: {}.", e),
                    duration: started.elapsed(),
                };
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

            match client.get(&endpoint).send().await {
                Ok(resp) => {
                    let actual = resp.status().as_u16();
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
