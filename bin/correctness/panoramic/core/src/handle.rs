//! Operations on one participant through its backend, and their failures.

use std::time::Duration;

use async_trait::async_trait;
use snafu::Snafu;

use crate::{
    backend::{
        courier,
        http::{self, HttpError},
    },
    endpoint::{ResolvedAddress, RunnerHostRoute},
    probe::{AttemptLimits, ProbeAnswer},
};

/// One command for a participant to run.
pub struct ExecCommand {
    /// The program followed by its arguments.
    pub argv: Vec<String>,
    /// Variables added to the participant's environment.
    pub env: Vec<(String, String)>,
    /// Bytes written to stdin, which is then closed; `None` runs with no stdin.
    ///
    /// The backend writes all bytes and closes stdin, even for an empty payload.
    pub stdin: Option<Vec<u8>>,
}

/// Captured command output and its exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutput {
    /// Bytes written to standard output.
    pub stdout: Vec<u8>,
    /// Bytes written to standard error.
    pub stderr: Vec<u8>,
    /// The command's exit code.
    pub exit_code: i64,
}

/// Certificate and handshake-signature verification for an HTTPS request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsVerification {
    /// Requires certificate verification; HTTPS verification is unsupported.
    Verify,
    /// Accepts any certificate and handshake signature. Valid only for HTTPS.
    Skip,
}

/// An HTTP response's status and complete body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// A path's kind, permission bits and size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStatus {
    /// The kind of path.
    pub kind: FileKind,
    /// Unix permission bits, including `setuid`, `setgid` and sticky bits.
    pub mode: u32,
    /// Size in bytes.
    pub size: u64,
}

/// The kind of a participant's filesystem path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link.
    Symlink {
        /// The target path reported by the backend.
        target: String,
    },
    /// A Unix socket.
    Socket,
    /// A named pipe.
    NamedPipe,
    /// A device or unrecognized kind.
    Other,
}

