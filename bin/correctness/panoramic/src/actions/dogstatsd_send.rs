use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::Instant;

use super::Action;
use crate::assertions::{AssertionContext, AssertionResult, AssertionStatus};

/// Sends a single DogStatsD datagram over UDP to the target.
///
/// Send failures are `errored`; assertions evaluate the target's response.
pub(super) struct DogstatsdSendAction {
    payload: String,
    port: u16,
    timeout: Duration,
}

impl DogstatsdSendAction {
    /// Creates an action that sends `payload` to the target's `port` DogStatsD listener.
    pub(super) fn new(payload: String, port: u16, timeout: Duration) -> Self {
        Self { payload, port, timeout }
    }
}

#[async_trait::async_trait]
impl Action for DogstatsdSendAction {
    fn name(&self) -> &'static str {
        "dogstatsd_send"
    }

    fn description(&self) -> String {
        format!("Send one DogStatsD datagram to port {}/udp.", self.port)
    }

    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();

        // Windows containers keep their ports internal, and the runtime hands out identity port
        // mappings for them. Sending to the host loopback would succeed without ever reaching the
        // listener, so refuse instead of passing. Reaching a Windows listener needs an
        // in-container send, which we don't support yet.
        if ctx.target_is_windows() {
            return AssertionResult::new(
                self.name(),
                started,
                AssertionStatus::Errored,
                "Sending DogStatsD datagrams to a Windows target is not supported.",
            );
        }

        // Docker publishes the target container's DogStatsD port on the test runner.
        let mapping_key = format!("{}/udp", self.port);
        let Some(published_port) = ctx.port_mappings.get(&mapping_key).copied() else {
            return AssertionResult::new(
                self.name(),
                started,
                AssertionStatus::Errored,
                format!(
                    "Target port {} is not published; expose it in the test case.",
                    mapping_key
                ),
            );
        };

        let send = async {
            // Bind an ephemeral source port, then send through Docker's published port to the target.
            let socket = UdpSocket::bind("127.0.0.1:0").await?;
            socket
                .send_to(self.payload.as_bytes(), ("127.0.0.1", published_port))
                .await
        };

        match tokio::time::timeout(self.timeout, send).await {
            Ok(Ok(sent)) => AssertionResult::new(
                self.name(),
                started,
                AssertionStatus::Passed,
                format!("Sent {} bytes to published port {}.", sent, published_port),
            ),
            Ok(Err(error)) => AssertionResult::new(
                self.name(),
                started,
                AssertionStatus::Errored,
                format!(
                    "Failed to send datagram to published port {}: {}.",
                    published_port, error
                ),
            ),
            Err(_) => AssertionResult::new(
                self.name(),
                started,
                AssertionStatus::Errored,
                format!("Timed out sending datagram to published port {}.", published_port),
            ),
        }
    }
}
