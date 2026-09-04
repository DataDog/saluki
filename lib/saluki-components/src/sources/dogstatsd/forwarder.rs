use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    sync::{Arc, OnceLock},
    time::Duration,
};

use bytes::Bytes;
use saluki_core::runtime::{self, ShutdownStrategy};
use stringtheory::MetaString;
use tokio::{net::UdpSocket, sync::mpsc, time::timeout};
use tracing::{debug, info, warn};

use super::metrics::Metrics;

const FORWARDER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IPV4_ANY_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
const IPV6_ANY_ADDR: SocketAddr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0));
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

#[derive(Clone)]
pub(super) struct PacketForwarder {
    target_host: MetaString,
    target_port: u16,
    metrics: Metrics,
    pub(super) connected: Arc<OnceLock<mpsc::Sender<Bytes>>>,
}

impl PacketForwarder {
    #[cfg(test)]
    pub(super) fn for_tests(channel_size: usize) -> (Self, mpsc::Receiver<Bytes>) {
        let (packets_tx, packets_rx) = mpsc::channel(channel_size);

        let connected = Arc::new(OnceLock::from(packets_tx));

        (
            Self {
                target_host: MetaString::empty(),
                target_port: 0,
                metrics: Metrics::for_tests(),
                connected,
            },
            packets_rx,
        )
    }

    /// Starts the packet forwarder.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a supervision tree.
    pub(super) fn spawn(&self) {
        let forwarder = self.clone();

        // Spawn the initial connection phase and set the shutdown strategy to brutal.
        //
        // This ensures the supervisor doesn't wait during shutdown if the connect happens to be timing out.
        runtime::worker("packet_forwarder_connect", async move {
            forwarder.connect().await;
        })
        .with_shutdown_strategy(ShutdownStrategy::Brutal)
        .spawn();
    }

    async fn connect(self) {
        let Self {
            target_host: host,
            target_port: port,
            metrics,
            connected,
        } = self;

        let (socket, target) = match timeout(FORWARDER_CONNECT_TIMEOUT, connect_to_target(&host, port)).await {
            Err(e) => {
                warn!(%host, port, error = %e, "Timed out connecting to statsd forward target. Packet forwarding disabled.");
                return;
            }
            Ok(Err(e)) => {
                warn!(%host, port, error = %e, "Failed to connect to statsd forward target. Packet forwarding disabled.");
                return;
            }
            Ok(Ok((socket, target))) => (socket, target),
        };

        // Spawn our actual forwarder worker here.
        //
        // We'll fallibly try to install our packet channel, which signals to the source that the packet forwarder is
        // available and ready to forward packets. If another invocation has beat us to this point, we just quietly
        // return.
        runtime::worker("packet_forwarder", async move {
            let (packets_tx, mut packets_rx) = mpsc::channel(FORWARDER_QUEUE_CAPACITY);

            if connected.set(packets_tx).is_err() {
                debug!("DogStatsD packet forwarding was already initialized.");
                return;
            }

            info!(%target, "DogStatsD packet forwarding enabled.");

            // Release our own reference to the published channel. Since we exit our forwarding loop when the packet
            // channel is finished (empty + no senders), we don't want to inadvertantly hold on the channel.
            drop(connected);

            while let Some(packet) = packets_rx.recv().await {
                match socket.send(&packet).await {
                    Ok(bytes_sent) => {
                        metrics.packets_forwarded().increment(1);
                        metrics.bytes_forwarded().increment(bytes_sent as u64);
                    }
                    Err(e) => {
                        metrics.packet_forwarding_errors().increment(1);
                        debug!(%target, error = %e, "Failed to forward DogStatsD packet.");
                    }
                }
            }
        })
        .spawn();
    }

    pub(super) async fn forward(&self, packet: Bytes) {
        if packet.is_empty() {
            return;
        }

        if let Some(packets_tx) = self.connected.get() {
            if packets_tx.send(packet).await.is_err() {
                self.metrics.packet_forwarding_errors().increment(1);
                debug!("Failed to enqueue DogStatsD packet for forwarding: receiver dropped.");
            }
        }
    }
}

