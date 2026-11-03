//! Waits for the readiness evidence required by a start level.

use airlock::files::FileKind;
use futures::future::join_all;
use tokio::time::{sleep, Instant};
use tokio_util::sync::CancellationToken;

use crate::{
    handle::{http_status_from_runner_host, probe_from_runner_host, HandleError, Primitives},
    observe::{ParticipantExit, ParticipantObservation},
    plan::{PlannedCheck, StartGate},
    probe::{probe, AttemptLimits, ProbeAnswer, ProbeError, POLL_INTERVAL},
};

struct Waiting<'a> {
    gate: &'a StartGate,
    consumers: Vec<&'a str>,
}

pub(super) async fn wait_for_level<'a>(
    gates: impl IntoIterator<Item = (&'a str, &'a StartGate)>,
    server: impl Fn(&str) -> (&'a dyn Primitives, &'a ParticipantObservation), cancel: &CancellationToken,
) -> Result<(), String> {
    let mut waiting: Vec<Waiting<'_>> = Vec::new();
    for (consumer, gate) in gates {
        if let Some(shared) = waiting
            .iter_mut()
            .find(|waiting| waiting.gate.server == gate.server && waiting.gate.check == gate.check)
        {
            if !shared.consumers.contains(&consumer) {
                shared.consumers.push(consumer);
            }
            // Sharing a check must still honor the shortest declared bound.
            if gate.evidence.within < shared.gate.evidence.within {
                shared.gate = gate;
            }
        } else {
            waiting.push(Waiting {
                gate,
                consumers: vec![consumer],
            });
        }
    }
    let failures = join_all(waiting.iter().map(|waiting| {
        let (server, observation) = server(&waiting.gate.server);
        waiting.wait(server, observation, cancel)
    }))
    .await
    .into_iter()
    .filter_map(Result::err)
    .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

impl Waiting<'_> {
    async fn wait(
        &self, server: &dyn Primitives, observation: &ParticipantObservation, cancel: &CancellationToken,
    ) -> Result<(), String> {
        let consumers = if self.consumers.len() == 1 {
            format!("participant '{}'", self.consumers[0])
        } else {
            format!("participants '{}'", self.consumers.join("', '"))
        };
        let description = self.gate.evidence.describe();
        let what = format!("{description} of participant '{}'", self.gate.server);
        let within = self.gate.evidence.within;
        let deadline = Instant::now() + within;
        let mut reason = "no check completed".to_string();
        let result = {
            let poll = async {
                loop {
                    let limits = AttemptLimits::new(deadline.saturating_duration_since(Instant::now()));
                    match check_once(&self.gate.check, server, &limits).await {
                        Ok(None) => return Ok(()),
                        Ok(Some(not_ready)) | Err(HandleError::Product { message: not_ready }) => reason = not_ready,
                        Err(HandleError::Harness { message }) => return Err(message),
                    }
                    sleep(POLL_INTERVAL).await;
                }
            };
            tokio::select! {
                biased;
                exit = observation.stopped() => Err(exit),
                checked = probe(&what, within, cancel, poll) => Ok(checked),
            }
        };
        match result {
            Err(ParticipantExit::Exited { code, error }) => {
                let docker = error.map(|error| format!(" (Docker: {error})")).unwrap_or_default();
                Err(format!(
                    "{consumers} cannot start: participant '{}' exited with code {code}{docker} \
                     before {description} was ready; last check: {reason}",
                    self.gate.server,
                ))
            }
            Err(_) => unreachable!("stopped resolves only on an observed exit"),
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(message))) => Err(format!("{consumers} cannot start: checking {what} failed: {message}")),
            Ok(Err(ProbeError::TimedOut { .. })) => Err(format!(
                "{consumers} cannot start: {what} was not ready within {within:?}: {reason}"
            )),
            Ok(Err(ProbeError::Cancelled { .. })) => Err(format!("cancelled while {consumers} waited for {what}")),
        }
    }
}

