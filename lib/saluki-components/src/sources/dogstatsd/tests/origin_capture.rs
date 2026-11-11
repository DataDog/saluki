use std::time::{Duration, Instant};

use saluki_io::net::{ConnectionAddress, ProcessCredentials, ProcessIdentity};
use saluki_metrics::test::TestRecorder;
use tokio::{task::yield_now, time::timeout};

use super::*;
use crate::sources::dogstatsd::{build_capture_record, StreamCaptureState, REPLAY_CREDENTIALS_GID};
use crate::sources::dogstatsd::{
    capture_named_pipe_frame, handle_frame, metrics::build_metrics, origin_detection_error_for_telemetry,
    resolve_process_origin, resolve_process_origin_if_needed, BufferDecodeContext, BufferDecodeMode, DecodeOutcome,
    DogStatsDDecoder, ProcessOrigin, ReceivedBuffer, TrafficCaptureReader,
};

#[test]
fn named_pipe_frames_are_written_to_an_active_capture() {
    let capture_directory = tempfile::tempdir().expect("temporary capture directory should be created");
    let capture = TrafficCapture::new(capture_directory.path().to_path_buf(), 1);
    let capture_path = capture
        .start_capture(None, Duration::from_secs(30), false)
        .expect("capture should start");

    capture_named_pipe_frame(&capture, b"captured.named_pipe.one:1|c");
    capture_named_pipe_frame(&capture, b"captured.named_pipe.two:1|c");
    capture.stop_capture();

    let deadline = Instant::now() + Duration::from_secs(2);
    while capture.is_ongoing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!capture.is_ongoing(), "capture should stop after its sender is dropped");

    let mut reader = TrafficCaptureReader::from_path(&capture_path).expect("capture should be readable");
    let first_record = reader
        .read_next()
        .expect("first capture record should decode")
        .expect("first named-pipe frame should be captured");
    let second_record = reader
        .read_next()
        .expect("second capture record should decode")
        .expect("second named-pipe frame should be captured");
    assert_eq!(first_record.payload, b"captured.named_pipe.one:1|c");
    assert_eq!(first_record.pid, 0);
    assert_eq!(second_record.payload, b"captured.named_pipe.two:1|c");
    assert_eq!(second_record.pid, 0);
    assert!(reader.read_next().expect("capture should terminate cleanly").is_none());
}

fn length_delimited(payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(payload);
    frame
}

/// Feeds `data` through the decoder the same way the connected stream reader does: each read fills whatever
/// space is left in the I/O buffer after collapsing any partial frame retained from the previous read.
async fn feed_connected_stream(
    decoder: &mut DogStatsDDecoder, context: &mut BufferDecodeContext<'_>, mut io_buffer: BytesBuffer, mut data: &[u8],
    peer_addr: &ConnectionAddress,
) -> BytesBuffer {
    use saluki_io::buf::CollapsibleReadWriteIoBuffer as _;

    while !data.is_empty() {
        io_buffer.collapse();
        let bytes_read = io_buffer.remaining_mut().min(data.len());
        io_buffer.put_slice(&data[..bytes_read]);
        data = &data[bytes_read..];

        let (received, returned_buffer) = ReceivedBuffer::with_return(io_buffer, bytes_read, peer_addr.clone(), None);
        let outcome = decoder.decode_buffer(context, received).await;
        assert_eq!(outcome, DecodeOutcome::Continue);
        io_buffer = returned_buffer.await.expect("decoder should return the I/O buffer");
    }
    io_buffer
}

async fn finish_capture(capture: &TrafficCapture, capture_path: &std::path::Path) -> Vec<(Vec<u8>, i32)> {
    capture.stop_capture();
    timeout(Duration::from_secs(2), async {
        while capture.is_ongoing() {
            yield_now().await;
        }
    })
    .await
    .expect("capture should stop");

    let mut reader = TrafficCaptureReader::from_path(capture_path).expect("capture should be readable");
    let mut records = Vec::new();
    while let Some(record) = reader.read_next().expect("capture record should decode") {
        records.push((record.payload, record.pid));
    }
    records
}

