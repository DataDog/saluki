use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    sync::{Arc, OnceLock},
    time::Duration,
};

use saluki_core::runtime::{self, Scope, ShutdownStrategy};
use stringtheory::MetaString;
use thingbuf::{mpsc, recycling::WithCapacity};
use tokio::{net::UdpSocket, time::timeout};
use tracing::{debug, info, warn};

use super::metrics::ForwarderMetrics;

const FORWARDER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IPV4_ANY_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
const IPV6_ANY_ADDR: SocketAddr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0));
/// The smallest forwarding queue, in packets.
const MIN_FORWARDER_QUEUE_CAPACITY: usize = 64;

/// Returns how many packets the forwarding queue holds for a DogStatsD queue size of `queue_size`.
///
/// Any slot can grow to the size of a receive buffer, so the queue holds only a quarter of `queue_size` packets to keep
/// the worst case small.
pub(super) fn forwarder_queue_capacity(queue_size: usize) -> usize {
    (queue_size / 4).max(MIN_FORWARDER_QUEUE_CAPACITY)
}

/// Sends packets to the forwarding loop, through a queue of reusable packet buffers.
pub(super) type PacketSender = mpsc::Sender<Vec<u8>, WithCapacity>;

/// Receives packets in the forwarding loop.
pub(super) type PacketReceiver = mpsc::Receiver<Vec<u8>, WithCapacity>;

/// Creates the packet queue.
///
/// Each slot holds one packet. A slot's buffer grows to the largest packet it has carried and is reused from then on,
/// so steady-state forwarding doesn't allocate. A buffer that grows past `packet_capacity` shrinks back to it when the
/// slot is next used.
fn packet_queue(queue_capacity: usize, packet_capacity: usize) -> (PacketSender, PacketReceiver) {
    mpsc::with_recycle(queue_capacity, WithCapacity::new().with_max_capacity(packet_capacity))
}

#[derive(Clone)]
pub(super) struct PacketForwarderTarget {
    target_host: MetaString,
    target_port: u16,
    queue_capacity: usize,
    packet_capacity: usize,
}

impl PacketForwarderTarget {
    /// Creates a forwarding target.
    ///
    /// The forwarding queue holds `queue_capacity` packets. `packet_capacity` is the most a queue slot keeps allocated
    /// between packets. Larger packets are still forwarded.
    pub(super) fn new(
        target_host: MetaString, target_port: u16, queue_capacity: usize, packet_capacity: usize,
    ) -> Self {
        Self {
            target_host,
            target_port,
            queue_capacity,
            packet_capacity,
        }
    }

    pub(super) fn to_forwarder(&self, metrics: ForwarderMetrics) -> PacketForwarder {
        PacketForwarder {
            target_host: self.target_host.clone(),
            target_port: self.target_port,
            queue_capacity: self.queue_capacity,
            packet_capacity: self.packet_capacity,
            metrics,
            connected: Arc::new(OnceLock::new()),
        }
    }
}

#[derive(Clone)]
pub(super) struct PacketForwarder {
    target_host: MetaString,
    target_port: u16,
    queue_capacity: usize,
    packet_capacity: usize,
    metrics: ForwarderMetrics,
    pub(super) connected: Arc<OnceLock<PacketSender>>,
}

