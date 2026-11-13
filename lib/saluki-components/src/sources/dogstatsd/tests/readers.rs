#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use saluki_core::accounting::MemoryLimiter;
use saluki_core::pooling::ObjectPool as _;
#[cfg(target_os = "linux")]
use saluki_env::workload::providers::TestWorkloadProvider;
#[cfg(target_os = "linux")]
use saluki_io::deser::codec::dogstatsd::ParsedPacket;
#[cfg(unix)]
use saluki_io::net::Stream;
#[cfg(unix)]
use tokio::task::yield_now;
use tokio::time::timeout;
#[cfg(unix)]
use tokio::{
    io::AsyncWriteExt as _,
    net::{UnixDatagram, UnixStream},
    sync::mpsc,
};

use super::*;
use crate::sources::dogstatsd::build_io_buffer_pool;
#[cfg(unix)]
use crate::sources::dogstatsd::metrics::build_metrics;
use crate::sources::dogstatsd::should_drop_oversized_named_pipe_frame;
#[cfg(target_os = "linux")]
use crate::sources::dogstatsd::{
    handle_metric_packet, CapturedTaggerHandle, DogStatsDOriginTagResolver, Listener, OriginEnrichmentConfiguration,
    ProcessOrigin,
};
#[cfg(unix)]
use crate::sources::dogstatsd::{
    receive_connected_stream, receive_connectionless_stream, received_payload, DatagramSocketContext,
};

#[cfg(target_os = "linux")]
fn test_datagram_socket_context(listen_addr: ListenAddress) -> Arc<DatagramSocketContext> {
    Arc::new(DatagramSocketContext {
        metrics: build_metrics(&listen_addr, &test_component_context(), false),
        listen_addr,
        eol_required: false,
        packet_forwarder: None,
    })
}

#[tokio::test]
async fn dogstatsd_io_buffer_pool_grows_on_demand_until_limit() {
    let min_buffers = 2;
    let max_buffers = 3;
    let (pool, shrinker) = build_io_buffer_pool(min_buffers, max_buffers, TEST_BUFFER_SIZE);

    let mut initial_buffers = Vec::with_capacity(min_buffers);
    for _ in 0..min_buffers {
        initial_buffers.push(
            timeout(Duration::from_secs(1), pool.acquire())
                .await
                .expect("initial buffer should be available"),
        );
    }
    let on_demand_buffer = timeout(Duration::from_secs(1), pool.acquire())
        .await
        .expect("pool should grow on demand before hitting the limit");

    let capped_acquire = timeout(Duration::from_millis(25), pool.acquire()).await;
    assert!(capped_acquire.is_err(), "pool should wait once it reaches the limit");

    drop(initial_buffers.pop().expect("initial buffer should still be held"));
    timeout(Duration::from_secs(1), pool.acquire())
        .await
        .expect("returned buffer should unblock acquisition");

    drop(on_demand_buffer);
    drop(shrinker);
}

