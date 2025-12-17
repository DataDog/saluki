use std::time::{Duration, Instant};

use tracing::trace;

use crate::assertions::{Assertion, AssertionContext, AssertionResult};

/// Assertion that checks an HTTP health endpoint.
pub struct HealthCheckAssertion {
    endpoint: String,
    expected_status: u16,
    timeout: Duration,
}

impl HealthCheckAssertion {
    pub fn new(endpoint: String, expected_status: u16, timeout: Duration) -> Self {
        Self {
            endpoint,
            expected_status,
            timeout,
        }
    }

    /// Resolve the endpoint URL, replacing container ports with mapped host ports.
    fn resolve_endpoint(&self, port_mappings: &std::collections::HashMap<String, u16>) -> String {
        let mut endpoint = self.endpoint.clone();

        // Try to replace localhost:PORT with the mapped port.
        for (internal_spec, host_port) in port_mappings {
            // Extract just the port number from "PORT/protocol".
            if let Some(internal_port) = internal_spec.split('/').next() {
                // Replace patterns like "localhost:PORT" or "127.0.0.1:PORT".
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
}

#[async_trait::async_trait]
impl Assertion for HealthCheckAssertion {
    fn name(&self) -> &'static str {
        "health_check"
    }

    fn description(&self) -> String {
        format!(
            "HTTP endpoint '{}' returns status {}.",
            self.endpoint, self.expected_status
        )
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let deadline = Instant::now() + self.timeout;

        let endpoint = self.resolve_endpoint(&ctx.port_mappings);

        loop {
            if Instant::now() > deadline {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!(
                        "Health check at '{}' did not return status {} after {:?}.",
                        endpoint, self.expected_status, self.timeout
                    ),
                    duration: started.elapsed(),
                };
            }

            if ctx.cancel_token.is_cancelled() {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: "Assertion cancelled because container exited.".to_string(),
                    duration: started.elapsed(),
                };
            }

            // Try to make the HTTP request.
            match make_http_request(&endpoint).await {
                Ok(status) if status == self.expected_status => {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: true,
                        message: format!(
                            "Health check at '{}' returned status {}.",
                            endpoint, self.expected_status
                        ),
                        duration: started.elapsed(),
                    };
                }
                Ok(status) => {
                    trace!(
                        endpoint = %endpoint,
                        expected = self.expected_status,
                        actual = status,
                        "Health check returned unexpected status, retrying..."
                    );
                }
                Err(e) => {
                    trace!(
                        endpoint = %endpoint,
                        error = %e,
                        "Health check request failed, retrying..."
                    );
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Make an HTTP GET request and return the status code.
async fn make_http_request(url: &str) -> Result<u16, String> {
    // Use a simple TCP connection and manual HTTP request to avoid heavy dependencies.
    let url = url.parse::<url::Url>().map_err(|e| format!("Invalid URL: {}.", e))?;

    let host = url.host_str().ok_or("No host in URL.")?;
    let port = url.port().unwrap_or(80);
    let path = url.path();
    let query = url.query().map(|q| format!("?{}", q)).unwrap_or_default();

    let addr = format!("{}:{}", host, port);
    let mut stream = tokio::net::TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("Connection failed: {}.", e))?;

    // Send HTTP request.
    let request = format!(
        "GET {}{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, query, host
    );

    tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
        .await
        .map_err(|e| format!("Write failed: {}.", e))?;

    // Read response.
    let mut response = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
        .await
        .map_err(|e| format!("Read failed: {}.", e))?;

    // Parse status code from response.
    let response_str = String::from_utf8_lossy(&response);
    let first_line = response_str.lines().next().ok_or("Empty response.")?;

    // Parse "HTTP/1.1 200 OK" -> 200.
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1]
            .parse::<u16>()
            .map_err(|_| format!("Invalid status code: {}.", parts[1]))
    } else {
        Err(format!("Invalid HTTP response: {}.", first_line))
    }
}
