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

            if ctx.cancel_token.is_cancelled() || ctx.container_exit_token.is_cancelled() {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: "Assertion cancelled because container exited.".to_string(),
                    duration: started.elapsed(),
                };
            }

            let read_result = if ctx.is_host_process {
                read_file_local(&self.path).await
            } else {
                read_file_in_container(&ctx.container_name, &self.path, ctx.target_is_windows()).await
            };
            match read_result {
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
        Err(e) => Err(format!("Failed to read '{}': {}", path, e)),
    }
}

/// Reads a file from inside the container via `docker exec`.
///
/// Linux containers run `cat <path>`. Windows containers run a PowerShell snippet that uses
/// `Get-Content -Raw` and exits non-zero when the file is missing, so the failure modes line up.
/// Returns `Ok(Some(contents))` when the file exists and is readable, `Ok(None)` when the file
/// is missing or unreadable (the exec exits non-zero), and `Err` for any other failure (for
/// example, loss of Docker connectivity).
async fn read_file_in_container(container_name: &str, path: &str, is_windows: bool) -> Result<Option<String>, String> {
    let docker = docker::connect().map_err(|e| format!("Failed to connect to Docker: {}", e))?;

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

    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions::<String> {
                cmd: Some(cmd),
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
