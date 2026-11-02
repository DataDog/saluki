//! Bounded courier calls and their endpoint outcomes, with execution supplied by the backend.

use std::{fmt, future::Future};

use airlock::exec::ExecOutput;
use panoramic_courier::{Command, Invocation, Report, ReportError, Target};
use saluki_error::GenericError;

use crate::{
    endpoint::{EndpointKind, ResolvedAddress},
    handle::{ExecCommand, HandleError},
    probe::{AttemptLimits, ProbeAnswer},
};

/// Builds a courier target from an address's typed parts. HTTP and gRPC use a TCP connect.
///
/// # Errors
///
/// Returns [`HandleError::Harness`] for an unsupported endpoint kind or missing or invalid address parts.
pub(crate) fn target(address: &ResolvedAddress) -> Result<Target, HandleError> {
    let missing = |part| HandleError::Harness {
        message: format!(
            "The courier can't reach the {} endpoint {} without {part}",
            address.kind.name(),
            address.url,
        ),
    };
    let to = match address.kind {
        EndpointKind::Tcp | EndpointKind::Udp | EndpointKind::Http | EndpointKind::Grpc => {
            let host = address.host.as_ref().ok_or_else(|| missing("a host"))?.clone();
            let port = address.port.ok_or_else(|| missing("a port"))?;
            if address.kind == EndpointKind::Udp {
                Target::Udp { host, port }
            } else {
                Target::Tcp { host, port }
            }
        }
        EndpointKind::UnixStream | EndpointKind::UnixDatagram => {
            let path = address.path.as_ref().ok_or_else(|| missing("a path"))?.clone();
            if address.kind == EndpointKind::UnixStream {
                Target::UnixStream { path }
            } else {
                Target::UnixDatagram { path }
            }
        }
        EndpointKind::NamedPipe => {
            return Err(HandleError::Harness {
                message: format!("The courier can't reach the named-pipe endpoint {}", address.url),
            });
        }
    };
    // Target's fields must follow parse's rules; parsing its rendering checks them.
    Target::parse(&to.to_string()).map_err(|error| HandleError::Harness {
        message: format!(
            "The courier can't reach the {} endpoint {}: {error}",
            address.kind.name(),
            address.url,
        ),
    })
}

/// Probes an endpoint through a bounded courier exec.
///
/// # Errors
///
/// Returns [`HandleError::Harness`] for an unsupported or invalid address, an exec that failed, was abandoned or
/// never started, a missing, invalid or mismatched report, an outcome incompatible with a probe, or a courier
/// usage or local I/O failure.
pub(crate) async fn probe<E, F>(
    exec: E, executable: &str, address: &ResolvedAddress, limits: &AttemptLimits,
) -> Result<ProbeAnswer, HandleError>
where
    E: FnOnce(ExecCommand) -> F,
    F: Future<Output = Result<ExecOutput, GenericError>>,
{
    let command = Command::Probe { to: target(address)? };
    let (Command::Probe { to } | Command::Send { to }) = &command;
    match invoke(exec, executable, &command, None, limits).await? {
        Report::Listening => Ok(ProbeAnswer::Listening),
        Report::Addressable => Ok(ProbeAnswer::Addressable),
        Report::Unreachable { reason } => Ok(ProbeAnswer::NotListening { reason }),
        Report::TimedOut { limit_ms } => Ok(ProbeAnswer::NotListening {
            reason: format!("{to}: no answer within {limit_ms} ms"),
        }),
        Report::Usage { reason } | Report::Failed { reason } => Err(harness_error(&command, reason)),
        report @ (Report::Sent { .. } | Report::SendFailed { .. }) => {
            Err(harness_error(&command, format!("unexpected outcome {report:?}")))
        }
    }
}

