//! Tagged output lines and incremental file capture with a final read after exit.

use std::{
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use airlock::exec::OutputChunk;
use futures::{Stream, StreamExt as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{fs::File, io::AsyncWriteExt as _, sync::watch, time::timeout};

use super::{exit_state, Capture, ParticipantExit};
use crate::participant::{OutputChannel, OutputStream};

/// Names the host file for a file output channel; container output uses the participant's shared files.
pub(crate) fn host_output_file(process: &str, output: OutputChannel) -> Option<String> {
    match output {
        OutputChannel::Container => None,
        OutputChannel::File { stream, .. } => Some(match stream {
            OutputStream::Stdout => format!("{process}.stdout.log"),
            OutputStream::Stderr => format!("{process}.stderr.log"),
            OutputStream::Merged => format!("{process}.log"),
        }),
    }
}

/// Time for the container wait to catch up with a follower that stopped.
const FOLLOWER_EXIT_GRACE: Duration = Duration::from_secs(5);

/// One line of output, tagged with where it came from and when its last chunk arrived.
#[derive(Clone, Debug)]
pub(crate) struct OutputLine {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Log steps and reports read participant tags.")
    )]
    pub participant: Arc<str>,
    pub process: Option<&'static str>,
    #[cfg_attr(not(test), expect(dead_code, reason = "Log steps select output streams."))]
    pub stream: OutputStream,
    #[cfg_attr(not(test), expect(dead_code, reason = "Log reports read receipt times."))]
    pub received: SystemTime,
    pub text: String,
}

/// All lines of one participant, shared by its output sources and read with independent cursors.
#[derive(Clone, Default)]
pub(crate) struct OutputLines(Arc<Mutex<Vec<OutputLine>>>);

impl OutputLines {
    pub(crate) fn push(&self, line: OutputLine) {
        self.0.lock().unwrap().push(line);
    }

    pub(crate) fn since(&self, index: usize) -> (Vec<OutputLine>, usize) {
        let lines = self.0.lock().unwrap();
        (lines[index..].to_vec(), lines.len())
    }
}

/// Splits one source stream into lines, keeping an unterminated line until more bytes or the end of the source.
pub(crate) struct LineBuffer {
    lines: OutputLines,
    participant: Arc<str>,
    process: Option<&'static str>,
    stream: OutputStream,
    pending: Vec<u8>,
    received: SystemTime,
}

impl LineBuffer {
    pub(crate) fn new(
        lines: OutputLines, participant: Arc<str>, process: Option<&'static str>, stream: OutputStream,
    ) -> Self {
        Self {
            lines,
            participant,
            process,
            stream,
            pending: Vec::new(),
            received: SystemTime::now(),
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8], received: SystemTime) {
        if bytes.is_empty() {
            return;
        }
        self.received = received;
        let mut remaining = bytes;
        while let Some(end) = remaining.iter().position(|byte| *byte == b'\n') {
            self.pending.extend_from_slice(&remaining[..end]);
            self.publish();
            remaining = &remaining[end + 1..];
        }
        self.pending.extend_from_slice(remaining);
    }

    pub(crate) fn finish(&mut self) {
        if !self.pending.is_empty() {
            self.publish();
        }
    }

    fn publish(&mut self) {
        self.lines.push(OutputLine {
            participant: self.participant.clone(),
            process: self.process,
            stream: self.stream,
            received: self.received,
            text: String::from_utf8_lossy(&self.pending).into_owned(),
        });
        self.pending.clear();
    }
}

/// Written inside the container before following a file, distinguishing file bytes from exec-start errors.
pub(crate) const FILE_OUTPUT_START: &str = "panoramic-file-output\n";

/// One file source, copied to its host path and tagged as the declared process stream.
pub(crate) struct FileOutput {
    pub participant: Arc<str>,
    pub process: &'static str,
    pub stream: OutputStream,
    pub source_path: String,
    pub host_path: PathBuf,
    pub lines: OutputLines,
}

