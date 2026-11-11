#[cfg(unix)]
use std::{future::Future, io, sync::Arc, time::Duration};

#[cfg(unix)]
use saluki_common::sync::work_queue::{self, WorkQueueReceiver};
#[cfg(unix)]
use saluki_core::accounting::MemoryLimiter;
#[cfg(target_os = "linux")]
use saluki_env::workload::providers::TestWorkloadProvider;
#[cfg(unix)]
use saluki_io::net::Stream;
#[cfg(unix)]
use tokio::{
    io::AsyncWriteExt as _,
    net::{UnixDatagram, UnixStream},
    sync::mpsc,
    task::{yield_now, JoinHandle},
    time::timeout,
};

use super::*;
use crate::sources::dogstatsd::should_drop_oversized_named_pipe_frame;
#[cfg(unix)]
use crate::sources::dogstatsd::{
    build_io_buffer_pool, receive_connected_stream, receive_connectionless_stream, received_payload,
    DatagramSocketContext, QueuedDatagram,
};
#[cfg(target_os = "linux")]
use crate::sources::dogstatsd::{
    handle_metric_packet, CapturedTaggerHandle, DogStatsDOriginTagResolver, Listener, OriginEnrichmentConfiguration,
    ProcessOrigin,
};

#[cfg(unix)]
const RECV_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(unix)]
fn test_datagram_socket_context(listen_addr: ListenAddress) -> Arc<DatagramSocketContext> {
    Arc::new(DatagramSocketContext {
        metrics: build_metrics(&listen_addr, &test_component_context(), false),
        listen_addr,
        eol_required: false,
        packet_forwarder: None,
    })
}

#[cfg(unix)]
async fn recv_datagram(packets_rx: &mut WorkQueueReceiver<QueuedDatagram>) -> ReceivedBuffer {
    timeout(RECV_TIMEOUT, packets_rx.recv())
        .await
        .expect("packet should be received")
        .expect("reader should remain active")
        .result
        .expect("receive should succeed")
}

#[cfg(unix)]
async fn recv_stream(packets_rx: &mut mpsc::Receiver<io::Result<ReceivedBuffer>>) -> ReceivedBuffer {
    timeout(RECV_TIMEOUT, packets_rx.recv())
        .await
        .expect("read should finish")
        .expect("reader should remain active")
        .expect("read should succeed")
}

/// Closes the queue of a reader, wakes the reader with `wake`, and waits for it to stop.
#[cfg(unix)]
async fn stop_reader<Q, T>(packets_rx: Q, reader: JoinHandle<()>, wake: impl Future<Output = io::Result<T>>) {
    drop(packets_rx);
    wake.await.expect("shutdown payload should send");
    timeout(RECV_TIMEOUT, reader)
        .await
        .expect("reader should stop after observing the closed queue")
        .expect("reader task should not panic");
}