/// Sends a payload through one connection or as one datagram using a bounded courier exec.
///
/// # Errors
///
/// Returns [`HandleError::Harness`] for an unsupported or invalid address, an exec that failed, was abandoned or
/// never started, a missing, invalid or mismatched report, an outcome incompatible with a send, an incorrect byte
/// count, or a courier usage or local I/O failure. Returns [`HandleError::Product`] for an unreachable endpoint,
/// a failed write or a courier timeout.
pub(crate) async fn send<E, F>(
    exec: E, executable: &str, address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits,
) -> Result<(), HandleError>
where
    E: FnOnce(ExecCommand) -> F,
    F: Future<Output = Result<ExecOutput, GenericError>>,
{
    if matches!(
        address.kind,
        EndpointKind::Http | EndpointKind::Grpc | EndpointKind::NamedPipe
    ) {
        return Err(HandleError::Harness {
            message: format!(
                "The courier can't send raw bytes to {} endpoints ({})",
                address.kind.name(),
                address.url,
            ),
        });
    }
    let command = Command::Send { to: target(address)? };
    let (Command::Probe { to } | Command::Send { to }) = &command;
    match invoke(exec, executable, &command, Some(payload.to_vec()), limits).await? {
        Report::Sent { bytes } if bytes == payload.len() as u64 => Ok(()),
        Report::Sent { bytes } => Err(harness_error(
            &command,
            format!("reported {bytes} bytes sent, expected {}", payload.len()),
        )),
        Report::Unreachable { reason } | Report::SendFailed { reason } => Err(HandleError::Product { message: reason }),
        Report::TimedOut { limit_ms } => Err(HandleError::Product {
            message: format!("{to}: the send did not finish within {limit_ms} ms"),
        }),
        Report::Usage { reason } | Report::Failed { reason } => Err(harness_error(&command, reason)),
        report @ (Report::Listening | Report::Addressable) => {
            Err(harness_error(&command, format!("unexpected outcome {report:?}")))
        }
    }
}

async fn invoke<E, F>(
    exec: E, executable: &str, command: &Command, stdin: Option<Vec<u8>>, limits: &AttemptLimits,
) -> Result<Report, HandleError>
where
    E: FnOnce(ExecCommand) -> F,
    F: Future<Output = Result<ExecOutput, GenericError>>,
{
    let invocation = Invocation {
        command: command.clone(),
        timeout: limits.request,
    };
    let mut argv = vec![executable.to_owned()];
    argv.extend(invocation.args());
    let output = tokio::time::timeout(
        limits.attempt,
        exec(ExecCommand {
            argv,
            env: Vec::new(),
            stdin,
        }),
    )
    .await
    .map_err(|_| {
        harness_error(
            command,
            format!(
                "abandoned the courier after {:?} without an exit; it may still be running",
                limits.attempt,
            ),
        )
    })?
    .map_err(|error| harness_error(command, format!("exec failed: {error:#}")))?;
    Report::from_exec(output.exit_code, &output.stderr).map_err(|error| match error {
        ReportError::NoReport { .. } => {
            let tail = &output.stderr[output.stderr.len().saturating_sub(1024)..];
            let tail = String::from_utf8_lossy(tail);
            let tail = tail.trim();
            let message = if tail.is_empty() {
                format!("{error}; stderr was empty")
            } else {
                format!("{error}; stderr: {tail}")
            };
            harness_error(command, message)
        }
        ReportError::NotStarted { .. } | ReportError::Mismatch { .. } => harness_error(command, error),
    })
}

fn harness_error(command: &Command, problem: impl fmt::Display) -> HandleError {
    let (name, to) = match command {
        Command::Probe { to } => ("probe", to),
        Command::Send { to } => ("send", to),
    };
    HandleError::Harness {
        message: format!("The courier {name} to {to} failed: {problem}"),
    }
}

#[cfg(test)]
mod tests {
    use std::{future::pending, time::Duration};

    use saluki_error::{generic_error, ErrorContext as _};
    use tokio::time::Instant;

    use super::*;

    const PAYLOAD: &[u8] = b"private-payload-7d29\0\xff";

