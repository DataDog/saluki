//! Participant exit and output observations maintained by case-owned tasks.

use std::{future::Future, path::PathBuf};

use airlock::{exec::OutputChunk, observe::ContainerExit};
use futures::{Stream, StreamExt as _};
use saluki_error::{ErrorContext as _, GenericError};
use tokio::{fs::File, io::AsyncWriteExt as _, sync::watch};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParticipantExit {
    Running,
    Exited { code: i64, error: Option<String> },
    Undetermined { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Capture {
    Following,
    Complete,
    Failed { reason: String },
}

pub(crate) struct ParticipantObservation {
    pub exit_rx: watch::Receiver<ParticipantExit>,
    pub capture_rx: watch::Receiver<Capture>,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

impl ParticipantObservation {
    pub(crate) fn exit(&self) -> ParticipantExit {
        self.exit_rx.borrow().clone()
    }

    /// Resolves only when the participant is known to have exited.
    pub(crate) async fn stopped(&self) -> ParticipantExit {
        let mut receiver = self.exit_rx.clone();
        loop {
            let current = self.exit();
            if matches!(current, ParticipantExit::Exited { .. }) {
                return current;
            }
            if receiver.changed().await.is_err() {
                // Losing the watcher says nothing about whether the participant stopped.
                return std::future::pending().await;
            }
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Output evidence and log steps fail on a capture error.")
    )]
    pub(crate) fn capture(&self) -> Capture {
        self.capture_rx.borrow().clone()
    }
}

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
                observation: ParticipantObservation {
                    exit_rx,
                    capture_rx,
                    stdout_path: dir.path().join("container.stdout.log"),
                    stderr_path: dir.path().join("container.stderr.log"),
                },
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
            let expected = ParticipantExit::Exited { code, error };
            assert_eq!(fixture.observation.exit(), expected);
            assert_eq!(fixture.observation.stopped().await, expected);
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
