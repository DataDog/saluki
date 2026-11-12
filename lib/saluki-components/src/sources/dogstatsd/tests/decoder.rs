use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use saluki_core::data_model::event::Event;
use saluki_io::net::{ConnectionAddress, ProcessCredentialsError, ProcessIdentity};
use saluki_metrics::test::TestRecorder;
use tokio::time::timeout;

use super::*;
use crate::sources::dogstatsd::should_warn_stream_log_too_big;
use crate::sources::dogstatsd::{handle_metric_packet, BufferDecodeMode, DecodeOutcome, PacketForwarder};

fn packets_ok_key(listener_type: &str) -> Key {
    source_counter(
        "component_packets_received_total",
        &[("listener_type", listener_type), ("state", "ok")],
    )
}

fn bytes_received_key(listener_type: &str) -> Key {
    source_counter("component_bytes_received_total", &[("listener_type", listener_type)])
}

fn origin_detection_errors_key(reason: &str) -> Key {
    source_counter(
        "component_errors_total",
        &[("error_type", "origin_detection"), ("reason", reason)],
    )
}

#[tokio::test]
async fn connectionless_decoder_dispatches_full_and_flushed_buffers_and_forwards_frames() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable);
    let mut fixture = DecoderFixture::new(test_decoder_context(false));
    let mut metrics_rx = fixture.outputs.take_named_output("metrics");
    let event_buffer_capacity = EventsBuffer::default().capacity();
    let (packet_forwarder, packets_rx) = PacketForwarder::for_tests(event_buffer_capacity + 1);
    let listener = ListenerState {
        packet_forwarder: Some(packet_forwarder),
        ..test_listener_state(ListenAddress::Unixgram("/tmp/dsd.sock".into()))
    };
    let mut context = BufferDecodeContext::new(&listener, BufferDecodeMode::Connectionless);

    let mut payload = b"decoder.metric:1|c\n".repeat(event_buffer_capacity);
    payload.extend_from_slice(b"decoder.metric:1|c");
    let io_buffer = test_io_buffer(&payload, payload.len());
    let (outcome, io_buffer) = fixture
        .decode_once(&mut context, io_buffer, payload.len(), &peer_addr)
        .await;

    assert_eq!(outcome, DecodeOutcome::Continue);
    assert_eq!(io_buffer.remaining(), 0);
    let full_buffer = metrics_rx.next().await.expect("metrics output should remain connected");
    assert_eq!(full_buffer.len(), event_buffer_capacity);
    let forwarded = timeout(Duration::from_secs(1), packets_rx.recv())
        .await
        .expect("forwarded packet should not time out")
        .expect("forwarded packet should be queued");
    assert_eq!(&forwarded[..], b"decoder.metric:1|c");
    assert_eq!(packets_rx.len(), event_buffer_capacity);

    fixture.decoder.flush_events().await;
    let flushed_buffer = metrics_rx.next().await.expect("metrics output should remain connected");
    assert_eq!(flushed_buffer.len(), 1);
    fixture.decoder.flush_events().await;
    assert!(
        metrics_rx.try_next().is_none(),
        "empty flush should not dispatch another buffer"
    );

    assert_eq!(recorder.counter(packets_ok_key("unixgram")), Some(1));
    assert_eq!(
        recorder.counter(bytes_received_key("unixgram")),
        Some(payload.len() as u64)
    );
    assert_eq!(
        recorder.counter(processed_metric_key("unixgram", None)),
        Some((event_buffer_capacity + 1) as u64)
    );
}