    fn address(kind: EndpointKind) -> ResolvedAddress {
        let (url, host, port, path) = match kind {
            EndpointKind::Tcp | EndpointKind::Udp | EndpointKind::Http | EndpointKind::Grpc => (
                format!("{}://server:80", kind.name()),
                Some("server".into()),
                Some(80),
                None,
            ),
            EndpointKind::UnixStream | EndpointKind::UnixDatagram => (
                format!(
                    "{}:///airlock/srv/shared/a.sock",
                    if kind == EndpointKind::UnixStream {
                        "unix"
                    } else {
                        "unixgram"
                    },
                ),
                None,
                None,
                Some("/airlock/srv/shared/a.sock".into()),
            ),
            EndpointKind::NamedPipe => (r"\\.\pipe\listener".into(), None, None, None),
        };
        ResolvedAddress {
            kind,
            url,
            host,
            port,
            path,
        }
    }

    fn output(report: Report) -> ExecOutput {
        let mut stderr = serde_json::to_vec(&report).unwrap();
        stderr.push(b'\n');
        ExecOutput {
            stdout: PAYLOAD.to_vec(),
            stderr,
            exit_code: report.exit_code().into(),
        }
    }

    async fn probe_output(output: Result<ExecOutput, GenericError>) -> Result<ProbeAnswer, HandleError> {
        probe(
            |_| async { output },
            "/courier",
            &address(EndpointKind::Tcp),
            &AttemptLimits::new(Duration::from_secs(3)),
        )
        .await
    }

    async fn send_output(output: Result<ExecOutput, GenericError>) -> Result<(), HandleError> {
        send(
            |_| async { output },
            "/courier",
            &address(EndpointKind::Tcp),
            PAYLOAD,
            &AttemptLimits::new(Duration::from_secs(3)),
        )
        .await
    }

    fn unexpected_exec(_: ExecCommand) -> std::future::Ready<Result<ExecOutput, GenericError>> {
        panic!("an unsupported or invalid endpoint must not run the courier")
    }

    #[track_caller]
    fn assert_private_message(error: &HandleError) {
        let message = error.to_string();
        assert!(!message.contains("private-payload-7d29"), "{message}");
    }

    #[track_caller]
    fn assert_harness(error: &HandleError, command: &str, problem: &str) {
        let HandleError::Harness { message } = error else {
            panic!("expected a harness failure: {error:?}");
        };
        assert!(
            message.starts_with(&format!("The courier {command} to tcp://server:80 failed: ")),
            "{message}"
        );
        assert!(message.contains(problem), "{message}");
        assert_private_message(error);
    }

    #[tokio::test]
    async fn a_probe_runs_the_courier_with_no_stdin_and_half_the_attempt_as_its_timeout() {
        let mut recorded = None;
        let answer = probe(
            |request| {
                recorded = Some(request);
                async { Ok(output(Report::Listening)) }
            },
            "/courier",
            &address(EndpointKind::UnixStream),
            &AttemptLimits::new(Duration::from_secs(3)),
        )
        .await;

        assert_eq!(answer, Ok(ProbeAnswer::Listening));
        let request = recorded.expect("the courier should run");
        assert_eq!(
            request.argv,
            [
                "/courier",
                "probe",
                "--to",
                "unix:///airlock/srv/shared/a.sock",
                "--timeout-ms",
                "1500"
            ]
        );
        assert_eq!(request.stdin, None);
        assert!(request.env.is_empty());
    }

    #[tokio::test]
    async fn a_send_supplies_its_payload_as_stdin_even_when_empty() {
        for payload in [PAYLOAD, b"".as_slice()] {
            let mut recorded = None;
            let result = send(
                |request| {
                    recorded = Some(request);
                    async {
                        Ok(output(Report::Sent {
                            bytes: payload.len() as u64,
                        }))
                    }
                },
                "/courier",
                &address(EndpointKind::Udp),
                payload,
                &AttemptLimits::new(Duration::from_secs(3)),
            )
            .await;

            assert_eq!(result, Ok(()));
            let request = recorded.expect("the courier should run");
            assert_eq!(
                request.argv,
                ["/courier", "send", "--to", "udp://server:80", "--timeout-ms", "1500"]
            );
            assert_eq!(request.stdin.as_deref(), Some(payload));
            assert!(request.env.is_empty());
        }
    }

