//! Observation: how each process of a started participant exited, what it wrote, and the files copied out of it.
//!
//! Every process a participant declares has one exit channel and one output channel. The only process of a container
//! is observed through the container: its exit and its log. A supervised process is observed through files a
//! supervisor inside the participant writes: an exit record in the participant's exit directory, and a log file in its
//! log directory, which the courier follows.
//!
//! An exit is running, exited with a status, or undetermined. Undetermined is an observation error: a step that needs
//! the status errors. It never passes, and no code is invented.
//!
//! # Exit records
//!
//! A supervisor appends one JSON object per exit to the record, each on its own line:
//! `{"code": <code>, "signal": <signal>, "time": "<seconds>.<fraction>"}`, where the time is a Unix time with one to
//! nine fractional digits. A normal exit has signal 0. A death by signal has code 256 and the signal number. Other
//! fields are ignored, so a supervisor may add its own. A line counts once its newline is written. The first line is
//! final, so a restart never returns a process to running.
//!
//! # Output and collected files
//!
//! Output is captured line by line as it arrives: kept per process for steps, and written to
//! `<log dir>/<participant>/<process>.log`. A participant whose processes are all supervised also gets its container
//! log, which holds what the supervisor itself writes, as [`CONTAINER_LOG`]. A capture that stops while its
//! participant runs is an observation error: a step that relies on having seen all of a process's output errors.
//! Files to collect are copied to `<log dir>/<participant>/collected/<name>` after the procedure. Each carries its own
//! retention, which [`retain`] applies to the outcome.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use airlock::container::{Output, OutputStream};
use futures::StreamExt as _;
use serde::Serialize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::watch;
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::backend::Realized;
use super::courier::Courier;
use super::spec::{CollectFile, Keep};

/// The file, in a participant's log directory, of the container log of a participant whose processes are all
/// supervised. A process name starts with a letter, so no process's log takes this name.
pub(crate) const CONTAINER_LOG: &str = "_container.log";

/// The directory, in a participant's log directory, of the files collected from it.
const COLLECTED_DIR: &str = "collected";

/// How long the last read of a followed log file may take.
const LAST_READ_TIME: Duration = Duration::from_secs(5);

/// How long a participant's output must stay quiet, once its capture is told to finish, before the capture ends.
const QUIET_TIME: Duration = Duration::from_millis(500);

/// The longest a capture goes on reading after it is told to finish, however much output still arrives.
const DRAIN_TIME: Duration = Duration::from_secs(2);

/// How a process exited.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ExitStatus {
    /// The exit code. A container reports death by signal `n` as 128 + `n`, and an exit record as 256.
    pub(crate) code: i64,
    /// The signal that killed the process, where its exit channel reports one: an exit record does, a container does
    /// not.
    pub(crate) signal: Option<u32>,
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.signal {
            Some(signal) => write!(f, "code {} (signal {})", self.code, signal),
            None => write!(f, "code {}", self.code),
        }
    }
}

/// What is known of a process's exit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProcessExit {
    Running,
    Exited(ExitStatus),
    /// The status could not be determined, for the reason given. A step that needs the status errors.
    Undetermined(String),
}

/// The shape of a line of an exit record.
const EXIT_LINE: &str = r#"{"code": <code>, "signal": <signal>, "time": "<seconds>.<fraction>"}"#;

/// One line of an exit record. Fields it does not name are ignored.
#[derive(serde::Deserialize)]
struct ExitLine {
    code: i64,
    signal: u32,
    time: String,
}

/// The first exit an exit record holds, or `None` if it holds no complete line yet.
///
/// # Errors
///
/// Returns an error if the first line is not an object of [`EXIT_LINE`]'s shape with a signal that agrees with the
/// code.
pub(crate) fn first_exit(record: &[u8]) -> Result<Option<ExitStatus>, String> {
    // A read during an append may see part of a line.
    let Some(end) = record.iter().position(|&byte| byte == b'\n') else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&record[..end]);
    let line = text.strip_suffix('\r').unwrap_or(&text);
    let malformed = || format!("the first line '{}' is not '{}'", line, EXIT_LINE);
    let ExitLine { code, signal, time } = serde_json::from_str(line).map_err(|_| malformed())?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    match time.split_once('.') {
        Some((seconds, fraction)) if digits(seconds) && digits(fraction) && fraction.len() <= 9 => {}
        _ => return Err(malformed()),
    }
    match (code, signal) {
        (256, 0) => Err(format!(
            "the first line '{}' has code 256, a death by signal, and no signal",
            line
        )),
        (256, signal) => Ok(Some(ExitStatus {
            code,
            signal: Some(signal),
        })),
        (code, 0) => Ok(Some(ExitStatus { code, signal: None })),
        _ => Err(format!(
            "the first line '{}' has a signal, and a death by signal has code 256",
            line
        )),
    }
}

