//! Participant exit and output observations maintained by case-owned tasks.

use std::{collections::BTreeMap, fmt, future::Future, path::PathBuf, sync::Arc, time::SystemTime};

use airlock::{exec::OutputChunk, observe::ContainerExit};
use futures::{Stream, StreamExt as _};
use saluki_error::{ErrorContext as _, GenericError};
use tokio::{fs::File, io::AsyncWriteExt as _, sync::watch};

pub(crate) mod collect;
pub(crate) mod exit_record;
pub(crate) mod output;

use output::{LineBuffer, OutputLines};

use crate::{
    participant::{OutputChannel, OutputStream, ProcessSpec},
    result::{ExitSource, ProcessExitReport, ProcessOutputReport, ProcessReport},
};

/// A process exit reported by its declared channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitStatus {
    /// The exit code. Docker represents signal deaths as 128 plus the signal number.
    pub code: Option<i32>,
    /// The terminating signal, set for a signal death instead of `code`.
    pub signal: Option<i32>,
    /// Runtime error accompanying a container exit.
    pub reason: Option<String>,
    /// Timestamp on the container's clock, or `None` for waits. Never compare it with host time.
    pub recorded_at: Option<SystemTime>,
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(code) = self.code {
            write!(f, "code {code}")?;
        } else if let Some(signal) = self.signal {
            write!(f, "signal {signal}")?;
        } else {
            f.write_str("unknown status")?;
        }
        if let Some(reason) = &self.reason {
            write!(f, " ({reason})")?;
        }
        Ok(())
    }
}

/// A named process's first exit, or the state of waiting for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitObservation {
    /// The declared channel is awaiting an exit.
    Running,
    /// The first exit, retained across process restarts.
    Exited(ExitStatus),
    /// The channel could not determine an exit status.
    Undetermined(String),
}

/// A process's view of its declared exit channel.
#[derive(Clone)]
pub(crate) enum ProcessObservation {
    /// Shares the participant's container exit observation.
    Container(watch::Receiver<ParticipantExit>),
    /// Receives the first recorded exit from a case-owned watcher.
    Record {
        status: watch::Receiver<ExitObservation>,
        refresh: Option<Arc<exit_record::RecordRefresh>>,
    },
    /// Receives the spawned process's wait result from its reaper.
    #[cfg_attr(
        not(any(unix, test)),
        expect(dead_code, reason = "Host process reapers run on Unix.")
    )]
    Wait(watch::Receiver<ExitObservation>),
}

impl ProcessObservation {
    /// Returns the current status, treating a closed, unsettled channel as undetermined.
    pub(crate) fn current(&self) -> ExitObservation {
        // Hold the value through the channel-closure check to take one consistent snapshot.
        let (observation, closed) = match self {
            Self::Container(receiver) => {
                let current = receiver.borrow();
                let observation = match &*current {
                    ParticipantExit::Running => ExitObservation::Running,
                    ParticipantExit::Exited { code, error } => match i32::try_from(*code) {
                        Ok(code) => ExitObservation::Exited(ExitStatus {
                            code: Some(code),
                            signal: None,
                            reason: error.clone(),
                            recorded_at: None,
                        }),
                        Err(_) => ExitObservation::Undetermined(format!(
                            "container exit code {code} is outside the i32 range"
                        )),
                    },
                    ParticipantExit::Ended => {
                        ExitObservation::Undetermined("participant ended without a container exit status".into())
                    }
                    ParticipantExit::Undetermined { reason } => ExitObservation::Undetermined(reason.clone()),
                };
                (observation, receiver.has_changed().is_err())
            }
            Self::Record { status: receiver, .. } | Self::Wait(receiver) => {
                let current = receiver.borrow();
                (current.clone(), receiver.has_changed().is_err())
            }
        };
        if observation == ExitObservation::Running && closed {
            ExitObservation::Undetermined("exit observation ended without a status".into())
        } else {
            observation
        }
    }

    /// Re-reads a running process's record, waiting for a read started after this request.
    pub(crate) async fn refresh(&self) -> ExitObservation {
        let current = self.current();
        let Self::Record {
            refresh: Some(refresh), ..
        } = self
        else {
            return current;
        };
        if current != ExitObservation::Running {
            return current;
        }
        let mut served = refresh.served.clone();
        let mut ticket = 0;
        refresh.requests.send_modify(|value| {
            *value += 1;
            ticket = *value;
        });
        loop {
            if *served.borrow_and_update() >= ticket {
                return self.current();
            }
            tokio::select! {
                observation = self.settled() => return observation,
                changed = served.changed() => {
                    if changed.is_err() {
                        return self.current();
                    }
                }
            }
        }
    }

    pub(crate) fn snapshot(&self) -> ProcessExitReport {
        let source = match self {
            Self::Container(_) => ExitSource::Container,
            Self::Record { .. } => ExitSource::Record,
            Self::Wait(_) => ExitSource::Process,
        };
        match self.current() {
            ExitObservation::Running => ProcessExitReport::Running { source },
            ExitObservation::Exited(status) => ProcessExitReport::Exited {
                source,
                code: status.code,
                signal: status.signal,
                reason: status.reason,
            },
            ExitObservation::Undetermined(reason) => ProcessExitReport::Undetermined { source, reason },
        }
    }

    /// Waits for an exit or an observation error, including loss of the channel.
    pub(crate) async fn settled(&self) -> ExitObservation {
        let mut observation = self.clone();
        loop {
            let current = observation.current();
            if current != ExitObservation::Running {
                return current;
            }
            match &mut observation {
                Self::Container(receiver) => {
                    let _ = receiver.changed().await;
                }
                Self::Record { status: receiver, .. } | Self::Wait(receiver) => {
                    let _ = receiver.changed().await;
                }
            }
        }
    }
}

