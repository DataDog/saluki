//! Participant exit and output observations maintained by case-owned tasks.

use std::{collections::BTreeMap, future::Future, path::PathBuf, time::SystemTime};

use airlock::{exec::OutputChunk, observe::ContainerExit};
use futures::{Stream, StreamExt as _};
use saluki_error::{ErrorContext as _, GenericError};
use tokio::{fs::File, io::AsyncWriteExt as _, sync::watch};

pub(crate) mod exit_record;

/// A process exit reported by its declared channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExitStatus {
    /// The exit code. Docker represents signal deaths as 128 plus the signal number.
    pub code: Option<i32>,
    /// The signal from an exit record, set for a signal death instead of `code`.
    pub signal: Option<i32>,
    /// Timestamp on the container's clock, or `None` for container waits. Never compare it with host time.
    pub recorded_at: Option<SystemTime>,
}

/// A named process's first exit, or the state of waiting for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExitObservation {
    /// The declared channel is awaiting an exit.
    Running,
    /// The first exit, retained across process restarts.
    Exited(ExitStatus),
    /// The channel could not determine an exit status.
    Undetermined(String),
}

/// A process's view of its container wait or supervisor-written exit record.
#[derive(Clone)]
pub(crate) enum ProcessObservation {
    /// Shares the participant's container exit observation.
    Container(watch::Receiver<ParticipantExit>),
    /// Receives the first recorded exit from a case-owned watcher.
    Record(watch::Receiver<ExitObservation>),
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
                    // TODO: preserve Docker's exit error and the observation source in exit-step diagnostics.
                    ParticipantExit::Exited { code, .. } => match i32::try_from(*code) {
                        Ok(code) => ExitObservation::Exited(ExitStatus {
                            code: Some(code),
                            signal: None,
                            recorded_at: None,
                        }),
                        Err(_) => ExitObservation::Undetermined(format!(
                            "container exit code {code} is outside the i32 range"
                        )),
                    },
                    ParticipantExit::Undetermined { reason } => ExitObservation::Undetermined(reason.clone()),
                };
                (observation, receiver.has_changed().is_err())
            }
            Self::Record(receiver) => {
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

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Exit steps and the run report read process exit observations.")
    )]
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
                Self::Record(receiver) => {
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
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub processes: BTreeMap<&'static str, ProcessObservation>,
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
            stdout_path,
            stderr_path,
            processes: BTreeMap::new(),
        }
    }

    /// Returns the participant's exit state, or an undetermined state if its unsettled channel closes.
    pub(crate) fn exit(&self) -> ParticipantExit {
        let current = self.exit_rx.borrow();
        if *current == ParticipantExit::Running && self.exit_rx.has_changed().is_err() {
            ParticipantExit::Undetermined {
                reason: "exit observation ended without a status".into(),
            }
        } else {
            current.clone()
        }
    }

    /// Resolves only when the participant is known to have exited.
    pub(crate) async fn stopped(&self) -> ContainerExit {
        let mut receiver = self.exit_rx.clone();
        loop {
            let current = self.exit();
            if let ParticipantExit::Exited { code, error } = current {
                return ContainerExit { code, error };
            }
            if receiver.changed().await.is_err() {
                // Channel closure leaves the participant's stop unconfirmed.
                return std::future::pending().await;
            }
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Output evidence and log steps fail on a capture error.")
    )]
    /// Returns capture progress, reporting a closed, unfinished channel as failed.
    pub(crate) fn capture(&self) -> Capture {
        let current = self.capture_rx.borrow();
        if *current == Capture::Following && self.capture_rx.has_changed().is_err() {
            Capture::Failed {
                reason: "output capture ended before the stream completed".into(),
            }
        } else {
            current.clone()
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Exit steps and the run report read process exit observations.")
    )]
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
    tx: watch::Sender<Capture>,
) {
    let capture: Result<(), GenericError> = async {
        let mut stdout = File::create(&stdout_path)
            .await
            .with_error_context(|| format!("cannot create output file '{}'", stdout_path.display()))?;
        let mut stderr = File::create(&stderr_path)
            .await
            .with_error_context(|| format!("cannot create output file '{}'", stderr_path.display()))?;
        let mut stream = std::pin::pin!(stream);
        while let Some(chunk) = stream.next().await {
            let (file, path, bytes) = match chunk? {
                OutputChunk::Stdout(bytes) => (&mut stdout, &stdout_path, bytes),
                OutputChunk::Stderr(bytes) => (&mut stderr, &stderr_path, bytes),
            };
            file.write_all(&bytes)
                .await
                .with_error_context(|| format!("cannot write output file '{}'", path.display()))?;
            file.flush()
                .await
                .with_error_context(|| format!("cannot flush output file '{}'", path.display()))?;
        }
        Ok(())
    }
    .await;
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
                self.observation.stdout_path.clone(),
                self.observation.stderr_path.clone(),
                self.capture_tx.clone(),
            )
            .await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_container_exit_channel_reports_the_containers_exit_or_undetermined() {
        for (exit, expected) in [
            (
                ParticipantExit::Exited { code: 0, error: None },
                ExitObservation::Exited(ExitStatus {
                    code: Some(0),
                    signal: None,
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
                    recorded_at: None,
                }),
            ),
            (
                ParticipantExit::Exited { code: 137, error: None },
                ExitObservation::Exited(ExitStatus {
                    code: Some(137),
                    signal: None,
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
            assert_eq!(started.elapsed(), Duration::from_secs(1));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_process_channel_without_a_status_is_undetermined() {
        let (container_tx, container_rx) = watch::channel(ParticipantExit::Running);
        let (record_tx, record_rx) = watch::channel(ExitObservation::Running);
        drop((container_tx, record_tx));
        for observation in [
            ProcessObservation::Container(container_rx),
            ProcessObservation::Record(record_rx),
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
                ParticipantExit::Exited { code, error } => {
                    assert_eq!(fixture.observation.stopped().await, ContainerExit { code, error });
                    assert_eq!(
                        process.current(),
                        ExitObservation::Exited(ExitStatus {
                            code: Some(3),
                            signal: None,
                            recorded_at: None,
                        })
                    );
                }
                ParticipantExit::Undetermined { reason } => {
                    let expected = match exit {
                        ParticipantExit::Running => "exit observation ended without a status",
                        ParticipantExit::Undetermined { .. } => "wait failed",
                        ParticipantExit::Exited { .. } => panic!("a reported exit must stay settled"),
                    };
                    assert_eq!(reason, expected);
                    assert_eq!(process.current(), ExitObservation::Undetermined(reason));
                    assert!(timeout(Duration::from_secs(1), fixture.observation.stopped())
                        .await
                        .is_err());
                }
                ParticipantExit::Running => panic!("a closed channel must settle"),
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
            assert_eq!(fixture.observation.stopped().await, ContainerExit { code, error });
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
            fixture.observation.stdout_path.clone(),
            fixture.observation.stderr_path.clone(),
            fixture.capture_tx.clone(),
        ));
        let stdout = b"out\x00\xff\n";
        let stderr = b"err\xfe\n";
        tx.unbounded_send(Ok(OutputChunk::Stdout(stdout.as_slice().into())))
            .unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::read(&fixture.observation.stdout_path).ok().as_deref() == Some(stdout) {
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
        assert_eq!(std::fs::read(&fixture.observation.stdout_path).unwrap(), stdout);
        assert_eq!(std::fs::read(&fixture.observation.stderr_path).unwrap(), stderr);
        assert_eq!(fixture.observation.capture(), Capture::Complete);
        assert_eq!(tasks.stop_all(Instant::now() + TASK_STOP_TIME).await, 0);
    }

    #[tokio::test]
    async fn both_output_files_exist_when_the_container_prints_nothing() {
        let fixture = Fixture::new();
        fixture.follow(stream::empty()).await;
        assert_eq!(std::fs::read(&fixture.observation.stdout_path).unwrap(), b"");
        assert_eq!(std::fs::read(&fixture.observation.stderr_path).unwrap(), b"");
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
        assert_eq!(std::fs::read(&fixture.observation.stdout_path).unwrap(), b"before\n");
        assert_eq!(std::fs::read(&fixture.observation.stderr_path).unwrap(), b"");
    }

    #[tokio::test]
    async fn a_log_stream_that_ends_completes_capture() {
        let fixture = Fixture::new();
        for path in [&fixture.observation.stdout_path, &fixture.observation.stderr_path] {
            std::fs::write(path, b"old output").unwrap();
        }
        fixture
            .follow(stream::iter([
                Ok(OutputChunk::Stdout(b"new\n".as_slice().into())),
                Ok(OutputChunk::Stderr(b"error\n".as_slice().into())),
            ]))
            .await;
        assert_eq!(fixture.observation.capture(), Capture::Complete);
        assert_eq!(std::fs::read(&fixture.observation.stdout_path).unwrap(), b"new\n");
        assert_eq!(std::fs::read(&fixture.observation.stderr_path).unwrap(), b"error\n");
    }

    #[tokio::test]
    async fn an_output_file_creation_error_fails_capture_before_reading_the_stream() {
        for stderr in [false, true] {
            let fixture = Fixture::new();
            let path = if stderr {
                &fixture.observation.stderr_path
            } else {
                &fixture.observation.stdout_path
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
