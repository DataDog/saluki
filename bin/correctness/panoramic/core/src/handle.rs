//! Operations on one participant through its backend, and their failures.

use std::time::Duration;

use airlock::{exec::ExecOutput, files::FileStat};
use async_trait::async_trait;
use snafu::Snafu;

use crate::{
    backend::courier,
    endpoint::{ResolvedAddress, RunnerHostRoute},
    probe::{AttemptLimits, ProbeAnswer},
};

/// One command for a participant to run.
pub(crate) struct ExecCommand {
    /// The program followed by its arguments.
    pub(crate) argv: Vec<String>,
    /// Variables added to the participant's environment.
    pub(crate) env: Vec<(String, String)>,
    /// Bytes written to stdin, which is then closed; `None` runs with no stdin.
    ///
    /// The backend writes all bytes and closes stdin, even for an empty payload.
    pub(crate) stdin: Option<Vec<u8>>,
}

/// Operations on one participant, implemented by its backend.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "Handles reach participants through their backend's primitives.")
)]
#[async_trait]
pub(crate) trait Primitives: Send + Sync {
    /// Runs a command, abandoning it after `limit`. Any exit code is data.
    async fn exec(&self, command: ExecCommand, limit: Duration) -> Result<ExecOutput, HandleError>;
    /// Reads a regular file; `None` when the path is missing.
    async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, HandleError>;
    /// Reports a path's kind, mode and size; `None` when the path is missing.
    async fn stat(&self, path: &str) -> Result<Option<FileStat>, HandleError>;
    /// Probes an address as seen from inside this participant.
    async fn probe(&self, address: &ResolvedAddress, limits: &AttemptLimits) -> Result<ProbeAnswer, HandleError>;
    /// Sends a payload to an address as seen from inside this participant.
    async fn send(&self, address: &ResolvedAddress, payload: &[u8], limits: &AttemptLimits) -> Result<(), HandleError>;
}

/// Probes through the plan's route for `EndpointOperation::Probe`, using the server for a courier route.
#[cfg_attr(not(test), expect(dead_code, reason = "Steps reach endpoints through these routes."))]
pub(crate) async fn probe_from_runner_host(
    route: &RunnerHostRoute, server: &dyn Primitives, limits: &AttemptLimits,
) -> Result<ProbeAnswer, HandleError> {
    match route {
        RunnerHostRoute::Direct(address) => courier::probe_in_process(address, limits).await,
        RunnerHostRoute::Courier(address) => server.probe(address, limits).await,
    }
}

/// Sends through the plan's route for `EndpointOperation::Send`, using the server for a courier route.
#[cfg_attr(not(test), expect(dead_code, reason = "Steps reach endpoints through these routes."))]
pub(crate) async fn send_from_runner_host(
    route: &RunnerHostRoute, server: &dyn Primitives, payload: &[u8], limits: &AttemptLimits,
) -> Result<(), HandleError> {
    match route {
        RunnerHostRoute::Direct(address) => courier::send_in_process(address, payload, limits).await,
        RunnerHostRoute::Courier(address) => server.send(address, payload, limits).await,
    }
}

/// Why an operation on a participant failed.
#[derive(Debug, PartialEq, Eq, Snafu)]
pub(crate) enum HandleError {
    /// The execution mechanism failed.
    #[snafu(display("{message}"))]
    Harness { message: String },
    /// The mechanism worked and the endpoint answered badly.
    #[snafu(display("{message}"))]
    Product { message: String },
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
                command => panic!("unexpected command: {command}"),
            };
            Ok(ExecOutput {
                stdout: vec![],
                stderr: serde_json::to_vec(&report).unwrap(),
                exit_code: report.exit_code().into(),
            })
        }

        async fn read_file(&self, _path: &str) -> Result<Option<Vec<u8>>, HandleError> {
            panic!("routing does not read files");
        }

        async fn stat(&self, _path: &str) -> Result<Option<FileStat>, HandleError> {
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
}