/// Whether the participant as a whole has exited; for a container, this is the container's exit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParticipantExit {
    /// The exit watcher is waiting for a status.
    Running,
    /// The participant stopped with the reported code and optional Docker error.
    Exited { code: i64, error: Option<String> },
    /// Every process in the participant has an observed exit.
    #[cfg_attr(not(any(unix, test)), expect(dead_code, reason = "Host process groups run on Unix."))]
    Ended,
    /// The wait failed, leaving the participant's running state unknown.
    Undetermined { reason: String },
}

/// Progress of copying the participant's output into its log files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Capture {
    /// The watcher is following the output stream.
    Following,
    /// The stream ended and all received bytes were flushed to disk.
    Complete,
    /// A stream or file operation failed; bytes already written remain available.
    Failed { reason: String },
}

/// Participant-wide exit and output state, named process exits, and paths to captured output.
pub(crate) struct ParticipantObservation {
    exit_rx: watch::Receiver<ParticipantExit>,
    capture_rx: watch::Receiver<Capture>,
    pub stdout_path: Option<PathBuf>,
    pub stderr_path: Option<PathBuf>,
    pub outputs: BTreeMap<&'static str, ProcessOutput>,
    pub lines: OutputLines,
    pub processes: BTreeMap<&'static str, ProcessObservation>,
}

/// Capture progress and host output files for a named process.
pub(crate) struct ProcessOutput {
    pub capture: watch::Receiver<Capture>,
    pub paths: Vec<PathBuf>,
}

impl ProcessOutput {
    pub(crate) fn current(&self) -> Capture {
        capture_state(&self.capture)
    }
}

fn exit_state(receiver: &watch::Receiver<ParticipantExit>) -> ParticipantExit {
    let current = receiver.borrow();
    if *current == ParticipantExit::Running && receiver.has_changed().is_err() {
        ParticipantExit::Undetermined {
            reason: "exit observation ended without a status".into(),
        }
    } else {
        current.clone()
    }
}

fn capture_state(receiver: &watch::Receiver<Capture>) -> Capture {
    let current = receiver.borrow();
    if *current == Capture::Following && receiver.has_changed().is_err() {
        Capture::Failed {
            reason: "output capture ended before the stream completed".into(),
        }
    } else {
        current.clone()
    }
}

impl ParticipantObservation {
    /// Combines the participant's observation channels and output paths.
    pub(crate) fn new(
        exit_rx: watch::Receiver<ParticipantExit>, capture_rx: watch::Receiver<Capture>, stdout_path: PathBuf,
        stderr_path: PathBuf,
    ) -> Self {
        Self {
            exit_rx,
            capture_rx,
            stdout_path: Some(stdout_path),
            stderr_path: Some(stderr_path),
            processes: BTreeMap::new(),
            outputs: BTreeMap::new(),
            lines: OutputLines::default(),
        }
    }

    /// Combines host process observations with per-process output captures.
    #[cfg_attr(not(any(unix, test)), expect(dead_code, reason = "Host process groups run on Unix."))]
    pub(crate) fn host(exit_rx: watch::Receiver<ParticipantExit>) -> Self {
        Self {
            exit_rx,
            capture_rx: watch::channel(Capture::Complete).1,
            stdout_path: None,
            stderr_path: None,
            processes: BTreeMap::new(),
            outputs: BTreeMap::new(),
            lines: OutputLines::default(),
        }
    }

    /// Reads the retained output prefix; overflow errors name the files that still receive the full output.
    pub(crate) fn lines_since(&self, cursor: usize) -> Result<(Vec<output::OutputLine>, usize), String> {
        self.lines.since(cursor).map_err(|reason| {
            let paths = self
                .stdout_path
                .iter()
                .chain(self.stderr_path.iter())
                .chain(self.outputs.values().flat_map(|output| &output.paths))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>();
            format!(
                "output capture failed: {reason}; see output files: {}",
                paths.join(", ")
            )
        })
    }

    pub(crate) fn snapshot(&self, specs: &[ProcessSpec]) -> Vec<ProcessReport> {
        specs
            .iter()
            .map(|spec| {
                let output = &self.outputs[spec.name];
                let first = output.paths.first().cloned();
                let output = match spec.output {
                    OutputChannel::Container => ProcessOutputReport {
                        stdout: self.stdout_path.clone(),
                        stderr: self.stderr_path.clone(),
                        merged: None,
                    },
                    OutputChannel::Pipes => ProcessOutputReport {
                        stdout: first,
                        stderr: output.paths.get(1).cloned(),
                        merged: None,
                    },
                    OutputChannel::File { stream, .. } => match stream {
                        OutputStream::Stdout => ProcessOutputReport {
                            stdout: first,
                            ..Default::default()
                        },
                        OutputStream::Stderr => ProcessOutputReport {
                            stderr: first,
                            ..Default::default()
                        },
                        OutputStream::Merged => ProcessOutputReport {
                            merged: first,
                            ..Default::default()
                        },
                    },
                };
                ProcessReport {
                    name: spec.name.into(),
                    exit: self.processes[spec.name].snapshot(),
                    output,
                }
            })
            .collect()
    }

