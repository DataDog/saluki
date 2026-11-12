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
    task::{yield_now, JoinHandle},
    time::timeout,
};

use super::*;
use crate::sources::dogstatsd::should_drop_oversized_named_pipe_frame;
#[cfg(unix)]
use crate::sources::dogstatsd::{
    build_io_buffer_pool, drive_connected_stream, receive_connectionless_stream, received_payload, QueuedDatagram,
    ReceivedDatagram,
};
#[cfg(target_os = "linux")]
use crate::sources::dogstatsd::{
    handle_metric_packet, replay::CapturedTaggerHandle, DogStatsDOriginTagResolver, Listener,
    OriginEnrichmentConfiguration, ProcessOrigin,
};

#[cfg(unix)]
const RECV_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(unix)]
async fn recv_datagram(packets_rx: &mut WorkQueueReceiver<QueuedDatagram>) -> ReceivedDatagram {
    timeout(RECV_TIMEOUT, packets_rx.recv())
        .await
        .expect("packet should be received")
        .expect("reader should remain active")
        .result
        .expect("receive should succeed")
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
    let listener_state = ListenerState {
        io_buffer_pool: pool,
        ..test_listener_state(ListenAddress::Unixgram(socket_path.clone()))
    };
    let reader = tokio::spawn(receive_connectionless_stream(
        Stream::from(receiver),
        MemoryLimiter::noop(),
        packets_tx,
        Arc::new(listener_state),
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
    assert_eq!(received_payload(&first.buffer, first.bytes_read), payloads[0]);
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
        assert_eq!(received_payload(&received.buffer, received.bytes_read), *expected);
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
    let listener_state = ListenerState {
        io_buffer_pool: pool,
        decoder_context: test_decoder_context(true),
        capture_entity_resolver: Some(capture_entity_resolver.clone()),
        ..test_listener_state(listen_addr)
    };
    let reader = tokio::spawn(receive_connectionless_stream(
        stream,
        MemoryLimiter::noop(),
        packets_tx,
        Arc::new(listener_state),
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
    let origin_resolver = DogStatsDOriginTagResolver::new(
        origin_config,
        Arc::new(workload_provider),
        CapturedTaggerHandle::default(),
    );
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

/// Drives a UDS stream connection over one end of a socket pair, with a single-buffer pool, until the other end closes.
///
/// Returns the other end of the pair, the handler, and the metrics output the handler dispatches to.
#[cfg(unix)]
fn spawn_connected_stream() -> (UnixStream, JoinHandle<()>, Outputs<EventsBuffer>) {
    let (sender, receiver) = UnixStream::pair().expect("stream pair should be created");
    let listener_state = test_listener_state(ListenAddress::Unix("/tmp/dsd.socket".into()));
    let source = test_source_context();
    let handler = tokio::spawn(async move {
        drive_connected_stream(
            Stream::from(receiver),
            source.context,
            MemoryLimiter::noop(),
            &listener_state,
        )
        .await
    });
    (sender, handler, source.outputs)
}

/// Waits for the handler to stop, and returns the names of every metric it dispatched, in order.
#[cfg(unix)]
async fn dispatched_metric_names(handler: JoinHandle<()>, mut outputs: Outputs<EventsBuffer>) -> Vec<String> {
    timeout(RECV_TIMEOUT, handler)
        .await
        .expect("handler should stop once the peer closes the stream")
        .expect("handler should not panic");

    let mut metrics_rx = outputs.take_named_output("metrics");
    let mut names = Vec::new();
    while let Some(buffer) = metrics_rx.try_next() {
        for event in buffer {
            let metric = event.try_into_metric().expect("should be a metric");
            names.push(metric.context().name().to_string());
        }
    }
    names
}

#[cfg(unix)]
#[tokio::test]
async fn connected_stream_reassembles_frames_split_across_reads() {
    let (mut sender, handler, outputs) = spawn_connected_stream();

    let frame = length_delimited(b"split.metric:1|c");
    let (head, tail) = frame.split_at(frame.len() / 2);
    sender.write_all(head).await.expect("first half should send");
    sender.flush().await.expect("first half should flush");
    // Let the handler read the first half on its own before the rest arrives.
    tokio::time::sleep(Duration::from_millis(50)).await;
    sender.write_all(tail).await.expect("second half should send");
    drop(sender);

    assert_eq!(dispatched_metric_names(handler, outputs).await, ["split.metric"]);
}

#[cfg(unix)]
#[tokio::test]
async fn connected_stream_returns_a_drained_buffer_before_reading_again() {
    // The pool holds a single buffer, so the handler can only read the second frame once it has given the first
    // read's buffer back.
    let (mut sender, handler, outputs) = spawn_connected_stream();

    sender
        .write_all(&length_delimited(b"first.metric:1|c"))
        .await
        .expect("first frame should send");
    tokio::time::sleep(Duration::from_millis(50)).await;
    sender
        .write_all(&length_delimited(b"second.metric:1|c"))
        .await
        .expect("second frame should send");
    drop(sender);

    assert_eq!(
        dispatched_metric_names(handler, outputs).await,
        ["first.metric", "second.metric"]
    );
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