impl FileOutput {
    /// Opens the stream, consumes [`FILE_OUTPUT_START`], then follows the file until the participant exits.
    /// An open or stream failure gets a short grace for the exit observation to settle; a final read after a
    /// confirmed or undetermined exit appends missed bytes, with an undetermined exit failing capture.
    pub(crate) async fn follow<S, F>(
        self, open: impl Future<Output = Result<S, GenericError>>, read: impl FnOnce() -> F,
        mut exit: watch::Receiver<ParticipantExit>, tx: watch::Sender<Capture>,
    ) where
        S: Stream<Item = Result<OutputChunk, GenericError>>,
        F: Future<Output = Result<Option<Vec<u8>>, GenericError>>,
    {
        let mut lines = LineBuffer::new(
            self.lines.clone(),
            self.participant.clone(),
            Some(self.process),
            self.stream,
        );
        let result: Result<(), GenericError> = async {
            let mut file = File::create(&self.host_path).await.with_error_context(|| {
                format!("cannot create output file '{}'", self.host_path.display())
            })?;
            let mut followed = 0;
            let stream = tokio::select! {
                biased;
                _ = wait_for_exit(&mut exit, false) => None,
                stream = open => Some(stream),
            };
            let failure = match stream {
                None => None,
                Some(Err(error)) => Some(format!("cannot follow output file '{}': {error:#}", self.source_path)),
                Some(Ok(stream)) => {
                    let mut stream = std::pin::pin!(stream);
                    let mut prefix = FILE_OUTPUT_START.as_bytes();
                    loop {
                        let chunk = tokio::select! {
                            biased;
                            _ = wait_for_exit(&mut exit, false) => break None,
                            chunk = stream.next() => chunk,
                        };
                        match chunk {
                            Some(Ok(OutputChunk::Stdout(bytes))) => {
                                let matched = prefix.len().min(bytes.len());
                                if bytes[..matched] != prefix[..matched] {
                                    break Some(format!("the follower of '{}' did not announce successful startup", self.source_path));
                                }
                                prefix = &prefix[matched..];
                                let bytes = &bytes[matched..];
                                if !bytes.is_empty() {
                                    self.append(&mut file, &mut lines, bytes).await?;
                                    followed += bytes.len();
                                }
                            }
                            Some(Ok(OutputChunk::Stderr(bytes))) => {
                                tracing::debug!(path = %self.source_path, diagnostic = %String::from_utf8_lossy(&bytes), "Output follower diagnostic.");
                            }
                            Some(Err(error)) => break Some(format!("cannot follow output file '{}': {error:#}", self.source_path)),
                            None => break Some(format!(
                                "the follower of '{}' ended while participant '{}' was running", self.source_path, self.participant
                            )),
                        }
                    }
                }
            };
            // A stopped container can close its exec stream before Docker delivers its wait result.
            if let Some(failure) = failure {
                if timeout(FOLLOWER_EXIT_GRACE, wait_for_exit(&mut exit, true)).await.is_err() {
                    return Err(generic_error!(failure));
                }
            }
            let final_exit = exit_state(&exit);
            match read().await.with_error_context(|| format!("cannot read output file '{}'", self.source_path))? {
                Some(bytes) => {
                    if bytes.len() < followed {
                        return Err(generic_error!(
                            "output file '{}' is shorter than the {followed} bytes already followed", self.source_path
                        ));
                    }
                    self.append(&mut file, &mut lines, &bytes[followed..]).await?;
                }
                None if followed != 0 => return Err(generic_error!(
                    "output file '{}' is missing after {followed} bytes were already followed", self.source_path
                )),
                None => {}
            }
            if let ParticipantExit::Undetermined { reason } = final_exit {
                return Err(generic_error!("the exit of participant '{}' is undetermined ({reason}); output may be incomplete", self.participant));
            }
            Ok(())
        }.await;
        lines.finish();
        tx.send_replace(match result {
            Ok(()) => Capture::Complete,
            Err(error) => Capture::Failed {
                reason: format!("{error:#}"),
            },
        });
    }