#[tokio::test]
async fn connected_decoder_waits_for_complete_outer_frame_and_stops_on_eof() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(ProcessCredentialsError::InvalidCredentials));
    let mut fixture = DecoderFixture::new(test_decoder_context(true));
    let mut metrics_rx = fixture.outputs.take_named_output("metrics");
    let (packet_forwarder, packets_rx) = PacketForwarder::for_tests(1);
    let listener = ListenerState {
        packet_forwarder: Some(packet_forwarder),
        ..test_listener_state(ListenAddress::Unix("/tmp/dsd.socket".into()))
    };
    let mut context = BufferDecodeContext::new(&listener, BufferDecodeMode::Connected);

    let frame = b"stream.metric:1|c\n";
    let mut payload = Vec::with_capacity(frame.len() + 4);
    payload.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    payload.extend_from_slice(frame);
    let io_buffer = test_io_buffer(&payload[..2], payload.len() + 16);

    let (partial_outcome, mut io_buffer) = fixture.decode_once(&mut context, io_buffer, 2, &peer_addr).await;
    assert_eq!(partial_outcome, DecodeOutcome::Continue);
    assert_eq!(io_buffer.remaining(), 2);
    assert_eq!(recorder.counter(packets_ok_key("unix")), Some(0));
    assert_eq!(
        recorder.counter(origin_detection_errors_key("invalid_credentials")),
        Some(0)
    );

    io_buffer.put_slice(&payload[2..]);
    let (complete_outcome, io_buffer) = fixture
        .decode_once(&mut context, io_buffer, payload.len() - 2, &peer_addr)
        .await;
    assert_eq!(complete_outcome, DecodeOutcome::Continue);
    assert_eq!(io_buffer.remaining(), 0);
    let forwarded = timeout(Duration::from_secs(1), packets_rx.recv())
        .await
        .expect("forwarded stream packet should not time out")
        .expect("complete outer frame should be forwarded");
    assert_eq!(&forwarded[..], b"stream.metric:1|c");

    let (eof_outcome, io_buffer) = fixture.decode_once(&mut context, io_buffer, 0, &peer_addr).await;
    assert_eq!(eof_outcome, DecodeOutcome::Stop);
    assert!(!io_buffer.has_remaining());

    fixture.decoder.flush_events().await;
    let flushed_buffer = metrics_rx.next().await.expect("metrics output should remain connected");
    assert_eq!(flushed_buffer.len(), 1);
    assert_eq!(recorder.counter(packets_ok_key("unix")), Some(1));
    assert_eq!(recorder.counter(bytes_received_key("unix")), Some(payload.len() as u64));
    assert_eq!(
        recorder.counter(origin_detection_errors_key("invalid_credentials")),
        Some(1)
    );
    assert_eq!(recorder.counter(processed_metric_key("unix", None)), Some(1));
}

#[tokio::test]
async fn connectionless_decoder_counts_origin_detection_errors_by_reason() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let listener = test_listener_state(ListenAddress::Unixgram("/tmp/dsd.sock".into()));
    let mut fixture = DecoderFixture::new(test_decoder_context(true));
    let mut context = BufferDecodeContext::new(&listener, BufferDecodeMode::Connectionless);

    // Each datagram counts as a single origin detection error, regardless of how many metrics it holds, and is
    // tagged with the reason that origin detection failed.
    let payload = b"decoder.metric:1|c\ndecoder.metric:2|c";
    for (error, datagrams) in [
        (ProcessCredentialsError::ZeroPid, 2),
        (ProcessCredentialsError::InvalidCredentials, 1),
    ] {
        for _ in 0..datagrams {
            let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(error));
            let io_buffer = test_io_buffer(payload, payload.len());
            let (outcome, _) = fixture
                .decode_once(&mut context, io_buffer, payload.len(), &peer_addr)
                .await;
            assert_eq!(outcome, DecodeOutcome::Continue);
        }
    }

    for (reason, expected) in [("zero_pid", 2), ("invalid_credentials", 1)] {
        assert_eq!(
            recorder.counter(origin_detection_errors_key(reason)),
            Some(expected),
            "unexpected origin detection error count for reason `{reason}`"
        );
    }
}