#[cfg(unix)]
#[tokio::test]
async fn uds_datagram_reader_is_bounded_by_io_buffer_pool() {
    let temp_dir = tempfile::tempdir().expect("temp directory should be created");
    let socket_path = temp_dir.path().join("dogstatsd.socket");
    let receiver = UnixDatagram::bind(&socket_path).expect("receiver should bind");
    let sender = UnixDatagram::unbound().expect("sender should be created");
    let (pool, shrinker) = build_io_buffer_pool(2, 2, TEST_BUFFER_SIZE);
    let (packets_tx, mut packets_rx) = saluki_common::sync::work_queue::bounded(std::num::NonZeroUsize::MIN, 3);
    let mut packets_rx = packets_rx.pop().expect("one decoder queue");
    let listen_addr = ListenAddress::Unixgram(socket_path.clone());
    let socket_context = Arc::new(DatagramSocketContext {
        metrics: build_metrics(&listen_addr, &test_component_context(), false),
        listen_addr,
        eol_required: false,
        packet_forwarder: None,
    });
    let reader = tokio::spawn(receive_connectionless_stream(
        Stream::from(receiver),
        pool,
        MemoryLimiter::noop(),
        false,
        TrafficCapture::new(PathBuf::new(), 1),
        None,
        packets_tx,
        socket_context,
    ));
    let payloads: [&[u8]; 3] = [b"first", b"second", b"third"];

    for payload in payloads {
        sender
            .send_to(payload, &socket_path)
            .await
            .expect("payload should send");
    }

    timeout(Duration::from_secs(1), async {
        while packets_rx.len() < 2 {
            yield_now().await;
        }
    })
    .await
    .expect("reader should fill the two-buffer pool");
    assert_eq!(packets_rx.len(), 2);

    let first = packets_rx
        .recv()
        .await
        .expect("first packet should be queued")
        .result
        .expect("first receive should succeed");
    assert_eq!(received_payload(first.buffer(), first.bytes_read), payloads[0]);
    drop(first);

    timeout(Duration::from_secs(1), async {
        while packets_rx.len() < 2 {
            yield_now().await;
        }
    })
    .await
    .expect("returning a buffer should allow the third packet to be read");

    for expected in &payloads[1..] {
        let received = packets_rx
            .recv()
            .await
            .expect("packet should be queued")
            .result
            .expect("receive should succeed");
        assert_eq!(received_payload(received.buffer(), received.bytes_read), *expected);
    }

    drop(packets_rx);
    sender
        .send_to(b"shutdown", &socket_path)
        .await
        .expect("shutdown payload should send");
    timeout(Duration::from_secs(1), reader)
        .await
        .expect("reader should stop after observing the closed queue")
        .expect("reader task should not panic");
    drop(shrinker);
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
    let (pool, shrinker) = build_io_buffer_pool(1, 1, TEST_BUFFER_SIZE);
    let (packets_tx, mut packets_rx) = saluki_common::sync::work_queue::bounded(std::num::NonZeroUsize::MIN, 1);
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
    let received = timeout(Duration::from_secs(1), packets_rx.recv())
        .await
        .expect("packet should be received")
        .expect("reader should remain active")
        .result
        .expect("receive should succeed");
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
    let tags_resolver = TagsResolverBuilder::for_tests().build();
    let context_resolver = ContextResolverBuilder::for_tests()
        .with_tags_resolver(Some(tags_resolver.clone()))
        .build();
    let mut context_resolvers = ContextResolvers::manual_with_origin(
        context_resolver.clone(),
        context_resolver,
        tags_resolver,
        origin_resolver,
    );
    let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
    let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(b"test.metric:1|c") else {
        panic!("metric should parse");
    };
    let metric = handle_metric_packet(
        packet,
        &mut context_resolvers,
        received.process_origin.as_ref(),
        &[],
        &MetaString::from_static("default-host"),
    )
    .expect("metric context should resolve");

    assert!(metric.context().origin_tags().has_tag("container:original"));
    assert!(!metric.context().origin_tags().has_tag("container:reused"));

    drop(received);
    drop(packets_rx);
    sender
        .send_to(b"shutdown", &socket_path)
        .await
        .expect("shutdown payload should send");
    timeout(Duration::from_secs(1), reader)
        .await
        .expect("reader should stop after observing the closed queue")
        .expect("reader task should not panic");
    drop(shrinker);
}

