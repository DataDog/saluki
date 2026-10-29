use std::time::Duration;

use tokio::time::Instant;
use tracing::trace;

use crate::actions::docker_exec;
use crate::assertions::{Assertion, AssertionContext, AssertionResult, AssertionStatus, Attempts};
use crate::probe::{probe, ProbeError, POLL_INTERVAL};

/// Assertion that checks a file exists in the container, and optionally that its contents match a pattern.
///
/// The assertion polls the file via `docker exec cat <path>` until the deadline. If `pattern` is `None`, the
/// assertion passes as soon as the file is readable. If `pattern` is `Some`, the assertion waits until the file
/// is readable AND its contents match the pattern.
///
/// Dropping a read doesn't stop it: a Docker exec keeps running, and a host read keeps its blocking thread. A read that
/// never finishes, such as one of a FIFO, therefore gets the rest of the step and is never retried.
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

    fn timed_out_result(&self, started: Instant, attempts: &Attempts) -> AssertionResult {
        let summary = match &self.pattern {
            None => format!("File '{}' did not exist after {:?}.", self.path, self.timeout),
            Some(p) => format!(
                "Pattern '{}' not found in file '{}' after {:?}.",
                p, self.path, self.timeout
            ),
        };
        AssertionResult::new(
            self.name(),
            started,
            attempts.status(),
            attempts.explain_expired(&summary),
        )
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

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.timed_out_result(started, &attempts);
            }

            let read = async {
                if ctx.is_host_process {
                    read_file_local(&self.path).await
                } else {
                    read_file_in_container(&ctx.container_name, &self.path, ctx.target_is_windows()).await
                }
            };
            match probe(&what, remaining, &ctx.cancel_token, read).await {
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
                Err(error @ ProbeError::TimedOut { .. }) => {
                    attempts.errored(error);
                    return self.timed_out_result(started, &attempts);
                }
            }

            // The next iteration reports cancellation.
            ctx.cancel_token
                .run_until_cancelled(tokio::time::sleep(POLL_INTERVAL))
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

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io::Write as _;
    use std::time::Duration;

    use airlock::driver::ContainerOs;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    use super::FileContainsAssertion;
    use crate::actions::test_docker::{Exec, FakeDocker};
    use crate::assertions::test_support::host_context;
    use crate::assertions::{Assertion as _, AssertionContext, AssertionResult, AssertionStatus};
    use crate::probe::PROBE_TIMEOUT;

    /// Long enough that a read capped at [`PROBE_TIMEOUT`] would be retried within the step.
    const STEP: Duration = PROBE_TIMEOUT.saturating_add(Duration::from_secs(2));

    /// Checks for a file in a container target running `os`, with its execs served by `docker`.
    async fn check_container(docker: &FakeDocker, os: ContainerOs) -> AssertionResult {
        let mut ctx = host_context(CancellationToken::new());
        ctx.is_host_process = false;
        ctx.target_os = Some(os);
        ctx.container_name = "target".to_string();
        let assertion = FileContainsAssertion::new("/var/log/adp.log".to_string(), None, false, STEP);

        docker.scope(assertion.check(&ctx)).await
    }

    /// Runs `test` on a paused runtime whose only blocking thread stays busy until `test` ends.
    ///
    /// A host read queues behind the busy thread, so it never finishes within the step. Unlike a read of a FIFO, it
    /// can't outlive the test: the busy thread is released when `test` ends, even on panic, and dropping the runtime
    /// cancels the queued read or lets it finish.
    ///
    /// The busy thread keeps the paused clock from advancing on its own, so `test` must advance it.
    fn with_host_reads_stalled<F: Future<Output = ()>>(test: impl FnOnce() -> F) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .max_blocking_threads(1)
            .build()
            .expect("should build a runtime");
        // Declared after `runtime` so it's dropped first, because dropping the runtime waits for the busy thread.
        let (_release, released) = std::sync::mpsc::channel::<()>();
        runtime.spawn_blocking(move || {
            let _ = released.recv();
        });

        runtime.block_on(test());
    }

    /// Returns a readable file, so a host read of it that finished would pass the check.
    fn readable_file() -> (tempfile::NamedTempFile, String) {
        let mut file = tempfile::NamedTempFile::new().expect("should create a temporary file");
        file.write_all(b"ready").expect("should write the temporary file");
        let path = file.path().display().to_string();
        (file, path)
    }

    /// Runs `assertion` while advancing the paused clock, which doesn't advance on its own while a host read is
    /// pending.
    async fn check_advancing_time(assertion: &FileContainsAssertion, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let check = assertion.check(ctx);
        tokio::pin!(check);
        loop {
            tokio::select! {
                biased;
                result = &mut check => return result,
                () = tokio::time::advance(Duration::from_millis(100)) => {
                    assert!(started.elapsed() <= STEP * 2, "the check should end with its step");
                }
            }
        }
    }

    #[tokio::test]
    async fn a_container_read_that_never_finishes_is_not_retried() {
        // Read attempts were capped at 10 s and retried, but a timed-out exec keeps running, so a FIFO or a stalled
        // filesystem left one more blocked command in the container for each retry.
        let check = |os| async move {
            let docker = FakeDocker::start(Duration::ZERO, |_, _| Exec::Running).await;
            let started = Instant::now();
            let result = check_container(&docker, os).await;
            (result, started.elapsed(), docker.launched())
        };

        let (linux, windows) = tokio::join!(check(ContainerOs::Linux), check(ContainerOs::Windows));

        for ((result, elapsed, launched), program) in [(linux, "cat"), (windows, "pwsh")] {
            assert_eq!(result.status, AssertionStatus::Errored, "{program}: {}", result.message);
            assert!(
                result.message.starts_with("Timed out after"),
                "{program}: {}",
                result.message
            );
            assert!(
                elapsed >= STEP,
                "{program}: the read should get the whole step, not {elapsed:?}"
            );
            assert_eq!(launched.len(), 1, "{program}: {launched:?}");
            assert_eq!(launched[0][0], program);
        }
    }

    #[tokio::test]
    async fn a_read_that_never_finishes_after_a_missing_file_fails_the_check() {
        let docker = FakeDocker::start(Duration::ZERO, |index, _| match index {
            0 => Exec::Exits {
                duration: Duration::ZERO,
                stdout: String::new(),
                exit_code: 1,
            },
            _ => Exec::Running,
        })
        .await;

        let result = check_container(&docker, ContainerOs::Linux).await;

        assert_eq!(result.status, AssertionStatus::Failed, "{}", result.message);
        assert!(
            result.message.contains("\nLast attempt: Timed out after"),
            "{}",
            result.message
        );
        assert_eq!(docker.launched().len(), 2, "the missing file should be read again");
    }

    #[tokio::test]
    async fn a_step_with_no_time_left_does_not_read_the_file() {
        let docker = FakeDocker::start(Duration::ZERO, |_, _| Exec::Running).await;
        let mut ctx = host_context(CancellationToken::new());
        ctx.is_host_process = false;
        ctx.container_name = "target".to_string();
        let assertion = FileContainsAssertion::new("/var/log/adp.log".to_string(), None, false, Duration::ZERO);

        let result = docker.scope(assertion.check(&ctx)).await;

        assert_eq!(result.status, AssertionStatus::Errored, "{}", result.message);
        assert_eq!(result.message, "File '/var/log/adp.log' did not exist after 0ns.");
        assert_eq!(docker.launched(), Vec::<Vec<String>>::new());
    }

    #[test]
    fn a_host_read_that_never_finishes_gets_the_rest_of_the_step() {
        with_host_reads_stalled(|| async {
            let (_file, path) = readable_file();
            let assertion = FileContainsAssertion::new(path.clone(), None, false, STEP);

            let result = check_advancing_time(&assertion, &host_context(CancellationToken::new())).await;

            assert_eq!(result.status, AssertionStatus::Errored, "{}", result.message);
            let timed_out = format!("Timed out after {STEP:?} waiting for a read of '{path}'.");
            assert_eq!(
                result.message.lines().next(),
                Some(timed_out.as_str()),
                "{}",
                result.message
            );
        });
    }

    #[test]
    fn cancellation_ends_a_host_read_that_never_finishes() {
        with_host_reads_stalled(|| async {
            let (_file, path) = readable_file();
            let assertion = FileContainsAssertion::new(path.clone(), None, false, STEP);
            let cancel = CancellationToken::new();
            let canceller = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                canceller.cancel();
            });

            let result = check_advancing_time(&assertion, &host_context(cancel)).await;

            assert_eq!(result.status, AssertionStatus::Errored, "{}", result.message);
            assert!(result.message.starts_with("Cancelled after 1."), "{}", result.message);
            assert!(
                result.message.ends_with(&format!("waiting for a read of '{path}'.")),
                "{}",
                result.message
            );
        });
    }
}
