use std::time::{Duration, Instant};

use tracing::trace;

use crate::{
    assertions::{Assertion, AssertionContext, AssertionResult},
    config::LogStream,
};

/// Assertion that checks for a pattern in the container logs.
pub struct LogContainsAssertion {
    pattern: String,
    is_regex: bool,
    timeout: Duration,
    stream: LogStream,
}

impl LogContainsAssertion {
    pub fn new(pattern: String, is_regex: bool, timeout: Duration, stream: LogStream) -> Self {
        Self {
            pattern,
            is_regex,
            timeout,
            stream,
        }
    }
}

#[async_trait::async_trait]
impl Assertion for LogContainsAssertion {
    fn name(&self) -> &'static str {
        "log_contains"
    }

    fn description(&self) -> String {
        let pattern_type = if self.is_regex { "regex" } else { "literal" };
        format!("Logs contain {} pattern '{}'.", pattern_type, self.pattern)
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let deadline = Instant::now() + self.timeout;

        // Validate regex pattern up front.
        if self.is_regex {
            if let Err(e) = regex::Regex::new(&self.pattern) {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!("Invalid regex pattern '{}': {}.", self.pattern, e),
                    duration: started.elapsed(),
                };
            }
        }

        loop {
            if Instant::now() > deadline {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!("Pattern '{}' not found in logs after {:?}.", self.pattern, self.timeout),
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

            // Check the log buffer.
            {
                let buffer = ctx.log_buffer.read().await;
                if buffer.contains_match(&self.pattern, self.is_regex, &self.stream) {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: true,
                        message: format!("Found pattern '{}' in logs.", self.pattern),
                        duration: started.elapsed(),
                    };
                }

                trace!(
                    pattern = %self.pattern,
                    stdout_lines = buffer.stdout.len(),
                    stderr_lines = buffer.stderr.len(),
                    "Pattern not yet found, continuing to poll logs..."
                );
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Assertion that checks a pattern does NOT appear in the logs for a duration.
pub struct LogNotContainsAssertion {
    pattern: String,
    is_regex: bool,
    during: Duration,
    stream: LogStream,
}

impl LogNotContainsAssertion {
    pub fn new(pattern: String, is_regex: bool, during: Duration, stream: LogStream) -> Self {
        Self {
            pattern,
            is_regex,
            during,
            stream,
        }
    }
}

#[async_trait::async_trait]
impl Assertion for LogNotContainsAssertion {
    fn name(&self) -> &'static str {
        "log_not_contains"
    }

    fn description(&self) -> String {
        let pattern_type = if self.is_regex { "regex" } else { "literal" };
        format!(
            "Logs do not contain {} pattern '{}' for {:?}.",
            pattern_type, self.pattern, self.during
        )
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let deadline = Instant::now() + self.during;

        // Validate regex pattern up front.
        if self.is_regex {
            if let Err(e) = regex::Regex::new(&self.pattern) {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!("Invalid regex pattern '{}': {}.", self.pattern, e),
                    duration: started.elapsed(),
                };
            }
        }

        loop {
            if Instant::now() > deadline {
                // Success - pattern was not found during the entire period.
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: true,
                    message: format!("Pattern '{}' not found in logs during {:?}.", self.pattern, self.during),
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

            // Check the log buffer for the unwanted pattern.
            {
                let buffer = ctx.log_buffer.read().await;
                if let Some(matching_line) = buffer.find_match(&self.pattern, self.is_regex, &self.stream) {
                    // Truncate the matching line for display.
                    let display_line = if matching_line.len() > 100 {
                        format!("{}...", &matching_line[..100])
                    } else {
                        matching_line
                    };

                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: format!("Found unwanted pattern '{}' in logs: {}", self.pattern, display_line),
                        duration: started.elapsed(),
                    };
                }
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