#[tokio::test]
async fn decoder_continues_connectionless_but_stops_connected_on_framing_error() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable);
    let mut fixture = DecoderFixture::new(test_decoder_context(false));

    let datagram_listener = ListenerState {
        eol_required: true,
        ..test_listener_state(ListenAddress::Unixgram("/tmp/dsd.sock".into()))
    };
    let mut datagram_context = BufferDecodeContext::new(&datagram_listener, BufferDecodeMode::Connectionless);
    let payload = b"missing.newline:1|c";
    let datagram_buffer = test_io_buffer(payload, payload.len());
    let (datagram_outcome, _) = fixture
        .decode_once(&mut datagram_context, datagram_buffer, payload.len(), &peer_addr)
        .await;
    assert_eq!(datagram_outcome, DecodeOutcome::Continue);

    let stream_listener = test_listener_state(ListenAddress::Unix("/tmp/dsd.socket".into()));
    let mut stream_context = BufferDecodeContext::new(&stream_listener, BufferDecodeMode::Connected);
    let stream_buffer_capacity = 64;
    let oversized_frame = (stream_buffer_capacity as u32).to_le_bytes();
    let stream_buffer = test_io_buffer(&oversized_frame, stream_buffer_capacity);
    let (stream_outcome, _) = fixture
        .decode_once(&mut stream_context, stream_buffer, oversized_frame.len(), &peer_addr)
        .await;
    assert_eq!(stream_outcome, DecodeOutcome::Stop);

    for listener_type in ["unixgram", "unix"] {
        assert_eq!(
            recorder.counter(source_counter(
                "component_errors_total",
                &[("listener_type", listener_type), ("error_type", "framing")],
            )),
            Some(1),
            "framing errors for {listener_type}"
        );
    }
    assert_eq!(recorder.counter(packets_ok_key("unixgram")), Some(1));
    assert_eq!(recorder.counter(packets_ok_key("unix")), Some(0));
}

#[tokio::test]
async fn events_and_service_checks_fall_back_to_host_tag_for_hostname() {
    let listen_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
    let metrics = build_metrics(&listen_addr, &test_component_context(), false);
    let mut fixture = DecoderFixture::new(test_decoder_context(false));

    // Like the reference DogStatsD server, a `host:` tag sets the hostname when there's no `h:` field, and `h:`
    // wins otherwise. The `host:` tag itself is removed, and an empty one gives an explicitly empty hostname.
    let cases: [(&[u8], &str); 6] = [
        (b"_e{5,4}:title|text|#host:tagged-host,env:prod", "tagged-host"),
        (
            b"_e{5,4}:title|text|h:field-host|#host:tagged-host,env:prod",
            "field-host",
        ),
        (b"_e{5,4}:title|text|#host:,env:prod", ""),
        (b"_sc|my.check|0|#host:tagged-host,env:prod", "tagged-host"),
        (b"_sc|my.check|0|h:field-host|#host:tagged-host,env:prod", "field-host"),
        (b"_sc|my.check|0|#host:,env:prod", ""),
    ];
    for (frame, expected_hostname) in cases {
        let event = fixture
            .decoder
            .handle_frame(frame, &metrics, None)
            .expect("frame should parse")
            .expect("frame should produce an event");
        let (hostname, tags) = match event {
            Event::EventD(eventd) => (eventd.hostname().map(str::to_string), eventd.tags().clone()),
            Event::ServiceCheck(check) => (check.hostname().map(str::to_string), check.tags().clone()),
            other => panic!("unexpected event: {other:?}"),
        };
        let frame = String::from_utf8_lossy(frame);
        assert_eq!(hostname.as_deref(), Some(expected_hostname), "{frame}");
        assert_eq!(
            tags.into_iter().map(|t| t.as_str().to_string()).collect::<Vec<_>>(),
            ["env:prod"],
            "{frame}"
        );
    }
}