/// What is observed of one started participant: its exit as its realization reports it, and the output of each of its
/// processes. Clones share it.
#[derive(Clone, Debug)]
pub(crate) struct Observation(Arc<Observed>);

#[derive(Debug)]
struct Observed {
    /// `None` while the participant runs, then its exit code as the realization reports it, or why it could not.
    exit: watch::Sender<Option<Result<i64, String>>>,
    /// Each process's output so far, line by line. The whole output is kept for the case's length, on purpose: steps
    /// match against all of it, and a case is bounded in time.
    output: BTreeMap<String, watch::Sender<Vec<String>>>,
    /// Why the capture of a process's output stopped before its output ended, for each process whose capture did.
    capture_failures: Mutex<BTreeMap<String, String>>,
}

impl Observation {
    /// Observes a running participant with these processes, which have written nothing yet.
    pub(crate) fn new<'a>(processes: impl IntoIterator<Item = &'a str>) -> Self {
        let output = processes
            .into_iter()
            .map(|process| (process.to_string(), watch::Sender::new(Vec::new())))
            .collect();
        Self(Arc::new(Observed {
            exit: watch::Sender::new(None),
            output,
            capture_failures: Mutex::default(),
        }))
    }

    /// Records the participant's exit code as its realization reports it, or why it could not report one.
    pub(crate) fn exited(&self, exit: Result<i64, String>) {
        self.0.exit.send_replace(Some(exit));
    }

    /// The participant's exit code as its realization reports it, or `None` while it runs.
    pub(crate) fn exit(&self) -> Option<Result<i64, String>> {
        self.0.exit.borrow().clone()
    }

    /// Returns once the realization reports the participant's exit.
    pub(crate) async fn wait_for_exit(&self) {
        // The sender lives as long as `self`, so the wait ends only with a report.
        let _ = self.0.exit.subscribe().wait_for(Option::is_some).await;
    }

    /// The output `process` has written so far, which grows as more is captured, or `None` if the participant has no
    /// such process.
    pub(crate) fn output(&self, process: &str) -> Option<watch::Receiver<Vec<String>>> {
        self.0.output.get(process).map(watch::Sender::subscribe)
    }

    /// Records that the capture of `process`'s output stopped, for the reason given, whether or not the participant
    /// has exited: output that ended is not a failure, and callers report only a capture that broke, which may have
    /// left the output short.
    pub(crate) fn capture_failed(&self, process: &str, why: String) {
        let mut failures = self.0.capture_failures.lock().unwrap_or_else(|e| e.into_inner());
        failures.entry(process.to_string()).or_insert(why);
    }

    /// Why the capture of `process`'s output stopped before its output ended, or `None` if it did not.
    pub(crate) fn capture_failure(&self, process: &str) -> Option<String> {
        let failures = self.0.capture_failures.lock().unwrap_or_else(|e| e.into_inner());
        failures.get(process).cloned()
    }

    /// Adds `line` to the output of `process`.
    pub(crate) fn line(&self, process: &str, line: String) {
        if let Some(output) = self.0.output.get(process) {
            output.send_modify(|lines| lines.push(line));
        }
    }
}

/// Splits output into lines as it arrives in chunks.
#[derive(Default)]
struct Lines {
    partial: Vec<u8>,
}

impl Lines {
    /// Adds `chunk` and returns the lines it completes.
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.partial.extend_from_slice(chunk);
        let Some(end) = self.partial.iter().rposition(|&byte| byte == b'\n') else {
            return Vec::new();
        };
        let complete: Vec<u8> = self.partial.drain(..=end).collect();
        complete[..end].split(|&byte| byte == b'\n').map(line).collect()
    }

    /// The last line, if it has no newline.
    fn finish(self) -> Option<String> {
        (!self.partial.is_empty()).then(|| line(&self.partial))
    }
}

/// One line of output as text, without the carriage return of a Windows line end.
fn line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.strip_suffix(b"\r").unwrap_or(bytes)).into_owned()
}

/// Where one stream of captured output goes: a file in the case's log directory and, for a process, its lines in the
/// observation.
pub(crate) struct Sink {
    file: tokio::fs::File,
    path: PathBuf,
    process: Option<(Observation, String)>,
}

impl Sink {
    /// Creates the file at `path`, and feeds the lines to `process` of `observation` too, when given.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created.
    pub(crate) async fn create(path: PathBuf, process: Option<(Observation, String)>) -> Result<Self, String> {
        let file = tokio::fs::File::create(&path)
            .await
            .map_err(|e| format!("Failed to create '{}': {}", path.display(), e))?;
        Ok(Self { file, path, process })
    }

