use std::{
    future::Future,
    time::{Duration, Instant},
};

use airlock::docker;
use bollard::exec::{CreateExecOptions, StartExecResults};
use futures::TryStreamExt as _;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::trace;

use crate::assertions::{Assertion, AssertionContext, AssertionResult};

const PROBE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Assertion that checks a port is listening.
pub struct PortListeningAssertion {
    port: u16,
    protocol: String,
    timeout: Duration,
}

impl PortListeningAssertion {
    pub fn new(port: u16, protocol: String, timeout: Duration) -> Self {
        Self {
            port,
            protocol,
            timeout,
        }
    }
}

#[async_trait::async_trait]
impl Assertion for PortListeningAssertion {
    fn name(&self) -> &'static str {
        "port_listening"
    }

    fn description(&self) -> String {
        format!("Port {}/{} is listening.", self.port, self.protocol)
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();

        // Pick a probe strategy. In-container probes (currently only Windows) target the listener
        // on `127.0.0.1` from inside the test container itself; host-side probes target the
        // mapped ephemeral port on the runner's loopback.
        let probe = if ctx.target_is_windows() {
            match self.protocol.as_str() {
                "tcp" => Probe::InContainerTcp { port: self.port },
                "udp" => Probe::InContainerUdp { port: self.port },
                other => {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: format!(
                            "Port {}/{}: unsupported protocol for in-container probing.",
                            self.port, other
                        ),
                        duration: started.elapsed(),
                    };
                }
            }
        } else {
            let port_key = format!("{}/{}", self.port, self.protocol);
            let host_port = match ctx.port_mappings.get(&port_key) {
                Some(port) => *port,
                None => {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: format!(
                            "Port {}/{} not exposed in container configuration.",
                            self.port, self.protocol
                        ),
                        duration: started.elapsed(),
                    };
                }
            };
            match self.protocol.as_str() {
                "tcp" => Probe::HostTcp { port: host_port },
                "udp" => Probe::HostUdp { port: host_port },
                other => {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: format!("Port {}/{}: unsupported protocol.", self.port, other),
                        duration: started.elapsed(),
                    };
                }
            }
        };

        let deadline = Instant::now() + self.timeout;

        loop {
            match run_probe_attempt_until(
                deadline,
                &ctx.cancel_token,
                &ctx.container_exit_token,
                probe.run(&ctx.container_name),
            )
            .await
            {
                ProbeAttemptResult::Listening => {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: true,
                        message: format!(
                            "Port {}/{} ({}) is listening.",
                            self.port,
                            self.protocol,
                            probe.target_label()
                        ),
                        duration: started.elapsed(),
                    };
                }
                ProbeAttemptResult::NotListening => {}
                ProbeAttemptResult::TimedOut => {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: format!(
                            "Port {}/{} ({}) not listening after {:?}.",
                            self.port,
                            self.protocol,
                            probe.target_label(),
                            self.timeout
                        ),
                        duration: started.elapsed(),
                    };
                }
                ProbeAttemptResult::Cancelled => {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: "Assertion cancelled because container exited.".to_string(),
                        duration: started.elapsed(),
                    };
                }
            }

            trace!(
                port = self.port,
                protocol = %self.protocol,
                target = %probe.target_label(),
                "Port not yet listening, retrying..."
            );

            tokio::time::sleep(PROBE_RETRY_INTERVAL).await;
        }
    }
}

enum ProbeAttemptResult {
    Listening,
    NotListening,
    TimedOut,
    Cancelled,
}

async fn run_probe_attempt_until<F>(
    deadline: Instant, cancel_token: &CancellationToken, container_exit_token: &CancellationToken, probe: F,
) -> ProbeAttemptResult
where
    F: Future<Output = bool>,
{
    // Keep the assertion deadline and cancellation active while a Docker-backed probe is pending.
    tokio::select! {
        result = tokio::time::timeout_at(deadline.into(), probe) => match result {
            Ok(true) => ProbeAttemptResult::Listening,
            Ok(false) => ProbeAttemptResult::NotListening,
            Err(_) => ProbeAttemptResult::TimedOut,
        },
        _ = cancel_token.cancelled() => ProbeAttemptResult::Cancelled,
        _ = container_exit_token.cancelled() => ProbeAttemptResult::Cancelled,
    }
}

enum Probe {
    HostTcp { port: u16 },
    HostUdp { port: u16 },
    InContainerTcp { port: u16 },
    InContainerUdp { port: u16 },
}

