use std::time::Duration;

use saluki_io::net::ProcessCredentialsError;
use saluki_metrics::test::TestRecorder;

use super::*;
use crate::sources::dogstatsd::{build_capture_record, REPLAY_CREDENTIALS_GID};
use crate::sources::dogstatsd::{
    capture_named_pipe_frame, origin_detection_error_for_telemetry, resolve_process_origin,
    resolve_process_origin_if_needed, BufferDecodeMode, DecodeOutcome, ProcessOrigin,
};

/// Starts a capture written to a fresh temporary directory, returned alongside it to keep it alive.
fn start_capture() -> (tempfile::TempDir, TrafficCapture, PathBuf) {
    let capture_directory = tempfile::tempdir().expect("temporary capture directory should be created");
    let capture = TrafficCapture::new(capture_directory.path().to_path_buf(), 1);
    let capture_path = capture
        .start_capture(None, Duration::from_secs(30), false)
        .expect("capture should start");
    (capture_directory, capture, capture_path)
}

#[test]
fn named_pipe_frames_are_written_to_an_active_capture() {
    let (_capture_directory, capture, capture_path) = start_capture();

    capture_named_pipe_frame(&capture, b"captured.named_pipe.one:1|c");
    capture_named_pipe_frame(&capture, b"captured.named_pipe.two:1|c");
    stop_capture(&capture);

    assert_eq!(
        read_capture_records(&capture_path),
        [
            (b"captured.named_pipe.one:1|c".to_vec(), 0),
            (b"captured.named_pipe.two:1|c".to_vec(), 0),
        ]
    );
}

fn length_delimited(payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(payload);
    frame
}

/// Feeds `data` through the decoder the same way the connected stream reader does: each read fills whatever
/// space is left in the I/O buffer after collapsing any partial frame retained from the previous read.
async fn feed_connected_stream(
    fixture: &mut DecoderFixture, context: &mut BufferDecodeContext<'_>, mut io_buffer: BytesBuffer, mut data: &[u8],
    peer_addr: &ConnectionAddress,
) -> BytesBuffer {
    use saluki_io::buf::CollapsibleReadWriteIoBuffer as _;

    while !data.is_empty() {
        io_buffer.collapse();
        let bytes_read = io_buffer.remaining_mut().min(data.len());
        io_buffer.put_slice(&data[..bytes_read]);
        data = &data[bytes_read..];

        let (outcome, returned_buffer) = fixture.decode_once(context, io_buffer, bytes_read, peer_addr).await;
        assert_eq!(outcome, DecodeOutcome::Continue);
        io_buffer = returned_buffer;
    }
    io_buffer
}

/// A decoder for a UDS stream listener that writes to `capture`.
fn capturing_stream_decoder(capture: &TrafficCapture) -> (TestListener, DecoderFixture) {
    let listener = TestListener::new(ListenAddress::Unix("/tmp/dsd.socket".into()));
    // The metrics output holds more buffers than the test dispatches, so nothing needs to drain it.
    let fixture = DecoderFixture::new(DecoderContext {
        traffic_capture: capture.clone(),
        ..test_decoder_context(false)
    });
    (listener, fixture)
}

fn stream_test_payloads(count: usize) -> Vec<Vec<u8>> {
    // 57 lines of 26 bytes gives 1,486-byte frames, which never align with the 8,192-byte I/O buffer.
    (0..count).map(|_| b"capture.stream.metric:1|c\n".repeat(57)).collect()
}

#[tokio::test]
async fn uds_stream_capture_records_every_outer_frame_across_reads() {
    let peer_addr = creds_peer(TEST_PID);
    let (_capture_directory, capture, capture_path) = start_capture();
    let (listener, mut fixture) = capturing_stream_decoder(&capture);
    let mut context = listener.decode_context(false, None, BufferDecodeMode::Connected);

    let payloads = stream_test_payloads(40);
    let stream = payloads
        .iter()
        .flat_map(|payload| length_delimited(payload))
        .collect::<Vec<_>>();
    let io_buffer = test_io_buffer(&[], TEST_BUFFER_SIZE);
    let io_buffer = feed_connected_stream(&mut fixture, &mut context, io_buffer, &stream, &peer_addr).await;
    assert_eq!(io_buffer.remaining(), 0);

    stop_capture(&capture);
    let records = read_capture_records(&capture_path);
    assert_eq!(records.len(), payloads.len());
    for ((payload, pid), expected) in records.iter().zip(&payloads) {
        assert_eq!(payload, expected);
        assert_eq!(*pid, TEST_PID);
    }
}

#[tokio::test]
async fn uds_stream_capture_starting_mid_frame_captures_from_next_whole_frame_and_stops_cleanly() {
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable);
    let capture_directory = tempfile::tempdir().expect("temporary capture directory should be created");
    let capture = TrafficCapture::new(capture_directory.path().to_path_buf(), 1);
    let (listener, mut fixture) = capturing_stream_decoder(&capture);
    let mut context = listener.decode_context(false, None, BufferDecodeMode::Connected);

    let payloads = stream_test_payloads(20);
    let stream = payloads
        .iter()
        .flat_map(|payload| length_delimited(payload))
        .collect::<Vec<_>>();
    let frame_len = stream.len() / payloads.len();

    // Before capture starts, deliver the first frame plus half of the second.
    let split = frame_len + frame_len / 2;
    let io_buffer = test_io_buffer(&[], TEST_BUFFER_SIZE);
    let io_buffer = feed_connected_stream(&mut fixture, &mut context, io_buffer, &stream[..split], &peer_addr).await;
    assert_eq!(io_buffer.remaining(), frame_len / 2);

    let capture_path = capture
        .start_capture(None, Duration::from_secs(30), false)
        .expect("capture should start");
    let io_buffer = feed_connected_stream(&mut fixture, &mut context, io_buffer, &stream[split..], &peer_addr).await;
    stop_capture(&capture);
    let records = read_capture_records(&capture_path);

    assert_eq!(records.len(), payloads.len() - 1);
    for ((payload, _), expected) in records.iter().zip(&payloads[1..]) {
        assert_eq!(payload, expected);
    }

    // Once capture has stopped, the connection keeps decoding.
    let more = length_delimited(&payloads[0]);
    let io_buffer = feed_connected_stream(&mut fixture, &mut context, io_buffer, &more, &peer_addr).await;
    assert_eq!(io_buffer.remaining(), 0);
}