/// Returns `None` when ready, otherwise the reason to retry; mechanism failures are errors.
async fn check_once(
    check: &PlannedCheck, server: &dyn Primitives, limits: &AttemptLimits,
) -> Result<Option<String>, HandleError> {
    match check {
        PlannedCheck::Accepts(route) => match probe_from_runner_host(route, server, limits).await? {
            ProbeAnswer::Listening => Ok(None),
            ProbeAnswer::NotListening { reason } => Ok(Some(reason)),
            ProbeAnswer::Addressable => Ok(Some(
                "the address is reachable but listener availability is unknown".into(),
            )),
        },
        PlannedCheck::HttpOk { route, path } => {
            Ok(match http_status_from_runner_host(route, server, path, limits).await? {
                200 => None,
                status => Some(format!("answered HTTP {status}")),
            })
        }
        PlannedCheck::Socket { path } | PlannedCheck::File { path } => {
            let Some(stat) = server.stat(path).await? else {
                return Ok(Some(format!("'{path}' is missing")));
            };
            if matches!(
                (check, &stat.kind),
                (PlannedCheck::Socket { .. }, FileKind::Socket) | (PlannedCheck::File { .. }, FileKind::File)
            ) {
                return Ok(None);
            }
            let kind = match stat.kind {
                FileKind::File => "regular file",
                FileKind::Directory => "directory",
                FileKind::Symlink { .. } => "symbolic link",
                FileKind::Socket => "socket",
                FileKind::NamedPipe => "named pipe",
                FileKind::Other => "device or unrecognized file kind",
            };
            Ok(Some(format!("'{path}' is a {kind}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, future::pending, sync::Mutex, time::Duration};

    use airlock::{exec::ExecOutput, files::FileStat};
    use async_trait::async_trait;
    use tokio::sync::watch;

    use super::*;
    use crate::observe::Capture;
    use crate::{
        endpoint::{EndpointKind, Evidence, ResolvedAddress, RunnerHostRoute},
        handle::ExecCommand,
    };

    enum Answer {
        Probe(ProbeAnswer),
        Stat(Option<FileKind>),
        Error(HandleError),
    }

    struct Server {
        answers: Mutex<VecDeque<Answer>>,
        calls: Mutex<Vec<String>>,
        observation: ParticipantObservation,
        exit_tx: watch::Sender<ParticipantExit>,
    }

    impl Server {
        fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
            let (exit_tx, exit_rx) = watch::channel(ParticipantExit::Running);
            let (_, capture_rx) = watch::channel(Capture::Following);
            Self {
                exit_tx,
                observation: ParticipantObservation {
                    exit_rx,
                    capture_rx,
                    stdout_path: Default::default(),
                    stderr_path: Default::default(),
                },
                answers: Mutex::new(answers.into_iter().collect()),
                calls: Mutex::default(),
            }
        }

        async fn answer(&self, call: String) -> Result<Answer, HandleError> {
            self.calls.lock().unwrap().push(call);
            let answer = self.answers.lock().unwrap().pop_front();
            match answer {
                Some(Answer::Error(error)) => Err(error),
                Some(answer) => Ok(answer),
                None => pending().await,
            }
        }
    }

    #[async_trait]
    impl Primitives for Server {
        async fn exec(&self, _: ExecCommand, _: Duration) -> Result<ExecOutput, HandleError> {
            panic!("readiness must use probe or stat");
        }
        async fn read_file(&self, _: &str) -> Result<Option<Vec<u8>>, HandleError> {
            panic!("readiness must stat files");
        }
        async fn stat(&self, path: &str) -> Result<Option<FileStat>, HandleError> {
            let Answer::Stat(kind) = self.answer(format!("stat {path}")).await? else {
                panic!("expected a stat answer");
            };
            Ok(kind.map(|kind| FileStat {
                kind,
                mode: 0o600,
                size: 0,
            }))
        }
        async fn probe(&self, address: &ResolvedAddress, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
            assert!(limits.attempt <= crate::probe::PROBE_TIMEOUT);
            let Answer::Probe(answer) = self.answer(format!("probe {}", address.url)).await? else {
                panic!("expected a probe answer");
            };
            Ok(answer)
        }
        async fn send(&self, _: &ResolvedAddress, _: &[u8], _: &AttemptLimits) -> Result<(), HandleError> {
            panic!("readiness must not send payloads");
        }
    }

    fn tcp_gate() -> StartGate {
        StartGate {
            server: "server".into(),
            endpoint: Some("api".into()),
            evidence: Evidence::tcp_accepts("api"),
            check: PlannedCheck::Accepts(RunnerHostRoute::Courier(ResolvedAddress {
                kind: EndpointKind::Tcp,
                url: "tcp://server:8080".into(),
                host: Some("server".into()),
                port: Some(8080),
                path: None,
            })),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_gate_ends_when_its_server_exits_and_names_the_code_and_last_check() {
        for error in [None, Some("runtime failure".to_string())] {
            let server = Server::new([Answer::Probe(ProbeAnswer::NotListening {
                reason: "connection refused".into(),
            })]);
            let gate = tcp_gate();
            let cancel = CancellationToken::new();
            let started = Instant::now();
            let (_, result) = tokio::join!(
                async {
                    sleep(Duration::from_secs(1)).await;
                    server.exit_tx.send_replace(ParticipantExit::Exited {
                        code: 3,
                        error: error.clone(),
                    });
                },
                wait_for_level(
                    [("consumer", &gate)],
                    |_| (&server as &dyn Primitives, &server.observation),
                    &cancel
                ),
            );
            let docker = error.map(|error| format!(" (Docker: {error})")).unwrap_or_default();
            assert_eq!(
                result,
                Err(format!(
                    "participant 'consumer' cannot start: participant 'server' exited with code 3{docker} \
                 before tcp-accepts on 'api' was ready; last check: connection refused"
                ))
            );
            assert_eq!(started.elapsed(), Duration::from_secs(1));
            assert!(started.elapsed() < gate.evidence.within);
            assert_eq!(server.calls.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_exited_before_the_wait_fails_its_gate_without_a_check() {
        let server = Server::new([Answer::Probe(ProbeAnswer::Listening)]);
        server
            .exit_tx
            .send_replace(ParticipantExit::Exited { code: 0, error: None });
        let started = Instant::now();
        assert_eq!(
            wait_for_level(
                [("consumer", &tcp_gate())],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Err(
                "participant 'consumer' cannot start: participant 'server' exited with code 0 \
                 before tcp-accepts on 'api' was ready; last check: no check completed"
                    .into()
            ),
        );
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(server.calls.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_undetermined_server_exit_leaves_the_gate_to_its_check() {
        let server = Server::new([
            Answer::Probe(ProbeAnswer::NotListening {
                reason: "warming up".into(),
            }),
            Answer::Probe(ProbeAnswer::Listening),
        ]);
        server.exit_tx.send_replace(ParticipantExit::Undetermined {
            reason: "wait connection lost".into(),
        });
        let started = Instant::now();
        assert_eq!(
            wait_for_level(
                [("consumer", &tcp_gate())],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Ok(()),
        );
        assert_eq!(started.elapsed(), POLL_INTERVAL);
        assert_eq!(server.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_gate_retries_until_its_endpoint_accepts() {
        let server = Server::new([
            Answer::Probe(ProbeAnswer::NotListening {
                reason: "connection refused".into(),
            }),
            Answer::Error(HandleError::Product {
                message: "not yet".into(),
            }),
            Answer::Probe(ProbeAnswer::Addressable),
            Answer::Probe(ProbeAnswer::Listening),
        ]);
        let started = Instant::now();
        assert_eq!(
            wait_for_level(
                [("consumer", &tcp_gate())],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Ok(())
        );
        assert_eq!(started.elapsed(), 3 * POLL_INTERVAL);
        assert_eq!(*server.calls.lock().unwrap(), ["probe tcp://server:8080"; 4]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_gate_that_never_passes_fails_after_its_bound_with_the_last_reason() {
        let server = Server::new([
            Answer::Probe(ProbeAnswer::NotListening {
                reason: "warming up".into(),
            }),
            Answer::Error(HandleError::Product {
                message: "connection refused".into(),
            }),
        ]);
        let gate = StartGate {
            evidence: Evidence::tcp_accepts("api").within(Duration::from_secs(2)),
            ..tcp_gate()
        };
        let started = Instant::now();
        let expected =
            "participant 'consumer' cannot start: tcp-accepts on 'api' of participant 'server' was not ready \
            within 2s: connection refused";
        assert_eq!(
            wait_for_level(
                [("consumer", &gate)],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Err(expected.into()),
        );
        assert_eq!(started.elapsed(), Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_gate_with_no_completed_attempt_reports_that_at_its_bound() {
        let server = Server::new([]);
        let started = Instant::now();
        let expected =
            "participant 'consumer' cannot start: tcp-accepts on 'api' of participant 'server' was not ready \
            within 60s: no check completed";
        assert_eq!(
            wait_for_level(
                [("consumer", &tcp_gate())],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Err(expected.into()),
        );
        assert_eq!(started.elapsed(), Duration::from_secs(60));
        assert_eq!(server.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_harness_error_ends_the_wait_at_once() {
        let server = Server::new([Answer::Error(HandleError::Harness {
            message: "exec failed: server has exited".into(),
        })]);
        let started = Instant::now();
        let expected = "participant 'consumer' cannot start: checking tcp-accepts on 'api' of participant 'server' \
            failed: exec failed: server has exited";
        assert_eq!(
            wait_for_level(
                [("consumer", &tcp_gate())],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Err(expected.into()),
        );
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(server.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_ends_the_wait_and_says_what_was_awaited() {
        let server = Server::new([]);
        let cancel = CancellationToken::new();
        let gate = tcp_gate();
        let started = Instant::now();
        let (_, result) = tokio::join!(
            async {
                sleep(Duration::from_secs(1)).await;
                cancel.cancel();
            },
            wait_for_level(
                [("consumer", &gate)],
                |_| (&server as &dyn Primitives, &server.observation),
                &cancel
            ),
        );
        assert_eq!(
            result,
            Err(
                "cancelled while participant 'consumer' waited for tcp-accepts on 'api' of participant 'server'".into()
            )
        );
        assert_eq!(started.elapsed(), Duration::from_secs(1));
        assert_eq!(server.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn socket_and_file_gates_require_a_socket_and_a_regular_file() {
        for (kind, description) in [
            (None, "'/ready' is missing"),
            (Some(FileKind::File), "'/ready' is a regular file"),
            (Some(FileKind::Socket), "'/ready' is a socket"),
            (Some(FileKind::Directory), "'/ready' is a directory"),
            (
                Some(FileKind::Symlink {
                    target: "/target".into(),
                }),
                "'/ready' is a symbolic link",
            ),
            (Some(FileKind::NamedPipe), "'/ready' is a named pipe"),
            (Some(FileKind::Other), "'/ready' is a device or unrecognized file kind"),
        ] {
            for check in [
                PlannedCheck::Socket { path: "/ready".into() },
                PlannedCheck::File { path: "/ready".into() },
            ] {
                let ready = matches!(
                    (&check, &kind),
                    (PlannedCheck::Socket { .. }, Some(FileKind::Socket))
                        | (PlannedCheck::File { .. }, Some(FileKind::File))
                );
                let server = Server::new([Answer::Stat(kind.clone())]);
                assert_eq!(
                    check_once(&check, &server, &AttemptLimits::new(Duration::from_secs(1))).await,
                    Ok((!ready).then(|| description.to_string()))
                );
                assert_eq!(*server.calls.lock().unwrap(), ["stat /ready"]);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn file_evidence_retries_missing_paths_and_reports_stat_failures() {
        let gate = StartGate {
            server: "server".into(),
            endpoint: None,
            evidence: Evidence::file_exists("/ready"),
            check: PlannedCheck::File { path: "/ready".into() },
        };
        let server = Server::new([Answer::Stat(None), Answer::Stat(Some(FileKind::File))]);
        assert_eq!(
            wait_for_level(
                [("consumer", &gate)],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Ok(())
        );
        assert_eq!(*server.calls.lock().unwrap(), ["stat /ready"; 2]);
        let server = Server::new([Answer::Error(HandleError::Harness {
            message: "archive unavailable".into(),
        })]);
        let expected = "participant 'consumer' cannot start: checking file-exists at '/ready' of participant 'server' \
            failed: archive unavailable";
        assert_eq!(
            wait_for_level(
                [("consumer", &gate)],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Err(expected.into()),
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_gate_shared_by_a_level_is_checked_once_and_names_every_waiting_consumer() {
        let gate = tcp_gate();
        let alias = StartGate {
            endpoint: Some("other".into()),
            ..gate.clone()
        };
        let failure = "participants 'a', 'b' cannot start: checking tcp-accepts on 'api' of participant 'server' \
            failed: server exited";
        for (answer, expected) in [
            (Answer::Probe(ProbeAnswer::Listening), Ok(())),
            (
                Answer::Error(HandleError::Harness {
                    message: "server exited".into(),
                }),
                Err(failure.into()),
            ),
        ] {
            let server = Server::new([answer]);
            assert_eq!(
                wait_for_level(
                    [("a", &gate), ("a", &alias), ("b", &gate)],
                    |_| (&server as &dyn Primitives, &server.observation),
                    &CancellationToken::new()
                )
                .await,
                expected
            );
            assert_eq!(server.calls.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shared_checks_honor_the_shortest_declared_bound() {
        let gate = tcp_gate();
        let shorter = StartGate {
            evidence: Evidence::tcp_accepts("api").within(Duration::from_secs(1)),
            ..gate.clone()
        };
        let server = Server::new([]);
        let started = Instant::now();
        let expected =
            "participants 'a', 'b' cannot start: tcp-accepts on 'api' of participant 'server' was not ready \
            within 1s: no check completed";
        assert_eq!(
            wait_for_level(
                [("a", &gate), ("b", &shorter)],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Err(expected.into()),
        );
        assert_eq!(started.elapsed(), Duration::from_secs(1));
        assert_eq!(server.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn independent_gates_wait_concurrently_and_report_every_failure() {
        let gate = StartGate {
            evidence: Evidence::tcp_accepts("api").within(Duration::from_secs(2)),
            ..tcp_gate()
        };
        let other = StartGate {
            server: "other".into(),
            ..gate.clone()
        };
        let server = Server::new([]);
        let started = Instant::now();
        let expected =
            "participant 'a' cannot start: tcp-accepts on 'api' of participant 'server' was not ready within \
            2s: no check completed; participant 'b' cannot start: tcp-accepts on 'api' of participant \
            'other' was not ready within 2s: no check completed";
        assert_eq!(
            wait_for_level(
                [("a", &gate), ("b", &other)],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Err(expected.into()),
        );
        assert_eq!(started.elapsed(), Duration::from_secs(2));
        assert_eq!(server.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn http_checks_through_the_courier_fail_as_a_harness_error() {
        let tcp = tcp_gate();
        let PlannedCheck::Accepts(route) = tcp.check else {
            unreachable!()
        };
        let gate = StartGate {
            check: PlannedCheck::HttpOk {
                route,
                path: "/ready".into(),
            },
            evidence: Evidence::http_ok("api", "/ready"),
            ..tcp
        };
        let server = Server::new([]);
        let expected = "participant 'consumer' cannot start: checking http-ok on 'api' at '/ready' of participant \
            'server' failed: HTTP checks through the courier are not supported";
        assert_eq!(
            wait_for_level(
                [("consumer", &gate)],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Err(expected.into()),
        );
        assert!(server.calls.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn an_invalid_http_request_ends_readiness_at_once() {
        let tcp = tcp_gate();
        let PlannedCheck::Accepts(RunnerHostRoute::Courier(address)) = tcp.check else {
            unreachable!()
        };
        let gate = StartGate {
            check: PlannedCheck::HttpOk {
                route: RunnerHostRoute::Direct(address),
                path: "/a b".into(),
            },
            evidence: Evidence::http_ok("api", "/a b"),
            ..tcp
        };
        let server = Server::new([]);
        let started = Instant::now();
        let expected = "participant 'consumer' cannot start: checking http-ok on 'api' at '/a b' of participant \
            'server' failed: invalid uri character";
        assert_eq!(
            wait_for_level(
                [("consumer", &gate)],
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Err(expected.into())
        );
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(server.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn http_gates_on_different_paths_name_each_path_in_their_failures() {
        let tcp = tcp_gate();
        let PlannedCheck::Accepts(route) = &tcp.check else {
            unreachable!()
        };
        let gates = ["/ready", "/live"].map(|path| StartGate {
            check: PlannedCheck::HttpOk {
                route: route.clone(),
                path: path.into(),
            },
            evidence: Evidence::http_ok("api", path),
            ..tcp.clone()
        });
        let server = Server::new([]);
        let expected = "participant 'consumer' cannot start: checking http-ok on 'api' at '/ready' of participant \
            'server' failed: HTTP checks through the courier are not supported; participant 'consumer' \
            cannot start: checking http-ok on 'api' at '/live' of participant 'server' failed: HTTP checks \
            through the courier are not supported";
        assert_eq!(
            wait_for_level(
                gates.iter().map(|gate| ("consumer", gate)),
                |_| (&server as &dyn Primitives, &server.observation),
                &CancellationToken::new()
            )
            .await,
            Err(expected.into())
        );
        assert!(server.calls.lock().unwrap().is_empty());
    }
}
