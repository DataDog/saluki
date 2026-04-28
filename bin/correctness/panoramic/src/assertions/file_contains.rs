use std::time::{Duration, Instant};

use airlock::docker;
use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecResults},
};
use futures::TryStreamExt as _;
use tracing::trace;

use crate::assertions::{Assertion, AssertionContext, AssertionResult};

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
        let deadline = Instant::now() + self.timeout;

        // Validate the regex pattern up front, if applicable.
        if self.is_regex {
            if let Some(p) = &self.pattern {
                if let Err(e) = regex::Regex::new(p) {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: format!("Invalid regex pattern '{}': {}.", p, e),
                        duration: started.elapsed(),
                    };
                }
            }
        }

        loop {
            if Instant::now() > deadline {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: match &self.pattern {
                        None => format!("File '{}' did not exist after {:?}.", self.path, self.timeout),
                        Some(p) => format!(
                            "Pattern '{}' not found in file '{}' after {:?}.",
                            p, self.path, self.timeout
                        ),
                    },
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

            match read_file_in_container(&ctx.container_name, &self.path).await {
                Ok(Some(content)) => {
                    let matches = match &self.pattern {
                        None => true,
                        Some(p) => {
                            if self.is_regex {
                                regex::Regex::new(p).expect("regex validated above").is_match(&content)
                            } else {
                                content.contains(p)
                            }
                        }
                    };

                    if matches {
                        return AssertionResult {
                            name: self.name().to_string(),
                            passed: true,
                            message: match &self.pattern {
                                None => format!("File '{}' exists.", self.path),
                                Some(p) => format!("Pattern '{}' found in file '{}'.", p, self.path),
                            },
                            duration: started.elapsed(),
                        };
                    }

                    trace!(path = %self.path, "File present but pattern not yet matched, polling...");
                }
                Ok(None) => trace!(path = %self.path, "File not yet present, polling..."),
                Err(e) => trace!(path = %self.path, error = %e, "Error reading file, polling..."),
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Reads a file from inside the container via `docker exec cat <path>`.
///
/// Returns `Ok(Some(contents))` when the file exists and is readable, `Ok(None)` when the file is missing or
/// unreadable (`cat` exits non-zero), and `Err` for any other failure (e.g., loss of Docker connectivity).
async fn read_file_in_container(container_name: &str, path: &str) -> Result<Option<String>, String> {
    let docker = docker::connect().map_err(|e| format!("Failed to connect to Docker: {}", e))?;

    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions::<String> {
                cmd: Some(vec!["cat".into(), path.into()]),
                attach_stdout: Some(true),
                attach_stderr: Some(false),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("Failed to create exec: {}", e))?;

    let exec_id = exec.id.clone();

    let result = docker
        .start_exec(&exec_id, None)
        .await
        .map_err(|e| format!("Failed to start exec: {}", e))?;

    let mut stdout = String::new();
    if let StartExecResults::Attached { mut output, .. } = result {
        while let Some(chunk) = output
            .try_next()
            .await
            .map_err(|e| format!("Failed to read exec output: {}", e))?
        {
            if let LogOutput::StdOut { message } = chunk {
                stdout.push_str(&String::from_utf8_lossy(&message));
            }
        }
    }

    let inspect = docker
        .inspect_exec(&exec_id)
        .await
        .map_err(|e| format!("Failed to inspect exec: {}", e))?;

    match inspect.exit_code {
        Some(0) => Ok(Some(stdout)),
        _ => Ok(None),
    }
}
