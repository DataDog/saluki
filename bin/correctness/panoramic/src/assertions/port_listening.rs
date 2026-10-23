use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::Instant;
use tracing::trace;

use crate::actions::docker_exec;
use crate::assertions::{Assertion, AssertionContext, AssertionResult, AssertionStatus, Attempts};
use crate::probe::{probe, ProbeError, PROBE_TIMEOUT};

const RETRY_INTERVAL: Duration = Duration::from_millis(100);

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
        let port_probe = if ctx.target_is_windows() {
            match self.protocol.as_str() {
                "tcp" => PortProbe::InContainerTcp { port: self.port },
                "udp" => PortProbe::InContainerUdp { port: self.port },
                other => {
                    let message = format!(
                        "Port {}/{}: unsupported protocol for in-container probing.",
                        self.port, other
                    );
                    return AssertionResult::new(self.name(), started, AssertionStatus::Errored, message);
                }
            }
        } else {
            let port_key = format!("{}/{}", self.port, self.protocol);
            let host_port = match ctx.port_mappings.get(&port_key) {
                Some(port) => *port,
                None => {
                    let message = format!(
                        "Port {}/{} not exposed in container configuration.",
                        self.port, self.protocol
                    );
                    return AssertionResult::new(self.name(), started, AssertionStatus::Errored, message);
                }
            };
            match self.protocol.as_str() {
                "tcp" => PortProbe::HostTcp { port: host_port },
                "udp" => PortProbe::HostUdp { port: host_port },
                other => {
                    let message = format!("Port {}/{}: unsupported protocol.", self.port, other);
                    return AssertionResult::new(self.name(), started, AssertionStatus::Errored, message);
                }
            }
        };

        let what = port_probe.what(&ctx.container_name);
        let deadline = Instant::now() + self.timeout;
        let mut attempts = Attempts::default();

        loop {
            if ctx.cancel_token.is_cancelled() {
                return AssertionResult::new(
                    self.name(),
                    started,
                    attempts.status(),
                    attempts.explain_cancelled("Assertion cancelled."),
                );
            }
            if ctx.container_exit_token.is_cancelled() {
                let message = attempts.explain_cancelled("Assertion cancelled because container exited.");
                return AssertionResult::new(self.name(), started, AssertionStatus::Failed, message);
            }

            if Instant::now() > deadline {
                let summary = format!(
                    "Port {}/{} ({}) not listening after {:?}.",
                    self.port,
                    self.protocol,
                    port_probe.target_label(),
                    self.timeout
                );
                return AssertionResult::new(
                    self.name(),
                    started,
                    attempts.status(),
                    attempts.explain_expired(&summary),
                );
            }

            let limit = deadline.saturating_duration_since(Instant::now()).min(PROBE_TIMEOUT);
            match probe(&what, limit, &ctx.cancel_token, port_probe.run(&ctx.container_name)).await {
                Ok(PortState::Listening) => {
                    let message = format!(
                        "Port {}/{} ({}) is listening.",
                        self.port,
                        self.protocol,
                        port_probe.target_label()
                    );
                    return AssertionResult::new(self.name(), started, AssertionStatus::Passed, message);
                }
                Ok(PortState::NotListening(observation)) => attempts.observed(observation),
                Ok(PortState::Unknown(error)) => attempts.errored(error),
                Err(error @ ProbeError::Cancelled { .. }) => {
                    return AssertionResult::new(
                        self.name(),
                        started,
                        attempts.status(),
                        attempts.explain_cancelled(error),
                    );
                }
                // TCP timeouts test target responsiveness; UDP setup and Docker exec timeouts are harness errors.
                Err(error @ ProbeError::TimedOut { .. }) => match port_probe {
                    PortProbe::HostTcp { .. } => attempts.observed(error),
                    PortProbe::HostUdp { .. } | PortProbe::InContainerTcp { .. } | PortProbe::InContainerUdp { .. } => {
                        attempts.errored(error)
                    }
                },
            }

            trace!(
                port = self.port,
                protocol = %self.protocol,
                target = %port_probe.target_label(),
                "Port not yet listening, retrying..."
            );

            // The next iteration reports cancellation.
            ctx.cancel_token
                .run_until_cancelled(tokio::time::sleep(RETRY_INTERVAL))
                .await;
        }
    }
}