fn stream_test_payloads(count: usize) -> Vec<Vec<u8>> {
    // 57 lines of 26 bytes gives 1,486-byte frames, which never align with the 8,192-byte I/O buffer.
    (0..count).map(|_| b"capture.stream.metric:1|c\n".repeat(57)).collect()
}

#[tokio::test]
async fn uds_stream_capture_records_every_outer_frame_across_reads() {
    let listen_addr = ListenAddress::Unix("/tmp/dsd.socket".into());
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
        pid: 42,
        uid: 0,
        gid: 0,
    }));
    let capture_directory = tempfile::tempdir().expect("temporary capture directory should be created");
    let capture = TrafficCapture::new(capture_directory.path().to_path_buf(), 1);
    let metrics = build_metrics(&listen_addr, &test_component_context(), false);
    // The metrics output holds more buffers than the test dispatches, so nothing needs to drain it.
    let source = test_source_context();
    let decoder_context = DecoderContext {
        traffic_capture: capture.clone(),
        ..test_decoder_context(false)
    };
    let mut decoder = DogStatsDDecoder::new(source.context, decoder_context);
    let mut context = BufferDecodeContext::new(&listen_addr, false, &metrics, None, BufferDecodeMode::Connected);

    let capture_path = capture
        .start_capture(None, Duration::from_secs(30), false)
        .expect("capture should start");
    let payloads = stream_test_payloads(40);
    let stream = payloads
        .iter()
        .flat_map(|payload| length_delimited(payload))
        .collect::<Vec<_>>();
    let io_buffer = test_io_buffer(&[], TEST_BUFFER_SIZE);
    let io_buffer = feed_connected_stream(&mut decoder, &mut context, io_buffer, &stream, &peer_addr).await;
    assert_eq!(io_buffer.remaining(), 0);

    let records = finish_capture(&capture, &capture_path).await;
    assert_eq!(records.len(), payloads.len());
    for ((payload, pid), expected) in records.iter().zip(&payloads) {
        assert_eq!(payload, expected);
        assert_eq!(*pid, 42);
    }
}

#[tokio::test]
async fn uds_stream_capture_starting_mid_frame_captures_from_next_whole_frame_and_stops_cleanly() {
    let listen_addr = ListenAddress::Unix("/tmp/dsd.socket".into());
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable);
    let capture_directory = tempfile::tempdir().expect("temporary capture directory should be created");
    let capture = TrafficCapture::new(capture_directory.path().to_path_buf(), 1);
    let metrics = build_metrics(&listen_addr, &test_component_context(), false);
    // The metrics output holds more buffers than the test dispatches, so nothing needs to drain it.
    let source = test_source_context();
    let decoder_context = DecoderContext {
        traffic_capture: capture.clone(),
        ..test_decoder_context(false)
    };
    let mut decoder = DogStatsDDecoder::new(source.context, decoder_context);
    let mut context = BufferDecodeContext::new(&listen_addr, false, &metrics, None, BufferDecodeMode::Connected);

    let payloads = stream_test_payloads(20);
    let stream = payloads
        .iter()
        .flat_map(|payload| length_delimited(payload))
        .collect::<Vec<_>>();
    let frame_len = stream.len() / payloads.len();

    // Before capture starts, deliver the first frame plus half of the second.
    let split = frame_len + frame_len / 2;
    let io_buffer = test_io_buffer(&[], TEST_BUFFER_SIZE);
    let io_buffer = feed_connected_stream(&mut decoder, &mut context, io_buffer, &stream[..split], &peer_addr).await;
    assert_eq!(io_buffer.remaining(), frame_len / 2);

    let capture_path = capture
        .start_capture(None, Duration::from_secs(30), false)
        .expect("capture should start");
    let io_buffer = feed_connected_stream(&mut decoder, &mut context, io_buffer, &stream[split..], &peer_addr).await;
    let records = finish_capture(&capture, &capture_path).await;

    assert_eq!(records.len(), payloads.len() - 1);
    for ((payload, _), expected) in records.iter().zip(&payloads[1..]) {
        assert_eq!(payload, expected);
    }

    // Once capture has stopped, the connection keeps decoding.
    let more = length_delimited(&payloads[0]);
    let io_buffer = feed_connected_stream(&mut decoder, &mut context, io_buffer, &more, &peer_addr).await;
    assert_eq!(io_buffer.remaining(), 0);
}

