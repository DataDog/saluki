//! Reads a process's first supervisor-written exit record, with a final read when its exit observation settles.

use std::{
    future::Future,
    time::{Duration, SystemTime},
};

use saluki_error::GenericError;
use tokio::{sync::watch, time::sleep};

use super::{ExitObservation, ExitStatus, ParticipantExit};
use crate::probe::POLL_INTERVAL;

/// Tickets linking a refresh request to a read that began after it.
pub(crate) struct RecordRefresh {
    pub(crate) requests: watch::Sender<u64>,
    pub(crate) served: watch::Receiver<u64>,
}

/// Polls the first record line, waiting for its newline while the participant is running.
/// A line ending in `\r\n` loses its `\r`; Windows tools write CRLF.
/// Reads once more when the participant exits or its wait fails, accepting a final line without a newline.
/// Publishes one terminal state: the first exit, or an error for a missing, unreadable or malformed record.
pub(crate) async fn watch_record<F>(
    mut read: impl FnMut() -> F, mut participant_exit: watch::Receiver<ParticipantExit>,
    tx: watch::Sender<ExitObservation>, participant: &str, process: &str, path: &str,
    (mut requests, served): (watch::Receiver<u64>, watch::Sender<u64>),
) where
    F: Future<Output = Result<Option<Vec<u8>>, GenericError>>,
{
    let observation = loop {
        // Snapshot before reading so an exit during the read still triggers a final read.
        let requested = *requests.borrow_and_update();
        let exit = participant_exit.borrow_and_update().clone();
        let final_read = !matches!(exit, ParticipantExit::Running);
        let contents = match read().await {
            Ok(contents) => contents,
            Err(error) => {
                break ExitObservation::Undetermined(format!(
                    "cannot read exit record '{path}' of process '{process}': {error:#}"
                ))
            }
        };
        if let Some(contents) = contents.filter(|contents| !contents.is_empty()) {
            let end = contents.iter().position(|byte| *byte == b'\n');
            if end.is_some() || final_read {
                let line = &contents[..end.unwrap_or(contents.len())];
                let line = match end {
                    Some(_) => line.strip_suffix(b"\r").unwrap_or(line),
                    None => line,
                };
                let status = String::from_utf8(line.to_vec())
                    .map_err(|error| format!("record is not UTF-8: {error}"))
                    .and_then(|line| parse(&line));
                break match status {
                    Ok(status) => ExitObservation::Exited(status),
                    Err(why) => {
                        let line = String::from_utf8_lossy(line);
                        let mut chars = line.chars();
                        let mut quoted = chars.by_ref().take(200).collect::<String>();
                        if chars.next().is_some() {
                            quoted.push('…');
                        }
                        ExitObservation::Undetermined(format!("malformed exit record '{quoted}' in '{path}': {why}"))
                    }
                };
            }
        }
        match exit {
            ParticipantExit::Exited { .. } | ParticipantExit::Ended => {
                break ExitObservation::Undetermined(format!(
                    "participant '{participant}' stopped without an exit record for process '{process}'"
                ))
            }
            ParticipantExit::Undetermined { reason } => {
                break ExitObservation::Undetermined(format!(
                    "cannot tell whether participant '{participant}' stopped ({reason}); \
                 no exit record for process '{process}' was found in '{path}'"
                ))
            }
            ParticipantExit::Running => {}
        }
        served.send_replace(requested);
        tokio::select! {
            Ok(()) = requests.changed() => {},
            _ = sleep(POLL_INTERVAL) => {},
            Ok(()) = participant_exit.changed() => {},
        }
    };
    tx.send_replace(observation);
}