impl PacketForwarder {
    #[cfg(test)]
    pub(super) fn for_tests(channel_size: usize) -> (Self, PacketReceiver) {
        const PACKET_CAPACITY: usize = 8196;
        let (packets_tx, packets_rx) = packet_queue(channel_size, PACKET_CAPACITY);

        let connected = Arc::new(OnceLock::from(packets_tx));

        (
            Self {
                target_host: MetaString::empty(),
                target_port: 0,
                queue_capacity: channel_size,
                packet_capacity: PACKET_CAPACITY,
                metrics: ForwarderMetrics::for_tests(),
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

        // The connection phase starts the forwarding loop, but the loop is not a child of the connection phase. The
        // loop is a child of the process that starts the forwarder, beside the connection phase. The connection phase
        // stops brutally. If the loop is a child of the connection phase, the loop stops with it and drops the packets
        // that are still in its queue.
        let owner = runtime::scope::current();

        // Spawn the initial connection phase and set the shutdown strategy to brutal.
        //
        // This makes sure that shutdown does not wait for a slow connect attempt to time out.
        runtime::worker("packet_forwarder_connect", async move {
            forwarder.connect_into(owner).await;
        })
        .with_shutdown_strategy(ShutdownStrategy::Brutal)
        .spawn();
    }

    /// Connects, and if the connection succeeds, starts the forwarding loop as a child of the current process.
    #[cfg(test)]
    async fn connect(self) {
        self.connect_into(None).await;
    }

    /// Connects, and if the connection succeeds, starts the forwarding loop as a child of `owner`.
    ///
    /// If there is no owner, the forwarding loop is a child of the current process.
    async fn connect_into(self, owner: Option<Scope>) {
        let Self {
            target_host: host,
            target_port: port,
            queue_capacity,
            packet_capacity,
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
        let forwarding = runtime::worker("packet_forwarder", async move {
            let (packets_tx, packets_rx) = packet_queue(queue_capacity, packet_capacity);

            if connected.set(packets_tx).is_err() {
                debug!("DogStatsD packet forwarding was already initialized.");
                return;
            }

            info!(%target, "DogStatsD packet forwarding enabled.");

            // Release our own reference to the published channel. Since we exit our forwarding loop when the packet
            // channel is finished (empty + no senders), we don't want to inadvertantly hold on the channel.
            drop(connected);

            while let Some(packet) = packets_rx.recv_ref().await {
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
        });
        // The loop stops once every sender is gone, after sending what's still queued. The decoders hold senders and
        // drain without a timeout of their own, so neither does the loop: the component's shutdown timeout bounds both.
        let forwarding = forwarding.with_shutdown_timeout(Duration::MAX);
        match owner {
            Some(owner) => forwarding.spawn_in(&owner),
            None => forwarding.spawn(),
        };
    }

    /// Queues a copy of `packet` for forwarding, waiting for room in the queue if it's full.
    ///
    /// Does nothing if forwarding isn't enabled.
    pub(super) async fn forward(&self, packet: &[u8]) {
        if packet.is_empty() {
            return;
        }

        if let Some(packets_tx) = self.connected.get() {
            match packets_tx.send_ref().await {
                Ok(mut slot) => {
                    slot.clear();
                    // Exact, so a slot never keeps more than its largest packet and the memory bound stays a worst case.
                    slot.reserve_exact(packet.len());
                    slot.extend_from_slice(packet);
                }
                Err(_) => {
                    self.metrics.packet_forwarding_errors().increment(1);
                    debug!("Failed to enqueue DogStatsD packet for forwarding: receiver dropped.");
                }
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

    use saluki_core::components::ComponentContext;
    use saluki_core::runtime::{test_util::TestSupervisor, Supervisor};
    use saluki_metrics::test::TestRecorder;
    use tokio::{net::UdpSocket, time::timeout};

    use super::super::metrics::ForwarderMetrics;
    use super::{forwarder_queue_capacity, PacketForwarder, PacketForwarderTarget, PacketSender, IPV4_ANY_ADDR};

    /// The forwarding queue capacity for the default DogStatsD queue size.
    const QUEUE_CAPACITY: usize = 256;

    /// Waits for `forwarder` to publish its packet channel, which the forwarding loop does once it starts running.
    ///
    /// Spawning only queues the loop for the supervisor, so forwarding becomes available a moment after `connect`
    /// returns rather than synchronously with it.
    async fn wait_for_forwarding(forwarder: &PacketForwarder) -> PacketSender {
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
        PacketForwarderTarget::new(
            target_addr.ip().to_string().into(),
            target_addr.port(),
            QUEUE_CAPACITY,
            8196,
        )
        .to_forwarder(ForwarderMetrics::new(&context))
    }

    /// Builds a forwarder for `target_addr` and connects it under a fresh test supervisor.
    ///
    /// The forwarder spawns on the ambient supervisor, so it has to run under the component's own.
    async fn start_forwarder(target_addr: SocketAddr) -> (TestSupervisor, PacketForwarder) {
        let forwarder = build_forwarder(target_addr);
        let supervisor = TestSupervisor::start("dogstatsd").await;
        supervisor.scope(forwarder.clone().connect()).await;
        (supervisor, forwarder)
    }

    #[test]
    fn queue_capacity_is_a_quarter_of_the_queue_size_and_at_least_64() {
        for (queue_size, capacity) in [
            (0, 64),
            (255, 64),
            (256, 64),
            (260, 65),
            (1024, QUEUE_CAPACITY),
            (4096, 1024),
        ] {
            assert_eq!(
                forwarder_queue_capacity(queue_size),
                capacity,
                "queue size {queue_size}"
            );
        }
    }

    #[tokio::test]
    async fn connect_enables_forwarding_and_delivers_payload() {
        // Connecting to a live UDP target enables forwarding, and `forward` delivers the payload verbatim.
        for bind_addr in ["127.0.0.1:0", "[::1]:0"] {
            let target = match UdpSocket::bind(bind_addr).await {
                Ok(target) => target,
                Err(e) if bind_addr.starts_with('[') && is_ipv6_unavailable_error(&e) => continue,
                Err(e) => panic!("target should bind to {bind_addr}: {e}"),
            };
            let target_addr = target.local_addr().expect("target should have an address");

            let recorder = TestRecorder::default();
            let _recorder_guard = metrics::set_default_local_recorder(&recorder);
            let (supervisor, forwarder) = start_forwarder(target_addr).await;

            wait_for_forwarding(&forwarder).await;

            forwarder.forward(b"metric.name:1|c").await;

            let mut buf = [0u8; 64];
            let received = timeout(Duration::from_secs(5), target.recv(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("forwarded payload should arrive at {bind_addr} before the timeout"))
                .expect("recv should succeed");
            let payload = &buf[..received];
            assert_eq!(payload, b"metric.name:1|c", "target {bind_addr}");

            assert_eq!(
                recorder.counter((
                    "component_packets_forwarded_total",
                    &[
                        ("component_id", "dogstatsd_forwarder_test"),
                        ("component_type", "source"),
                        ("state", "ok"),
                    ]
                )),
                Some(1),
                "target {bind_addr}"
            );
            assert_eq!(
                recorder.counter((
                    "component_bytes_forwarded_total",
                    &[
                        ("component_id", "dogstatsd_forwarder_test"),
                        ("component_type", "source"),
                    ]
                )),
                Some(payload.len() as u64),
                "target {bind_addr}"
            );

            // The forwarding loop is a supervised child, so it goes away with the component rather than outliving it.
            supervisor.wait_for_children(1).await;

            // The loop stops on its senders closing, not on the shutdown signal, so release our clone before shutting
            // down. Holding it here would leave the loop waiting until it was halted at its shutdown timeout.
            drop(forwarder);
            assert!(supervisor.shutdown().await.is_ok(), "target {bind_addr}");
        }
    }

    #[tokio::test]
    async fn connect_is_idempotent() {
        // A second connect must not replace the already-established sender (the `OnceLock::set` is a no-op once set).
        let target = UdpSocket::bind("127.0.0.1:0").await.expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");
        let (supervisor, forwarder) = start_forwarder(target_addr).await;

        let first = wait_for_forwarding(&forwarder).await;

        supervisor.scope(forwarder.clone().connect()).await;

        // The second forwarding loop is only queued when `connect` returns, so it can't be observed here. A packet
        // arriving exactly once is what shows the first sender wasn't replaced.
        forwarder.forward(b"after.second.connect:1|c").await;
        let mut buf = [0u8; 64];
        let received = timeout(Duration::from_secs(5), target.recv(&mut buf))
            .await
            .expect("forwarded payload should arrive before the timeout")
            .expect("recv should succeed");
        assert_eq!(&buf[..received], b"after.second.connect:1|c");
        assert!(
            timeout(RECEIVE_IDLE_TIMEOUT, target.recv(&mut buf)).await.is_err(),
            "the packet must be forwarded only once"
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
        let (supervisor, forwarder) = start_forwarder(IPV4_ANY_ADDR).await;

        assert!(
            forwarder.connected.get().is_none(),
            "a failed connect must leave forwarding disabled"
        );

        // Nothing was spawned, so there is no forwarding loop left running either.
        assert_eq!(supervisor.active_children(), 0);

        // Must not panic even though forwarding is disabled.
        forwarder.forward(b"metric.name:1|c").await;
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
        let (_supervisor, forwarder) = start_forwarder(target_addr).await;

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
        forwarder.forward(b"metric.name:1|c").await;
        forwarder.forward(b"metric.name:1|c").await;

        yield_now().await;

        assert_eq!(
            recorder.counter((
                "component_packets_forwarded_total",
                &[
                    ("component_id", "dogstatsd_forwarder_test"),
                    ("component_type", "source"),
                    ("state", "error"),
                ],
            )),
            Some(1)
        );
    }

    #[tokio::test]
    async fn queued_packets_are_forwarded_during_shutdown() {
        // Packets already queued when shutdown begins must still be sent. A forwarding loop that was aborted at
        // shutdown would be dropped at its current await point and take the rest of the queue with it.
        //
        // Drain the target on its own thread, started before anything is forwarded. The forwarding loop can outrun a
        // reader that has to wait its turn on the test's runtime, and a full socket receive buffer would lose packets
        // in the harness rather than in the code under test.
        let target = std::net::UdpSocket::bind("127.0.0.1:0").expect("target should bind");
        let target_addr = target.local_addr().expect("target should have an address");
        let receiver = std::thread::spawn(move || {
            let mut received = 0;
            let mut buf = [0u8; 128];
            target
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout should be set");
            while target.recv(&mut buf).is_ok() {
                received += 1;
                target
                    .set_read_timeout(Some(RECEIVE_IDLE_TIMEOUT))
                    .expect("read timeout should be set");
            }
            received
        });

        let (supervisor, forwarder) = start_forwarder(target_addr).await;

        // Fill the queue via the sender directly. Going through `forward` would await `send_ref`, which yields to the
        // loop on every call and paces it, so no backlog would ever build up.
        let packets_tx = wait_for_forwarding(&forwarder).await;

        let mut queued = 0;
        for i in 0..QUEUE_CAPACITY {
            let packet = format!("queued.metric.{i}:1|c").into_bytes();
            if packets_tx.try_send(packet).is_err() {
                break;
            }
            queued += 1;
        }
        assert_eq!(queued, QUEUE_CAPACITY, "the queue should have accepted a full batch");

        // Close every sender, which is what lets the loop finish, and then shut down while it is still draining.
        drop(packets_tx);
        drop(forwarder);
        supervisor.shutdown().await.expect("supervisor should stop cleanly");

        let received = receiver.join().expect("receiver should not panic");
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

        dead_supervisor.scope(forwarder.clone().connect()).await;

        // Give a loop that was (wrongly) started a chance to publish before concluding it never ran.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            forwarder.connected.get().is_none(),
            "forwarding must stay disabled when the forwarding loop could not be started"
        );
    }
}
