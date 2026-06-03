use std::time::{Duration, Instant};

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

        // Look up the mapped host port, or use the container IP directly when available (Windows containers).
        let port_key = format!("{}/{}", self.port, self.protocol);
        let target = match ctx.container_ip.as_deref() {
            Some(container_ip) => (container_ip.to_string(), self.port),
            None => match ctx.port_mappings.get(&port_key) {
                Some(port) => ("127.0.0.1".to_string(), *port),
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
            },
        };

        let deadline = Instant::now() + self.timeout;

        loop {
            if Instant::now() > deadline {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!(
                        "Port {}/{} (target {}:{}) not listening after {:?}.",
                        self.port, self.protocol, target.0, target.1, self.timeout
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

            let is_listening = match self.protocol.as_str() {
                "tcp" => check_tcp_port(&target.0, target.1).await,
                "udp" => check_udp_port(&target.0, target.1).await,
                _ => false,
            };

            if is_listening {
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: true,
                    message: format!(
                        "Port {}/{} (target {}:{}) is listening.",
                        self.port, self.protocol, target.0, target.1
                    ),
                    duration: started.elapsed(),
                };
            }

            trace!(
                port = self.port,
                protocol = %self.protocol,
                target_host = %target.0,
                target_port = target.1,
                "Port not yet listening, retrying..."
            );

            tokio::time::sleep(Duration::from_millis(100)).await;
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
