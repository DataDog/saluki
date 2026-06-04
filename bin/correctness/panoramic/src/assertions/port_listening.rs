use std::time::{Duration, Instant};

use airlock::docker;
use bollard::exec::{CreateExecOptions, StartExecResults};
use futures::TryStreamExt as _;
use tokio::net::TcpStream;
use tracing::trace;

use crate::assertions::{Assertion, AssertionContext, AssertionResult};

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
        let probe = if ctx.use_container_exec_for_network_checks {
            match self.protocol.as_str() {
                "tcp" => Probe::InContainerTcp { port: self.port },
                other => {
                    return AssertionResult {
                        name: self.name().to_string(),
                        passed: false,
                        message: format!(
                            "Port {}/{}: in-container probing is only supported for tcp, not {}.",
                            self.port, self.protocol, other
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
            if Instant::now() > deadline {
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

            if ctx.cancel_token.is_cancelled() || ctx.container_exit_token.is_cancelled() {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: "Assertion cancelled because container exited.".to_string(),
                    duration: started.elapsed(),
                };
            }

            if probe.run(&ctx.container_name).await {
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

            trace!(
                port = self.port,
                protocol = %self.protocol,
                target = %probe.target_label(),
                "Port not yet listening, retrying..."
            );

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

enum Probe {
    HostTcp { port: u16 },
    HostUdp { port: u16 },
    InContainerTcp { port: u16 },
}

impl Probe {
    fn target_label(&self) -> String {
        match self {
            Self::HostTcp { port } | Self::HostUdp { port } => format!("host 127.0.0.1:{}", port),
            Self::InContainerTcp { port } => format!("in-container 127.0.0.1:{}", port),
        }
    }

    async fn run(&self, container_name: &str) -> bool {
        match *self {
            Self::HostTcp { port } => check_tcp_port("127.0.0.1", port).await,
            Self::HostUdp { port } => check_udp_port("127.0.0.1", port).await,
            Self::InContainerTcp { port } => check_tcp_port_in_container(container_name, port).await,
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