/// Operations on one participant, implemented by its backend.
#[async_trait]
pub(crate) trait Primitives: Send + Sync {
    /// Runs a command, abandoning it after `limit`. Any exit code is data.
    async fn exec(&self, command: ExecCommand, limit: Duration) -> Result<ExecOutput, HandleError>;
    /// Reads a regular file; `None` when the path is missing.
    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError>;
    /// Reports a path's kind, mode and size; `None` when the path is missing.
    async fn stat(&self, path: &str) -> Result<Option<FileStatus>, HandleError>;
    /// Probes an address as seen from inside this participant.
    async fn probe(&self, address: &ResolvedAddress, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError>;
    /// Sends a payload to an address as seen from inside this participant.
    async fn send(&self, address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits) -> Result<(), HandleError>;
    /// Makes an HTTP GET to an address as seen from inside this participant.
    async fn http(
        &self, address: &ResolvedAddress, path: &str, headers_only: bool, limits: &AttemptLimits,
    ) -> Result<HttpResponse, HandleError>;
}

/// Probes through the plan's route for `EndpointOperation::Probe`, using the server for a courier route.
pub(crate) async fn probe_from_runner_host(
    route: &RunnerHostRoute, server: &dyn Primitives, limits: &AttemptLimits,
) -> Result<ProbeAnswer, HandleError> {
    match route {
        RunnerHostRoute::Direct(address) => courier::probe_in_process(address, limits).await,
        RunnerHostRoute::Courier(address) => server.probe(address, limits).await,
    }
}

/// Requests an HTTP status through the plan's request route.
/// Invalid requests and unsupported routes are harness failures; connection errors and timeouts are product failures.
pub(crate) async fn http_status_from_runner_host(
    route: &RunnerHostRoute, server: &dyn Primitives, path: &str, limits: &AttemptLimits,
) -> Result<u16, HandleError> {
    match route {
        RunnerHostRoute::Direct(address) => {
            match tokio::time::timeout(limits.attempt, http::get_status(address, path)).await {
                Ok(Ok(status)) => Ok(status),
                Ok(Err(HttpError::Request(message))) => Err(HandleError::Harness { message }),
                Ok(Err(HttpError::Connection(message))) => Err(HandleError::Product { message }),
                Err(_) => Err(HandleError::Product {
                    message: format!("HTTP check timed out after {:?}", limits.attempt),
                }),
            }
        }
        RunnerHostRoute::Courier(address) => server
            .http(address, path, true, limits)
            .await
            .map(|response| response.status),
    }
}

/// Gets an HTTP response through the plan's request route, bounded through completion of its body.
pub(crate) async fn http_get_from_runner_host(
    route: &RunnerHostRoute, server: &dyn Primitives, path: &str, limits: &AttemptLimits,
) -> Result<HttpResponse, HandleError> {
    http_get_with_from_runner_host(route, server, path, TlsVerification::Verify, limits).await
}

/// Gets an HTTP response with an explicit TLS verification policy.
pub(crate) async fn http_get_with_from_runner_host(
    route: &RunnerHostRoute, server: &dyn Primitives, path: &str, verification: TlsVerification, limits: &AttemptLimits,
) -> Result<HttpResponse, HandleError> {
    match route {
        RunnerHostRoute::Direct(address) => {
            match tokio::time::timeout(limits.attempt, http::get(address, path, verification)).await {
                Ok(Ok((status, body))) => Ok(HttpResponse {
                    status,
                    body: body.to_vec(),
                }),
                Ok(Err(HttpError::Request(message))) => Err(HandleError::Harness { message }),
                Ok(Err(HttpError::Connection(message))) => Err(HandleError::Product { message }),
                Err(_) => Err(HandleError::Product {
                    message: format!("HTTP request timed out after {:?}", limits.attempt),
                }),
            }
        }
        RunnerHostRoute::Courier(address) => {
            if verification == TlsVerification::Skip {
                return Err(HandleError::Harness {
                    message: "courier HTTP requests do not support skipping TLS verification".into(),
                });
            }
            server.http(address, path, false, limits).await
        }
    }
}

/// Sends through the plan's route for `EndpointOperation::Send`, using the server for a courier route.
pub(crate) async fn send_from_runner_host(
    route: &RunnerHostRoute, server: &dyn Primitives, payload: &[u8], limits: &AttemptLimits,
) -> Result<(), HandleError> {
    match route {
        RunnerHostRoute::Direct(address) => courier::send_in_process(address, payload, limits).await,
        RunnerHostRoute::Courier(address) => server.send(address, payload, limits).await,
    }
}

/// Why an operation on a participant failed.
#[derive(Clone, Debug, PartialEq, Eq, Snafu)]
pub enum HandleError {
    /// The execution mechanism failed.
    #[snafu(display("{message}"))]
    Harness {
        /// Why the mechanism failed.
        message: String,
    },
    /// The mechanism worked and the endpoint answered badly.
    #[snafu(display("{message}"))]
    Product {
        /// The unsuccessful answer.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use panoramic_courier::Report;
    use saluki_error::GenericError;

    use super::*;
    use crate::endpoint::EndpointKind;

    struct Server {
        address: Option<ResolvedAddress>,
        exited: bool,
        calls: Mutex<Vec<&'static str>>,
    }

    #[async_trait]
    impl Primitives for Server {
        async fn exec(&self, command: ExecCommand, _limit: Duration) -> Result<ExecOutput, HandleError> {
            assert!(self.address.is_some(), "a direct route must not use the server");
            if self.exited {
                return Err(HandleError::Harness {
                    message: "participant has exited".into(),
                });
            }
            let report = match command.argv[1].as_str() {
                "probe" => Report::Listening,
                "send" => {
                    assert_eq!(command.stdin.as_deref(), Some(b"payload".as_slice()));
                    Report::Sent { bytes: 7 }
                }
                "http" => {
                    let invocation = panoramic_courier::Invocation::parse(&command.argv[1..]).unwrap();
                    let panoramic_courier::Command::Http { url, headers_only } = invocation.command else {
                        unreachable!()
                    };
                    assert_eq!(url, "http://127.0.0.1:1003/dump");
                    assert!(command.stdin.is_none());
                    Report::Responded {
                        status: 503,
                        bytes: if headers_only { 0 } else { 3 },
                    }
                }
                command => panic!("unexpected command: {command}"),
            };
            Ok(ExecOutput {
                stdout: if matches!(report, Report::Responded { bytes: 3, .. }) {
                    b"x\0\xff".to_vec()
                } else {
                    vec![]
                },
                stderr: serde_json::to_vec(&report).unwrap(),
                exit_code: report.exit_code().into(),
            })
        }

        async fn read_file(&self, _path: &str) -> Result<Option<Vec<u8>>, HandleError> {
            panic!("routing does not read files");
        }

        async fn stat(&self, _path: &str) -> Result<Option<FileStatus>, HandleError> {
            panic!("routing does not stat files");
        }

        async fn probe(&self, address: &ResolvedAddress, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
            assert_eq!(
                Some(address),
                self.address.as_ref(),
                "a direct route must not use the server"
            );
            self.calls.lock().unwrap().push("probe");
            courier::probe(
                |command| async { self.exec(command, limits.attempt).await.map_err(GenericError::new) },
                "/courier",
                address,
                limits,
            )
            .await
        }

        async fn send(
            &self, address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits,
        ) -> Result<(), HandleError> {
            assert_eq!(
                Some(address),
                self.address.as_ref(),
                "a direct route must not use the server"
            );
            self.calls.lock().unwrap().push("send");
            courier::send(
                |command| async { self.exec(command, limits.attempt).await.map_err(GenericError::new) },
                "/courier",
                address,
                payload,
                limits,
            )
            .await
        }

        async fn http(
            &self, address: &ResolvedAddress, path: &str, headers_only: bool, limits: &AttemptLimits,
        ) -> Result<HttpResponse, HandleError> {
            assert_eq!(
                Some(address),
                self.address.as_ref(),
                "a direct route must not use the server"
            );
            self.calls.lock().unwrap().push("http");
            courier::http(
                |command| async { self.exec(command, limits.attempt).await.map_err(GenericError::new) },
                "/courier",
                address,
                path,
                headers_only,
                limits,
            )
            .await
        }
    }

    fn server(exited: bool) -> Server {
        Server {
            address: Some(ResolvedAddress {
                kind: EndpointKind::UnixStream,
                url: "unix:///run/server.sock".into(),
                host: None,
                port: None,
                path: Some("/run/server.sock".into()),
            }),
            exited,
            calls: Mutex::new(Vec::new()),
        }
    }

    #[tokio::test]
    async fn a_direct_route_never_uses_the_server_participant() {
        let server = Server {
            address: None,
            ..server(false)
        };
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let route = RunnerHostRoute::Direct(ResolvedAddress {
            kind: EndpointKind::Udp,
            url: format!("udp://127.0.0.1:{port}"),
            host: Some("127.0.0.1".into()),
            port: Some(port),
            path: None,
        });
        let limits = AttemptLimits::new(Duration::from_secs(5));
        assert_eq!(
            probe_from_runner_host(&route, &server, &limits).await,
            Ok(ProbeAnswer::Addressable)
        );
        send_from_runner_host(&route, &server, b"payload", &limits)
            .await
            .unwrap();
        let mut bytes = [0; 128];
        let read = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&bytes[..read], b"payload");
        assert!(server.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_courier_route_runs_in_the_server_with_its_stored_address() {
        let server = server(false);
        let route = RunnerHostRoute::Courier(server.address.clone().unwrap());
        let limits = AttemptLimits::new(Duration::from_secs(5));
        assert_eq!(
            probe_from_runner_host(&route, &server, &limits).await,
            Ok(ProbeAnswer::Listening)
        );
        assert_eq!(
            send_from_runner_host(&route, &server, b"payload", &limits).await,
            Ok(())
        );
        assert_eq!(*server.calls.lock().unwrap(), ["probe", "send"]);
    }

    #[tokio::test]
    async fn a_courier_route_whose_exec_fails_is_a_harness_error() {
        let server = server(true);
        let route = RunnerHostRoute::Courier(server.address.clone().unwrap());
        let limits = AttemptLimits::new(Duration::from_secs(5));
        for error in [
            probe_from_runner_host(&route, &server, &limits).await.unwrap_err(),
            send_from_runner_host(&route, &server, b"payload", &limits)
                .await
                .unwrap_err(),
        ] {
            assert!(
                matches!(error, HandleError::Harness { message } if message.contains("exec failed: participant has exited"))
            );
        }
        assert_eq!(*server.calls.lock().unwrap(), ["probe", "send"]);
    }

    fn http_route(port: u16) -> RunnerHostRoute {
        RunnerHostRoute::Direct(ResolvedAddress {
            kind: EndpointKind::Http,
            url: format!("http://127.0.0.1:{port}"),
            host: Some("127.0.0.1".into()),
            port: Some(port),
            path: None,
        })
    }

    #[tokio::test]
    async fn a_direct_http_route_returns_the_status_without_using_the_participant() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let server = Server {
            address: None,
            ..server(false)
        };
        for status in [200, 503] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let responder = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                assert!(stream.read(&mut [0; 1024]).await.unwrap() > 0);
                stream
                    .write_all(format!("HTTP/1.1 {status} Status\r\nContent-Length: 0\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
            };
            let limits = AttemptLimits::new(Duration::from_secs(5));
            let route = http_route(port);
            tokio::time::timeout(Duration::from_secs(5), async {
                let (_, result) = tokio::join!(
                    responder,
                    http_status_from_runner_host(&route, &server, "/ready", &limits)
                );
                assert_eq!(result, Ok(status));
            })
            .await
            .unwrap();
        }
        assert!(server.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_direct_http_get_returns_the_status_and_complete_binary_body() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        for status in [200, 503] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let route = http_route(listener.local_addr().unwrap().port());
            let responder = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                assert!(request.starts_with(b"GET /dump?kind=records HTTP/1.1\r\n"));
                stream
                    .write_all(format!("HTTP/1.1 {status} Status\r\nTransfer-Encoding: chunked\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                stream.write_all(b"3\r\none\r\n").await.unwrap();
                tokio::task::yield_now().await;
                stream.write_all(b"2\r\n\x00\xff\r\n0\r\n\r\n").await.unwrap();
                assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            };
            let limits = AttemptLimits::new(Duration::from_secs(5));
            let server = Server {
                address: None,
                ..server(false)
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                let (_, result) = tokio::join!(
                    responder,
                    http_get_from_runner_host(&route, &server, "/dump?kind=records", &limits)
                );
                assert_eq!(
                    result,
                    Ok(HttpResponse {
                        status,
                        body: b"one\x00\xff".to_vec(),
                    })
                );
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn courier_http_routes_return_the_response_and_status_checks_request_only_headers() {
        let RunnerHostRoute::Direct(address) = http_route(1003) else {
            unreachable!()
        };
        let server = Server {
            address: Some(address.clone()),
            ..server(false)
        };
        let route = RunnerHostRoute::Courier(address);
        let limits = AttemptLimits::new(Duration::from_secs(5));
        assert_eq!(
            http_get_from_runner_host(&route, &server, "/dump", &limits).await,
            Ok(HttpResponse {
                status: 503,
                body: b"x\0\xff".to_vec()
            })
        );
        assert_eq!(
            http_status_from_runner_host(&route, &server, "/dump", &limits).await,
            Ok(503)
        );
        assert_eq!(*server.calls.lock().unwrap(), ["http", "http"]);
    }

    #[tokio::test]
    async fn courier_tls_gets_with_verification_and_status_checks_fail_before_exec() {
        let RunnerHostRoute::Direct(mut address) = http_route(1003) else {
            unreachable!()
        };
        address.url = address.url.replacen("http:", "https:", 1);
        let server = Server {
            address: Some(address.clone()),
            ..server(true)
        };
        let expected = HandleError::Harness {
            message: format!("courier HTTP requests do not support TLS endpoints ({})", address.url),
        };
        let route = RunnerHostRoute::Courier(address);
        let limits = AttemptLimits::new(Duration::from_secs(5));
        assert_eq!(
            http_get_with_from_runner_host(&route, &server, "/dump", TlsVerification::Verify, &limits).await,
            Err(expected.clone())
        );
        assert_eq!(
            http_status_from_runner_host(&route, &server, "/dump", &limits).await,
            Err(expected)
        );
        assert_eq!(*server.calls.lock().unwrap(), ["http", "http"]);
    }

    #[tokio::test]
    async fn courier_http_rejects_skipped_tls_verification_before_dispatch() {
        for scheme in ["http", "https"] {
            let RunnerHostRoute::Direct(mut address) = http_route(1003) else {
                unreachable!()
            };
            address.url = address.url.replacen("http:", &format!("{scheme}:"), 1);
            let server = Server {
                address: Some(address.clone()),
                ..server(false)
            };
            let route = RunnerHostRoute::Courier(address);
            let limits = AttemptLimits::new(Duration::from_secs(5));
            assert_eq!(
                http_get_with_from_runner_host(&route, &server, "/dump", TlsVerification::Skip, &limits).await,
                Err(HandleError::Harness {
                    message: "courier HTTP requests do not support skipping TLS verification".into(),
                })
            );
            assert!(server.calls.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn a_stalled_http_get_times_out_during_headers_or_body_and_closes_its_connection() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        for headers_received in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let route = http_route(listener.local_addr().unwrap().port());
            let responder = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                assert!(request.starts_with(b"GET /dump HTTP/1.1\r\n"));
                if headers_received {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\none")
                        .await
                        .unwrap();
                }
                assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            };
            let limits = AttemptLimits::new(Duration::from_millis(100));
            let server = Server {
                address: None,
                ..server(false)
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                let (_, result) = tokio::join!(responder, http_get_from_runner_host(&route, &server, "/dump", &limits));
                assert_eq!(
                    result,
                    Err(HandleError::Product {
                        message: "HTTP request timed out after 100ms".into(),
                    })
                );
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn http_request_errors_are_harness_failures_and_connection_errors_are_product_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let server = Server {
            address: None,
            ..server(false)
        };
        let limits = AttemptLimits::new(Duration::from_secs(5));
        let route = http_route(port);
        for result in [
            http_status_from_runner_host(&route, &server, "/a b", &limits).await,
            http_get_from_runner_host(&route, &server, "/a b", &limits)
                .await
                .map(|response| response.status),
        ] {
            assert_eq!(
                result,
                Err(HandleError::Harness {
                    message: "invalid uri character".into()
                })
            );
        }
        for error in [
            http_status_from_runner_host(&route, &server, "/ready", &limits)
                .await
                .unwrap_err(),
            http_get_from_runner_host(&route, &server, "/ready", &limits)
                .await
                .unwrap_err(),
        ] {
            assert!(
                matches!(error, HandleError::Product { ref message } if message.to_lowercase().contains("refused")),
                "{error:?}"
            );
        }
        assert!(server.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_stalled_http_request_is_a_product_timeout_and_closes_its_connection() {
        use tokio::{io::AsyncReadExt, net::TcpListener};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let route = http_route(listener.local_addr().unwrap().port());
        let server = Server {
            address: None,
            ..server(false)
        };
        let limits = AttemptLimits::new(Duration::from_millis(100));
        let responder = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await.unwrap();
            assert!(request.starts_with(b"GET /ready HTTP/1.1\r\n"));
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let (_, result) = tokio::join!(
                responder,
                http_status_from_runner_host(&route, &server, "/ready", &limits)
            );
            assert_eq!(
                result,
                Err(HandleError::Product {
                    message: "HTTP check timed out after 100ms".into()
                })
            );
        })
        .await
        .unwrap();
        assert!(server.calls.lock().unwrap().is_empty());
    }
}
