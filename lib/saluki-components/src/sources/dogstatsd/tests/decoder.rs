use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use saluki_io::{
    deser::codec::dogstatsd::ParsedPacket,
    net::{ConnectionAddress, ProcessIdentity},
};
use saluki_metrics::test::TestRecorder;
use tokio::time::timeout;

use super::*;
use crate::sources::dogstatsd::should_warn_stream_log_too_big;
use crate::sources::dogstatsd::{
    handle_frame, handle_metric_packet, metrics::build_metrics, BufferDecodeContext, BufferDecodeMode, DecodeOutcome,
    DogStatsDDecoder, PacketForwarder, ReceivedBuffer,
};

#[tokio::test]
async fn connectionless_decoder_dispatches_full_and_flushed_buffers_and_forwards_frames() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let listen_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable);
    let metrics = build_metrics(&listen_addr, &test_component_context(), false);
    let mut source = test_source_context();
    let mut metrics_rx = source.outputs.take_named_output("metrics");
    let mut decoder = DogStatsDDecoder::new(source.context, test_decoder_context(false));
    let event_buffer_capacity = EventsBuffer::default().capacity();
    let (packet_forwarder, packets_rx) = PacketForwarder::for_tests(event_buffer_capacity + 1);
    let mut buffer_decode_context = BufferDecodeContext::new(
        &listen_addr,
        false,
        &metrics,
        Some(&packet_forwarder),
        BufferDecodeMode::Connectionless,
    );

    let mut payload = b"decoder.metric:1|c\n".repeat(event_buffer_capacity);
    payload.extend_from_slice(b"decoder.metric:1|c");
    let io_buffer = test_io_buffer(&payload, payload.len());
    let (received, returned_buffer) = ReceivedBuffer::with_return(io_buffer, payload.len(), peer_addr, None);
    let outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;

    assert_eq!(outcome, DecodeOutcome::Continue);
    let io_buffer = returned_buffer
        .await
        .expect("connectionless decoder should return the I/O buffer");
    assert_eq!(io_buffer.remaining(), 0);
    let full_buffer = metrics_rx.next().await.expect("metrics output should remain connected");
    assert_eq!(full_buffer.len(), event_buffer_capacity);
    let forwarded = timeout(Duration::from_secs(1), packets_rx.recv())
        .await
        .expect("forwarded packet should not time out")
        .expect("forwarded packet should be queued");
    assert_eq!(&forwarded[..], b"decoder.metric:1|c");
    assert_eq!(packets_rx.len(), event_buffer_capacity);

    decoder.flush_events().await;
    let flushed_buffer = metrics_rx.next().await.expect("metrics output should remain connected");
    assert_eq!(flushed_buffer.len(), 1);
    decoder.flush_events().await;
    assert!(
        metrics_rx.try_next().is_none(),
        "empty flush should not dispatch another buffer"
    );

    assert_eq!(
        recorder.counter((
            "component_packets_received_total",
            &[
                ("component_id", "dogstatsd_test"),
                ("component_type", "source"),
                ("listener_type", "unixgram"),
                ("state", "ok"),
            ],
        )),
        Some(1)
    );
    assert_eq!(
        recorder.counter((
            "component_bytes_received_total",
            &[
                ("component_id", "dogstatsd_test"),
                ("component_type", "source"),
                ("listener_type", "unixgram"),
            ],
        )),
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
    let listen_addr = ListenAddress::Unix("/tmp/dsd.socket".into());
    let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(
        saluki_io::net::ProcessCredentialsError::InvalidCredentials,
    ));
    let metrics = build_metrics(&listen_addr, &test_component_context(), false);
    let mut source = test_source_context();
    let mut metrics_rx = source.outputs.take_named_output("metrics");
    let mut decoder = DogStatsDDecoder::new(source.context, test_decoder_context(true));
    let (packet_forwarder, packets_rx) = PacketForwarder::for_tests(1);
    let mut buffer_decode_context = BufferDecodeContext::new(
        &listen_addr,
        false,
        &metrics,
        Some(&packet_forwarder),
        BufferDecodeMode::Connected,
    );

    let frame = b"stream.metric:1|c\n";
    let mut payload = Vec::with_capacity(frame.len() + 4);
    payload.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    payload.extend_from_slice(frame);
    let io_buffer = test_io_buffer(&payload[..2], payload.len() + 16);
    let (received, returned_buffer) = ReceivedBuffer::with_return(io_buffer, 2, peer_addr.clone(), None);

    let partial_outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;
    assert_eq!(partial_outcome, DecodeOutcome::Continue);
    let mut io_buffer = returned_buffer
        .await
        .expect("connected decoder should return the partial I/O buffer");
    assert_eq!(io_buffer.remaining(), 2);
    assert_eq!(
        recorder.counter((
            "component_packets_received_total",
            &[
                ("component_id", "dogstatsd_test"),
                ("component_type", "source"),
                ("listener_type", "unix"),
                ("state", "ok"),
            ],
        )),
        Some(0)
    );
    assert_eq!(
        recorder.counter((
            "component_errors_total",
            &[
                ("component_id", "dogstatsd_test"),
                ("component_type", "source"),
                ("error_type", "origin_detection"),
                ("reason", "invalid_credentials"),
            ],
        )),
        Some(0)
    );

    io_buffer.put_slice(&payload[2..]);
    let (received, returned_buffer) =
        ReceivedBuffer::with_return(io_buffer, payload.len() - 2, peer_addr.clone(), None);
    let complete_outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;
    assert_eq!(complete_outcome, DecodeOutcome::Continue);
    let io_buffer = returned_buffer
        .await
        .expect("connected decoder should return the consumed I/O buffer");
    assert_eq!(io_buffer.remaining(), 0);
    let forwarded = timeout(Duration::from_secs(1), packets_rx.recv())
        .await
        .expect("forwarded stream packet should not time out")
        .expect("complete outer frame should be forwarded");
    assert_eq!(&forwarded[..], b"stream.metric:1|c");

    let (received, returned_buffer) = ReceivedBuffer::with_return(io_buffer, 0, peer_addr, None);
    let eof_outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;
    assert_eq!(eof_outcome, DecodeOutcome::Stop);
    let returned_buffer = returned_buffer
        .await
        .expect("stopped decoder should return the I/O buffer");
    assert!(!returned_buffer.has_remaining());

    decoder.flush_events().await;
    let flushed_buffer = metrics_rx.next().await.expect("metrics output should remain connected");
    assert_eq!(flushed_buffer.len(), 1);
    assert_eq!(
        recorder.counter((
            "component_packets_received_total",
            &[
                ("component_id", "dogstatsd_test"),
                ("component_type", "source"),
                ("listener_type", "unix"),
                ("state", "ok"),
            ],
        )),
        Some(1)
    );
    assert_eq!(
        recorder.counter((
            "component_bytes_received_total",
            &[
                ("component_id", "dogstatsd_test"),
                ("component_type", "source"),
                ("listener_type", "unix"),
            ],
        )),
        Some(payload.len() as u64)
    );
    assert_eq!(
        recorder.counter((
            "component_errors_total",
            &[
                ("component_id", "dogstatsd_test"),
                ("component_type", "source"),
                ("error_type", "origin_detection"),
                ("reason", "invalid_credentials"),
            ],
        )),
        Some(1)
    );
    assert_eq!(recorder.counter(processed_metric_key("unix", None)), Some(1));
}