    /// Hands `lines` to the process, then appends them to the file.
    async fn write(&mut self, lines: impl IntoIterator<Item = String>) {
        let mut text = String::new();
        for line in lines {
            text.push_str(&line);
            text.push('\n');
            if let Some((observation, process)) = &self.process {
                observation.line(process, line);
            }
        }
        if text.is_empty() {
            return;
        }
        // Flushed at once, so the file holds what arrived even when the task is dropped.
        let written = async {
            self.file.write_all(text.as_bytes()).await?;
            self.file.flush().await
        };
        if let Err(e) = written.await {
            warn!(path = %self.path.display(), error = %e, "Failed to write captured output.");
        }
    }

    /// Records that the capture broke, so a step does not take the output as whole.
    fn failed(&self, why: String) {
        if let Some((observation, process)) = &self.process {
            observation.capture_failed(process, why);
        }
    }

    /// Captures what the realization reports as the output of a participant's only process, until it ends, or until
    /// it is quiet for [`QUIET_TIME`] after `finish` fires, reading for at most [`DRAIN_TIME`] after that, even when
    /// chunks keep arriving.
    pub(crate) async fn capture(mut self, mut output: OutputStream, finish: CancellationToken) {
        let (mut stdout, mut stderr) = (Lines::default(), Lines::default());
        let mut drain_end = None;
        loop {
            let chunk = match drain_end {
                None => tokio::select! {
                    biased;
                    () = finish.cancelled() => {
                        drain_end = Some(Instant::now() + DRAIN_TIME);
                        continue;
                    }
                    chunk = output.next() => chunk,
                },
                Some(end) if Instant::now() >= end => break,
                Some(end) => timeout_at((Instant::now() + QUIET_TIME).min(end), output.next())
                    .await
                    .ok()
                    .flatten(),
            };
            let Some(chunk) = chunk else {
                break;
            };
            let lines = match chunk {
                Ok(Output::Stdout(bytes)) => stdout.push(&bytes),
                Ok(Output::Stderr(bytes)) => stderr.push(&bytes),
                Err(e) => {
                    warn!(path = %self.path.display(), error = %e, "Stopped capturing output.");
                    self.failed(format!("{:#}", e));
                    break;
                }
            };
            self.write(lines).await;
        }
        self.write(stdout.finish().into_iter().chain(stderr.finish())).await;
    }

    /// Captures the log file at `path` in `participant`, which the courier at `courier` follows for `limit` or until
    /// `finish` fires, then reads the file once more for what the follow missed.
    ///
    /// The follow polls the file and ends when the participant exits, so the last lines before an exit, or every line
    /// of a participant that exits before the follow starts, reach only the file.
    pub(crate) async fn follow(
        mut self, participant: &dyn Realized, courier: &str, path: &str, limit: Duration, finish: CancellationToken,
    ) {
        let (writer, mut reader) = tokio::io::duplex(64 * 1024);
        let follow = async {
            // Dropped when the follow ends, which ends the read once it has what the follow wrote.
            let mut writer = writer;
            let courier = Courier::new(participant, courier);
            tokio::select! {
                followed = courier.follow(path, limit, &mut writer) => followed,
                () = finish.cancelled() => Ok(()),
            }
        };
        let mut lines = Lines::default();
        // The bytes of the file that arrived, from its start.
        let mut received = 0;
        let mut read_failure = None;
        let read = async {
            let mut chunk = vec![0; 8192];
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(read) => {
                        received += read;
                        self.write(lines.push(&chunk[..read])).await;
                    }
                    Err(e) => {
                        warn!(path, error = %e, "Stopped capturing a followed file.");
                        read_failure = Some(e.to_string());
                        break;
                    }
                }
            }
        };
        let (followed, ()) = tokio::join!(follow, read);
        if let Some(why) = read_failure {
            self.failed(why);
        }
        // The follow fails when the participant exits, which leaves nothing more to capture. While the participant
        // runs, a failure means the courier could not run, or could not open or read the file, so the capture stopped.
        if let Err(e) = followed {
            if participant.has_exited().await {
                debug!(path, error = %e, "Stopped following a file.");
            } else {
                warn!(path, error = %e, "Failed to follow a file.");
                self.failed(e.to_string());
            }
        }
        match participant.read_file(path, Instant::now() + LAST_READ_TIME).await {
            Ok(Some(contents)) => {
                if let Some(rest) = contents.get(received..) {
                    self.write(lines.push(rest)).await;
                }
            }
            Ok(None) => {}
            Err(e) => warn!(path, error = %e, "Failed to read a followed file once more."),
        }
        self.write(lines.finish()).await;
    }
}

/// A file copied out of a participant after the procedure.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Collected {
    pub(crate) participant: String,
    /// Where the copy is on the runner host.
    pub(crate) path: PathBuf,
    pub(crate) keep: Keep,
}