async fn connect_to_target(host: &str, port: u16) -> io::Result<(UdpSocket, SocketAddr)> {
    match connect_from_bind_addr(IPV4_ANY_ADDR, host, port).await {
        Ok((socket, target)) => Ok((socket, target)),
        Err(e) => {
            debug!(%host, port, error = %e, "Could not connect to statsd forward target with IPv4 UDP socket.");

            // Try falling back to going at this from the perspective of it being an IPv6 address.
            match connect_from_bind_addr(IPV6_ANY_ADDR, host, port).await {
                Ok((socket, target)) => Ok((socket, target)),
                Err(e2) => Err(io::Error::new(
                    e2.kind(),
                    format!(
                        "could not connect to statsd forward target with IPv4 or IPv6 UDP socket: \
                             IPv4 error: {e}; IPv6 error: {e2}"
                    ),
                )),
            }
        }
    }
}

async fn connect_from_bind_addr(bind_addr: SocketAddr, host: &str, port: u16) -> io::Result<(UdpSocket, SocketAddr)> {
    let socket = UdpSocket::bind(bind_addr).await?;
    socket.connect((host, port)).await?;

    let target = socket.peer_addr()?;
    Ok((socket, target))
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::{net::SocketAddr, time::Duration};

    use bytes::Bytes;
    use saluki_core::components::{test_util::TestComponentSupervisor, ComponentContext};
    use saluki_core::runtime::Supervisor;
    use saluki_io::net::ListenAddress;
    use saluki_metrics::test::TestRecorder;
    use tokio::{net::UdpSocket, time::timeout};

    use super::super::metrics::build_metrics;
    use super::{PacketForwarder, PacketForwarderTarget, FORWARDER_QUEUE_CAPACITY};
    use crate::sources::dogstatsd::forwarder::IPV4_ANY_ADDR;

    /// Waits for `forwarder` to publish its packet channel, which the forwarding loop does once it starts running.
    ///
    /// Spawning only queues the loop for the supervisor, so forwarding becomes available a moment after `connect`
    /// returns rather than synchronously with it.
    async fn wait_for_forwarding(forwarder: &PacketForwarder) -> tokio::sync::mpsc::Sender<Bytes> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(packets_tx) = forwarder.connected.get() {
                return packets_tx.clone();
            }

            assert!(tokio::time::Instant::now() < deadline, "forwarding was never enabled");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn is_ipv6_unavailable_error(error: &io::Error) -> bool {
        const LINUX_EAFNOSUPPORT: i32 = 97;
        const MACOS_EAFNOSUPPORT: i32 = 47;

        matches!(
            error.kind(),
            io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
        ) || matches!(error.raw_os_error(), Some(LINUX_EAFNOSUPPORT | MACOS_EAFNOSUPPORT))
    }

    /// How long the harness waits for another forwarded packet before deciding the flow has stopped.
    const RECEIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(300);

    fn build_forwarder(target_addr: SocketAddr) -> PacketForwarder {
        let context = ComponentContext::test_source("dogstatsd_forwarder_test");
        let listen_addr = ListenAddress::Udp(IPV4_ANY_ADDR);
        let metrics = build_metrics(&listen_addr, &context, false);
        PacketForwarderTarget::new(target_addr.ip().to_string().into(), target_addr.port()).to_forwarder(metrics)
    }

    #[tokio::test]
    async fn connect_enables_forwarding_and_delivers_payload_ipv4() {
        // Connecting to a live UDP target enables forwarding, and `forward` delivers the payload verbatim.
        let target = UdpSocket::bind("127.0.0.1:0").await.expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");

        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let forwarder = build_forwarder(target_addr);

        // The forwarder spawns on the ambient supervisor, so it has to run under the component's own.
        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let forwarder_spawn = forwarder.clone();
        supervisor.scope(forwarder_spawn.connect()).await;

        wait_for_forwarding(&forwarder).await;

        forwarder.forward(Bytes::from_static(b"metric.name:1|c")).await;

        let mut buf = [0u8; 64];
        let received = timeout(Duration::from_secs(5), target.recv(&mut buf))
            .await
            .expect("forwarded payload should arrive before the timeout")
            .expect("recv should succeed");
        let payload = &buf[..received];
        assert_eq!(payload, b"metric.name:1|c");

        assert_eq!(
            recorder.counter((
                "component_packets_forwarded_total",
                &[
                    ("component_id", "dogstatsd_forwarder_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                    ("state", "ok"),
                ]
            )),
            Some(1)
        );
        assert_eq!(
            recorder.counter((
                "component_bytes_forwarded_total",
                &[
                    ("component_id", "dogstatsd_forwarder_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                ]
            )),
            Some(payload.len() as u64)
        );

        // The forwarding loop is a supervised child, so it goes away with the component rather than outliving it.
        supervisor.wait_for_children(1).await;

        // The loop stops on its senders closing, not on the shutdown signal, so release our clone before shutting
        // down. Holding it here would leave the loop waiting until the shutdown budget elapsed.
        drop(forwarder);
        assert!(supervisor.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn connect_enables_forwarding_and_delivers_payload_ipv6() {
        // Connecting to a live UDP target enables forwarding, and `forward` delivers the payload verbatim... but IPv6 edition.
        let target = match UdpSocket::bind("[::1]:0").await {
            Ok(target) => target,
            Err(e) if is_ipv6_unavailable_error(&e) => return,
            Err(e) => panic!("target should bind: {e}"),
        };
        let target_addr = target.local_addr().expect("target should have an address");

        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let forwarder = build_forwarder(target_addr);

        // The forwarder spawns on the ambient supervisor, so it has to run under the component's own.
        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let forwarder_spawn = forwarder.clone();
        supervisor.scope(forwarder_spawn.connect()).await;

        wait_for_forwarding(&forwarder).await;

        forwarder.forward(Bytes::from_static(b"metric.name:1|c")).await;

        let mut buf = [0u8; 64];
        let received = timeout(Duration::from_secs(5), target.recv(&mut buf))
            .await
            .expect("forwarded payload should arrive before the timeout")
            .expect("recv should succeed");
        let payload = &buf[..received];
        assert_eq!(payload, b"metric.name:1|c");

        assert_eq!(
            recorder.counter((
                "component_packets_forwarded_total",
                &[
                    ("component_id", "dogstatsd_forwarder_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                    ("state", "ok"),
                ]
            )),
            Some(1)
        );
        assert_eq!(
            recorder.counter((
                "component_bytes_forwarded_total",
                &[
                    ("component_id", "dogstatsd_forwarder_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                ]
            )),
            Some(payload.len() as u64)
        );

        // The forwarding loop is a supervised child, so it goes away with the component rather than outliving it.
        supervisor.wait_for_children(1).await;

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
        let forwarder = build_forwarder(target_addr);

        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let forwarder_spawn = forwarder.clone();
        supervisor.scope(forwarder_spawn.connect()).await;

        let first = wait_for_forwarding(&forwarder).await;

        let forwarder_spawn = forwarder.clone();
        supervisor.scope(forwarder_spawn.connect()).await;

        // No wait is needed for the second loop here: `OnceLock::set` can never replace an existing value, so whether
        // the second loop has run yet or not, what `connected` holds is the first sender.
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
        // When the target can't be connected (here, trying to use `0.0.0.0:0` as a destination address), forwarding
        // stays disabled: `connected` is never set, and `forward` becomes a safe no-op rather than panicking.
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let forwarder = build_forwarder(IPV4_ANY_ADDR);

        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let forwarder_spawn = forwarder.clone();
        supervisor.scope(forwarder_spawn.connect()).await;

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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn failed_send_increments_error_count() {
        use std::time::Instant;

        use tokio::task::yield_now;
        use tokio::time::sleep;

        // When we fail to send a packet to the target, we should properly track that in our metrics.
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        // We bind our own UDP socket and then immediately drop it right before the actual forward calls
        // to simulate a routable address that is, in fact, not listening on the given port/
        let target = UdpSocket::bind("127.0.0.1:0").await.expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");
        let forwarder = build_forwarder(target_addr);

        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let forwarder_spawn = forwarder.clone();
        supervisor.scope(forwarder_spawn.connect()).await;

        yield_now().await;

        // Wait until the packet forwarder connects and sets up the underlying channels:
        let mut connected = false;
        let connected_deadline = Instant::now() + Duration::from_secs(1);
        while connected_deadline > Instant::now() {
            if forwarder.connected.get().is_some() {
                connected = true;
                break;
            }

            sleep(Duration::from_millis(100)).await;
        }

        assert!(
            connected,
            "packet forwarder failed to connect to target within 1 second"
        );

        // Drop our UDP socket so that sends fail, and then try and forward.
        //
        // We only observe failure every _other_ send: it takes one attempt to send to actually get back a response from
        // the networking stack that the address/port is unreachable, so send #1 "succeeds" (UDP is
        // unreliable/connectionless, remember?) and then send #2 basically responds with the error generated
        // asynchronously by send #1.
        //
        // This goes on and on, such that we'd see two failures for four actual sends, three failures for six actual
        // sends, etc.
        drop(target);
        forwarder.forward(Bytes::from_static(b"metric.name:1|c")).await;
        forwarder.forward(Bytes::from_static(b"metric.name:1|c")).await;

        yield_now().await;

        assert_eq!(
            recorder.counter((
                "component_packets_forwarded_total",
                &[
                    ("component_id", "dogstatsd_forwarder_test"),
                    ("component_type", "source"),
                    ("listener_type", "udp"),
                    ("state", "error"),
                ],
            )),
            Some(1)
        );
    }

    #[tokio::test]
    async fn queued_packets_are_forwarded_during_shutdown() {
        // Packets already queued when shutdown begins must still be sent. A forwarding loop that was aborted at
        // shutdown would be dropped at its current await point and take the whole queue with it -- measured at 64 of
        // 1024 delivered.
        let target = UdpSocket::bind("127.0.0.1:0").await.expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");
        let forwarder = build_forwarder(target_addr);

        let supervisor = TestComponentSupervisor::start("dogstatsd").await;
        let forwarder_spawn = forwarder.clone();
        supervisor.scope(forwarder_spawn.connect()).await;

        // Fill the queue via the sender directly. Going through `forward` would await `send`, which yields to the
        // loop on every call and paces it, so no backlog would ever build up.
        let packets_tx = wait_for_forwarding(&forwarder).await;

        let mut queued = 0;
        for i in 0..FORWARDER_QUEUE_CAPACITY {
            let packet = Bytes::from(format!("queued.metric.{i}:1|c"));
            if packets_tx.try_send(packet).is_err() {
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
        // The forwarding loop publishes the sender that enables forwarding, so a loop the supervisor never starts
        // leaves forwarding disabled. Publishing it from `connect` instead would queue packets into a channel nothing
        // drains -- and, once the queue filled, block the listener that was forwarding them.
        let target = UdpSocket::bind("127.0.0.1:0").await.expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");
        let forwarder = build_forwarder(target_addr);

        // A supervisor that never ran: spawning through it is accepted, but nothing is ever started.
        let dead_supervisor = Supervisor::new("dogstatsd")
            .expect("supervisor name should be valid")
            .handle();

        let forwarder_spawn = forwarder.clone();
        dead_supervisor.scope(forwarder_spawn.connect()).await;

        // Give a loop that was (wrongly) started a chance to publish before concluding it never ran.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            forwarder.connected.get().is_none(),
            "forwarding must stay disabled when the forwarding loop could not be started"
        );
    }
}