#[tokio::test]
async fn connectionless_decoder_counts_origin_detection_errors_by_reason() {
    let recorder = TestRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let listen_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
    let metrics = build_metrics(&listen_addr, &test_component_context(), false);
    let source = test_source_context();
    let mut decoder = DogStatsDDecoder::new(source.context, test_decoder_context(true));
    let mut buffer_decode_context =
        BufferDecodeContext::new(&listen_addr, false, &metrics, None, BufferDecodeMode::Connectionless);

    // Each datagram counts as a single origin detection error, regardless of how many metrics it holds, and is
    // tagged with the reason that origin detection failed.
    let payload = b"decoder.metric:1|c\ndecoder.metric:2|c";
    for (error, datagrams) in [
        (saluki_io::net::ProcessCredentialsError::ZeroPid, 2),
        (saluki_io::net::ProcessCredentialsError::InvalidCredentials, 1),
    ] {
        for _ in 0..datagrams {
            let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(error));
            let io_buffer = test_io_buffer(payload, payload.len());
            let (received, _returned_buffer) = ReceivedBuffer::with_return(io_buffer, payload.len(), peer_addr, None);
            let outcome = decoder.decode_buffer(&mut buffer_decode_context, received).await;
            assert_eq!(outcome, DecodeOutcome::Continue);
        }
    }

    for (reason, expected) in [("zero_pid", 2), ("invalid_credentials", 1)] {
        assert_eq!(
            recorder.counter((
                "component_errors_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("error_type", "origin_detection"),
                    ("reason", reason),
                ],
            )),
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
    let source = test_source_context();
    let mut decoder = DogStatsDDecoder::new(source.context, test_decoder_context(false));

    let datagram_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
    let datagram_metrics = build_metrics(&datagram_addr, &test_component_context(), false);
    let payload = b"missing.newline:1|c";
    let datagram_buffer = test_io_buffer(payload, payload.len());
    let mut datagram_context = BufferDecodeContext::new(
        &datagram_addr,
        true,
        &datagram_metrics,
        None,
        BufferDecodeMode::Connectionless,
    );
    let (received, _returned_datagram_buffer) =
        ReceivedBuffer::with_return(datagram_buffer, payload.len(), peer_addr.clone(), None);
    let datagram_outcome = decoder.decode_buffer(&mut datagram_context, received).await;
    assert_eq!(datagram_outcome, DecodeOutcome::Continue);

    let stream_addr = ListenAddress::Unix("/tmp/dsd.socket".into());
    let stream_metrics = build_metrics(&stream_addr, &test_component_context(), false);
    let stream_buffer_capacity = 64;
    let oversized_frame = (stream_buffer_capacity as u32).to_le_bytes();
    let stream_buffer = test_io_buffer(&oversized_frame, stream_buffer_capacity);
    let mut stream_context =
        BufferDecodeContext::new(&stream_addr, false, &stream_metrics, None, BufferDecodeMode::Connected);
    let (received, _returned_stream_buffer) =
        ReceivedBuffer::with_return(stream_buffer, oversized_frame.len(), peer_addr, None);
    let stream_outcome = decoder.decode_buffer(&mut stream_context, received).await;
    assert_eq!(stream_outcome, DecodeOutcome::Stop);

    for listener_type in ["unixgram", "unix"] {
        assert_eq!(
            recorder.counter((
                "component_errors_total",
                &[
                    ("component_id", "dogstatsd_test"),
                    ("component_type", "source"),
                    ("listener_type", listener_type),
                    ("error_type", "framing"),
                ],
            )),
            Some(1)
        );
    }
    assert_eq!(
        recorder.counter((
            "component_packets_received_total",
            &[
                ("component_id", "dogstatsd_test"),
                ("component_type", "source"),
                ("listener_type", "unixgram"),
                ("state", "ok"),
            ],
        )),
        Some(1)
    );
    assert_eq!(
        recorder.counter((
            "component_packets_received_total",
            &[
                ("component_id", "dogstatsd_test"),
                ("component_type", "source"),
                ("listener_type", "unix"),
                ("state", "ok"),
            ],
        )),
        Some(0)
    );
}