    /// Finishes output for an exited participant or each exited host process.
    pub(crate) async fn finish_output(&self) {
        let participant_exited = matches!(self.exit(), ParticipantExit::Exited { .. } | ParticipantExit::Ended);
        let captures = std::iter::once(self.capture_rx.clone())
            .filter(|_| participant_exited)
            .chain(self.outputs.iter().filter_map(|(name, output)| {
                // Drain exited host processes independently; their siblings may still be running.
                let host_exited = self.processes.get(name).is_some_and(|process| {
                    matches!(process, ProcessObservation::Wait(_))
                        && matches!(process.current(), ExitObservation::Exited(_))
                });
                (participant_exited || host_exited).then(|| output.capture.clone())
            }));
        futures::future::join_all(captures.map(|mut capture| async move {
            while capture_state(&capture) == Capture::Following {
                if capture.changed().await.is_err() {
                    break;
                }
            }
        }))
        .await;
    }

    /// Returns the participant's exit state, or an undetermined state if its unsettled channel closes.
    pub(crate) fn exit(&self) -> ParticipantExit {
        exit_state(&self.exit_rx)
    }

    /// Resolves only when the participant is known to have exited.
    pub(crate) async fn stopped(&self) -> String {
        let mut receiver = self.exit_rx.clone();
        loop {
            match self.exit() {
                ParticipantExit::Exited { code, error } => {
                    let docker = error.map(|error| format!(" (Docker: {error})")).unwrap_or_default();
                    return format!("exited with code {code}{docker}");
                }
                ParticipantExit::Ended => {
                    let statuses = self
                        .processes
                        .iter()
                        .map(|(name, process)| {
                            let status = match process.current() {
                                ExitObservation::Exited(ExitStatus { code: Some(code), .. }) => {
                                    format!("exited with code {code}")
                                }
                                ExitObservation::Exited(ExitStatus {
                                    signal: Some(signal), ..
                                }) => {
                                    format!("was killed by signal {signal}")
                                }
                                ExitObservation::Exited(_) => "exited without a code or signal".into(),
                                ExitObservation::Undetermined(reason) => format!("has an undetermined exit ({reason})"),
                                ExitObservation::Running => "is still running".into(),
                            };
                            format!("process '{name}' {status}")
                        })
                        .collect::<Vec<_>>();
                    return format!("ended: {}", statuses.join(", "));
                }
                ParticipantExit::Running | ParticipantExit::Undetermined { .. } => {}
            }
            if receiver.changed().await.is_err() {
                // Channel closure leaves the participant's stop unconfirmed.
                return std::future::pending().await;
            }
        }
    }

    /// Returns capture progress, reporting a closed, unfinished channel as failed.
    pub(crate) fn capture(&self) -> Capture {
        capture_state(&self.capture_rx)
    }

    /// Looks up a process declared by the participant's type.
    pub(crate) fn process(&self, name: &str) -> Option<&ProcessObservation> {
        self.processes.get(name)
    }
}

/// Publishes the container's exit code and Docker error, or an undetermined state when waiting fails.
pub(crate) async fn watch_exit(
    wait: impl Future<Output = Result<ContainerExit, GenericError>>, tx: watch::Sender<ParticipantExit>,
) {
    let exit = match wait.await {
        Ok(ContainerExit { code, error }) => ParticipantExit::Exited { code, error },
        Err(error) => ParticipantExit::Undetermined {
            reason: format!("{error:#}"),
        },
    };
    tx.send_replace(exit);
}

