use std::{sync::Arc, time::Duration};

use saluki_error::GenericError;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::config::{AssertionConfig, LogStream};

mod health_check;
mod log_contains;
mod port_listening;
mod process_stable;

pub use health_check::HealthCheckAssertion;
pub use log_contains::{LogContainsAssertion, LogNotContainsAssertion};
pub use port_listening::PortListeningAssertion;
pub use process_stable::ProcessStableForAssertion;

/// Result of running an assertion.
#[derive(Clone, Debug)]
pub struct AssertionResult {
    /// Name of the assertion type.
    pub name: String,
    /// Whether the assertion passed.
    pub passed: bool,
    /// Human-readable message about the result.
    pub message: String,
    /// How long the assertion took to run.
    pub duration: Duration,
}

/// Buffer for captured container logs.
#[derive(Debug, Default)]
pub struct LogBuffer {
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

impl LogBuffer {
    /// Check if any line matches the given pattern.
    pub fn contains_match(&self, pattern: &str, is_regex: bool, stream: &LogStream) -> bool {
        let lines: Vec<&str> = match stream {
            LogStream::Stdout => self.stdout.iter().map(|s| s.as_str()).collect(),
            LogStream::Stderr => self.stderr.iter().map(|s| s.as_str()).collect(),
            LogStream::Both => self
                .stdout
                .iter()
                .chain(self.stderr.iter())
                .map(|s| s.as_str())
                .collect(),
        };

        if is_regex {
            if let Ok(re) = regex::Regex::new(pattern) {
                lines.iter().any(|line| re.is_match(line))
            } else {
                false
            }
        } else {
            lines.iter().any(|line| line.contains(pattern))
        }
    }

    /// Find the first line that matches the given pattern.
    pub fn find_match(&self, pattern: &str, is_regex: bool, stream: &LogStream) -> Option<String> {
        let lines: Vec<&str> = match stream {
            LogStream::Stdout => self.stdout.iter().map(|s| s.as_str()).collect(),
            LogStream::Stderr => self.stderr.iter().map(|s| s.as_str()).collect(),
            LogStream::Both => self
                .stdout
                .iter()
                .chain(self.stderr.iter())
                .map(|s| s.as_str())
                .collect(),
        };

        if is_regex {
            if let Ok(re) = regex::Regex::new(pattern) {
                lines
                    .iter()
                    .find(|line| re.is_match(line))
                    .map(|s| s.to_string())
            } else {
                None
            }
        } else {
            lines
                .iter()
                .find(|line| line.contains(pattern))
                .map(|s| s.to_string())
        }
    }
}

/// Context provided to assertions during execution.
pub struct AssertionContext {
    /// Shared log buffer for reading container logs.
    pub log_buffer: Arc<RwLock<LogBuffer>>,
    /// Cancellation token for cooperative shutdown.
    pub cancel_token: CancellationToken,
    /// Port mappings from internal port to host port.
    pub port_mappings: std::collections::HashMap<String, u16>,
}

/// Trait for assertion implementations.
#[async_trait::async_trait]
pub trait Assertion: Send + Sync {
    /// Returns the name of this assertion type.
    fn name(&self) -> &'static str;

    /// Returns a human-readable description of what this assertion checks.
    fn description(&self) -> String;

    /// Execute the assertion and return the result.
    async fn check(&self, ctx: &AssertionContext) -> AssertionResult;
}

/// Create an assertion from its configuration.
pub fn create_assertion(config: &AssertionConfig) -> Result<Box<dyn Assertion>, GenericError> {
    match config {
        AssertionConfig::ProcessStableFor { duration } => {
            Ok(Box::new(ProcessStableForAssertion::new(duration.0)))
        }
        AssertionConfig::PortListening {
            port,
            protocol,
            timeout,
        } => Ok(Box::new(PortListeningAssertion::new(
            *port,
            protocol.clone(),
            timeout.0,
        ))),
        AssertionConfig::LogContains {
            pattern,
            regex,
            timeout,
            stream,
        } => Ok(Box::new(LogContainsAssertion::new(
            pattern.clone(),
            *regex,
            timeout.0,
            stream.clone(),
        ))),
        AssertionConfig::LogNotContains {
            pattern,
            regex,
            during,
            stream,
        } => Ok(Box::new(LogNotContainsAssertion::new(
            pattern.clone(),
            *regex,
            during.0,
            stream.clone(),
        ))),
        AssertionConfig::HealthCheck {
            endpoint,
            expected_status,
            timeout,
        } => Ok(Box::new(HealthCheckAssertion::new(
            endpoint.clone(),
            *expected_status,
            timeout.0,
        ))),
    }
}