#[cfg(unix)]
#[tokio::test]
async fn uds_datagram_reader_is_bounded_by_io_buffer_pool() {
    let temp_dir = tempfile::tempdir().expect("temp directory should be created");
    let socket_path = temp_dir.path().join("dogstatsd.socket");
    let receiver = UnixDatagram::bind(&socket_path).expect("receiver should bind");
    let sender = UnixDatagram::unbound().expect("sender should be created");
    // The pool starts below its limit, so the reader has to grow it on demand to fill the queue.
    let (pool, _shrinker) = build_io_buffer_pool(1, 2, TEST_BUFFER_SIZE);
    let (packets_tx, mut packets_rx) = work_queue::bounded(std::num::NonZeroUsize::MIN, 3);
    let mut packets_rx = packets_rx.pop().expect("one decoder queue");
    let reader = tokio::spawn(receive_connectionless_stream(
        Stream::from(receiver),
        pool,
        MemoryLimiter::noop(),
        false,
        TrafficCapture::new(PathBuf::new(), 1),
        None,
        packets_tx,
        test_datagram_socket_context(ListenAddress::Unixgram(socket_path.clone())),
    ));
    let payloads: [&[u8]; 3] = [b"first", b"second", b"third"];

    for payload in payloads {
        sender
            .send_to(payload, &socket_path)
            .await
            .expect("payload should send");
    }

    timeout(RECV_TIMEOUT, async {
        while packets_rx.len() < 2 {
            yield_now().await;
        }
    })
    .await
    .expect("reader should fill the two-buffer pool");
    assert_eq!(packets_rx.len(), 2);

    let first = recv_datagram(&mut packets_rx).await;
    assert_eq!(received_payload(first.buffer(), first.bytes_read), payloads[0]);
    drop(first);

    timeout(RECV_TIMEOUT, async {
        while packets_rx.len() < 2 {
            yield_now().await;
        }
    })
    .await
    .expect("returning a buffer should allow the third packet to be read");

    for expected in &payloads[1..] {
        let received = recv_datagram(&mut packets_rx).await;
        assert_eq!(received_payload(received.buffer(), received.bytes_read), *expected);
    }

    stop_reader(packets_rx, reader, sender.send_to(b"shutdown", &socket_path)).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn uds_datagram_reader_pins_origin_before_decode() {
    let temp_dir = tempfile::tempdir().expect("temp directory should be created");
    let socket_path = temp_dir.path().join("dogstatsd.socket");
    let listen_addr = ListenAddress::Unixgram(socket_path.clone());
    let mut listener = Listener::from_listen_address(listen_addr.clone(), None)
        .await
        .expect("listener should bind");
    let stream = listener.accept().await.expect("listener should yield its socket");
    let sender = UnixDatagram::unbound().expect("sender should be created");
    let process_id = std::process::id();
    let original_entity = EntityId::from_local_data("ci-original-container").expect("container entity");
    let reused_entity = EntityId::from_local_data("ci-reused-container").expect("container entity");
    let capture_entity_resolver = Arc::new(CaptureTestEntityResolver::with_pid_mapping(
        process_id,
        original_entity.clone(),
    ));
    let (pool, _shrinker) = build_io_buffer_pool(1, 1, TEST_BUFFER_SIZE);
    let (packets_tx, mut packets_rx) = work_queue::bounded(std::num::NonZeroUsize::MIN, 1);
    let mut packets_rx = packets_rx.pop().expect("one decoder queue");
    let reader = tokio::spawn(receive_connectionless_stream(
        stream,
        pool,
        MemoryLimiter::noop(),
        true,
        TrafficCapture::new(PathBuf::new(), 1),
        Some(capture_entity_resolver.clone()),
        packets_tx,
        test_datagram_socket_context(listen_addr),
    ));

    sender
        .send_to(b"test.metric:1|c", &socket_path)
        .await
        .expect("payload should send");
    let received = recv_datagram(&mut packets_rx).await;
    assert_eq!(
        received.process_origin,
        Some(ProcessOrigin::Pinned(Some(original_entity.clone())))
    );
    assert_eq!(capture_entity_resolver.resolution_count(), 1);

    // Simulate the sender exiting and its PID being reused before a decoder worker reaches the queued packet.
    capture_entity_resolver.set_pid_mapping(process_id, reused_entity.clone());

    let mut workload_provider = TestWorkloadProvider::new();
    workload_provider.add_entity(original_entity, &["container:original"]);
    workload_provider.add_entity(reused_entity, &["container:reused"]);
    let origin_config = OriginEnrichmentConfiguration {
        enabled: true,
        ..OriginEnrichmentConfiguration::for_test()
    };
    let origin_resolver =
        DogStatsDOriginTagResolver::new(origin_config, Arc::new(workload_provider), CapturedTaggerHandle::new());
    let mut context_resolvers = ContextResolvers::for_tests(true, Some(origin_resolver));
    let metric = handle_metric_packet(
        parse_metric(b"test.metric:1|c"),
        &mut context_resolvers,
        received.process_origin.as_ref(),
        &[],
        &MetaString::from_static("default-host"),
    )
    .expect("metric context should resolve");

    assert!(metric.context().origin_tags().has_tag("container:original"));
    assert!(!metric.context().origin_tags().has_tag("container:reused"));

    drop(received);
    stop_reader(packets_rx, reader, sender.send_to(b"shutdown", &socket_path)).await;
}

/// Spawns a connected stream reader over one end of a socket pair with a single-buffer pool, returning the other end.
#[cfg(unix)]
fn spawn_connected_reader() -> (
    UnixStream,
    mpsc::Receiver<io::Result<ReceivedBuffer>>,
    JoinHandle<()>,
    impl Future<Output = ()>,
) {
    let (sender, receiver) = UnixStream::pair().expect("stream pair should be created");
    let (pool, shrinker) = build_io_buffer_pool(1, 1, TEST_BUFFER_SIZE);
    let (packets_tx, packets_rx) = mpsc::channel(1);
    let reader = tokio::spawn(receive_connected_stream(
        Stream::from(receiver),
        pool,
        MemoryLimiter::noop(),
        false,
        TrafficCapture::new(PathBuf::new(), 1),
        None,
        packets_tx,
    ));
    (sender, packets_rx, reader, shrinker)
}

#[cfg(unix)]
#[tokio::test]
async fn connection_oriented_reader_preserves_partial_frames() {
    let (mut sender, mut packets_rx, reader, _shrinker) = spawn_connected_reader();

    sender.write_all(b"partial").await.expect("first payload should send");
    let first = recv_stream(&mut packets_rx).await;
    assert_eq!(first.buffer().chunk(), b"partial");
    drop(first);

    sender.write_all(b"-frame").await.expect("second payload should send");
    let second = recv_stream(&mut packets_rx).await;
    assert_eq!(second.bytes_read, b"-frame".len());
    assert_eq!(second.buffer().chunk(), b"partial-frame");
    drop(second);

    stop_reader(packets_rx, reader, sender.write_all(b"shutdown")).await;
}

#[cfg(unix)]
#[tokio::test]
async fn connection_oriented_reader_releases_drained_buffer_before_reacquiring() {
    let (mut sender, mut packets_rx, reader, _shrinker) = spawn_connected_reader();

    sender.write_all(b"first").await.expect("first payload should send");
    let mut first = recv_stream(&mut packets_rx).await;
    let bytes_read = first.bytes_read;
    first.buffer_mut().advance(bytes_read);
    drop(first);

    // The pool holds a single buffer, so the reader can only read again once it gets the drained one back.
    sender.write_all(b"second").await.expect("second payload should send");
    let mut second = recv_stream(&mut packets_rx).await;
    let bytes_read = second.bytes_read;
    assert_eq!(received_payload(second.buffer(), bytes_read), b"second");
    second.buffer_mut().advance(bytes_read);
    drop(second);

    stop_reader(packets_rx, reader, sender.write_all(b"shutdown")).await;
}

#[test]
fn drops_oversized_named_pipe_frame_only_when_the_full_buffer_has_no_newline() {
    let cases = [
        (&b"12345678"[..], 8, true),
        (b"12345678", 9, false),
        (b"1234567\n", 8, false),
    ];

    for (payload, capacity, expected) in cases {
        let buffer = test_io_buffer(payload, capacity);
        assert_eq!(
            should_drop_oversized_named_pipe_frame(&named_pipe_listen_address(), &buffer),
            expected,
            "payload {:?} in a buffer of {capacity} bytes",
            String::from_utf8_lossy(payload)
        );
    }
}