/// Truncates both output files, copies each stream's bytes unchanged, and flushes after every chunk.
/// Publishes `Complete` at stream end or `Failed` on the first stream or file error.
pub(crate) async fn follow_output(
    stream: impl Stream<Item = Result<OutputChunk, GenericError>>, stdout_path: PathBuf, stderr_path: PathBuf,
    tx: watch::Sender<Capture>, lines: OutputLines, participant: Arc<str>, process: Option<&'static str>,
) {
    let mut stdout_lines = LineBuffer::new(lines.clone(), participant.clone(), process, OutputStream::Stdout);
    let mut stderr_lines = LineBuffer::new(lines, participant, process, OutputStream::Stderr);
    let capture: Result<(), GenericError> = async {
        let mut stdout = File::create(&stdout_path)
            .await
            .with_error_context(|| format!("cannot create output file '{}'", stdout_path.display()))?;
        let mut stderr = File::create(&stderr_path)
            .await
            .with_error_context(|| format!("cannot create output file '{}'", stderr_path.display()))?;
        let mut stream = std::pin::pin!(stream);
        while let Some(chunk) = stream.next().await {
            let received = SystemTime::now();
            let (file, path, lines, bytes) = match chunk? {
                OutputChunk::Stdout(bytes) => (&mut stdout, &stdout_path, &mut stdout_lines, bytes),
                OutputChunk::Stderr(bytes) => (&mut stderr, &stderr_path, &mut stderr_lines, bytes),
            };
            file.write_all(&bytes)
                .await
                .with_error_context(|| format!("cannot write output file '{}'", path.display()))?;
            file.flush()
                .await
                .with_error_context(|| format!("cannot flush output file '{}'", path.display()))?;
            lines.push(&bytes, received);
        }
        Ok(())
    }
    .await;
    stdout_lines.finish();
    stderr_lines.finish();
    tx.send_replace(match capture {
        Ok(()) => Capture::Complete,
        Err(error) => Capture::Failed {
            reason: format!("{error:#}"),
        },
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::{channel::mpsc, stream};
    use saluki_error::generic_error;
    use tokio::time::{sleep, timeout, Instant};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::tasks::{CaseTasks, TASK_STOP_TIME};

    struct Fixture {
        observation: ParticipantObservation,
        exit_tx: watch::Sender<ParticipantExit>,
        capture_tx: watch::Sender<Capture>,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let (exit_tx, exit_rx) = watch::channel(ParticipantExit::Running);
            let (capture_tx, capture_rx) = watch::channel(Capture::Following);
            Self {
                observation: ParticipantObservation::new(
                    exit_rx,
                    capture_rx,
                    dir.path().join("container.stdout.log"),
                    dir.path().join("container.stderr.log"),
                ),
                exit_tx,
                capture_tx,
                _dir: dir,
            }
        }

        async fn follow(&self, stream: impl Stream<Item = Result<OutputChunk, GenericError>>) {
            follow_output(
                stream,
                self.observation.stdout_path.clone().unwrap(),
                self.observation.stderr_path.clone().unwrap(),
                self.capture_tx.clone(),
                self.observation.lines.clone(),
                "p".into(),
                None,
            )
            .await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn host_output_finishes_when_process_waits_settle_before_the_group_end_task_runs() {
        let (_exit_tx, exit_rx) = watch::channel(ParticipantExit::Running);
        let mut observation = ParticipantObservation::host(exit_rx);
        let (_process_tx, process_rx) = watch::channel(ExitObservation::Exited(ExitStatus {
            code: Some(0),
            signal: None,
            reason: None,
            recorded_at: None,
        }));
        let (capture_tx, capture_rx) = watch::channel(Capture::Following);
        observation
            .processes
            .insert("main", ProcessObservation::Wait(process_rx));
        observation.outputs.insert(
            "main",
            ProcessOutput {
                capture: capture_rx,
                paths: vec![],
            },
        );
        let start = Instant::now();
        let (finished, ()) = tokio::join!(
            async {
                observation.finish_output().await;
                start.elapsed()
            },
            async {
                sleep(Duration::from_secs(1)).await;
                capture_tx.send_replace(Capture::Complete);
            }
        );
        assert_eq!(finished, Duration::from_secs(1));
        assert_eq!(observation.outputs["main"].current(), Capture::Complete);
    }

    #[tokio::test]
    async fn an_exited_host_process_drains_both_pipes_while_its_sibling_runs() {
        // Stopping case tasks cancels output followers, so an exited process's output is drained without waiting for its siblings.
        let (_exit_tx, exit_rx) = watch::channel(ParticipantExit::Running);
        let mut observation = ParticipantObservation::host(exit_rx);
        let (_, exited_rx) = watch::channel(ExitObservation::Exited(ExitStatus {
            code: Some(0),
            signal: None,
            reason: None,
            recorded_at: None,
        }));
        let (_running_tx, running_rx) = watch::channel(ExitObservation::Running);
        observation
            .processes
            .insert("main", ProcessObservation::Wait(exited_rx));
        observation
            .processes
            .insert("worker", ProcessObservation::Wait(running_rx));
        let dir = tempfile::tempdir().unwrap();
        let stdout = dir.path().join("main.stdout.log");
        let stderr = dir.path().join("main.stderr.log");
        let (capture_tx, capture_rx) = watch::channel(Capture::Following);
        let (_worker_tx, worker_rx) = watch::channel(Capture::Following);
        observation.outputs.insert(
            "main",
            ProcessOutput {
                capture: capture_rx,
                paths: vec![stdout.clone(), stderr.clone()],
            },
        );
        observation.outputs.insert(
            "worker",
            ProcessOutput {
                capture: worker_rx,
                paths: vec![],
            },
        );
        let tasks = CaseTasks::new(&CancellationToken::new());
        let (tx, rx) = mpsc::unbounded();
        tasks.spawn(follow_output(
            rx,
            stdout.clone(),
            stderr.clone(),
            capture_tx,
            observation.lines.clone(),
            "p".into(),
            Some("main"),
        ));
        let mut finishing = std::pin::pin!(observation.finish_output());
        std::future::poll_fn(|cx| {
            assert!(finishing.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tx.unbounded_send(Ok(OutputChunk::Stdout(b"last stdout\n".as_slice().into())))
            .unwrap();
        tx.unbounded_send(Ok(OutputChunk::Stderr(b"last stderr\n".as_slice().into())))
            .unwrap();
        drop(tx);
        timeout(Duration::from_secs(5), finishing).await.unwrap();
        assert_eq!(tasks.stop_all(Instant::now() + TASK_STOP_TIME).await, 0);
        assert_eq!(observation.outputs["main"].current(), Capture::Complete);
        assert_eq!(observation.outputs["worker"].current(), Capture::Following);
        assert_eq!(observation.processes["worker"].current(), ExitObservation::Running);
        assert_eq!(std::fs::read(stdout).unwrap(), b"last stdout\n");
        assert_eq!(std::fs::read(stderr).unwrap(), b"last stderr\n");
    }

    #[test]
    fn exit_status_displays_codes_signals_and_reasons() {
        for (code, signal, reason, expected) in [
            (Some(1), None, None, "code 1"),
            (None, Some(9), None, "signal 9"),
            (Some(3), None, Some("runtime failure"), "code 3 (runtime failure)"),
            (None, Some(15), Some("stopped"), "signal 15 (stopped)"),
        ] {
            let status = ExitStatus {
                code,
                signal,
                reason: reason.map(str::to_owned),
                recorded_at: None,
            };
            assert_eq!(status.to_string(), expected);
        }
    }

    #[test]
    fn exit_reports_keep_the_state_source_code_signal_and_failure_reason() {
        use serde_json::json;
        for (exit, expected) in [
            (
                ParticipantExit::Running,
                json!({"state": "running", "source": "container"}),
            ),
            (
                ParticipantExit::Exited {
                    code: 3,
                    error: Some("runtime failure".into()),
                },
                json!({"state": "exited", "source": "container", "code": 3, "reason": "runtime failure"}),
            ),
            (
                ParticipantExit::Undetermined {
                    reason: "connection lost".into(),
                },
                json!({"state": "undetermined", "source": "container", "reason": "connection lost"}),
            ),
        ] {
            let (_tx, rx) = watch::channel(exit);
            assert_eq!(
                serde_json::to_value(ProcessObservation::Container(rx).snapshot()).unwrap(),
                expected
            );
        }
        for (exit, expected) in [
            (
                ExitObservation::Running,
                json!({"state": "running", "source": "record"}),
            ),
            (
                ExitObservation::Exited(ExitStatus {
                    code: None,
                    signal: Some(9),
                    recorded_at: None,
                    reason: None,
                }),
                json!({"state": "exited", "source": "record", "signal": 9}),
            ),
            (
                ExitObservation::Exited(ExitStatus {
                    code: Some(3),
                    signal: None,
                    recorded_at: None,
                    reason: None,
                }),
                json!({"state": "exited", "source": "record", "code": 3}),
            ),
            (
                ExitObservation::Undetermined("record missing".into()),
                json!({"state": "undetermined", "source": "record", "reason": "record missing"}),
            ),
        ] {
            for source in ["record", "process"] {
                let (_tx, rx) = watch::channel(exit.clone());
                let observation = if source == "record" {
                    ProcessObservation::Record {
                        status: rx,
                        refresh: None,
                    }
                } else {
                    ProcessObservation::Wait(rx)
                };
                let mut expected = expected.clone();
                expected["source"] = json!(source);
                assert_eq!(serde_json::to_value(observation.snapshot()).unwrap(), expected);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn process_waits_settle_with_the_reapers_status_and_preserve_it_after_closure() {
        for exit in [
            ExitObservation::Exited(ExitStatus {
                code: Some(3),
                signal: None,
                reason: None,
                recorded_at: None,
            }),
            ExitObservation::Exited(ExitStatus {
                code: None,
                signal: Some(9),
                reason: None,
                recorded_at: None,
            }),
            ExitObservation::Undetermined("waiting for process 'main' failed: wait error".into()),
        ] {
            let (tx, rx) = watch::channel(ExitObservation::Running);
            let process = ProcessObservation::Wait(rx);
            assert_eq!(process.current(), ExitObservation::Running);
            let started = Instant::now();
            let (_, observed) = tokio::join!(
                async {
                    sleep(Duration::from_secs(1)).await;
                    tx.send_replace(exit.clone());
                    drop(tx);
                },
                process.settled(),
            );
            assert_eq!(observed, exit);
            assert_eq!(process.current(), exit);
            assert_eq!(started.elapsed(), Duration::from_secs(1));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_stop_description_waits_for_the_group_end_and_names_every_process_status() {
        let (tx, rx) = watch::channel(ParticipantExit::Running);
        let mut observation = ParticipantObservation::host(rx);
        for (name, code, signal) in [("y", None, Some(9)), ("x", Some(3), None)] {
            let (_, rx) = watch::channel(ExitObservation::Exited(ExitStatus {
                code,
                signal,
                reason: None,
                recorded_at: None,
            }));
            observation.processes.insert(name, ProcessObservation::Wait(rx));
        }
        assert!(timeout(Duration::from_secs(1), observation.stopped()).await.is_err());
        tx.send_replace(ParticipantExit::Ended);
        drop(tx);
        assert_eq!(
            observation.stopped().await,
            "ended: process 'x' exited with code 3, process 'y' was killed by signal 9"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_ended_host_waits_for_every_process_output_to_flush() {
        let (_, rx) = watch::channel(ParticipantExit::Ended);
        let mut observation = ParticipantObservation::host(rx);
        let (first_tx, first_rx) = watch::channel(Capture::Following);
        let (last_tx, last_rx) = watch::channel(Capture::Following);
        for (name, capture) in [("first", first_rx), ("last", last_rx)] {
            observation
                .outputs
                .insert(name, ProcessOutput { capture, paths: vec![] });
        }
        assert_eq!(observation.capture(), Capture::Complete);
        let started = Instant::now();
        let (finished_after, ()) = tokio::join!(
            async {
                observation.finish_output().await;
                started.elapsed()
            },
            async {
                sleep(Duration::from_secs(1)).await;
                first_tx.send_replace(Capture::Complete);
                sleep(Duration::from_secs(1)).await;
                last_tx.send_replace(Capture::Complete);
            },
        );
        assert_eq!(finished_after, Duration::from_secs(2));
        assert!(observation
            .outputs
            .values()
            .all(|output| output.current() == Capture::Complete));
    }

    #[test]
    fn host_reports_use_each_processs_two_pipe_paths_and_process_exit_source() {
        use crate::participant::ExitChannel;
        let (_, rx) = watch::channel(ParticipantExit::Ended);
        let mut observation = ParticipantObservation::host(rx);
        let specs = ["first", "last"].map(|name| ProcessSpec {
            name,
            exit: ExitChannel::Wait,
            output: OutputChannel::Pipes,
        });
        for spec in specs {
            let (_, rx) = watch::channel(ExitObservation::Exited(ExitStatus {
                code: Some(3),
                signal: None,
                reason: None,
                recorded_at: None,
            }));
            observation.processes.insert(spec.name, ProcessObservation::Wait(rx));
            observation.outputs.insert(
                spec.name,
                ProcessOutput {
                    capture: watch::channel(Capture::Complete).1,
                    paths: vec![
                        format!("p/{}.stdout.log", spec.name).into(),
                        format!("p/{}.stderr.log", spec.name).into(),
                    ],
                },
            );
        }
        assert!(observation.stdout_path.is_none());
        assert!(observation.stderr_path.is_none());
        assert_eq!(observation.capture(), Capture::Complete);
        let reports = observation.snapshot(&specs);
        assert_eq!(
            reports.iter().map(|report| report.name.as_str()).collect::<Vec<_>>(),
            ["first", "last"]
        );
        for report in reports {
            assert_eq!(
                report.exit,
                ProcessExitReport::Exited {
                    source: ExitSource::Process,
                    code: Some(3),
                    signal: None,
                    reason: None,
                }
            );
            assert_eq!(
                report.output,
                ProcessOutputReport {
                    stdout: Some(format!("p/{}.stdout.log", report.name).into()),
                    stderr: Some(format!("p/{}.stderr.log", report.name).into()),
                    merged: None,
                }
            );
        }
    }

    #[test]
    fn host_overflow_diagnostics_name_only_the_existing_process_capture_paths() {
        let (_, rx) = watch::channel(ParticipantExit::Ended);
        let mut observation = ParticipantObservation::host(rx);
        observation.lines = OutputLines::new(1);
        observation.outputs.insert(
            "main",
            ProcessOutput {
                capture: watch::channel(Capture::Complete).1,
                paths: vec!["p/main.stdout.log".into(), "p/main.stderr.log".into()],
            },
        );
        observation.lines.push(output::OutputLine {
            participant: "p".into(),
            process: Some("main"),
            stream: OutputStream::Stdout,
            received: SystemTime::now(),
            text: "too long".into(),
        });
        assert_eq!(observation.lines_since(0).unwrap_err(),
            "output capture failed: in-memory output exceeded the 1-byte line-text bound; see output files: p/main.stderr.log, p/main.stdout.log");
    }

    #[test]
    fn process_reports_take_output_paths_and_streams_from_the_declared_channels() {
        use serde_json::json;

        use crate::participant::ExitChannel;
        let mut fixture = Fixture::new();
        let dir = fixture._dir.path();
        let mut specs = vec![ProcessSpec {
            name: "main",
            exit: ExitChannel::Container,
            output: OutputChannel::Container,
        }];
        fixture.observation.processes.insert(
            "main",
            ProcessObservation::Container(fixture.observation.exit_rx.clone()),
        );
        fixture.observation.outputs.insert(
            "main",
            ProcessOutput {
                capture: fixture.observation.capture_rx.clone(),
                // Reverse the list to check that stream labels follow the named paths.
                paths: vec![dir.join("container.stderr.log"), dir.join("container.stdout.log")],
            },
        );
        for (name, stream) in [
            ("a", OutputStream::Stdout),
            ("b", OutputStream::Stderr),
            ("c", OutputStream::Merged),
        ] {
            let spec = ProcessSpec {
                name,
                exit: ExitChannel::Record { file: name },
                output: OutputChannel::File { file: name, stream },
            };
            let path = dir.join(output::host_output_file(name, spec.output).unwrap());
            fixture.observation.outputs.insert(
                name,
                ProcessOutput {
                    capture: fixture.observation.capture_rx.clone(),
                    paths: vec![path],
                },
            );
            let (_tx, rx) = watch::channel(ExitObservation::Undetermined("record missing".into()));
            fixture.observation.processes.insert(
                name,
                ProcessObservation::Record {
                    status: rx,
                    refresh: None,
                },
            );
            specs.push(spec);
        }
        let report = serde_json::to_value(fixture.observation.snapshot(&specs)).unwrap();
        assert_eq!(
            report,
            json!([
                {"name": "main", "exit": {"state": "running", "source": "container"}, "output": {"stdout": dir.join("container.stdout.log"), "stderr": dir.join("container.stderr.log")}},
                {"name": "a", "exit": {"state": "undetermined", "source": "record", "reason": "record missing"}, "output": {"stdout": dir.join("a.stdout.log")}},
                {"name": "b", "exit": {"state": "undetermined", "source": "record", "reason": "record missing"}, "output": {"stderr": dir.join("b.stderr.log")}},
                {"name": "c", "exit": {"state": "undetermined", "source": "record", "reason": "record missing"}, "output": {"merged": dir.join("c.log")}},
            ])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_container_exit_channel_reports_the_containers_exit_or_undetermined() {
        for (exit, expected) in [
            (
                ParticipantExit::Exited { code: 0, error: None },
                ExitObservation::Exited(ExitStatus {
                    code: Some(0),
                    signal: None,
                    reason: None,
                    recorded_at: None,
                }),
            ),
            (
                ParticipantExit::Exited {
                    code: 3,
                    error: Some("runtime failure".into()),
                },
                ExitObservation::Exited(ExitStatus {
                    code: Some(3),
                    signal: None,
                    reason: Some("runtime failure".into()),
                    recorded_at: None,
                }),
            ),
            (
                ParticipantExit::Exited { code: 137, error: None },
                ExitObservation::Exited(ExitStatus {
                    code: Some(137),
                    signal: None,
                    reason: None,
                    recorded_at: None,
                }),
            ),
            (
                ParticipantExit::Undetermined {
                    reason: "wait failed".into(),
                },
                ExitObservation::Undetermined("wait failed".into()),
            ),
            (
                ParticipantExit::Ended,
                ExitObservation::Undetermined("participant ended without a container exit status".into()),
            ),
            (
                ParticipantExit::Exited {
                    code: i64::MAX,
                    error: None,
                },
                ExitObservation::Undetermined(format!("container exit code {} is outside the i32 range", i64::MAX)),
            ),
        ] {
            let mut fixture = Fixture::new();
            fixture.observation.processes.insert(
                "main",
                ProcessObservation::Container(fixture.observation.exit_rx.clone()),
            );
            let process = fixture.observation.process("main").unwrap();
            assert!(fixture.observation.process("unknown").is_none());
            assert_eq!(process.current(), ExitObservation::Running);
            assert_eq!(process.refresh().await, ExitObservation::Running);
            let started = Instant::now();
            let (_, observed) = tokio::join!(
                async {
                    sleep(Duration::from_secs(1)).await;
                    fixture.exit_tx.send_replace(exit);
                },
                process.settled(),
            );
            assert_eq!(observed, expected);
            assert_eq!(process.current(), expected);
            assert_eq!(process.refresh().await, expected);
            assert_eq!(started.elapsed(), Duration::from_secs(1));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_process_channel_without_a_status_is_undetermined() {
        let (container_tx, container_rx) = watch::channel(ParticipantExit::Running);
        let (record_tx, record_rx) = watch::channel(ExitObservation::Running);
        let (wait_tx, wait_rx) = watch::channel(ExitObservation::Running);
        drop((container_tx, record_tx, wait_tx));
        for observation in [
            ProcessObservation::Container(container_rx),
            ProcessObservation::Record {
                status: record_rx,
                refresh: None,
            },
            ProcessObservation::Wait(wait_rx),
        ] {
            let expected = ExitObservation::Undetermined("exit observation ended without a status".into());
            assert_eq!(observation.current(), expected);
            assert_eq!(
                timeout(Duration::from_secs(1), observation.settled()).await.unwrap(),
                expected
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_participant_exit_channel_agrees_with_its_container_process() {
        for exit in [
            ParticipantExit::Running,
            ParticipantExit::Exited { code: 3, error: None },
            ParticipantExit::Undetermined {
                reason: "wait failed".into(),
            },
        ] {
            let fixture = Fixture::new();
            let process = ProcessObservation::Container(fixture.observation.exit_rx.clone());
            fixture.exit_tx.send_replace(exit.clone());
            drop(fixture.exit_tx);
            match fixture.observation.exit() {
                ParticipantExit::Exited { .. } => {
                    assert_eq!(fixture.observation.stopped().await, "exited with code 3");
                    assert_eq!(
                        process.current(),
                        ExitObservation::Exited(ExitStatus {
                            code: Some(3),
                            signal: None,
                            reason: None,
                            recorded_at: None,
                        })
                    );
                }
                ParticipantExit::Undetermined { reason } => {
                    let expected = match exit {
                        ParticipantExit::Running => "exit observation ended without a status",
                        ParticipantExit::Undetermined { .. } => "wait failed",
                        ParticipantExit::Exited { .. } | ParticipantExit::Ended => {
                            panic!("a reported exit must stay settled")
                        }
                    };
                    assert_eq!(reason, expected);
                    assert_eq!(process.current(), ExitObservation::Undetermined(reason));
                    assert!(timeout(Duration::from_secs(1), fixture.observation.stopped())
                        .await
                        .is_err());
                }
                ParticipantExit::Running => panic!("a closed channel must settle"),
                ParticipantExit::Ended => panic!("a container must report its own exit"),
            }
        }
    }

    #[tokio::test]
    async fn a_closed_capture_channel_preserves_completion_or_reports_failure() {
        for capture in [
            Capture::Following,
            Capture::Complete,
            Capture::Failed {
                reason: "file write failed".into(),
            },
        ] {
            let fixture = Fixture::new();
            fixture.capture_tx.send_replace(capture.clone());
            drop(fixture.capture_tx);
            let expected = match capture {
                Capture::Following => Capture::Failed {
                    reason: "output capture ended before the stream completed".into(),
                },
                settled => settled,
            };
            assert_eq!(fixture.observation.capture(), expected);
        }
    }

    #[tokio::test]
    async fn a_container_exit_is_observed_with_its_code_and_docker_error() {
        for (code, error) in [(0, None), (3, Some("runtime failure".to_string()))] {
            let fixture = Fixture::new();
            assert_eq!(fixture.observation.exit(), ParticipantExit::Running);
            watch_exit(
                async {
                    Ok(ContainerExit {
                        code,
                        error: error.clone(),
                    })
                },
                fixture.exit_tx,
            )
            .await;
            assert_eq!(
                fixture.observation.exit(),
                ParticipantExit::Exited {
                    code,
                    error: error.clone()
                }
            );
            let docker = error.map(|error| format!(" (Docker: {error})")).unwrap_or_default();
            assert_eq!(
                fixture.observation.stopped().await,
                format!("exited with code {code}{docker}")
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_wait_leaves_the_participant_exit_undetermined() {
        let fixture = Fixture::new();
        watch_exit(async { Err(generic_error!("wait connection lost")) }, fixture.exit_tx).await;
        assert_eq!(
            fixture.observation.exit(),
            ParticipantExit::Undetermined {
                reason: "wait connection lost".into()
            }
        );
        assert!(timeout(Duration::from_secs(1), fixture.observation.stopped())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn container_output_reaches_separate_stdout_and_stderr_files_as_it_arrives() {
        let fixture = Fixture::new();
        let (tx, rx) = mpsc::unbounded();
        let tasks = CaseTasks::new(&CancellationToken::new());
        let following = tasks.spawn(follow_output(
            rx,
            fixture.observation.stdout_path.clone().unwrap(),
            fixture.observation.stderr_path.clone().unwrap(),
            fixture.capture_tx.clone(),
            fixture.observation.lines.clone(),
            "p".into(),
            None,
        ));
        let stdout = b"out\x00\xff\n";
        let stderr = b"err\xfe\n";
        tx.unbounded_send(Ok(OutputChunk::Stdout(stdout.as_slice().into())))
            .unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::read(fixture.observation.stdout_path.as_ref().unwrap())
                    .ok()
                    .as_deref()
                    == Some(stdout)
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stdout must reach disk before the stream ends");
        assert_eq!(fixture.observation.capture(), Capture::Following);
        tx.unbounded_send(Ok(OutputChunk::Stderr(stderr.as_slice().into())))
            .unwrap();
        drop(tx);
        assert_eq!(
            timeout(Duration::from_secs(5), following).await.unwrap().unwrap(),
            Some(())
        );
        assert_eq!(
            std::fs::read(fixture.observation.stdout_path.as_ref().unwrap()).unwrap(),
            stdout
        );
        assert_eq!(
            std::fs::read(fixture.observation.stderr_path.as_ref().unwrap()).unwrap(),
            stderr
        );
        assert_eq!(fixture.observation.capture(), Capture::Complete);
        assert_eq!(tasks.stop_all(Instant::now() + TASK_STOP_TIME).await, 0);
    }

    #[tokio::test]
    async fn container_log_lines_are_tagged_with_the_process_owning_the_container_output_or_none() {
        for process in [Some("main"), None] {
            let fixture = Fixture::new();
            follow_output(
                stream::iter([
                    Ok(OutputChunk::Stdout(b"out\n".as_slice().into())),
                    Ok(OutputChunk::Stderr(b"err".as_slice().into())),
                ]),
                fixture.observation.stdout_path.clone().unwrap(),
                fixture.observation.stderr_path.clone().unwrap(),
                fixture.capture_tx.clone(),
                fixture.observation.lines.clone(),
                "p".into(),
                process,
            )
            .await;
            let (lines, cursor) = fixture.observation.lines.since(0).unwrap();
            assert_eq!(cursor, 2);
            for (line, text, stream) in [
                (&lines[0], "out", OutputStream::Stdout),
                (&lines[1], "err", OutputStream::Stderr),
            ] {
                assert_eq!(&*line.participant, "p");
                assert_eq!(line.process, process);
                assert_eq!(line.stream, stream);
                assert_eq!(line.text, text);
            }
            assert_eq!(fixture.observation.capture(), Capture::Complete);
        }
    }

    #[tokio::test]
    async fn both_output_files_exist_when_the_container_prints_nothing() {
        let fixture = Fixture::new();
        fixture.follow(stream::empty()).await;
        assert_eq!(
            std::fs::read(fixture.observation.stdout_path.as_ref().unwrap()).unwrap(),
            b""
        );
        assert_eq!(
            std::fs::read(fixture.observation.stderr_path.as_ref().unwrap()).unwrap(),
            b""
        );
        assert_eq!(fixture.observation.capture(), Capture::Complete);
    }

    #[tokio::test]
    async fn a_failed_log_stream_is_a_capture_error_and_keeps_the_output_already_written() {
        let fixture = Fixture::new();
        fixture
            .follow(stream::iter([
                Ok(OutputChunk::Stdout(b"before\n".as_slice().into())),
                Err(generic_error!("log connection lost")),
                Ok(OutputChunk::Stdout(b"after\n".as_slice().into())),
            ]))
            .await;
        assert_eq!(
            fixture.observation.capture(),
            Capture::Failed {
                reason: "log connection lost".into()
            }
        );
        assert_eq!(
            std::fs::read(fixture.observation.stdout_path.as_ref().unwrap()).unwrap(),
            b"before\n"
        );
        assert_eq!(
            std::fs::read(fixture.observation.stderr_path.as_ref().unwrap()).unwrap(),
            b""
        );
    }

    #[tokio::test]
    async fn a_log_stream_that_ends_completes_capture() {
        let fixture = Fixture::new();
        for path in [
            fixture.observation.stdout_path.as_ref().unwrap(),
            fixture.observation.stderr_path.as_ref().unwrap(),
        ] {
            std::fs::write(path, b"old output").unwrap();
        }
        fixture
            .follow(stream::iter([
                Ok(OutputChunk::Stdout(b"new\n".as_slice().into())),
                Ok(OutputChunk::Stderr(b"error\n".as_slice().into())),
            ]))
            .await;
        assert_eq!(fixture.observation.capture(), Capture::Complete);
        assert_eq!(
            std::fs::read(fixture.observation.stdout_path.as_ref().unwrap()).unwrap(),
            b"new\n"
        );
        assert_eq!(
            std::fs::read(fixture.observation.stderr_path.as_ref().unwrap()).unwrap(),
            b"error\n"
        );
    }

    #[tokio::test]
    async fn an_output_file_creation_error_fails_capture_before_reading_the_stream() {
        for stderr in [false, true] {
            let fixture = Fixture::new();
            let path = if stderr {
                fixture.observation.stderr_path.as_ref().unwrap()
            } else {
                fixture.observation.stdout_path.as_ref().unwrap()
            };
            std::fs::create_dir(path).unwrap();
            fixture
                .follow(stream::poll_fn(|_| panic!("a failed capture must stop reading")))
                .await;
            let Capture::Failed { reason } = fixture.observation.capture() else {
                panic!("file creation must fail capture");
            };
            assert!(
                reason.contains(&format!("cannot create output file '{}'", path.display())),
                "{reason}"
            );
        }
    }
}
