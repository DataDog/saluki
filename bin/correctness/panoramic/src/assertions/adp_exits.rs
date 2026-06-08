use std::time::{Duration, Instant};

use crate::{
    assertions::{Assertion, AssertionContext, AssertionResult},
    config::LogStream,
};

/// Assertion that checks ADP exited with a specific exit code.
///
/// The detection mechanism differs per runtime because ADP isn't a top-level process in every
/// case:
///
/// - **`linux`**: ADP runs under s6 inside the converged container; s6 keeps the container
///   alive across ADP restarts and logs `"agent-data-plane exited with code N"` from
///   `docker/s6-services/agent-data-plane/finish` when ADP exits. We grep the captured log
///   buffer for that line. The container exit code is the s6 exit code, not ADP's, so we
///   deliberately do not consult `docker_container_exit_code` here.
/// - **`windows`**: ADP is the top-level process inside the test container (the entrypoint
///   `& "C:\\adp\\agent-data-plane.exe"; exit $LASTEXITCODE`), so the container exit code is
///   ADP's exit code. We use that as a fallback when the supervisor log line is unavailable.
/// - **`mac`** (and any host-process runtime): the Unix runner observes ADP's child process
///   exit directly and records the exit code in the shared cell on
///   [`AssertionContext::host_process_exit_code`]; we read it from there.
pub struct AdpExitsWithAssertion {
    expected_code: i64,
    timeout: Duration,
}

impl AdpExitsWithAssertion {
    pub fn new(expected_code: i64, timeout: Duration) -> Self {
        Self { expected_code, timeout }
    }
}

#[async_trait::async_trait]
impl Assertion for AdpExitsWithAssertion {
    fn name(&self) -> &'static str {
        "adp_exits_with"
    }

    fn description(&self) -> String {
        format!("ADP exits with code {} within {:?}.", self.expected_code, self.timeout)
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        if ctx.is_host_process {
            self.check_native(ctx, started).await
        } else {
            self.check_docker_via_supervisor_log(ctx, started).await
        }
    }
}

impl AdpExitsWithAssertion {
    async fn check_native(&self, ctx: &AssertionContext, started: Instant) -> AssertionResult {
        let cell = match ctx.host_process_exit_code.as_ref() {
            Some(c) => c.clone(),
            None => {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: "Host-process exit code cell not provided in AssertionContext.".to_string(),
                    duration: started.elapsed(),
                };
            }
        };

        // Wait until either the exit token fires (process exited or was cleaned up) or the
        // timeout elapses.
        tokio::select! {
            _ = ctx.container_exit_token.cancelled() => {}
            _ = tokio::time::sleep(self.timeout) => {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!("ADP did not exit within {:?}.", self.timeout),
                    duration: started.elapsed(),
                };
            }
        }

        match cell.get() {
            Some(Some(code)) => {
                let code = *code as i64;
                if code == self.expected_code {
                    AssertionResult {
                        name: self.name().to_string(),
                        passed: true,
                        message: format!("ADP exited with expected code {}.", code),
                        duration: started.elapsed(),
                    }
                } else {
                    AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: format!("ADP exited with code {}, expected {}.", code, self.expected_code),
                        duration: started.elapsed(),
                    }
                }
            }
            Some(None) => AssertionResult {
                name: self.name().to_string(),
                passed: false,
                message: "ADP was terminated by signal; no exit code available.".to_string(),
                duration: started.elapsed(),
            },
            None => AssertionResult {
                name: self.name().to_string(),
                passed: false,
                message: "Exit token fired but exit code not yet recorded.".to_string(),
                duration: started.elapsed(),
            },
        }
    }

    async fn check_docker_via_supervisor_log(&self, ctx: &AssertionContext, started: Instant) -> AssertionResult {
        // s6 writes `agent-data-plane exited with code N` to the container's log stream when
        // ADP exits. Poll the captured log buffer for that line.
        let pattern = format!("agent-data-plane exited with code {}", self.expected_code);
        let deadline = Instant::now() + self.timeout;
        loop {
            if Instant::now() > deadline {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!(
                        "Did not observe ADP exit with code {} within {:?}.",
                        self.expected_code, self.timeout
                    ),
                    duration: started.elapsed(),
                };
            }
            if ctx.cancel_token.is_cancelled() {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: "Assertion cancelled.".to_string(),
                    duration: started.elapsed(),
                };
            }
            // Only treat the container exit code as ADP's exit code on Windows, where ADP is
            // the top-level process. On Linux the converged image runs s6, and the container
            // exit code reflects s6 (or whatever else exited last), not ADP itself.
            if ctx.target_is_windows() && ctx.container_exit_token.is_cancelled() {
                if let Some(code) = ctx
                    .docker_container_exit_code
                    .as_ref()
                    .and_then(|cell| cell.get().copied())
                {
                    let passed = code == self.expected_code;
                    let message = if passed {
                        format!("Container exited with expected ADP code {}.", code)
                    } else {
                        format!("Container exited with code {}, expected {}.", code, self.expected_code)
                    };
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed,
                        message,
                        duration: started.elapsed(),
                    };
                }
            }
            let matched = {
                let buf = ctx.log_buffer.read().unwrap();
                buf.contains_match(&pattern, false, &LogStream::Both)
            };
            if matched {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: true,
                    message: format!("Observed ADP exit with expected code {}.", self.expected_code),
                    duration: started.elapsed(),
                };
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}