/// Parses the code, signal and container-clock timestamp from one complete record line, without its line ending.
///
/// With signal 0, any `u32` code is accepted on every OS, including codes above 255, and stored as an `i32` with the
/// same bits to preserve Windows NTSTATUS values. Code 256 with a positive signal records a signal death instead.
fn parse(line: &str) -> Result<ExitStatus, String> {
    let mut fields = line.split(' ');
    let (Some(code), Some(signal), Some(timestamp), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err("expected three space-separated fields".into());
    };
    if code.is_empty() || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("code must contain only decimal digits".into());
    }
    let code: u32 = code.parse().map_err(|_| "code must be between 0 and 4294967295")?;
    if signal.is_empty() || !signal.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("signal must contain only decimal digits".into());
    }
    let signal: i32 = signal.parse().map_err(|_| "signal is outside the i32 range")?;
    let (code, signal) = match (code, signal) {
        (_, 0) => (Some(code as i32), None),
        (256, 1..) => (None, Some(signal)),
        (0..=255, _) => return Err("a normal exit requires signal 0".into()),
        _ => return Err("a signal requires code 256".into()),
    };
    let Some((seconds, fraction)) = timestamp.split_once('.') else {
        return Err("timestamp must have seconds and 1–9 fractional digits".into());
    };
    if seconds.is_empty()
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || !(1..=9).contains(&fraction.len())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("timestamp must have seconds and 1–9 fractional digits".into());
    }
    let seconds: u64 = seconds
        .parse()
        .map_err(|_| "timestamp seconds are outside the u64 range")?;
    let nanos =
        fraction.parse::<u32>().expect("at most nine decimal digits fit in u32") * 10u32.pow(9 - fraction.len() as u32);
    let recorded_at = SystemTime::UNIX_EPOCH
        .checked_add(Duration::new(seconds, nanos))
        .ok_or("timestamp is outside the system time range")?;
    Ok(ExitStatus {
        code,
        signal,
        reason: None,
        recorded_at: Some(recorded_at),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use saluki_error::generic_error;
    use tokio::time::{advance, timeout, Instant};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        observe::ProcessObservation,
        tasks::{CaseTasks, TASK_STOP_TIME},
    };

    #[test]
    fn exit_records_parse_codes_signals_and_timestamps() {
        for (line, code, signal, seconds, nanos) in [
            ("1 0 1793575631.274329308", Some(1), None, 1793575631, 274329308),
            ("0 0 1793575631.000000001", Some(0), None, 1793575631, 1),
            ("256 9 1700000000.000000001", None, Some(9), 1700000000, 1),
            ("256 15 1700000000.000000002", None, Some(15), 1700000000, 2),
            ("255 0 1.125", Some(255), None, 1, 125000000),
            ("0 0 0.5", Some(0), None, 0, 500000000),
            ("0 0 0.0", Some(0), None, 0, 0),
            ("256 0 1.5", Some(256), None, 1, 500000000),
            ("257 0 1.5", Some(257), None, 1, 500000000),
            ("2147483647 0 1.5", Some(i32::MAX), None, 1, 500000000),
            ("2147483648 0 1.5", Some(i32::MIN), None, 1, 500000000),
            ("3221225477 0 1.5", Some(0xC0000005_u32 as i32), None, 1, 500000000),
            ("4294967295 0 1.5", Some(-1), None, 1, 500000000),
        ] {
            assert_eq!(
                parse(line),
                Ok(ExitStatus {
                    code,
                    signal,
                    reason: None,
                    recorded_at: Some(SystemTime::UNIX_EPOCH + Duration::new(seconds, nanos)),
                }),
                "{line}"
            );
        }
    }

    #[test]
    fn malformed_exit_records_are_rejected() {
        for line in [
            "3 9 1.5",
            "257 9 1.5",
            "4294967295 9 1.5",
            "4294967296 0 1.5",
            "4294967296 9 1.5",
            "256 -1 1.5",
            "-1 0 1.5",
            "x 0 1.5",
            "1 0",
            "1 0 1",
            "1 0 1.1234567890",
            "1 0 1.5 extra",
            "1  0 1.5",
            "1\t0 1.5",
            " 1 0 1.5",
            "1 0 1.5 ",
            "+1 0 1.5",
            "1 -1 1.5",
            "1 +0 1.5",
            "1 0 -1.5",
            "1 0 +1.5",
            "1 0 .5",
            "1 0 1.",
            "1 0 1.5.0",
            "1 0 1.x",
            "1 0 1.5\r",
            "1 0 1.5\n",
            "",
            "1 0 1.５",
            "١ 0 1.5",
            "999999999999999999999 0 1.5",
            "256 2147483648 1.5",
            "1 0 18446744073709551616.5",
            "1 0 18446744073709551615.5",
        ] {
            assert!(parse(line).is_err(), "accepted {line:?}");
        }
    }

    struct FileState {
        contents: Option<Vec<u8>>,
        error: bool,
        reads: usize,
    }

    struct Fixture {
        file: Arc<Mutex<FileState>>,
        exit_tx: watch::Sender<ParticipantExit>,
        observation: ProcessObservation,
        tasks: CaseTasks,
        watcher: tokio::task::JoinHandle<Option<()>>,
    }

    impl Fixture {
        fn new(contents: Option<&[u8]>, error: bool) -> Self {
            let file = Arc::new(Mutex::new(FileState {
                contents: contents.map(<[u8]>::to_vec),
                error,
                reads: 0,
            }));
            let (exit_tx, exit_rx) = watch::channel(ParticipantExit::Running);
            let (tx, rx) = watch::channel(ExitObservation::Running);
            let (requests, requests_rx) = watch::channel(0);
            let (served_tx, served) = watch::channel(0);
            let tasks = CaseTasks::new(&CancellationToken::new());
            let watcher = tasks.spawn({
                let file = file.clone();
                async move {
                    watch_record(
                        || {
                            let mut file = file.lock().unwrap();
                            file.reads += 1;
                            let result = if file.error {
                                Err(generic_error!("archive unavailable"))
                            } else {
                                Ok(file.contents.clone())
                            };
                            std::future::ready(result)
                        },
                        exit_rx,
                        tx,
                        "p",
                        "svc",
                        "/exit/svc",
                        (requests_rx, served_tx),
                    )
                    .await;
                }
            });
            Self {
                file,
                exit_tx,
                observation: ProcessObservation::Record {
                    status: rx,
                    refresh: Some(Arc::new(RecordRefresh { requests, served })),
                },
                tasks,
                watcher,
            }
        }

        async fn read_once(&self) {
            tokio::task::yield_now().await;
            assert_eq!(self.file.lock().unwrap().reads, 1);
            assert_eq!(self.observation.current(), ExitObservation::Running);
        }

        async fn settled(&self) -> ExitObservation {
            timeout(Duration::from_secs(2), self.observation.settled())
                .await
                .expect("the exit must settle")
        }

        async fn finish(self) {
            assert_eq!(self.tasks.stop_all(Instant::now() + TASK_STOP_TIME).await, 0);
            self.watcher.await.unwrap();
        }
    }

    fn exited(code: i32) -> ExitObservation {
        ExitObservation::Exited(ExitStatus {
            code: Some(code),
            signal: None,
            reason: None,
            recorded_at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1500)),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_record_line_ending_in_crlf_is_read() {
        for (contents, expected) in [
            (b"3 0 1.5\r\n".as_slice(), exited(3)),
            (
                b"3221225477 0 1.5\r\n3 0 1.5\r\n".as_slice(),
                exited(0xC0000005_u32 as i32),
            ),
        ] {
            let fixture = Fixture::new(Some(contents), false);
            assert_eq!(fixture.settled().await, expected);
            assert_eq!(fixture.file.lock().unwrap().reads, 1);
            fixture.finish().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn only_one_carriage_return_before_the_newline_is_a_line_ending() {
        for contents in [b"3 0 1.5\r\r\n".as_slice(), b"3 0 1.5\r".as_slice()] {
            let fixture = Fixture::new(Some(contents), false);
            fixture
                .exit_tx
                .send_replace(ParticipantExit::Exited { code: 0, error: None });
            let ExitObservation::Undetermined(reason) = fixture.settled().await else {
                panic!("a stray carriage return must be malformed");
            };
            assert!(reason.starts_with("malformed exit record '3 0 1.5"), "{reason}");
            fixture.finish().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_reads_an_exit_written_since_the_last_poll() {
        let fixture = Fixture::new(None, false);
        fixture.read_once().await;
        fixture.file.lock().unwrap().contents = Some(b"3 0 1.5\n".to_vec());
        let started = Instant::now();
        assert_eq!(fixture.observation.refresh().await, exited(3));
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(fixture.file.lock().unwrap().reads, 2);
        fixture.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_of_a_running_record_waits_for_a_new_read() {
        let fixture = Fixture::new(None, false);
        fixture.read_once().await;
        let started = Instant::now();
        let (first, second) = tokio::join!(fixture.observation.refresh(), fixture.observation.refresh());
        assert_eq!(first, ExitObservation::Running);
        assert_eq!(second, ExitObservation::Running);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(fixture.file.lock().unwrap().reads, 2);
        fixture.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_already_in_progress_cannot_serve_a_new_refresh_ticket() {
        let file = Arc::new(Mutex::new(FileState {
            contents: None,
            error: false,
            reads: 0,
        }));
        let (_exit, exit_rx) = watch::channel(ParticipantExit::Running);
        let (status_tx, status) = watch::channel(ExitObservation::Running);
        let (requests, requests_rx) = watch::channel(0);
        let (served_tx, served) = watch::channel(0);
        let observation = ProcessObservation::Record {
            status,
            refresh: Some(Arc::new(RecordRefresh { requests, served })),
        };
        let tasks = CaseTasks::new(&CancellationToken::new());
        let watcher = tasks.spawn({
            let file = file.clone();
            async move {
                watch_record(
                    || {
                        let mut file = file.lock().unwrap();
                        file.reads += 1;
                        let contents = file.contents.clone();
                        async move {
                            sleep(Duration::from_millis(100)).await;
                            Ok(contents)
                        }
                    },
                    exit_rx,
                    status_tx,
                    "p",
                    "svc",
                    "/exit/svc",
                    (requests_rx, served_tx),
                )
                .await;
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(file.lock().unwrap().reads, 1);
        file.lock().unwrap().contents = Some(b"3 0 1.5\n".to_vec());
        let started = Instant::now();
        assert_eq!(observation.refresh().await, exited(3));
        assert_eq!(started.elapsed(), Duration::from_millis(200));
        assert_eq!(file.lock().unwrap().reads, 2);
        assert_eq!(tasks.stop_all(Instant::now() + TASK_STOP_TIME).await, 0);
        watcher.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_of_a_settled_record_returns_without_reading_again() {
        let fixture = Fixture::new(Some(b"3 0 1.5\n"), false);
        assert_eq!(fixture.settled().await, exited(3));
        let started = Instant::now();
        assert_eq!(fixture.observation.refresh().await, exited(3));
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(fixture.file.lock().unwrap().reads, 1);
        fixture.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_after_the_record_watcher_stops_is_undetermined() {
        let fixture = Fixture::new(None, false);
        fixture.read_once().await;
        assert_eq!(fixture.tasks.stop_all(Instant::now() + TASK_STOP_TIME).await, 0);
        assert_eq!(
            fixture.observation.refresh().await,
            ExitObservation::Undetermined("exit observation ended without a status".into())
        );
        assert_eq!(fixture.file.lock().unwrap().reads, 1);
        fixture.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_recorded_exit_is_final() {
        let fixture = Fixture::new(Some(b"3 0 1.5\n"), false);
        assert_eq!(fixture.settled().await, exited(3));
        fixture.file.lock().unwrap().contents = Some(b"3 0 1.5\n4 0 2.5\n".to_vec());
        fixture
            .exit_tx
            .send_replace(ParticipantExit::Exited { code: 0, error: None });
        advance(2 * POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        assert_eq!(fixture.observation.current(), exited(3));
        assert_eq!(fixture.file.lock().unwrap().reads, 1);
        fixture.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_partial_record_line_waits_for_its_newline_while_the_participant_runs() {
        let fixture = Fixture::new(Some(b"3 0 1.5"), false);
        fixture.read_once().await;
        fixture.file.lock().unwrap().contents = Some(b"3 0 1.5\n".to_vec());
        let started = Instant::now();
        assert_eq!(fixture.settled().await, exited(3));
        assert_eq!(started.elapsed(), POLL_INTERVAL);
        assert_eq!(fixture.file.lock().unwrap().reads, 2);
        fixture.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_record_file_is_read_once_more_after_the_participant_stops() {
        let fixture = Fixture::new(None, false);
        fixture.read_once().await;
        fixture.file.lock().unwrap().contents = Some(b"3 0 1.5".to_vec());
        let stopped = Instant::now();
        fixture
            .exit_tx
            .send_replace(ParticipantExit::Exited { code: 0, error: None });
        assert_eq!(fixture.settled().await, exited(3));
        assert_eq!(stopped.elapsed(), Duration::ZERO);
        assert_eq!(fixture.file.lock().unwrap().reads, 2);
        fixture.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_exit_during_a_record_read_still_gets_a_final_read() {
        let (exit_tx, exit_rx) = watch::channel(ParticipantExit::Running);
        let (tx, rx) = watch::channel(ExitObservation::Running);
        let mut reads = 0;
        watch_record(
            || {
                reads += 1;
                let contents = if reads == 1 {
                    exit_tx.send_replace(ParticipantExit::Exited { code: 0, error: None });
                    None
                } else {
                    Some(b"3 0 1.5".to_vec())
                };
                std::future::ready(Ok(contents))
            },
            exit_rx,
            tx,
            "p",
            "svc",
            "/exit/svc",
            (watch::channel(0).1, watch::channel(0).0),
        )
        .await;
        assert_eq!(reads, 2);
        assert_eq!(
            ProcessObservation::Record {
                status: rx,
                refresh: None
            }
            .current(),
            exited(3)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_participant_that_stops_without_a_record_leaves_its_process_undetermined() {
        for contents in [None, Some(b"".as_slice())] {
            let fixture = Fixture::new(contents, false);
            fixture.read_once().await;
            fixture
                .exit_tx
                .send_replace(ParticipantExit::Exited { code: 0, error: None });
            assert_eq!(
                fixture.settled().await,
                ExitObservation::Undetermined(
                    "participant 'p' stopped without an exit record for process 'svc'".into()
                )
            );
            assert_eq!(fixture.file.lock().unwrap().reads, 2);
            fixture.finish().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_record_read_leaves_its_process_undetermined() {
        let fixture = Fixture::new(None, true);
        assert_eq!(
            fixture.settled().await,
            ExitObservation::Undetermined(
                "cannot read exit record '/exit/svc' of process 'svc': archive unavailable".into()
            )
        );
        assert_eq!(fixture.file.lock().unwrap().reads, 1);
        fixture.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_undetermined_participant_exit_ends_record_polling_after_a_final_read() {
        for contents in [None, Some(b"3 0 1.5".as_slice())] {
            let fixture = Fixture::new(None, false);
            fixture.read_once().await;
            fixture.file.lock().unwrap().contents = contents.map(<[u8]>::to_vec);
            let stopped = Instant::now();
            fixture.exit_tx.send_replace(ParticipantExit::Undetermined {
                reason: "wait failed".into(),
            });
            let expected = if contents.is_some() {
                exited(3)
            } else {
                ExitObservation::Undetermined(
                    "cannot tell whether participant 'p' stopped (wait failed); \
                     no exit record for process 'svc' was found in '/exit/svc'"
                        .into(),
                )
            };
            assert_eq!(fixture.settled().await, expected);
            assert_eq!(stopped.elapsed(), Duration::ZERO);
            advance(2 * POLL_INTERVAL).await;
            tokio::task::yield_now().await;
            assert_eq!(fixture.file.lock().unwrap().reads, 2);
            fixture.finish().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_malformed_first_record_is_final_even_when_a_later_record_is_valid() {
        let fixture = Fixture::new(Some(b"257 9 1.5\n3 0 1.5\n"), false);
        assert_eq!(
            fixture.settled().await,
            ExitObservation::Undetermined(
                "malformed exit record '257 9 1.5' in '/exit/svc': a signal requires code 256".into()
            )
        );
        assert_eq!(fixture.file.lock().unwrap().reads, 1);
        fixture.finish().await;
    }

    #[tokio::test(start_paused = true)]
    async fn empty_and_non_utf8_first_lines_are_malformed_records() {
        for contents in [b"\n3 0 1.5\n".as_slice(), b"\xff\n".as_slice()] {
            let fixture = Fixture::new(Some(contents), false);
            let ExitObservation::Undetermined(reason) = fixture.settled().await else {
                panic!("a malformed first line must be undetermined");
            };
            assert!(reason.starts_with("malformed exit record '"), "{reason}");
            assert!(reason.contains("in '/exit/svc':"), "{reason}");
            assert_eq!(fixture.file.lock().unwrap().reads, 1);
            fixture.finish().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_record_diagnostics_bound_the_quoted_line_by_characters() {
        for length in [200, 201, 10000] {
            let line = "é".repeat(length);
            let fixture = Fixture::new(Some(line.as_bytes()), false);
            fixture
                .exit_tx
                .send_replace(ParticipantExit::Exited { code: 0, error: None });
            let quoted = format!("{}{}", "é".repeat(200), if length > 200 { "…" } else { "" });
            assert_eq!(
                fixture.settled().await,
                ExitObservation::Undetermined(format!(
                    "malformed exit record '{quoted}' in '/exit/svc': expected three space-separated fields"
                ))
            );
            fixture.finish().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_case_tasks_prevents_record_reads_during_reclaim() {
        let fixture = Fixture::new(None, false);
        fixture.read_once().await;
        assert_eq!(fixture.tasks.stop_all(Instant::now() + TASK_STOP_TIME).await, 0);
        fixture.file.lock().unwrap().contents = Some(b"3 0 1.5\n".to_vec());
        fixture
            .exit_tx
            .send_replace(ParticipantExit::Exited { code: 0, error: None });
        advance(2 * POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        assert_eq!(fixture.file.lock().unwrap().reads, 1);
        fixture.finish().await;
    }
}
