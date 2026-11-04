//! Shared endpoint outcomes for courier execs and in-process network calls.

use std::{fmt, future::Future};

use panoramic_courier::{Command, Invocation, Report, ReportError, Target};
use saluki_error::GenericError;

use crate::{
    endpoint::{EndpointKind, ResolvedAddress},
    handle::{ExecCommand, ExecOutput, HandleError, HttpResponse},
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
            let path = address.path.as_deref().ok_or_else(|| missing("a path"))?;
            let name = path.strip_prefix(r"\\.\pipe\").ok_or_else(|| HandleError::Harness {
                message: format!("The courier needs a local named-pipe path, got {path:?}"),
            })?;
            Target::NamedPipe { name: name.into() }
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
    let (report, _) = invoke(exec, executable, &command, None, limits).await?;
    probe_answer(&command, report)
}

fn probe_answer(command: &Command, report: Report) -> Result<ProbeAnswer, HandleError> {
    let (Command::Probe { to } | Command::Send { to }) = command else {
        unreachable!()
    };
    match report {
        Report::Listening => Ok(ProbeAnswer::Listening),
        Report::Addressable => Ok(ProbeAnswer::Addressable),
        Report::Unreachable { reason } => Ok(ProbeAnswer::NotListening { reason }),
        Report::TimedOut { limit_ms } => Ok(ProbeAnswer::NotListening {
            reason: format!("{to}: no answer within {limit_ms} ms"),
        }),
        Report::Usage { reason } | Report::Failed { reason } => Err(harness_error(command, reason)),
        report @ (Report::Sent { .. } | Report::SendFailed { .. } | Report::Responded { .. }) => {
            Err(harness_error(command, format!("unexpected outcome {report:?}")))
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
    let command = send_command(address)?;
    let (report, _) = invoke(exec, executable, &command, Some(payload.to_vec()), limits).await?;
    send_outcome(&command, report, payload.len())
}

fn send_command(address: &ResolvedAddress) -> Result<Command, HandleError> {
    if matches!(address.kind, EndpointKind::Http | EndpointKind::Grpc) {
        return Err(HandleError::Harness {
            message: format!(
                "The courier can't send raw bytes to {} endpoints ({})",
                address.kind.name(),
                address.url,
            ),
        });
    }
    Ok(Command::Send { to: target(address)? })
}

fn send_outcome(command: &Command, report: Report, payload_len: usize) -> Result<(), HandleError> {
    let (Command::Probe { to } | Command::Send { to }) = command else {
        unreachable!()
    };
    match report {
        Report::Sent { bytes } if bytes == payload_len as u64 => Ok(()),
        Report::Sent { bytes } => Err(harness_error(
            command,
            format!("reported {bytes} bytes sent, expected {}", payload_len),
        )),
        Report::Unreachable { reason } | Report::SendFailed { reason } => Err(HandleError::Product { message: reason }),
        Report::TimedOut { limit_ms } => Err(HandleError::Product {
            message: format!("{to}: the send did not finish within {limit_ms} ms"),
        }),
        Report::Usage { reason } | Report::Failed { reason } => Err(harness_error(command, reason)),
        report @ (Report::Listening | Report::Addressable | Report::Responded { .. }) => {
            Err(harness_error(command, format!("unexpected outcome {report:?}")))
        }
    }
}

/// Probes directly from the runner host with the courier's execution and outcome mapping.
pub(crate) async fn probe_in_process(
    address: &ResolvedAddress, limits: &AttemptLimits,
) -> Result<ProbeAnswer, HandleError> {
    let invocation = Invocation {
        command: Command::Probe { to: target(address)? },
        timeout: Some(limits.request),
    };
    let report = panoramic_courier::execute(&invocation, tokio::io::empty(), tokio::io::sink()).await;
    probe_answer(&invocation.command, report)
}

/// Sends directly from the runner host with the courier's execution and outcome mapping.
pub(crate) async fn send_in_process(
    address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits,
) -> Result<(), HandleError> {
    let invocation = Invocation {
        command: send_command(address)?,
        timeout: Some(limits.request),
    };
    let report = panoramic_courier::execute(&invocation, payload, tokio::io::sink()).await;
    send_outcome(&invocation.command, report, payload.len())
}

/// Fetches a plain HTTP response through a bounded courier exec.
pub(crate) async fn http<E, F>(
    exec: E, executable: &str, address: &ResolvedAddress, path: &str, headers_only: bool, limits: &AttemptLimits,
) -> Result<HttpResponse, HandleError>
where
    E: FnOnce(ExecCommand) -> F,
    F: Future<Output = Result<ExecOutput, GenericError>>,
{
    let command = http_command(address, path, headers_only)?;
    let (report, stdout) = invoke(exec, executable, &command, None, limits).await?;
    http_outcome(&command, report, stdout, headers_only)
}

fn http_command(address: &ResolvedAddress, path: &str, headers_only: bool) -> Result<Command, HandleError> {
    if address.kind != EndpointKind::Http || !path.starts_with('/') {
        return Err(HandleError::Harness {
            message: "Courier HTTP requests require an HTTP endpoint and a path starting with '/'".into(),
        });
    }
    if address.url.starts_with("https://") {
        return Err(HandleError::Harness {
            message: format!("courier HTTP requests do not support TLS endpoints ({})", address.url),
        });
    }
    let target = target(address)?.to_string();
    let authority = target.strip_prefix("tcp://").expect("HTTP targets use TCP");
    Ok(Command::Http {
        url: format!("http://{authority}{path}"),
        headers_only,
    })
}

fn http_outcome(
    command: &Command, report: Report, stdout: Vec<u8>, headers_only: bool,
) -> Result<HttpResponse, HandleError> {
    match report {
        Report::Responded { status, bytes } if bytes == stdout.len() as u64 && (!headers_only || bytes == 0) => {
            Ok(HttpResponse { status, body: stdout })
        }
        Report::Responded { bytes, .. } => Err(harness_error(
            command,
            format!(
                "reported {bytes} body bytes, captured {} (headers-only: {headers_only})",
                stdout.len()
            ),
        )),
        Report::Unreachable { reason } => Err(HandleError::Product { message: reason }),
        Report::TimedOut { limit_ms } => Err(HandleError::Product {
            message: format!("HTTP request timed out after {limit_ms} ms"),
        }),
        Report::Usage { reason } | Report::Failed { reason } => Err(harness_error(command, reason)),
        report => Err(harness_error(command, format!("unexpected outcome {report:?}"))),
    }
}

/// Fetches a plain HTTP response with the courier library on the runner host.
#[cfg(unix)]
pub(crate) async fn http_in_process(
    address: &ResolvedAddress, path: &str, headers_only: bool, limits: &AttemptLimits,
) -> Result<HttpResponse, HandleError> {
    let invocation = Invocation {
        command: http_command(address, path, headers_only)?,
        timeout: Some(limits.request),
    };
    let mut stdout = Vec::new();
    let report = panoramic_courier::execute(&invocation, tokio::io::empty(), &mut stdout).await;
    http_outcome(&invocation.command, report, stdout, headers_only)
}

async fn invoke<E, F>(
    exec: E, executable: &str, command: &Command, stdin: Option<Vec<u8>>, limits: &AttemptLimits,
) -> Result<(Report, Vec<u8>), HandleError>
where
    E: FnOnce(ExecCommand) -> F,
    F: Future<Output = Result<ExecOutput, GenericError>>,
{
    let invocation = Invocation {
        command: command.clone(),
        timeout: Some(limits.request),
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
    let report = Report::from_exec(output.exit_code, &output.stderr).map_err(|error| match error {
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
    })?;
    Ok((report, output.stdout))
}

fn harness_error(command: &Command, problem: impl fmt::Display) -> HandleError {
    let (name, to) = match command {
        Command::Probe { to } => ("probe", to.to_string()),
        Command::Send { to } => ("send", to.to_string()),
        Command::Http { url, .. } => ("http", url.clone()),
        Command::Follow { path } => ("follow", path.clone()),
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
            EndpointKind::NamedPipe => ("npipe://listener".into(), None, None, Some(r"\\.\pipe\listener".into())),
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

    fn local_address(kind: EndpointKind, port: u16) -> ResolvedAddress {
        ResolvedAddress {
            kind,
            url: format!("{}://127.0.0.1:{port}", kind.name()),
            host: Some("127.0.0.1".into()),
            port: Some(port),
            path: None,
        }
    }

    #[tokio::test]
    async fn an_in_process_probe_reports_listening_and_not_listening_like_the_courier() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = local_address(EndpointKind::Tcp, listener.local_addr().unwrap().port());
        let limits = AttemptLimits::new(Duration::from_secs(10));
        assert_eq!(probe_in_process(&address, &limits).await, Ok(ProbeAnswer::Listening));
        drop(listener);
        assert!(matches!(
            probe_in_process(&address, &limits).await.unwrap(),
            ProbeAnswer::NotListening { .. }
        ));
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = local_address(EndpointKind::Udp, socket.local_addr().unwrap().port());
        drop(socket);
        assert_eq!(probe_in_process(&address, &limits).await, Ok(ProbeAnswer::Addressable));
    }

    #[tokio::test]
    async fn an_in_process_send_delivers_the_payload_on_its_own_connection() {
        use tokio::io::AsyncReadExt;

        tokio::time::timeout(Duration::from_secs(10), async {
            let limits = AttemptLimits::new(Duration::from_secs(10));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = local_address(EndpointKind::Tcp, listener.local_addr().unwrap().port());
            for payload in [PAYLOAD, b"second"] {
                send_in_process(&address, payload, &limits).await.unwrap();
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).await.unwrap();
                assert_eq!(bytes, payload);
            }
            let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = local_address(EndpointKind::Udp, socket.local_addr().unwrap().port());
            send_in_process(&address, PAYLOAD, &limits).await.unwrap();
            let mut bytes = [0; 128];
            let read = socket.recv(&mut bytes).await.unwrap();
            assert_eq!(&bytes[..read], PAYLOAD);
            #[cfg(unix)]
            {
                let dir = tempfile::tempdir().unwrap();
                for kind in [EndpointKind::UnixStream, EndpointKind::UnixDatagram] {
                    let path = dir.path().join(kind.name());
                    let path = path.to_str().unwrap();
                    let scheme = if kind == EndpointKind::UnixStream {
                        "unix"
                    } else {
                        "unixgram"
                    };
                    let address = ResolvedAddress {
                        kind,
                        url: format!("{scheme}://{path}"),
                        host: None,
                        port: None,
                        path: Some(path.into()),
                    };
                    if kind == EndpointKind::UnixStream {
                        let listener = tokio::net::UnixListener::bind(path).unwrap();
                        for payload in [PAYLOAD, b"second"] {
                            send_in_process(&address, payload, &limits).await.unwrap();
                            let (mut stream, _) = listener.accept().await.unwrap();
                            let mut bytes = Vec::new();
                            stream.read_to_end(&mut bytes).await.unwrap();
                            assert_eq!(bytes, payload);
                        }
                    } else {
                        let socket = tokio::net::UnixDatagram::bind(path).unwrap();
                        send_in_process(&address, PAYLOAD, &limits).await.unwrap();
                        let read = socket.recv(&mut bytes).await.unwrap();
                        assert_eq!(&bytes[..read], PAYLOAD);
                    }
                }
            }
        })
        .await
        .expect("local sends should complete within ten seconds");
    }

    #[tokio::test]
    async fn an_in_process_send_to_nothing_listening_is_a_product_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = local_address(EndpointKind::Tcp, listener.local_addr().unwrap().port());
        drop(listener);
        let error = send_in_process(&address, PAYLOAD, &AttemptLimits::new(Duration::from_secs(10)))
            .await
            .unwrap_err();
        assert!(matches!(&error, HandleError::Product { message } if message.contains(&address.url)));
        assert_private_message(&error);
    }

    #[tokio::test]
    async fn an_in_process_send_to_an_http_endpoint_is_a_harness_error() {
        for kind in [EndpointKind::Http, EndpointKind::Grpc] {
            let address = address(kind);
            let error = send_in_process(&address, PAYLOAD, &AttemptLimits::new(Duration::from_secs(1)))
                .await
                .unwrap_err();
            assert_eq!(
                error,
                HandleError::Harness {
                    message: format!(
                        "The courier can't send raw bytes to {} endpoints ({})",
                        kind.name(),
                        address.url
                    ),
                }
            );
        }
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
    async fn unsupported_sends_fail_before_exec() {
        let limits = AttemptLimits::new(Duration::from_secs(3));
        for kind in [EndpointKind::Http, EndpointKind::Grpc] {
            let resolved = address(kind);
            let error = send(unexpected_exec, "/courier", &resolved, PAYLOAD, &limits)
                .await
                .unwrap_err();
            assert!(matches!(error, HandleError::Harness { .. }));
            assert!(error.to_string().contains(kind.name()));
            assert!(error.to_string().contains(&resolved.url));
            assert_private_message(&error);
        }
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

    #[tokio::test]
    async fn the_courier_reaches_named_pipe_endpoints_by_their_local_name() {
        let address = address(EndpointKind::NamedPipe);
        assert_eq!(
            target(&address).unwrap(),
            Target::NamedPipe {
                name: "listener".into()
            }
        );
        let limits = AttemptLimits::new(Duration::from_secs(3));
        send(
            |command| async move {
                assert_eq!(
                    command.argv,
                    [
                        "courier.exe",
                        "send",
                        "--to",
                        "npipe://listener",
                        "--timeout-ms",
                        "1500"
                    ]
                );
                assert_eq!(command.stdin.as_deref(), Some(PAYLOAD));
                Ok(output(Report::Sent {
                    bytes: PAYLOAD.len() as u64,
                }))
            },
            "courier.exe",
            &address,
            PAYLOAD,
            &limits,
        )
        .await
        .unwrap();
        assert_eq!(
            probe(
                |command| async move {
                    assert_eq!(
                        command.argv,
                        [
                            "courier.exe",
                            "probe",
                            "--to",
                            "npipe://listener",
                            "--timeout-ms",
                            "1500"
                        ]
                    );
                    Ok(output(Report::Listening))
                },
                "courier.exe",
                &address,
                &limits
            )
            .await,
            Ok(ProbeAnswer::Listening)
        );
        for path in [None, Some("listener"), Some(r"\\.\pipe\"), Some(r"\\.\pipe\a\b")] {
            let invalid = ResolvedAddress {
                path: path.map(str::to_owned),
                ..address.clone()
            };
            assert!(matches!(
                probe(unexpected_exec, "courier.exe", &invalid, &limits).await,
                Err(HandleError::Harness { .. })
            ));
        }
    }

    async fn http_output(
        output: Result<ExecOutput, GenericError>, headers_only: bool,
    ) -> Result<HttpResponse, HandleError> {
        http(
            |_| async { output },
            "/courier",
            &address(EndpointKind::Http),
            "/dump",
            headers_only,
            &AttemptLimits::new(Duration::from_secs(3)),
        )
        .await
    }

    #[tokio::test]
    async fn courier_http_outcomes_separate_product_responses_from_mechanism_failures() {
        let response = output(Report::Responded {
            status: 503,
            bytes: PAYLOAD.len() as u64,
        });
        assert_eq!(
            http_output(Ok(response.clone()), false).await,
            Ok(HttpResponse {
                status: 503,
                body: PAYLOAD.to_vec()
            })
        );
        assert!(matches!(
            http_output(Ok(response), true).await,
            Err(HandleError::Harness { .. })
        ));
        for report in [
            Report::Unreachable {
                reason: "refused".into(),
            },
            Report::TimedOut { limit_ms: 1500 },
        ] {
            assert!(matches!(
                http_output(Ok(output(report)), false).await,
                Err(HandleError::Product { .. })
            ));
        }
        for report in [
            Report::Responded { status: 200, bytes: 0 },
            Report::Responded {
                status: 200,
                bytes: PAYLOAD.len() as u64 + 1,
            },
            Report::Usage {
                reason: "invalid URL".into(),
            },
            Report::Failed {
                reason: "stdout unavailable".into(),
            },
            Report::Listening,
            Report::Addressable,
            Report::Sent {
                bytes: PAYLOAD.len() as u64,
            },
            Report::SendFailed {
                reason: "unexpected send failure".into(),
            },
        ] {
            let error = http_output(Ok(output(report)), false).await.unwrap_err();
            assert!(matches!(error, HandleError::Harness { .. }));
            assert_private_message(&error);
        }
        for exit_code in [10, 126, 127] {
            let mut result = output(Report::Responded {
                status: 200,
                bytes: PAYLOAD.len() as u64,
            });
            result.exit_code = exit_code;
            assert!(matches!(
                http_output(Ok(result), false).await,
                Err(HandleError::Harness { .. })
            ));
        }
        assert!(matches!(
            http_output(Err(generic_error!("exec failed")), false).await,
            Err(HandleError::Harness { .. })
        ));
        let result = ExecOutput {
            stdout: vec![],
            stderr: vec![],
            exit_code: 0,
        };
        assert!(matches!(
            http_output(Ok(result), false).await,
            Err(HandleError::Harness { .. })
        ));
    }

    #[tokio::test]
    async fn courier_http_uses_typed_http_addresses_and_headers_only_has_no_body() {
        let limits = AttemptLimits::new(Duration::from_secs(3));
        let address = ResolvedAddress {
            host: Some("::1".into()),
            url: "ignored://other:12".into(),
            ..address(EndpointKind::Http)
        };
        let result = http(
            |command| async move {
                assert_eq!(
                    command.argv,
                    [
                        "/courier",
                        "http",
                        "--url",
                        "http://[::1]:80/ready?a=1",
                        "--headers-only",
                        "--timeout-ms",
                        "1500"
                    ]
                );
                assert!(command.stdin.is_none());
                let mut output = output(Report::Responded { status: 204, bytes: 0 });
                output.stdout.clear();
                Ok(output)
            },
            "/courier",
            &address,
            "/ready?a=1",
            true,
            &limits,
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            HttpResponse {
                status: 204,
                body: vec![]
            }
        );
        let grpc = ResolvedAddress {
            kind: EndpointKind::Grpc,
            ..address
        };
        assert!(matches!(
            http(unexpected_exec, "/courier", &grpc, "/", false, &limits).await,
            Err(HandleError::Harness { .. })
        ));
    }

    #[tokio::test]
    async fn courier_http_rejects_tls_endpoints_before_exec_or_in_process_io() {
        // The TCP target loses the HTTPS scheme; reject TLS before constructing a plain HTTP command.
        let address = ResolvedAddress {
            url: "https://server:443/".into(),
            host: Some("server".into()),
            port: Some(443),
            ..address(EndpointKind::Http)
        };
        let limits = AttemptLimits::new(Duration::from_secs(3));
        let expected = Err(HandleError::Harness {
            message: "courier HTTP requests do not support TLS endpoints (https://server:443/)".into(),
        });
        for headers_only in [false, true] {
            assert_eq!(
                http(unexpected_exec, "/courier", &address, "/", headers_only, &limits).await,
                expected
            );
            #[cfg(unix)]
            assert_eq!(http_in_process(&address, "/", headers_only, &limits).await, expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_courier_http_exec_that_outlasts_the_attempt_is_a_harness_error() {
        let limits = AttemptLimits::new(Duration::from_secs(3));
        let start = Instant::now();
        let error = http(
            |_| pending(),
            "/courier",
            &address(EndpointKind::Http),
            "/dump",
            false,
            &limits,
        )
        .await
        .unwrap_err();
        assert_eq!(start.elapsed(), limits.attempt);
        assert!(matches!(error, HandleError::Harness { .. }));
        assert!(error.to_string().contains("abandoned the courier"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_process_http_uses_the_same_response_contract() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = local_address(EndpointKind::Http, listener.local_addr().unwrap().port());
        let limits = AttemptLimits::new(Duration::from_secs(5));
        tokio::time::timeout(Duration::from_secs(5), async {
            let responder = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                assert!(request.starts_with(b"GET /dump HTTP/1.1\r\n"));
                stream
                    .write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 3\r\n\r\n\0\xff\n")
                    .await
                    .unwrap();
            };
            let ((), result) = tokio::join!(responder, http_in_process(&address, "/dump", false, &limits));
            assert_eq!(
                result,
                Ok(HttpResponse {
                    status: 503,
                    body: b"\0\xff\n".to_vec()
                })
            );
        })
        .await
        .unwrap();
    }
}