impl Probe {
    fn target_label(&self) -> String {
        match self {
            Self::HostTcp { port } | Self::HostUdp { port } => format!("host 127.0.0.1:{}", port),
            Self::InContainerTcp { port } | Self::InContainerUdp { port } => {
                format!("in-container 127.0.0.1:{}", port)
            }
        }
    }

    async fn run(&self, container_name: &str) -> bool {
        match *self {
            Self::HostTcp { port } => check_tcp_port("127.0.0.1", port).await,
            Self::HostUdp { port } => check_udp_port("127.0.0.1", port).await,
            Self::InContainerTcp { port } => check_tcp_port_in_container(container_name, port).await,
            Self::InContainerUdp { port } => check_udp_port_in_container(container_name, port).await,
        }
    }
}

async fn check_tcp_port(host: &str, port: u16) -> bool {
    TcpStream::connect((host, port)).await.is_ok()
}

async fn check_udp_port(host: &str, port: u16) -> bool {
    // For UDP, we can only check if we can bind a socket and "connect" to the target.
    // This doesn't guarantee something is listening, but it's the best we can do.
    match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(socket) => socket.connect((host, port)).await.is_ok(),
        Err(_) => false,
    }
}

async fn check_tcp_port_in_container(container_name: &str, port: u16) -> bool {
    // The Datadog Agent LTSC image does not ship the NetTCPIP module, so we
    // probe with a .NET TcpClient against loopback inside the container.
    let command = format!(
        "$client = New-Object System.Net.Sockets.TcpClient; try {{ $task = $client.ConnectAsync('127.0.0.1', {}); if ($task.Wait(2000) -and $client.Connected) {{ exit 0 }} else {{ exit 1 }} }} catch {{ exit 1 }} finally {{ $client.Close() }}",
        port
    );
    exec_status(
        container_name,
        vec!["pwsh", "-NoProfile", "-NonInteractive", "-Command", &command],
    )
    .await
    .unwrap_or(false)
}

async fn check_udp_port_in_container(container_name: &str, port: u16) -> bool {
    // UDP is connectionless, so just like the host-side probe we can only verify that we can
    // bind a socket and "connect" to the target. This doesn't prove a listener exists, but it
    // matches the semantics of [`check_udp_port`].
    let command = format!(
        "$client = New-Object System.Net.Sockets.UdpClient; try {{ $client.Connect('127.0.0.1', {}); exit 0 }} catch {{ exit 1 }} finally {{ $client.Close() }}",
        port
    );
    exec_status(
        container_name,
        vec!["pwsh", "-NoProfile", "-NonInteractive", "-Command", &command],
    )
    .await
    .unwrap_or(false)
}

async fn exec_status(container_name: &str, cmd: Vec<&str>) -> Result<bool, String> {
    let docker = docker::connect().map_err(|e| format!("Failed to connect to Docker: {}", e))?;
    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions::<String> {
                cmd: Some(cmd.into_iter().map(String::from).collect()),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("Failed to create exec: {}", e))?;
    let exec_id = exec.id.clone();
    let result = docker
        .start_exec(&exec_id, None)
        .await
        .map_err(|e| format!("Failed to start exec: {}", e))?;
    if let StartExecResults::Attached { mut output, .. } = result {
        while output
            .try_next()
            .await
            .map_err(|e| format!("Failed to read exec output: {}", e))?
            .is_some()
        {}
    }
    let inspect = docker
        .inspect_exec(&exec_id)
        .await
        .map_err(|e| format!("Failed to inspect exec: {}", e))?;
    Ok(inspect.exit_code == Some(0))
}

#[cfg(test)]
mod tests {
    use std::future;

    use tokio_util::sync::CancellationToken;

    use super::*;

    #[tokio::test]
    async fn stalled_probe_respects_deadline() {
        let deadline = Instant::now() + Duration::from_millis(10);
        let result = run_probe_attempt_until(
            deadline,
            &CancellationToken::new(),
            &CancellationToken::new(),
            future::pending(),
        )
        .await;

        assert!(matches!(result, ProbeAttemptResult::TimedOut));
    }

    #[tokio::test]
    async fn stalled_probe_respects_cancellation() {
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();
        let result = run_probe_attempt_until(
            Instant::now() + Duration::from_secs(10),
            &cancel_token,
            &CancellationToken::new(),
            future::pending(),
        )
        .await;

        assert!(matches!(result, ProbeAttemptResult::Cancelled));
    }
}
