use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use bytes::Bytes;
use saluki_core::components::ComponentSpawner;
use saluki_core::runtime::SpawnError;
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
    /// Starts connecting to the forward target in the background.
    ///
    /// Connecting is deliberately off the caller's path -- a listener shouldn't wait on a remote target before it can
    /// accept traffic -- so this returns as soon as the child is registered, not once the target is reachable.
    ///
    /// # Errors
    ///
    /// If the component's supervisor is no longer running, an error is returned.
    pub(super) async fn spawn_connect(&self, spawner: &ComponentSpawner) -> Result<(), SpawnError> {
        let forwarder = self.clone();
        let forwarder_spawner = spawner.clone();

        spawner
            .spawn_interruptible("packet_forwarder_connect", async move {
                forwarder.connect(&forwarder_spawner).await;
            })
            .await?;

        Ok(())
    }

    async fn connect(self, spawner: &ComponentSpawner) {
        let host = &self.target_host;
        let port = self.target_port;

        let forwarder = match timeout(FORWARDER_CONNECT_TIMEOUT, ConnectedPacketForwarder::connect(host, port)).await {
            Err(e) => {
                warn!(%host, port, error = %e, "Timed out connecting to statsd forward target. Packet forwarding disabled.");
                return;
            }
            Ok(Err(e)) => {
                warn!(%host, port, error = %e, "Failed to connect to statsd forward target. Packet forwarding disabled.");
                return;
            }
            Ok(Ok(forwarder)) => forwarder,
        };

        let target = forwarder.target;
        let (packets_tx, packets_rx) = mpsc::channel(FORWARDER_QUEUE_CAPACITY);

        // Spawn the packet forwarder in noninterruptible mode so that it attempts to finish forwarding any
        // in-flight packets prior to shutdown.
        let metrics = self.metrics.clone();
        if let Err(e) = spawner
            .spawn_noninterruptible("packet_forwarder", move |_shutdown| forwarder.run(packets_rx, metrics))
            .await
        {
            warn!(%target, error = %e, "Could not start statsd packet forwarder. Packet forwarding disabled.");
            return;
        }

        // Set our packet channel, which signals to the listeners that packets can now be forwarded.
        //
        // If we get raced somehow and we lose the `set` call here, the dropping of `packets_tx` will cascade
        // to our forwarder task causing it to exit, so cleanup is handled automatically.
        if self.connected.set(packets_tx).is_err() {
            debug!("DogStatsD packet forwarding was already initialized.");
        } else {
            info!(%target, "DogStatsD packet forwarding enabled.");
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
    use saluki_core::components::{test_util::TestComponentSupervisor, ComponentContext};
    use saluki_io::net::ListenAddress;
    use stringtheory::MetaString;
    use tokio::{net::UdpSocket, time::timeout};

    use super::super::metrics::build_metrics;
    use super::{ComponentSpawner, ForwardPacket, PacketForwarder, PacketForwarderTarget, FORWARDER_QUEUE_CAPACITY};

    /// How long the harness waits for another forwarded packet before deciding the flow has stopped.
    const RECEIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(300);

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

        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let spawner = supervisor.spawner();

        let forwarder = build_forwarder(&target_addr.ip().to_string(), target_addr.port());

        let forwarder_spawn = forwarder.clone();
        forwarder_spawn.connect(&spawner).await;

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

        // The forwarding loop is a supervised child, so it goes away with the component rather than outliving it.
        assert_eq!(supervisor.active_children(), 1);

        // The loop stops on its senders closing, not on the shutdown signal, so release our clone before shutting
        // down. Holding it here would leave the loop waiting until the shutdown budget elapsed.
        drop(forwarder);
        assert!(supervisor.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn connect_is_idempotent() {
        // A second connect must not replace the already-established sender (the `OnceLock::set` is a no-op once set).
        let target = UdpSocket::bind("127.0.0.1:0").await.expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");
        let forwarder = build_forwarder(&target_addr.ip().to_string(), target_addr.port());

        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let spawner = supervisor.spawner();

        let forwarder_spawn = forwarder.clone();
        forwarder_spawn.connect(&spawner).await;

        let first = forwarder
            .connected
            .get()
            .expect("first connect should enable forwarding")
            .clone();

        let forwarder_spawn = forwarder.clone();
        forwarder_spawn.connect(&spawner).await;

        let second = forwarder
            .connected
            .get()
            .expect("forwarding should stay enabled after a second connect");

        assert!(
            second.same_channel(&first),
            "a second connect must keep the original sender, not replace it"
        );

        // As above: drop every sender so the forwarding loop can finish.
        drop(first);
        drop(forwarder);
        assert!(supervisor.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn connect_failure_leaves_forwarding_disabled() {
        // When the target can't be connected (here, an unresolvable host), forwarding stays disabled: `connected` is
        // never set, and `forward` becomes a safe no-op rather than panicking.
        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let spawner = supervisor.spawner();

        let forwarder = build_forwarder("invalid host", 8125);

        let forwarder_spawn = forwarder.clone();
        forwarder_spawn.connect(&spawner).await;

        assert!(
            forwarder.connected.get().is_none(),
            "a failed connect must leave forwarding disabled"
        );

        // Nothing was spawned, so there is no forwarding loop left running either.
        assert_eq!(supervisor.active_children(), 0);

        // Must not panic even though forwarding is disabled.
        forwarder.forward(Bytes::from_static(b"metric.name:1|c")).await;

        assert!(supervisor.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn queued_packets_are_forwarded_during_shutdown() {
        // Packets already queued when shutdown begins must still be sent. An interruptible forwarding loop would be
        // dropped at its current await point and take the whole queue with it -- measured at 64 of 1024 delivered.
        let target = UdpSocket::bind("127.0.0.1:0").await.expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");

        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let spawner = supervisor.spawner();

        let forwarder = build_forwarder(&target_addr.ip().to_string(), target_addr.port());

        let forwarder_spawn = forwarder.clone();
        forwarder_spawn.connect(&spawner).await;

        // Fill the queue via the sender directly. Going through `forward` would await `send`, which yields to the
        // loop on every call and paces it, so no backlog would ever build up.
        let packets_tx = forwarder
            .connected
            .get()
            .expect("connecting to a live target must enable forwarding")
            .clone();

        let mut queued = 0;
        for i in 0..FORWARDER_QUEUE_CAPACITY {
            let payload = Bytes::from(format!("queued.metric.{i}:1|c"));
            if packets_tx.try_send(ForwardPacket::from_payload(payload)).is_err() {
                break;
            }
            queued += 1;
        }
        assert_eq!(
            queued, FORWARDER_QUEUE_CAPACITY,
            "the queue should have accepted a full batch"
        );

        // Drain the target concurrently: sending a full batch before reading any of it would overflow the socket's
        // receive buffer and lose packets in the harness rather than in the code under test.
        let receiver = tokio::spawn(async move {
            let mut received = 0;
            let mut buf = [0u8; 128];
            while let Ok(Ok(_)) = timeout(RECEIVE_IDLE_TIMEOUT, target.recv(&mut buf)).await {
                received += 1;
            }
            received
        });

        // Close every sender, which is what lets the loop finish, and then shut down while it is still draining.
        drop(packets_tx);
        drop(forwarder);
        supervisor.shutdown().await.expect("supervisor should stop cleanly");

        let received = receiver.await.expect("receiver should not panic");
        assert_eq!(
            received, queued,
            "every queued packet should have been forwarded before the loop stopped"
        );
    }

    #[tokio::test]
    async fn forwarding_is_disabled_when_the_supervisor_is_gone() {
        // `connect` publishes the sender that enables forwarding only after the forwarding loop is running. If the
        // spawn fails, publishing it anyway would queue packets into a channel nothing drains.
        let target = UdpSocket::bind("127.0.0.1:0").await.expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");

        // A spawner whose supervisor never ran: spawning through it always fails with `SupervisorGone`.
        let dead_spawner = ComponentSpawner::new(
            saluki_core::runtime::Supervisor::new("dogstatsd")
                .expect("supervisor name should be valid")
                .handle(),
            tokio::runtime::Handle::current(),
        );

        let forwarder = build_forwarder(&target_addr.ip().to_string(), target_addr.port());

        let forwarder_spawn = forwarder.clone();
        forwarder_spawn.connect(&dead_spawner).await;

        assert!(
            forwarder.connected.get().is_none(),
            "forwarding must stay disabled when the forwarding loop could not be started"
        );
    }
}
