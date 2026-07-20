use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use bytes::Bytes;
use saluki_common::task::spawn_traced_named;
use stringtheory::MetaString;
use tokio::{net::UdpSocket, sync::mpsc, time::timeout};
use tracing::{debug, info, warn};

use super::metrics::Metrics;

const FORWARDER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const FORWARDER_IPV4_BIND_ADDR: &str = "0.0.0.0:0";
const FORWARDER_IPV6_BIND_ADDR: &str = "[::]:0";
const FORWARDER_SOCKET_READY_TIMEOUT: Duration = Duration::from_millis(100);
pub(super) const FORWARDER_QUEUE_CAPACITY: usize = 1024;

#[derive(Clone)]
pub(super) struct PacketForwarderTarget {
    target_host: MetaString,
    target_port: u16,
}

impl PacketForwarderTarget {
    pub(super) fn new(target_host: MetaString, target_port: u16) -> Self {
        Self {
            target_host,
            target_port,
        }
    }

    pub(super) fn to_forwarder(&self, metrics: Metrics) -> PacketForwarder {
        PacketForwarder {
            target_host: self.target_host.clone(),
            target_port: self.target_port,
            metrics,
            connected: Arc::new(OnceLock::new()),
        }
    }
}

pub(super) struct ConnectedPacketForwarder {
    pub(super) socket: UdpSocket,
    pub(super) target: SocketAddr,
}

impl ConnectedPacketForwarder {
    pub(super) async fn connect(host: &str, port: u16) -> std::io::Result<Self> {
        match Self::connect_from_bind_addr(FORWARDER_IPV4_BIND_ADDR, host, port).await {
            Ok(forwarder) => Ok(forwarder),
            Err(ipv4_error) => {
                debug!(
                    %host,
                    port,
                    error = %ipv4_error,
                    "Could not connect to statsd forward target with IPv4 UDP socket."
                );
                Self::connect_from_bind_addr(FORWARDER_IPV6_BIND_ADDR, host, port)
                    .await
                    .map_err(|ipv6_error| {
                        std::io::Error::new(
                            ipv6_error.kind(),
                            format!(
                                "could not connect to statsd forward target with IPv4 or IPv6 UDP socket: \
                                 IPv4 error: {ipv4_error}; IPv6 error: {ipv6_error}"
                            ),
                        )
                    })
            }
        }
    }

    async fn connect_from_bind_addr(bind_addr: &str, host: &str, port: u16) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(bind_addr).await?;
        socket.connect((host, port)).await?;
        timeout(FORWARDER_SOCKET_READY_TIMEOUT, socket.writable())
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out waiting for forwarder socket")
            })??;

        let target = socket.peer_addr()?;
        Ok(Self { socket, target })
    }

    pub(super) async fn run(self, mut packets_rx: mpsc::Receiver<ForwardPacket>, metrics: Metrics) {
        while let Some(packet) = packets_rx.recv().await {
            match self.socket.send(&packet.payload).await {
                Ok(bytes_sent) => {
                    metrics.packets_forwarded().increment(1);
                    metrics.bytes_forwarded().increment(bytes_sent as u64);
                }
                Err(e) => {
                    metrics.packet_forwarding_errors().increment(1);
                    debug!(target = %self.target, error = %e, "Failed to forward DogStatsD packet.");
                }
            }
        }
    }
}

pub(super) struct ForwardPacket {
    payload: Bytes,
}

impl ForwardPacket {
    fn from_payload(payload: Bytes) -> Self {
        Self { payload }
    }
}

#[derive(Clone)]
pub(super) struct PacketForwarder {
    target_host: MetaString,
    target_port: u16,
    metrics: Metrics,
    pub(super) connected: Arc<OnceLock<mpsc::Sender<ForwardPacket>>>,
}

impl PacketForwarder {
    pub(super) fn spawn_connect(&self) {
        let forwarder = self.clone();
        spawn_traced_named("dogstatsd-packet-forwarder-setup", async move {
            forwarder.connect().await;
        });
    }