#[cfg(unix)]
#[tokio::test]
async fn connection_oriented_reader_preserves_partial_frames() {
    let (mut sender, receiver) = UnixStream::pair().expect("stream pair should be created");
    let (pool, shrinker) = build_io_buffer_pool(1, 1, TEST_BUFFER_SIZE);
    let (packets_tx, mut packets_rx) = mpsc::channel(1);
    let reader = tokio::spawn(receive_connected_stream(
        Stream::from(receiver),
        pool,
        MemoryLimiter::noop(),
        false,
        TrafficCapture::new(PathBuf::new(), 1),
        None,
        packets_tx,
    ));

    sender.write_all(b"partial").await.expect("first payload should send");
    let first = timeout(Duration::from_secs(1), packets_rx.recv())
        .await
        .expect("first read should finish")
        .expect("reader should remain active")
        .expect("first read should succeed");
    assert_eq!(first.buffer().chunk(), b"partial");
    drop(first);

    sender.write_all(b"-frame").await.expect("second payload should send");
    let second = timeout(Duration::from_secs(1), packets_rx.recv())
        .await
        .expect("second read should finish")
        .expect("reader should remain active")
        .expect("second read should succeed");
    assert_eq!(second.bytes_read, b"-frame".len());
    assert_eq!(second.buffer().chunk(), b"partial-frame");
    drop(second);

    drop(packets_rx);
    sender
        .write_all(b"shutdown")
        .await
        .expect("shutdown payload should send");
    timeout(Duration::from_secs(1), reader)
        .await
        .expect("reader should stop after observing the closed queue")
        .expect("reader task should not panic");
    drop(shrinker);
}

#[cfg(unix)]
#[tokio::test]
async fn connection_oriented_reader_releases_drained_buffer_before_reacquiring() {
    let (mut sender, receiver) = UnixStream::pair().expect("stream pair should be created");
    let (pool, shrinker) = build_io_buffer_pool(1, 1, TEST_BUFFER_SIZE);
    let (packets_tx, mut packets_rx) = mpsc::channel(1);
    let reader = tokio::spawn(receive_connected_stream(
        Stream::from(receiver),
        pool,
        MemoryLimiter::noop(),
        false,
        TrafficCapture::new(PathBuf::new(), 1),
        None,
        packets_tx,
    ));

    sender.write_all(b"first").await.expect("first payload should send");
    let mut first = timeout(Duration::from_secs(1), packets_rx.recv())
        .await
        .expect("first read should finish")
        .expect("reader should remain active")
        .expect("first read should succeed");
    let bytes_read = first.bytes_read;
    first.buffer_mut().advance(bytes_read);
    drop(first);

    sender.write_all(b"second").await.expect("second payload should send");
    let mut second = timeout(Duration::from_secs(1), packets_rx.recv())
        .await
        .expect("reader should reacquire the released buffer")
        .expect("reader should remain active")
        .expect("second read should succeed");
    let bytes_read = second.bytes_read;
    assert_eq!(received_payload(second.buffer(), bytes_read), b"second");
    second.buffer_mut().advance(bytes_read);
    drop(second);

    drop(packets_rx);
    sender
        .write_all(b"shutdown")
        .await
        .expect("shutdown payload should send");
    timeout(Duration::from_secs(1), reader)
        .await
        .expect("reader should stop after observing the closed queue")
        .expect("reader task should not panic");
    drop(shrinker);
}

#[test]
fn drops_full_named_pipe_buffer_without_newline() {
    let named_pipe_stream = named_pipe_listen_address();
    let mut buffer = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(8));
    buffer.put_slice(b"12345678");

    assert!(should_drop_oversized_named_pipe_frame(&named_pipe_stream, &buffer));
}

#[test]
fn keeps_named_pipe_partial_frame_when_buffer_has_capacity() {
    let named_pipe_stream = named_pipe_listen_address();
    let mut buffer = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(9));
    buffer.put_slice(b"12345678");

    assert!(!should_drop_oversized_named_pipe_frame(&named_pipe_stream, &buffer));
}

#[test]
fn keeps_full_named_pipe_buffer_with_newline() {
    let named_pipe_stream = named_pipe_listen_address();
    let mut buffer = get_pooled_object_via_builder::<_, BytesBuffer>(|| FixedSizeVec::with_capacity(8));
    buffer.put_slice(b"1234567\n");

    assert!(!should_drop_oversized_named_pipe_frame(&named_pipe_stream, &buffer));
}