enum PortProbe {
    HostTcp { port: u16 },
    HostUdp { port: u16 },
    InContainerTcp { port: u16 },
    InContainerUdp { port: u16 },
}

/// The outcome of one port probe.
enum PortState {
    Listening,
    /// The TCP connection failed, or the in-container probe exited nonzero.
    NotListening(String),
    /// The harness couldn't probe the port.
    Unknown(String),
}

impl PortProbe {
    fn target_label(&self) -> String {
        match self {
            Self::HostTcp { port } | Self::HostUdp { port } => format!("host 127.0.0.1:{}", port),
            Self::InContainerTcp { port } | Self::InContainerUdp { port } => {
                format!("in-container 127.0.0.1:{}", port)
            }
        }
    }

    /// Identifies the probe in diagnostics.
    fn what(&self, container_name: &str) -> String {
        match self {
            Self::HostTcp { port } => format!("a TCP connection to 127.0.0.1:{port}"),
            Self::HostUdp { port } => format!("a UDP socket for 127.0.0.1:{port}"),
            Self::InContainerTcp { port } => {
                format!("the TCP probe of 127.0.0.1:{port} in container '{container_name}'")
            }
            Self::InContainerUdp { port } => {
                format!("the UDP probe of 127.0.0.1:{port} in container '{container_name}'")
            }
        }
    }

    async fn run(&self, container_name: &str) -> PortState {
        match *self {
            Self::HostTcp { port } => match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(_) => PortState::Listening,
                Err(error) => PortState::NotListening(format!("Failed to connect: {error}.")),
            },
            Self::HostUdp { port } => check_udp_port("127.0.0.1", port).await,
            Self::InContainerTcp { port } => check_tcp_port_in_container(container_name, port).await,
            Self::InContainerUdp { port } => check_udp_port_in_container(container_name, port).await,
        }
    }
}

async fn check_udp_port(host: &str, port: u16) -> PortState {
    // UDP bind and connect only configure the local socket. Their errors are harness failures;
    // success does not establish that the target is listening.
    let socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
        Ok(socket) => socket,
        Err(error) => return PortState::Unknown(format!("Failed to bind a UDP socket: {error}.")),
    };
    match socket.connect((host, port)).await {
        Ok(()) => PortState::Listening,
        Err(error) => PortState::Unknown(format!("Failed to connect a UDP socket: {error}.")),
    }
}

async fn check_tcp_port_in_container(container_name: &str, port: u16) -> PortState {
    // The Datadog Agent LTSC image does not ship the NetTCPIP module, so we
    // probe with a .NET TcpClient against loopback inside the container.
    let command = format!(
        "$client = New-Object System.Net.Sockets.TcpClient; try {{ $task = $client.ConnectAsync('127.0.0.1', {}); if ($task.Wait(2000) -and $client.Connected) {{ exit 0 }} else {{ exit 1 }} }} catch {{ exit 1 }} finally {{ $client.Close() }}",
        port
    );
    exec_status(container_name, &command).await
}

async fn check_udp_port_in_container(container_name: &str, port: u16) -> PortState {
    // UDP is connectionless, so just like the host-side probe we can only verify that we can
    // bind a socket and "connect" to the target. This doesn't prove a listener exists, but it
    // matches the semantics of [`check_udp_port`].
    let command = format!(
        "$client = New-Object System.Net.Sockets.UdpClient; try {{ $client.Connect('127.0.0.1', {}); exit 0 }} catch {{ exit 1 }} finally {{ $client.Close() }}",
        port
    );
    exec_status(container_name, &command).await
}

/// Runs the PowerShell probe in the container and maps its exit code to a port state.
async fn exec_status(container_name: &str, command: &str) -> PortState {
    let cmd = ["pwsh", "-NoProfile", "-NonInteractive", "-Command", command]
        .into_iter()
        .map(String::from)
        .collect();
    match docker_exec(container_name, cmd).await {
        Ok(output) if output.succeeded() => PortState::Listening,
        Ok(output) => PortState::NotListening(format!("The in-container probe exited {:?}.", output.exit_code)),
        Err(error) => PortState::Unknown(format!("{error:#}")),
    }
}