#[test]
fn no_metrics_when_interner_full_allocations_disallowed() {
    // We're specifically testing here that when we don't allow outside allocations, we should not be able to
    // resolve a context if the interner is full. A no-op interner has the smallest possible size, so that's going
    // to assure we can't intern anything... but we also need a string (name or one of the tags) that can't be
    // _inlined_ either, since that will get around the interner being full.
    //
    // We set our metric name to be longer than 31 bytes (the inlining limit) to ensure this.
    let mut context_resolvers = ContextResolvers::for_tests(false, None);
    let packet =
        parse_metric(b"big_metric_name_that_cant_possibly_be_inlined:1|c|#tag1:value1,tag2:value2,tag3:value3");

    let maybe_metric = handle_metric_packet(
        packet,
        &mut context_resolvers,
        None,
        &[],
        &MetaString::from_static("default-host"),
    );
    assert!(maybe_metric.is_none());
}

#[test]
fn metric_host_tag_disambiguates_contexts_without_remaining_tag() {
    let mut context_resolvers = ContextResolvers::for_tests(true, None);
    let default_hostname = MetaString::from_static("default-host");

    let packets = [
        ("unset", b"test_metric_name:1|g".as_slice(), "default-host"),
        ("empty", b"test_metric_name:2|g|#host:".as_slice(), ""),
        (
            "explicit_default",
            b"test_metric_name:3|g|#host:default-host".as_slice(),
            "default-host",
        ),
        (
            "custom",
            b"test_metric_name:4|g|#host:custom-host".as_slice(),
            "custom-host",
        ),
    ];

    let mut metrics = Vec::new();
    for (case, raw, expected_host) in packets {
        let metric = handle_metric_packet(parse_metric(raw), &mut context_resolvers, None, &[], &default_hostname)
            .unwrap_or_else(|| panic!("{case} metric should resolve"));

        assert_eq!(metric.context().host(), Some(expected_host), "{case} context host");
        assert!(metric.context().tags().into_iter().all(|tag| tag.name() != "host"));
        metrics.push(metric);
    }

    assert_eq!(metrics[0].context(), metrics[2].context());
    assert_ne!(metrics[0].context(), metrics[1].context());
    assert_ne!(metrics[0].context(), metrics[3].context());
    assert_ne!(metrics[1].context(), metrics[3].context());
}

#[test]
fn metric_with_additional_tags() {
    let mut context_resolvers = ContextResolvers::for_tests(false, None);
    let existing_tags = ["tag1:value1", "tag2:value2", "tag3:value3"];
    let input = format!("test_metric_name:1|c|#{}", existing_tags.join(","));
    let additional_tags = [
        "tag4:value4".to_string(),
        "tag5:value5".to_string(),
        "tag6:value6".to_string(),
    ];

    let metric = handle_metric_packet(
        parse_metric(input.as_bytes()),
        &mut context_resolvers,
        None,
        &additional_tags,
        &MetaString::from_static("default-host"),
    )
    .expect("metric should resolve");
    let context = metric.context();

    for tag in existing_tags {
        assert!(context.tags().has_tag(tag), "missing existing tag {tag}");
    }

    for tag in additional_tags {
        assert!(context.tags().has_tag(&tag), "missing additional tag {tag}");
    }
}

#[test]
fn stream_log_too_big_warns_for_enabled_length_delimited_stream_invalid_frames() {
    let uds_stream = ListenAddress::Unix("/tmp/dsd-stream.sock".into());
    let named_pipe_stream = named_pipe_listen_address();
    let tcp_stream = ListenAddress::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8125)));
    let error = saluki_io::deser::framing::FramingError::InvalidFrame {
        frame_len: 8193,
        reason: "frame length exceeds buffer capacity",
    };

    assert!(should_warn_stream_log_too_big(&uds_stream, &error, true));
    assert!(!should_warn_stream_log_too_big(&uds_stream, &error, false));
    assert!(!should_warn_stream_log_too_big(&named_pipe_stream, &error, true));
    assert!(!should_warn_stream_log_too_big(&tcp_stream, &error, true));
}
