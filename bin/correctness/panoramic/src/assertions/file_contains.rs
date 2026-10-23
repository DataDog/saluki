use std::time::Duration;

use tokio::time::Instant;
use tracing::trace;

use crate::actions::docker_exec;
use crate::assertions::{Assertion, AssertionContext, AssertionResult, AssertionStatus, Attempts};
use crate::probe::{probe, ProbeError, PROBE_TIMEOUT};

const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Assertion that checks a file exists in the container, and optionally that its contents match a pattern.
///
/// The assertion polls the file via `docker exec cat <path>` until the deadline. If `pattern` is `None`, the
/// assertion passes as soon as the file is readable. If `pattern` is `Some`, the assertion waits until the file
/// is readable AND its contents match the pattern.
pub struct FileContainsAssertion {
    path: String,
    pattern: Option<String>,
    is_regex: bool,
    timeout: Duration,
}

impl FileContainsAssertion {
    pub fn new(path: String, pattern: Option<String>, is_regex: bool, timeout: Duration) -> Self {
        Self {
            path,
            pattern,
            is_regex,
            timeout,
        }
    }
}

#[async_trait::async_trait]
impl Assertion for FileContainsAssertion {
    fn name(&self) -> &'static str {
        "file_contains"
    }

    fn description(&self) -> String {
        match &self.pattern {
            None => format!("File '{}' exists.", self.path),
            Some(p) => {
                let pattern_type = if self.is_regex { "regex" } else { "literal" };
                format!(
                    "File '{}' exists and contains {} pattern '{}'.",
                    self.path, pattern_type, p
                )
            }
        }
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let deadline = started + self.timeout;

        // Validate the regex pattern up front, if applicable.
        let regex = match (&self.pattern, self.is_regex) {
            (Some(p), true) => match regex::Regex::new(p) {
                Ok(regex) => Some(regex),
                Err(e) => {
                    let message = format!("Invalid regex pattern '{}': {}.", p, e);
                    return AssertionResult::new(self.name(), started, AssertionStatus::Errored, message);
                }
            },
            _ => None,
        };

        let what = if ctx.is_host_process {
            format!("a read of '{}'", self.path)
        } else {
            format!("a read of '{}' in container '{}'", self.path, ctx.container_name)
        };
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

            let read = async {
                if ctx.is_host_process {
                    read_file_local(&self.path).await
                } else {
                    read_file_in_container(&ctx.container_name, &self.path, ctx.target_is_windows()).await
                }
            };
            if Instant::now() > deadline {
                let summary = match &self.pattern {
                    None => format!("File '{}' did not exist after {:?}.", self.path, self.timeout),
                    Some(p) => format!(
                        "Pattern '{}' not found in file '{}' after {:?}.",
                        p, self.path, self.timeout
                    ),
                };
                return AssertionResult::new(
                    self.name(),
                    started,
                    attempts.status(),
                    attempts.explain_expired(&summary),
                );
            }

            let limit = deadline.saturating_duration_since(Instant::now()).min(PROBE_TIMEOUT);
            match probe(&what, limit, &ctx.cancel_token, read).await {
                Ok(Ok(Some(content))) => {
                    let matches = match (&self.pattern, &regex) {
                        (None, _) => true,
                        (Some(_), Some(regex)) => regex.is_match(&content),
                        (Some(p), None) => content.contains(p),
                    };

                    if matches {
                        let message = match &self.pattern {
                            None => format!("File '{}' exists.", self.path),
                            Some(p) => format!("Pattern '{}' found in file '{}'.", p, self.path),
                        };
                        return AssertionResult::new(self.name(), started, AssertionStatus::Passed, message);
                    }

                    trace!(path = %self.path, "File present but pattern not yet matched, polling...");
                    attempts.observed(format_args!("'{}' did not match.", self.path));
                }
                Ok(Ok(None)) => {
                    trace!(path = %self.path, "File not yet present, polling...");
                    attempts.observed(format_args!("'{}' was missing or unreadable.", self.path));
                }
                Ok(Err(e)) => {
                    trace!(path = %self.path, error = %e, "Error reading file, polling...");
                    attempts.errored(e);
                }
                Err(error @ ProbeError::Cancelled { .. }) => {
                    return AssertionResult::new(
                        self.name(),
                        started,
                        attempts.status(),
                        attempts.explain_cancelled(error),
                    );
                }
                Err(error @ ProbeError::TimedOut { .. }) => attempts.errored(error),
            }

            // The next iteration reports cancellation.
            ctx.cancel_token
                .run_until_cancelled(tokio::time::sleep(RETRY_INTERVAL))
                .await;
        }
    }
}

/// Reads a file from the host filesystem.
///
/// Used by the `mac` runtime (and any future host-process runtime) where ADP runs as a local
/// process and writes log files to real host paths. Returns the same shape as
/// [`read_file_in_container`]: `Ok(Some(contents))` when readable, `Ok(None)` when missing
/// or unreadable, `Err` for unexpected I/O failures.
async fn read_file_local(path: &str) -> Result<Option<String>, String> {
    match tokio::fs::read_to_string(path).await {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(None),
        Err(e) => Err(format!("Failed to read '{}': {}.", path, e)),
    }
}

/// Reads a file from inside the container via `docker exec`.
///
/// Uses `cat` on Linux and `Get-Content -Raw` on Windows. Returns `Ok(Some(contents))` on a successful read,
/// `Ok(None)` for a nonzero exec exit (a missing or unreadable file), and `Err` for Docker API failures.
async fn read_file_in_container(container_name: &str, path: &str, is_windows: bool) -> Result<Option<String>, String> {
    let cmd = if is_windows {
        // PowerShell on Windows containers: print the file's contents on stdout, or exit 1 when
        // the path is absent. -LiteralPath disables wildcard expansion so paths with `[`, `]`,
        // and similar characters work as written.
        let escaped = path.replace('"', "`\"");
        let command = format!(
            "if (Test-Path -LiteralPath '{0}') {{ Get-Content -Raw -LiteralPath '{0}' }} else {{ exit 1 }}",
            escaped
        );
        vec![
            "pwsh".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            command,
        ]
    } else {
        vec!["cat".to_string(), path.to_string()]
    };

    let output = docker_exec(container_name, cmd).await.map_err(|e| format!("{e:#}"))?;
    Ok(output.succeeded().then_some(output.stdout))
}