/// Copies each of `files` out of `participant`, through `realized`, into `<dir>/collected/`, within `deadline`. A file
/// that is not there, or cannot be copied, is left out with a warning.
pub(crate) async fn collect(
    participant: &str, files: &[CollectFile], realized: &dyn Realized, dir: &Path, deadline: Instant,
) -> Vec<Collected> {
    let mut collected = Vec::new();
    if files.is_empty() {
        return collected;
    }
    let dir = dir.join(COLLECTED_DIR);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        warn!(path = %dir.display(), error = %e, "Failed to create the directory for collected files.");
        return collected;
    }
    for file in files {
        let contents = match realized.read_file(&file.path, deadline).await {
            Ok(Some(contents)) => contents,
            Ok(None) => {
                warn!(participant, path = %file.path, "A file to collect is not there.");
                continue;
            }
            Err(e) => {
                warn!(participant, path = %file.path, error = %e, "Failed to collect a file.");
                continue;
            }
        };
        let path = dir.join(&file.name);
        match tokio::fs::write(&path, contents).await {
            Ok(()) => collected.push(Collected {
                participant: participant.to_string(),
                path,
                keep: file.keep,
            }),
            Err(e) => warn!(path = %path.display(), error = %e, "Failed to write a collected file."),
        }
    }
    collected
}

/// Removes each collected file the outcome does not keep: a file kept on failure goes when the case passed. Returns
/// the files that are kept.
pub(crate) async fn retain(collected: Vec<Collected>, passed: bool) -> Vec<Collected> {
    let mut kept = Vec::new();
    for file in collected {
        let keep = match file.keep {
            Keep::Always => true,
            Keep::OnFailure => !passed,
        };
        if keep {
            kept.push(file);
        } else if let Err(e) = tokio::fs::remove_file(&file.path).await {
            warn!(path = %file.path.display(), error = %e, "Failed to remove a collected file.");
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exit_record_gives_its_first_line_as_the_exit() {
        let exited = |code, signal| Ok(Some(ExitStatus { code, signal }));
        let cases: &[(&str, Result<Option<ExitStatus>, &str>)] = &[
            ("", Ok(None)),
            ("{\"code\":0,\"signal\":0,\"time\":\"1794511601.5\"}\n", exited(0, None)),
            (
                "{\"code\":3,\"signal\":0,\"time\":\"1794511601.123456789\"}\n",
                exited(3, None),
            ),
            // A supervisor may add fields of its own.
            (
                "{\"code\":3,\"signal\":0,\"time\":\"1794511601.5\",\"pid\":12}\n",
                exited(3, None),
            ),
            // The first line is final: a restart after it does not count.
            (
                "{\"code\":3,\"signal\":0,\"time\":\"1794511601.1\"}\n{\"code\":0,\"signal\":0,\"time\":\"1794511609.2\"}\n",
                exited(3, None),
            ),
            (
                "{\"code\":256,\"signal\":9,\"time\":\"1794511601.1\"}\r\n",
                exited(256, Some(9)),
            ),
            // A line counts once its newline is written.
            ("{\"code\":3,\"signal\":0,\"time\":\"1794511601.1\"}", Ok(None)),
            ("{\"code\":3,\"signal\":0,\"time\":\"1794511601.\"}\n", Err("is not '{")),
            (
                "{\"code\":3,\"signal\":0,\"time\":\"1794511601.1234567890\"}\n",
                Err("is not '{"),
            ),
            ("{\"code\":3,\"signal\":0,\"time\":\"1794511601\"}\n", Err("is not '{")),
            ("{\"code\":3,\"signal\":0,\"time\":1794511601.1}\n", Err("is not '{")),
            ("{\"code\":3,\"time\":\"1794511601.1\"}\n", Err("is not '{")),
            ("{\"code\":\"three\",\"signal\":0,\"time\":\"1794511601.1\"}\n", Err("is not '{")),
            ("3 0 1794511601.1\n", Err("is not '{")),
            ("\n", Err("is not '{")),
            ("{\"code\":256,\"signal\":0,\"time\":\"1794511601.1\"}\n", Err("no signal")),
            ("{\"code\":3,\"signal\":9,\"time\":\"1794511601.1\"}\n", Err("has a signal")),
        ];
        for (record, expected) in cases {
            let parsed = first_exit(record.as_bytes());
            match expected {
                Ok(expected) => assert_eq!(&parsed, &Ok(*expected), "{record:?}"),
                Err(text) => {
                    let error = parsed.expect_err(record);
                    assert!(error.contains(text), "{record:?}: {error}");
                }
            }
        }
        assert!(first_exit(&[b'{', 0xff, b'}', b'\n']).is_err());
    }
}
