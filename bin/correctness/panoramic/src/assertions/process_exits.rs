use std::time::{Duration, Instant};

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
            _ = ctx.container_exit_token.cancelled() => {
                if ctx.is_native {
                    self.check_native(ctx, started)
                } else {
                    self.check_docker(ctx, started).await
                }
            }

            // Timeout waiting for the process to exit.
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

impl ProcessExitsWithAssertion {
    fn check_native(&self, ctx: &AssertionContext, started: Instant) -> AssertionResult {
        let cell = match ctx.native_exit_code.as_ref() {
            Some(c) => c,
            None => {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: "Native exit code cell not provided in AssertionContext.".to_string(),
                    duration: started.elapsed(),
                };
            }
        };
        let exit_code = match cell.get() {
            Some(Some(code)) => *code as i64,
            Some(None) => {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: "Process was terminated by signal; no exit code available.".to_string(),
                    duration: started.elapsed(),
                };
            }
            None => {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: "Exit token fired but exit code not yet recorded.".to_string(),
                    duration: started.elapsed(),
                };
            }
        };
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

    async fn check_docker(&self, ctx: &AssertionContext, started: Instant) -> AssertionResult {
        let docker: bollard::Docker = match airlock::docker::connect() {
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

        match docker.inspect_container(&ctx.container_name, None).await {
            Ok(container) => {
                let exit_code = container.state.and_then(|s| s.exit_code).unwrap_or(-1);
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
            },
        }
    }
}