    async fn append(&self, file: &mut File, lines: &mut LineBuffer, bytes: &[u8]) -> Result<(), GenericError> {
        let received = SystemTime::now();
        file.write_all(bytes)
            .await
            .with_error_context(|| format!("cannot write output file '{}'", self.host_path.display()))?;
        file.flush()
            .await
            .with_error_context(|| format!("cannot flush output file '{}'", self.host_path.display()))?;
        lines.push(bytes, received);
        Ok(())
    }
}

async fn wait_for_exit(exit: &mut watch::Receiver<ParticipantExit>, accept_unknown: bool) {
    loop {
        match exit_state(exit) {
            ParticipantExit::Exited { .. } => return,
            ParticipantExit::Undetermined { .. } if accept_unknown => return,
            _ => {}
        }
        if exit.changed().await.is_err() {
            if accept_unknown {
                return;
            }
            return std::future::pending().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::stream;
    use tokio::time::Instant;

    use super::*;

    #[test]
    fn lines_split_across_chunks_are_pushed_once_complete_with_their_tags() {
        let lines = OutputLines::default();
        let mut source = LineBuffer::new(lines.clone(), "p".into(), Some("worker"), OutputStream::Merged);
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let second = first + Duration::from_secs(1);
        source.push(b"one\ntw", first);
        let (read, cursor) = lines.since(0);
        assert_eq!(cursor, 1);
        assert_eq!(read[0].text, "one");
        assert_eq!(read[0].received, first);
        source.push(b"o\nthree\n", second);
        let (read, next) = lines.since(cursor);
        assert_eq!(next, 3);
        assert_eq!(
            read.iter().map(|line| line.text.as_str()).collect::<Vec<_>>(),
            ["two", "three"]
        );
        for line in read {
            assert_eq!(&*line.participant, "p");
            assert_eq!(line.process, Some("worker"));
            assert_eq!(line.stream, OutputStream::Merged);
            assert_eq!(line.received, second);
        }
        source.finish();
        assert!(lines.since(next).0.is_empty());
    }

    #[test]
    fn a_final_line_without_a_newline_is_kept_when_its_source_ends() {
        let lines = OutputLines::default();
        let mut source = LineBuffer::new(lines.clone(), "p".into(), None, OutputStream::Stderr);
        let received = SystemTime::UNIX_EPOCH;
        source.push(b"partial", received);
        assert!(lines.since(0).0.is_empty());
        source.finish();
        source.finish();
        let (read, cursor) = lines.since(0);
        assert_eq!(cursor, 1);
        assert_eq!(read[0].text, "partial");
        assert_eq!(read[0].received, received);
    }

    struct Fixture {
        dir: tempfile::TempDir,
        lines: OutputLines,
        exit_tx: watch::Sender<ParticipantExit>,
        capture: watch::Receiver<Capture>,
        capture_tx: watch::Sender<Capture>,
        reads: AtomicUsize,
    }

    impl Fixture {
        fn new() -> Self {
            let (exit_tx, _) = watch::channel(ParticipantExit::Running);
            let (capture_tx, capture) = watch::channel(Capture::Following);
            Self {
                dir: tempfile::tempdir().unwrap(),
                lines: OutputLines::default(),
                exit_tx,
                capture,
                capture_tx,
                reads: AtomicUsize::new(0),
            }
        }

        fn output(&self) -> FileOutput {
            FileOutput {
                participant: "p".into(),
                process: "worker",
                stream: OutputStream::Merged,
                source_path: "/log/worker".into(),
                host_path: self.dir.path().join("worker.log"),
                lines: self.lines.clone(),
            }
        }

        async fn follow<S>(
            &self, open: impl Future<Output = Result<S, GenericError>>, contents: Result<Option<Vec<u8>>, GenericError>,
        ) where
            S: Stream<Item = Result<OutputChunk, GenericError>>,
        {
            self.output()
                .follow(
                    async {
                        open.await
                            .map(|stream| stream::iter([Ok(stdout(FILE_OUTPUT_START.as_bytes()))]).chain(stream))
                    },
                    || {
                        self.reads.fetch_add(1, Ordering::Relaxed);
                        std::future::ready(contents)
                    },
                    self.exit_tx.subscribe(),
                    self.capture_tx.clone(),
                )
                .await;
        }

        fn bytes(&self) -> Vec<u8> {
            std::fs::read(self.output().host_path).unwrap()
        }

        fn texts(&self) -> Vec<String> {
            self.lines.since(0).0.into_iter().map(|line| line.text).collect()
        }

        fn assert_failed(&self, expected: &str) {
            let Capture::Failed { reason } = self.capture.borrow().clone() else {
                panic!("capture must fail")
            };
            assert!(reason.contains(expected), "{reason}");
        }

        fn until_exit(&self, chunks: Vec<OutputChunk>) -> impl Stream<Item = Result<OutputChunk, GenericError>> + '_ {
            stream::iter(chunks.into_iter().map(Ok)).chain(stream::once(async {
                self.exit_tx
                    .send_replace(ParticipantExit::Exited { code: 0, error: None });
                std::future::pending().await
            }))
        }
    }

    fn stdout(bytes: &[u8]) -> OutputChunk {
        OutputChunk::Stdout(bytes.to_vec().into())
    }

    #[test]
    fn split_and_empty_frames_keep_partial_lines_until_the_stream_ends() {
        let fixture = Fixture::new();
        let mut stdout = LineBuffer::new(fixture.lines.clone(), "p".into(), Some("main"), OutputStream::Stdout);
        let mut stderr = LineBuffer::new(fixture.lines.clone(), "p".into(), Some("main"), OutputStream::Stderr);
        stdout.push(b"out\n", SystemTime::UNIX_EPOCH);
        for frame in [b"e".as_slice(), b"", b"rr", b""] {
            stderr.push(frame, SystemTime::UNIX_EPOCH);
            assert_eq!(fixture.texts(), ["out"]);
        }
        stdout.push(b"", SystemTime::UNIX_EPOCH);
        stdout.finish();
        stderr.finish();
        stderr.finish();
        assert_eq!(fixture.texts(), ["out", "err"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_file_followers_startup_failure_is_not_process_output() {
        // A container can stop during exec startup, and the runtime writes the failure to stdout.
        for contents in [None, Some(b"last line\n".as_slice())] {
            let fixture = Fixture::new();
            let stream = stream::once(async {
                fixture
                    .exit_tx
                    .send_replace(ParticipantExit::Exited { code: 0, error: None });
                Ok(stdout(
                    b"FailedPrecondition: container init process is not running: failed precondition\r\n",
                ))
            });
            fixture
                .output()
                .follow(
                    async { Ok(stream) },
                    || async { Ok(contents.map(Vec::from)) },
                    fixture.exit_tx.subscribe(),
                    fixture.capture_tx.clone(),
                )
                .await;
            assert_eq!(fixture.bytes(), contents.unwrap_or_default());
            assert_eq!(
                fixture.texts(),
                if contents.is_some() { vec!["last line"] } else { vec![] }
            );
            assert_eq!(*fixture.capture.borrow(), Capture::Complete);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_file_followers_startup_marker_is_removed_across_split_and_empty_frames() {
        let prefix = FILE_OUTPUT_START.as_bytes();
        let contents = format!("first\n{FILE_OUTPUT_START}last");
        for split in 0..=prefix.len() {
            let fixture = Fixture::new();
            let frames = vec![
                stdout(&prefix[..split]),
                stdout(b""),
                OutputChunk::Stderr(b"follower diagnostic\n".as_slice().into()),
                stdout(&[&prefix[split..], contents.as_bytes()].concat()),
            ];
            fixture
                .output()
                .follow(
                    async { Ok(fixture.until_exit(frames)) },
                    || async { Ok(Some(contents.as_bytes().to_vec())) },
                    fixture.exit_tx.subscribe(),
                    fixture.capture_tx.clone(),
                )
                .await;
            assert_eq!(fixture.bytes(), contents.as_bytes(), "split at {split}");
            assert_eq!(fixture.texts(), ["first", "panoramic-file-output", "last"]);
            assert_eq!(*fixture.capture.borrow(), Capture::Complete);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_participants_file_follower_must_announce_successful_startup() {
        for (bytes, expected) in [
            (b"exec failed\n".as_slice(), "did not announce successful startup"),
            (b"panoramic-file", "ended while participant 'p' was running"),
            (b"", "ended while participant 'p' was running"),
        ] {
            let fixture = Fixture::new();
            fixture
                .output()
                .follow(
                    async { Ok(stream::iter([Ok(stdout(bytes))])) },
                    || async { panic!("must not read a running participant's file after startup failure") },
                    fixture.exit_tx.subscribe(),
                    fixture.capture_tx.clone(),
                )
                .await;
            assert_eq!(fixture.bytes(), b"");
            assert!(fixture.texts().is_empty());
            fixture.assert_failed(expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_followed_file_is_completed_by_one_read_after_the_participant_exits() {
        let fixture = Fixture::new();
        fixture
            .follow(
                async { Ok(fixture.until_exit(vec![stdout(b"one\n"), stdout(b"two\n")])) },
                Ok(Some(b"one\ntwo\n".to_vec())),
            )
            .await;
        assert_eq!(*fixture.capture.borrow(), Capture::Complete);
        assert_eq!(fixture.reads.load(Ordering::Relaxed), 1);
        assert_eq!(fixture.bytes(), b"one\ntwo\n");
        assert_eq!(fixture.texts(), ["one", "two"]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_final_read_appends_only_the_bytes_the_follower_missed() {
        let fixture = Fixture::new();
        fixture
            .follow(
                async { Ok(fixture.until_exit(vec![stdout(b"one\ntw")])) },
                Ok(Some(b"one\ntwo\nlast".to_vec())),
            )
            .await;
        assert_eq!(*fixture.capture.borrow(), Capture::Complete);
        assert_eq!(fixture.bytes(), b"one\ntwo\nlast");
        assert_eq!(fixture.texts(), ["one", "two", "last"]);
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_utf8_reaches_the_file_unchanged_and_the_line_lossily() {
        let fixture = Fixture::new();
        let bytes = b"\xff\ncaf\xc3\xa9\n\xfe";
        fixture
            .follow(
                async { Ok(fixture.until_exit(vec![stdout(b"\xff\ncaf\xc3"), stdout(b"\xa9\n")])) },
                Ok(Some(bytes.to_vec())),
            )
            .await;
        assert_eq!(fixture.bytes(), bytes);
        assert_eq!(fixture.texts(), ["�", "café", "�"]);
        assert_eq!(*fixture.capture.borrow(), Capture::Complete);
    }

    #[tokio::test(start_paused = true)]
    async fn follower_diagnostics_on_stderr_are_not_process_output() {
        let fixture = Fixture::new();
        fixture
            .follow(
                async {
                    Ok(fixture.until_exit(vec![
                        OutputChunk::Stderr(b"cannot open\n".as_slice().into()),
                        stdout(b"one\n"),
                    ]))
                },
                Ok(Some(b"one\n".to_vec())),
            )
            .await;
        assert_eq!(fixture.bytes(), b"one\n");
        assert_eq!(fixture.texts(), ["one"]);
        assert_eq!(*fixture.capture.borrow(), Capture::Complete);
    }

    #[tokio::test(start_paused = true)]
    async fn a_follower_that_cannot_start_on_an_exited_participant_falls_back_to_the_final_read() {
        let fixture = Fixture::new();
        fixture
            .follow(
                async {
                    fixture
                        .exit_tx
                        .send_replace(ParticipantExit::Exited { code: 0, error: None });
                    Err::<stream::Empty<Result<OutputChunk, GenericError>>, _>(generic_error!(
                        "container is not running"
                    ))
                },
                Ok(Some(b"last\n".to_vec())),
            )
            .await;
        assert_eq!(*fixture.capture.borrow(), Capture::Complete);
        assert_eq!(fixture.bytes(), b"last\n");
        assert_eq!(fixture.texts(), ["last"]);
        assert_eq!(fixture.reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_follower_that_ends_while_the_participant_runs_is_a_capture_error_after_the_grace() {
        for failure in [None, Some("stream failed"), Some("open failed")] {
            let fixture = Fixture::new();
            let started = Instant::now();
            fixture
                .follow(
                    async {
                        match failure {
                            Some("open failed") => Err(generic_error!("open failed")),
                            Some(reason) => Ok(stream::iter(vec![Err(generic_error!(reason))])),
                            None => Ok(stream::iter(vec![])),
                        }
                    },
                    Ok(None),
                )
                .await;
            fixture.assert_failed(
                failure.unwrap_or("the follower of '/log/worker' ended while participant 'p' was running"),
            );
            assert_eq!(started.elapsed(), FOLLOWER_EXIT_GRACE);
            assert_eq!(fixture.reads.load(Ordering::Relaxed), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_undetermined_exit_does_not_stop_following() {
        let fixture = Fixture::new();
        fixture.exit_tx.send_replace(ParticipantExit::Undetermined {
            reason: "wait failed".into(),
        });
        fixture
            .follow(
                async { Ok(fixture.until_exit(vec![stdout(b"still running\n")])) },
                Ok(Some(b"still running\n".to_vec())),
            )
            .await;
        assert_eq!(*fixture.capture.borrow(), Capture::Complete);
        assert_eq!(fixture.texts(), ["still running"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_ended_follower_with_an_undetermined_exit_keeps_the_final_read_but_fails_capture() {
        let fixture = Fixture::new();
        fixture.exit_tx.send_replace(ParticipantExit::Undetermined {
            reason: "wait failed".into(),
        });
        fixture
            .follow(
                async { Ok(stream::iter([Ok(stdout(b"one\n"))])) },
                Ok(Some(b"one\ntwo".to_vec())),
            )
            .await;
        fixture.assert_failed("the exit of participant 'p' is undetermined (wait failed); output may be incomplete");
        assert_eq!(fixture.bytes(), b"one\ntwo");
        assert_eq!(fixture.texts(), ["one", "two"]);
        assert_eq!(fixture.reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_file_shorter_than_what_was_followed_is_a_capture_error() {
        let fixture = Fixture::new();
        fixture
            .follow(
                async { Ok(fixture.until_exit(vec![stdout(b"one\n")])) },
                Ok(Some(b"two".to_vec())),
            )
            .await;
        fixture.assert_failed("output file '/log/worker' is shorter than the 4 bytes already followed");
        assert_eq!(fixture.bytes(), b"one\n");
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_file_after_exit_is_empty_output_only_when_nothing_was_followed() {
        for bytes in [b"".as_slice(), b"one\n"] {
            let fixture = Fixture::new();
            fixture
                .follow(async { Ok(fixture.until_exit(vec![stdout(bytes)])) }, Ok(None))
                .await;
            if bytes.is_empty() {
                assert_eq!(*fixture.capture.borrow(), Capture::Complete);
            } else {
                fixture.assert_failed("output file '/log/worker' is missing after 4 bytes were already followed");
            }
            assert_eq!(fixture.bytes(), bytes);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_final_read_is_a_capture_error() {
        let fixture = Fixture::new();
        fixture
            .follow(
                async { Ok(fixture.until_exit(vec![stdout(b"one\n")])) },
                Err(generic_error!("archive unavailable")),
            )
            .await;
        fixture.assert_failed("cannot read output file '/log/worker': archive unavailable");
        assert_eq!(fixture.bytes(), b"one\n");
    }

    #[tokio::test]
    async fn an_output_file_creation_error_fails_the_processs_capture_before_following() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.output().host_path).unwrap();
        fixture
            .follow::<stream::Empty<Result<OutputChunk, GenericError>>>(
                async { panic!("must not open a follower") },
                Ok(None),
            )
            .await;
        fixture.assert_failed("cannot create output file");
        assert_eq!(fixture.reads.load(Ordering::Relaxed), 0);
    }
}