    async fn connect(&self) {
        let host = &self.target_host;
        let port = self.target_port;
        match timeout(FORWARDER_CONNECT_TIMEOUT, ConnectedPacketForwarder::connect(host, port)).await {
            Err(e) => {
                warn!(%host, port, error = %e, "Timed out connecting to statsd forward target. Packet forwarding disabled.");
            }
            Ok(forwarder) => {
                let forwarder = match forwarder {
                    Ok(forwarder) => forwarder,
                    Err(e) => {
                        warn!(%host, port, error = %e, "Could not connect to statsd forward target. Packet forwarding disabled.");
                        return;
                    }
                };
                let target = forwarder.target;
                let (packets_tx, packets_rx) = mpsc::channel(FORWARDER_QUEUE_CAPACITY);
                spawn_traced_named(
                    "dogstatsd-packet-forwarder",
                    forwarder.run(packets_rx, self.metrics.clone()),
                );

                info!(%target, "DogStatsD packet forwarding enabled.");
                if self.connected.set(packets_tx).is_err() {
                    debug!("DogStatsD packet forwarding was already initialized.");
                }
            }
        }
    }

    pub(super) async fn forward(&self, payload: Bytes) {
        if payload.is_empty() {
            return;
        }

        if let Some(packets_tx) = self.connected.get() {
            let packet = ForwardPacket::from_payload(payload);
            if packets_tx.send(packet).await.is_err() {
                self.metrics.packet_forwarding_errors().increment(1);
                debug!("Failed to enqueue DogStatsD packet for forwarding: receiver dropped.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use bytes::Bytes;
    use saluki_core::components::ComponentContext;
    use saluki_io::net::ListenAddress;
    use stringtheory::MetaString;
    use tokio::{net::UdpSocket, time::timeout};

    use super::super::metrics::build_metrics;
    use super::{PacketForwarder, PacketForwarderTarget};

    fn build_forwarder(host: &str, port: u16) -> PacketForwarder {
        let context = ComponentContext::test_source("dogstatsd_forwarder_test");
        let listen_addr = ListenAddress::Udp("127.0.0.1:0".parse::<SocketAddr>().expect("valid listen addr"));
        let metrics = build_metrics(&listen_addr, &context, false);
        PacketForwarderTarget::new(MetaString::from(host), port).to_forwarder(metrics)
    }

    #[tokio::test]
    async fn connect_enables_forwarding_and_delivers_payload() {
        // Connecting to a live UDP target enables forwarding, and `forward` delivers the payload verbatim.
        let target = UdpSocket::bind("127.0.0.1:0").await.expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");

        let forwarder = build_forwarder(&target_addr.ip().to_string(), target_addr.port());
        forwarder.connect().await;
        assert!(
            forwarder.connected.get().is_some(),
            "connecting to a live target must enable forwarding"
        );

        forwarder.forward(Bytes::from_static(b"metric.name:1|c")).await;

        let mut buf = [0u8; 64];
        let received = timeout(Duration::from_secs(5), target.recv(&mut buf))
            .await
            .expect("forwarded payload should arrive before the timeout")
            .expect("recv should succeed");
        assert_eq!(&buf[..received], b"metric.name:1|c");
    }

    #[tokio::test]
    async fn connect_is_idempotent() {
        // A second connect must not replace the already-established sender (the `OnceLock::set` is a no-op once set).
        let target = UdpSocket::bind("127.0.0.1:0").await.expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");
        let forwarder = build_forwarder(&target_addr.ip().to_string(), target_addr.port());

        forwarder.connect().await;
        let first = forwarder
            .connected
            .get()
            .expect("first connect should enable forwarding")
            .clone();

        forwarder.connect().await;
        let second = forwarder
            .connected
            .get()
            .expect("forwarding should stay enabled after a second connect");

        assert!(
            second.same_channel(&first),
            "a second connect must keep the original sender, not replace it"
        );
    }

    #[tokio::test]
    async fn connect_failure_leaves_forwarding_disabled() {
        // When the target can't be connected (here, an unresolvable host), forwarding stays disabled: `connected` is
        // never set, and `forward` becomes a safe no-op rather than panicking.
        let forwarder = build_forwarder("invalid host", 8125);
        forwarder.connect().await;
        assert!(
            forwarder.connected.get().is_none(),
            "a failed connect must leave forwarding disabled"
        );

        // Must not panic even though forwarding is disabled.
        forwarder.forward(Bytes::from_static(b"metric.name:1|c")).await;
    }
}