    #[tokio::test]
    async fn http_and_grpc_probes_use_a_tcp_connect_from_typed_parts() {
        for kind in [EndpointKind::Http, EndpointKind::Grpc] {
            let mut resolved = address(kind);
            resolved.url = "ignored://elsewhere:99".into();
            let answer = probe(
                |request| async move {
                    assert_eq!(
                        request.argv,
                        ["/courier", "probe", "--to", "tcp://server:80", "--timeout-ms", "1500"]
                    );
                    Ok(output(Report::Listening))
                },
                "/courier",
                &resolved,
                &AttemptLimits::new(Duration::from_secs(3)),
            )
            .await;
            assert_eq!(answer, Ok(ProbeAnswer::Listening));
        }
    }

    #[tokio::test]
    async fn unsupported_sends_and_named_pipe_probes_fail_before_exec() {
        let limits = AttemptLimits::new(Duration::from_secs(3));
        for kind in [EndpointKind::Http, EndpointKind::Grpc, EndpointKind::NamedPipe] {
            let resolved = address(kind);
            let error = send(unexpected_exec, "/courier", &resolved, PAYLOAD, &limits)
                .await
                .unwrap_err();
            assert!(matches!(error, HandleError::Harness { .. }));
            assert!(error.to_string().contains(kind.name()));
            assert!(error.to_string().contains(&resolved.url));
            assert_private_message(&error);
        }
        let error = probe(unexpected_exec, "/courier", &address(EndpointKind::NamedPipe), &limits)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            HandleError::Harness {
                message: r"The courier can't reach the named-pipe endpoint \\.\pipe\listener".into()
            }
        );
    }

    #[tokio::test]
    async fn missing_or_invalid_typed_parts_fail_before_exec() {
        let limits = AttemptLimits::new(Duration::from_secs(3));
        for kind in [
            EndpointKind::Tcp,
            EndpointKind::Udp,
            EndpointKind::Http,
            EndpointKind::Grpc,
        ] {
            for (host, port, problem) in [
                (None, Some(80), "without a host"),
                (Some("server"), None, "without a port"),
                (Some(""), Some(80), "host"),
                (Some("bad/host"), Some(80), "host"),
                (Some("[::1]"), Some(80), "IPv6"),
                (Some("server"), Some(0), "port"),
            ] {
                let resolved = ResolvedAddress {
                    host: host.map(str::to_owned),
                    port,
                    ..address(kind)
                };
                let error = probe(unexpected_exec, "/courier", &resolved, &limits)
                    .await
                    .unwrap_err();
                assert!(matches!(error, HandleError::Harness { .. }));
                assert!(error.to_string().contains(kind.name()));
                assert!(error.to_string().contains(&resolved.url));
                assert!(error.to_string().contains(problem), "{error}");
                let error = send(unexpected_exec, "/courier", &resolved, PAYLOAD, &limits)
                    .await
                    .unwrap_err();
                assert!(matches!(error, HandleError::Harness { .. }));
                assert!(error.to_string().contains(&resolved.url));
                assert_private_message(&error);
            }
        }
        for kind in [EndpointKind::UnixStream, EndpointKind::UnixDatagram] {
            for path in [None, Some(""), Some("relative.sock")] {
                let resolved = ResolvedAddress {
                    path: path.map(str::to_owned),
                    ..address(kind)
                };
                let error = probe(unexpected_exec, "/courier", &resolved, &limits)
                    .await
                    .unwrap_err();
                assert!(matches!(error, HandleError::Harness { .. }));
                assert!(error.to_string().contains(kind.name()));
                assert!(error.to_string().contains(&resolved.url));
                assert!(error.to_string().contains("path"));
                let error = send(unexpected_exec, "/courier", &resolved, PAYLOAD, &limits)
                    .await
                    .unwrap_err();
                assert!(matches!(error, HandleError::Harness { .. }));
                assert!(error.to_string().contains(&resolved.url));
                assert_private_message(&error);
            }
        }
    }

    #[test]
    fn ipv6_targets_use_unbracketed_typed_hosts() {
        let resolved = ResolvedAddress {
            host: Some("::1".into()),
            ..address(EndpointKind::Tcp)
        };
        assert_eq!(target(&resolved).unwrap(), Target::parse("tcp://[::1]:80").unwrap());
    }

    #[tokio::test]
    async fn probe_outcomes_distinguish_a_listener_from_an_addressable_or_unreachable_endpoint() {
        for (report, expected) in [
            (Report::Listening, ProbeAnswer::Listening),
            (Report::Addressable, ProbeAnswer::Addressable),
            (
                Report::Unreachable {
                    reason: "connection refused".into(),
                },
                ProbeAnswer::NotListening {
                    reason: "connection refused".into(),
                },
            ),
            (
                Report::TimedOut { limit_ms: 1500 },
                ProbeAnswer::NotListening {
                    reason: "tcp://server:80: no answer within 1500 ms".into(),
                },
            ),
        ] {
            assert_eq!(probe_output(Ok(output(report))).await, Ok(expected));
        }
    }

    #[tokio::test]
    async fn send_outcomes_separate_product_answers_from_harness_failures() {
        for (report, expected) in [
            (
                Report::Unreachable {
                    reason: "connection refused".into(),
                },
                HandleError::Product {
                    message: "connection refused".into(),
                },
            ),
            (
                Report::SendFailed {
                    reason: "broken pipe".into(),
                },
                HandleError::Product {
                    message: "broken pipe".into(),
                },
            ),
            (
                Report::TimedOut { limit_ms: 1500 },
                HandleError::Product {
                    message: "tcp://server:80: the send did not finish within 1500 ms".into(),
                },
            ),
            (
                Report::Usage {
                    reason: "invalid arguments".into(),
                },
                HandleError::Harness {
                    message: "The courier send to tcp://server:80 failed: invalid arguments".into(),
                },
            ),
            (
                Report::Failed {
                    reason: "local I/O failed".into(),
                },
                HandleError::Harness {
                    message: "The courier send to tcp://server:80 failed: local I/O failed".into(),
                },
            ),
        ] {
            let error = send_output(Ok(output(report))).await.unwrap_err();
            assert_eq!(error, expected);
            assert_private_message(&error);
        }
    }

    #[tokio::test]
    async fn usage_and_local_failures_are_harness_errors_for_a_probe() {
        for report in [
            Report::Usage {
                reason: "invalid arguments".into(),
            },
            Report::Failed {
                reason: "local I/O failed".into(),
            },
        ] {
            let reason = match &report {
                Report::Usage { reason } | Report::Failed { reason } => reason.clone(),
                _ => unreachable!(),
            };
            let error = probe_output(Ok(output(report))).await.unwrap_err();
            assert_harness(&error, "probe", &reason);
        }
    }

    #[tokio::test]
    async fn launch_failures_say_the_courier_never_ran() {
        for exit_code in [126, 127] {
            let mut result = output(Report::Listening);
            result.exit_code = exit_code;
            let probe_error = probe_output(Ok(result.clone())).await.unwrap_err();
            let send_error = send_output(Ok(result)).await.unwrap_err();
            for (command, error) in [("probe", probe_error), ("send", send_error)] {
                assert_harness(&error, command, &format!("The courier never ran (exit {exit_code})"));
                assert!(error
                    .to_string()
                    .contains("installed, executable and built for the container architecture"));
            }
        }
    }

    #[tokio::test]
    async fn reports_for_the_wrong_command_are_harness_errors() {
        for report in [
            Report::Sent { bytes: 0 },
            Report::SendFailed {
                reason: "broken pipe".into(),
            },
        ] {
            let error = probe_output(Ok(output(report))).await.unwrap_err();
            assert_harness(&error, "probe", "unexpected outcome");
        }
        for report in [Report::Listening, Report::Addressable] {
            let error = send_output(Ok(output(report))).await.unwrap_err();
            assert_harness(&error, "send", "unexpected outcome");
        }
    }

    #[tokio::test]
    async fn an_exit_code_that_disagrees_with_the_report_is_a_harness_error() {
        let mut result = output(Report::Sent {
            bytes: PAYLOAD.len() as u64,
        });
        result.exit_code = 10;
        assert_harness(
            &probe_output(Ok(result.clone())).await.unwrap_err(),
            "probe",
            "exit 10 disagrees with outcome",
        );
        assert_harness(
            &send_output(Ok(result)).await.unwrap_err(),
            "send",
            "exit 10 disagrees with outcome",
        );
    }

    #[tokio::test]
    async fn a_missing_report_quotes_only_the_last_kilobyte_of_stderr_lossily_and_trimmed() {
        for stderr in [
            Vec::new(),
            b" \r\n\t".to_vec(),
            b"  short diagnostic \n".to_vec(),
            [b"excluded prefix".as_slice(), &vec![b'x'; 1020], b"\xff \r\n"].concat(),
        ] {
            let result = ExecOutput {
                stdout: PAYLOAD.to_vec(),
                stderr: stderr.clone(),
                exit_code: 101,
            };
            for (command, error) in [
                ("probe", probe_output(Ok(result.clone())).await.unwrap_err()),
                ("send", send_output(Ok(result)).await.unwrap_err()),
            ] {
                assert_harness(&error, command, "exit 101 has no valid report");
                let message = error.to_string();
                if stderr.iter().all(u8::is_ascii_whitespace) {
                    assert!(message.ends_with("; stderr was empty"), "{message}");
                } else {
                    let (_, quoted) = message.split_once("; stderr: ").unwrap();
                    let expected = if stderr.len() > 1024 {
                        format!("{}\u{fffd}", "x".repeat(1020))
                    } else {
                        "short diagnostic".to_owned()
                    };
                    assert_eq!(quoted, expected);
                }
                assert!(!message.contains("excluded prefix"));
            }
        }
    }

    #[tokio::test]
    async fn a_wrong_sent_byte_count_is_a_harness_error() {
        for bytes in [0, PAYLOAD.len() as u64 - 1, PAYLOAD.len() as u64 + 1] {
            let error = send_output(Ok(output(Report::Sent { bytes }))).await.unwrap_err();
            assert_harness(
                &error,
                "send",
                &format!("reported {bytes} bytes sent, expected {}", PAYLOAD.len()),
            );
        }
    }

    #[tokio::test]
    async fn an_exec_error_is_a_harness_error_with_its_full_cause() {
        let failure = || Err::<ExecOutput, _>(generic_error!("connection lost")).error_context("exec transport failed");
        assert_harness(
            &probe_output(failure()).await.unwrap_err(),
            "probe",
            "exec failed: exec transport failed: connection lost",
        );
        assert_harness(
            &send_output(failure()).await.unwrap_err(),
            "send",
            "exec failed: exec transport failed: connection lost",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_exec_that_outlasts_its_attempt_is_abandoned() {
        let limits = AttemptLimits::new(Duration::from_secs(3));
        let resolved = address(EndpointKind::Tcp);
        let started = Instant::now();
        let error = probe(|_| pending(), "/courier", &resolved, &limits).await.unwrap_err();
        assert_eq!(started.elapsed(), limits.attempt);
        assert_harness(
            &error,
            "probe",
            "abandoned the courier after 3s without an exit; it may still be running",
        );

        let started = Instant::now();
        let error = send(|_| pending(), "/courier", &resolved, PAYLOAD, &limits)
            .await
            .unwrap_err();
        assert_eq!(started.elapsed(), limits.attempt);
        assert_harness(
            &error,
            "send",
            "abandoned the courier after 3s without an exit; it may still be running",
        );
    }
}
