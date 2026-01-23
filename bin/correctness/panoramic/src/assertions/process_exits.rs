use std::time::{Duration, Instant};

use bollard::Docker;

use crate::assertions::{Assertion, AssertionContext, AssertionResult};

/// Assertion that checks the container process exits with a specific exit code.
pub struct ProcessExitsWithAssertion {
    expected_code: i64,
    timeout: Duration,
}

impl ProcessExitsWithAssertion {
    pub fn new(expected_code: i64, timeout: Duration) -> Self {
        Self { expected_code, timeout }
    }
}

#[async_trait::async_trait]
impl Assertion for ProcessExitsWithAssertion {
    fn name(&self) -> &'static str {
        "process_exits_with"
    }

    fn description(&self) -> String {
        format!(
            "Process exits with code {} within {:?}.",
            self.expected_code, self.timeout
        )
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();

        tokio::select! {
            // Wait for the container to exit (signaled via cancel_token).
            _ = ctx.cancel_token.cancelled() => {
                // Container exited - check exit code via Docker API
                let docker = match Docker::connect_with_defaults() {
                    Ok(d) => d,
                    Err(e) => {
                        return AssertionResult {
                            name: self.name().to_string(),
                            passed: false,
                            message: format!("Failed to connect to Docker: {}", e),
                            duration: started.elapsed(),
                        };
                    }
                };

                let info = docker.inspect_container(&ctx.container_name, None).await;

                match info {
                    Ok(container) => {
                        let exit_code = container.state
                            .and_then(|s| s.exit_code)
                            .unwrap_or(-1);

                        if exit_code == self.expected_code {
                            AssertionResult {
                                name: self.name().to_string(),
                                passed: true,
                                message: format!("Process exited with expected code {}.", exit_code),
                                duration: started.elapsed(),
                            }
                        } else {
                            AssertionResult {
                                name: self.name().to_string(),
                                passed: false,
                                message: format!(
                                    "Process exited with code {}, expected {}.",
                                    exit_code, self.expected_code
                                ),
                                duration: started.elapsed(),
                            }
                        }
                    }
                    Err(e) => AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: format!("Failed to inspect container: {}", e),
                        duration: started.elapsed(),
                    }
                }
            }

            // Timeout waiting for the container to exit.
            _ = tokio::time::sleep(self.timeout) => {
                AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!("Process did not exit within {:?}.", self.timeout),
                    duration: started.elapsed(),
                }
            }
        }
    }
}