#[test]
fn origin_telemetry_does_not_resolve_origin_when_origin_detection_is_disabled() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let listen_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
    let context = test_component_context();
    let metrics = build_metrics(&listen_addr, &context, true);
    let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
    let mut context_resolvers = test_context_resolvers();
    let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
        42,
        EntityId::from_local_data("ci-pid-container").expect("container entity"),
    );
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
        pid: 42,
        uid: 0,
        gid: 0,
    }));
    let process_origin = resolve_process_origin(Some(&capture_entity_resolver), &peer_addr);

    let event = handle_frame(
        b"test_metric:1|c",
        &codec,
        &mut context_resolvers,
        &metrics,
        false,
        process_origin.as_ref(),
        EnablePayloadsFilter::default(),
        &[],
        &MetaString::from_static("default-host"),
    )
    .expect("frame should parse");

    assert!(event.is_some());
    assert_eq!(
        recorder.counter(processed_metric_key("unixgram", Some("container_id://pid-container"))),
        None
    );
    assert_eq!(recorder.counter(processed_metric_key("unixgram", Some(""))), Some(1));
}

#[test]
fn origin_telemetry_records_resolved_origin_when_origin_detection_is_enabled() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let listen_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
    let context = test_component_context();
    let metrics = build_metrics(&listen_addr, &context, true);
    let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
    let mut context_resolvers = test_context_resolvers();
    let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
        42,
        EntityId::from_local_data("ci-pid-container").expect("container entity"),
    );
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
        pid: 42,
        uid: 0,
        gid: 0,
    }));
    let process_origin = resolve_process_origin(Some(&capture_entity_resolver), &peer_addr);

    let event = handle_frame(
        b"test_metric:1|c",
        &codec,
        &mut context_resolvers,
        &metrics,
        true,
        process_origin.as_ref(),
        EnablePayloadsFilter::default(),
        &[],
        &MetaString::from_static("default-host"),
    )
    .expect("frame should parse");

    assert!(event.is_some());
    assert_eq!(
        recorder.counter(processed_metric_key("unixgram", Some("container_id://pid-container"))),
        Some(1)
    );
    assert_eq!(recorder.counter(processed_metric_key("unixgram", Some(""))), Some(0));
}

#[test]
fn unsupported_platform_process_credentials_do_not_count_as_origin_detection_telemetry_errors() {
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(
        saluki_io::net::ProcessCredentialsError::UnsupportedPlatform,
    ));

    assert!(origin_detection_error_for_telemetry(true, 1, &peer_addr).is_none());
}

#[test]
fn invalid_process_credentials_count_as_origin_detection_telemetry_errors() {
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(
        saluki_io::net::ProcessCredentialsError::InvalidCredentials,
    ));

    assert!(matches!(
        origin_detection_error_for_telemetry(true, 1, &peer_addr),
        Some(saluki_io::net::ProcessCredentialsError::InvalidCredentials)
    ));
}

#[test]
fn zero_pid_process_credentials_count_as_origin_detection_telemetry_errors() {
    let peer_addr =
        ConnectionAddress::ProcessLike(ProcessIdentity::Error(saluki_io::net::ProcessCredentialsError::ZeroPid));

    assert!(matches!(
        origin_detection_error_for_telemetry(true, 1, &peer_addr),
        Some(saluki_io::net::ProcessCredentialsError::ZeroPid)
    ));
}