#[test]
fn events_and_service_checks_fall_back_to_host_tag_for_hostname() {
    let listen_addr = ListenAddress::Unixgram("/tmp/dsd.sock".into());
    let context = test_component_context();
    let metrics = build_metrics(&listen_addr, &context, false);
    let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
    let mut context_resolvers = test_context_resolvers();
    let default_hostname = MetaString::from_static("default-host");

    let mut handle = |frame: &[u8]| {
        handle_frame(
            frame,
            &codec,
            &mut context_resolvers,
            &metrics,
            false,
            None,
            EnablePayloadsFilter::default(),
            &[],
            &default_hostname,
        )
        .expect("frame should parse")
        .expect("frame should produce an event")
    };

    // Like the reference DogStatsD server, a `host:` tag sets the hostname when there's no `h:` field, and `h:`
    // wins otherwise. An empty `host:` tag gives an explicitly empty hostname.
    let cases: [(&[u8], &str); 3] = [
        (b"_e{5,4}:title|text|#host:tagged-host,env:prod", "tagged-host"),
        (
            b"_e{5,4}:title|text|h:field-host|#host:tagged-host,env:prod",
            "field-host",
        ),
        (b"_e{5,4}:title|text|#host:,env:prod", ""),
    ];
    for (frame, expected_hostname) in cases {
        let eventd = handle(frame).try_into_eventd().expect("should be an event");
        assert_eq!(eventd.hostname(), Some(expected_hostname));
        assert_eq!(
            eventd.tags().into_iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            ["env:prod"]
        );
    }

    let cases: [(&[u8], &str); 3] = [
        (b"_sc|my.check|0|#host:tagged-host,env:prod", "tagged-host"),
        (b"_sc|my.check|0|h:field-host|#host:tagged-host,env:prod", "field-host"),
        (b"_sc|my.check|0|#host:,env:prod", ""),
    ];
    for (frame, expected_hostname) in cases {
        let service_check = handle(frame)
            .try_into_service_check()
            .expect("should be a service check");
        assert_eq!(service_check.hostname(), Some(expected_hostname));
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

    let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
    let tags_resolver = TagsResolverBuilder::for_tests().build();
    let context_resolver = ContextResolverBuilder::for_tests()
        .with_heap_allocations(false)
        .with_tags_resolver(Some(tags_resolver.clone()))
        .build();
    let mut context_resolvers = ContextResolvers::manual(context_resolver.clone(), context_resolver, tags_resolver);
    let input = "big_metric_name_that_cant_possibly_be_inlined:1|c|#tag1:value1,tag2:value2,tag3:value3";

    let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(input.as_bytes()) else {
        panic!("Failed to parse packet.");
    };

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
    let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
    let mut context_resolvers = test_context_resolvers();
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
        let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(raw) else {
            panic!("Failed to parse {case} packet.");
        };
        let metric = handle_metric_packet(packet, &mut context_resolvers, None, &[], &default_hostname)
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
    let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
    let tags_resolver = TagsResolverBuilder::for_tests().build();
    let context_resolver = ContextResolverBuilder::for_tests()
        .with_heap_allocations(false)
        .with_tags_resolver(Some(tags_resolver.clone()))
        .build();
    let mut context_resolvers = ContextResolvers::manual(context_resolver.clone(), context_resolver, tags_resolver);
    let existing_tags = ["tag1:value1", "tag2:value2", "tag3:value3"];
    let existing_tags_str = existing_tags.join(",");

    let input = format!("test_metric_name:1|c|#{}", existing_tags_str);
    let additional_tags = [
        "tag4:value4".to_string(),
        "tag5:value5".to_string(),
        "tag6:value6".to_string(),
    ];

    let Ok(ParsedPacket::Metric(packet)) = codec.decode_packet(input.as_bytes()) else {
        panic!("Failed to parse packet.");
    };
    let maybe_metric = handle_metric_packet(
        packet,
        &mut context_resolvers,
        None,
        &additional_tags,
        &MetaString::from_static("default-host"),
    );
    assert!(maybe_metric.is_some());

    let metric = maybe_metric.unwrap();
    let context = metric.context();

    for tag in existing_tags {
        assert!(context.tags().has_tag(tag));
    }

    for tag in additional_tags {
        assert!(context.tags().has_tag(tag));
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

#[test]
fn non_finite_metric_values_are_silently_dropped() {
    // The Datadog Agent sends NaN gauges (for example, encode_ms.avg computed as 0.0/0.0 in Go).
    // FloatIter skips non-finite values with a debug log, so decode_packet returns Ok with
    // num_points == 0. handle_frame then returns Ok(None) for zero-point packets, which is
    // the existing silent-drop path (no warning emitted).
    let codec = DogStatsDCodec::from_configuration(DogStatsDCodecConfiguration::default());
    for input in &[b"my.gauge:NaN|g" as &[u8], b"my.gauge:inf|g", b"my.gauge:-inf|g"] {
        match codec.decode_packet(input).expect("should decode without error") {
            ParsedPacket::Metric(packet) => assert_eq!(
                packet.num_points, 0,
                "non-finite value should be dropped, leaving 0 valid points"
            ),
            _ => panic!("expected Metric packet"),
        }
    }
}