#[tokio::test]
async fn uds_stream_capture_keeps_last_pid_for_reads_without_credentials() {
    let (_capture_directory, capture, capture_path) = start_capture();
    let (listener, mut fixture) = capturing_stream_decoder(&capture);
    let mut context = listener.decode_context(false, None, BufferDecodeMode::Connected);

    // The first read carries the sender's credentials, and the second carries none.
    let payloads = stream_test_payloads(2);
    let unavailable = ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable);
    let mut io_buffer = test_io_buffer(&[], TEST_BUFFER_SIZE);
    for (payload, peer_addr) in payloads.iter().zip([creds_peer(TEST_PID), unavailable]) {
        let frame = length_delimited(payload);
        io_buffer = feed_connected_stream(&mut fixture, &mut context, io_buffer, &frame, &peer_addr).await;
    }
    assert_eq!(io_buffer.remaining(), 0);

    stop_capture(&capture);
    let pids = read_capture_records(&capture_path)
        .into_iter()
        .map(|(_, pid)| pid)
        .collect::<Vec<_>>();
    assert_eq!(pids, [TEST_PID, TEST_PID]);
}

#[tokio::test]
async fn origin_telemetry_records_resolved_origin_only_when_origin_detection_is_enabled() {
    // Counts of processed metrics with the resolved origin, and with the empty origin.
    let cases = [(false, None, Some(1)), (true, Some(1), Some(0))];

    for (origin_detection_enabled, resolved_count, unresolved_count) in cases {
        let recorder = TestRecorder::default();
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);
        let listen_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
        let metrics = build_metrics(&listen_addr, &test_component_context(), true);
        let mut fixture = DecoderFixture::new(test_decoder_context(origin_detection_enabled));
        let process_origin = resolve_process_origin(Some(&pid_container_resolver()), &creds_peer(TEST_PID));

        let event = fixture
            .decoder
            .handle_frame(b"test_metric:1|c", &metrics, process_origin.as_ref())
            .expect("frame should parse");

        assert!(event.is_some(), "origin detection enabled: {origin_detection_enabled}");
        assert_eq!(
            recorder.counter(processed_metric_key("unixgram", Some("container_id://pid-container"))),
            resolved_count,
            "resolved origin, origin detection enabled: {origin_detection_enabled}"
        );
        assert_eq!(
            recorder.counter(processed_metric_key("unixgram", Some(""))),
            unresolved_count,
            "empty origin, origin detection enabled: {origin_detection_enabled}"
        );
    }
}

#[test]
fn origin_detection_telemetry_errors_require_origin_detection_and_received_bytes() {
    // Which credential errors count is up to `ProcessIdentity::is_telemetry_error`, so a single error suffices here.
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(ProcessCredentialsError::ZeroPid));
    let cases = [
        (true, 1, Some(ProcessCredentialsError::ZeroPid)),
        (false, 1, None),
        (true, 0, None),
    ];

    for (origin_detection_enabled, bytes_read, expected) in cases {
        let error = origin_detection_error_for_telemetry(origin_detection_enabled, bytes_read, &peer_addr);
        assert_eq!(
            error.map(|error| error.identifier()),
            expected.map(|error| error.identifier()),
            "origin detection enabled: {origin_detection_enabled}, bytes read: {bytes_read}"
        );
    }
}

#[test]
fn resolve_process_origin_pins_live_entity() {
    assert_eq!(
        resolve_process_origin(Some(&pid_container_resolver()), &creds_peer(TEST_PID)),
        Some(ProcessOrigin::Pinned(Some(pid_container_entity())))
    );
}

#[test]
fn resolve_process_origin_skips_live_lookup_when_unused() {
    let capture_entity_resolver = pid_container_resolver();
    let traffic_capture = TrafficCapture::new(PathBuf::new(), 1);

    assert_eq!(
        resolve_process_origin_if_needed(
            false,
            &traffic_capture,
            Some(&capture_entity_resolver),
            &creds_peer(TEST_PID)
        ),
        None
    );
    assert_eq!(capture_entity_resolver.resolution_count(), 0);
}

#[tokio::test]
async fn resolve_process_origin_pins_live_entity_during_capture() {
    let capture_entity_resolver = pid_container_resolver();
    let (_capture_directory, traffic_capture, _) = start_capture();

    assert_eq!(
        resolve_process_origin_if_needed(
            false,
            &traffic_capture,
            Some(&capture_entity_resolver),
            &creds_peer(TEST_PID)
        ),
        Some(ProcessOrigin::Pinned(Some(pid_container_entity())))
    );
    assert_eq!(capture_entity_resolver.resolution_count(), 1);

    stop_capture(&traffic_capture);
}

#[test]
fn build_capture_record_ignores_payload_local_data() {
    let record = build_capture_record(None, None, b"test.metric:1|c|c:ci-local-container\n");

    assert_eq!(record.container_id, None);
    assert!(record.ancillary.is_empty());
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