#[test]
fn origin_detection_telemetry_errors_require_origin_detection_and_received_bytes() {
    let peer_addr =
        ConnectionAddress::ProcessLike(ProcessIdentity::Error(saluki_io::net::ProcessCredentialsError::ZeroPid));

    assert!(origin_detection_error_for_telemetry(false, 1, &peer_addr).is_none());
    assert!(origin_detection_error_for_telemetry(true, 0, &peer_addr).is_none());
}

#[test]
fn resolve_process_origin_pins_live_entity() {
    let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
        42,
        EntityId::from_local_data("ci-pid-container").expect("container entity"),
    );
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
        pid: 42,
        uid: 0,
        gid: 0,
    }));

    assert_eq!(
        resolve_process_origin(Some(&capture_entity_resolver), &peer_addr),
        Some(ProcessOrigin::Pinned(Some(
            EntityId::from_local_data("ci-pid-container").expect("container entity")
        )))
    );
}

#[test]
fn resolve_process_origin_skips_live_lookup_when_unused() {
    let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
        42,
        EntityId::from_local_data("ci-pid-container").expect("container entity"),
    );
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
        pid: 42,
        uid: 0,
        gid: 0,
    }));
    let traffic_capture = TrafficCapture::new(PathBuf::new(), 1);

    assert_eq!(
        resolve_process_origin_if_needed(false, &traffic_capture, Some(&capture_entity_resolver), &peer_addr),
        None
    );
    assert_eq!(capture_entity_resolver.resolution_count(), 0);
}

#[tokio::test]
async fn resolve_process_origin_pins_live_entity_during_capture() {
    let capture_entity_resolver = CaptureTestEntityResolver::with_pid_mapping(
        42,
        EntityId::from_local_data("ci-pid-container").expect("container entity"),
    );
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
        pid: 42,
        uid: 0,
        gid: 0,
    }));
    let capture_dir = tempfile::tempdir().expect("capture directory should be created");
    let traffic_capture = TrafficCapture::new(capture_dir.path().to_path_buf(), 1);
    traffic_capture
        .start_capture(None, Duration::from_secs(30), false)
        .expect("capture should start");

    assert_eq!(
        resolve_process_origin_if_needed(false, &traffic_capture, Some(&capture_entity_resolver), &peer_addr),
        Some(ProcessOrigin::Pinned(Some(
            EntityId::from_local_data("ci-pid-container").expect("container entity")
        )))
    );
    assert_eq!(capture_entity_resolver.resolution_count(), 1);

    traffic_capture.stop_capture();
    timeout(Duration::from_secs(1), async {
        while traffic_capture.is_ongoing() {
            yield_now().await;
        }
    })
    .await
    .expect("capture should stop");
}

#[test]
fn build_capture_record_ignores_payload_local_data() {
    let record = build_capture_record(None, None, b"test.metric:1|c|c:ci-local-container\n");

    assert_eq!(record.container_id, None);
    assert!(record.ancillary.is_empty());
}

#[test]
fn stream_capture_state_preserves_last_pid_without_new_creds() {
    let mut stream_capture = StreamCaptureState::new();

    stream_capture.update_peer_metadata(&ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(
        ProcessCredentials {
            pid: 42,
            uid: 0,
            gid: 0,
        },
    )));
    stream_capture.update_peer_metadata(&ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable));

    assert_eq!(stream_capture.last_pid, Some(42));
}

#[test]
fn resolve_process_origin_preserves_live_pid_without_entity_resolver() {
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
        pid: 12345,
        uid: 1000,
        gid: 1000,
    }));

    assert_eq!(
        resolve_process_origin(None, &peer_addr),
        Some(ProcessOrigin::Unpinned(12345))
    );
}

#[test]
fn resolve_process_origin_unpacks_captured_pid_when_replay_gid_present() {
    let captured_pid: u32 = 99887766;
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Credentials(ProcessCredentials {
        pid: 12345,        // our PID (irrelevant for replay)
        uid: captured_pid, // captured PID packed by the sender
        gid: REPLAY_CREDENTIALS_GID,
    }));

    assert_eq!(
        resolve_process_origin(None, &peer_addr),
        Some(ProcessOrigin::Replay(captured_pid))
    );
}
