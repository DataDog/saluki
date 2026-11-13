//! Decoding read buffers into frames, and frames into events.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::{Buf, BufMut};
use saluki_core::{
    components::sources::*,
    data_model::event::Event,
    topology::{interconnect::EventBufferManager, DEFAULT_EVENTS_BUFFER_CAPACITY},
};
use saluki_io::{
    buf::{BytesBuffer, ClearableIoBuffer as _},
    deser::{codec::dogstatsd::*, framing::FramingError},
    net::{ConnectionAddress, ListenAddress, ProcessCredentialsError, ProcessIdentity},
};
use stringtheory::MetaString;
use tracing::{debug, trace, warn};

use super::{
    dispatch_events, get_framer, handle_event_packet, handle_metric_packet, handle_service_check_packet,
    process_id_from_peer_addr, CaptureRecord, ContextResolvers, DogStatsDCaptureControl, DsdFramer,
    EnablePayloadsConfiguration, Frame, ListenerState, Metrics, PacketForwarder, ProcessOrigin,
};

#[derive(Clone)]
pub(super) struct DecoderContext {
    pub(super) codec: DogStatsDCodec,
    pub(super) context_resolvers: ContextResolvers,
    pub(super) default_hostname: MetaString,
    pub(super) enable_payloads: EnablePayloadsConfiguration,
    pub(super) origin_detection_enabled: bool,
    pub(super) stream_log_too_big: bool,
    pub(super) disable_verbose_logs: bool,
    pub(super) additional_tags: Arc<[String]>,
    pub(super) traffic_capture: DogStatsDCaptureControl,
}

pub(super) struct DogStatsDDecoder {
    pub(super) source_context: SourceContext,
    pub(super) context: DecoderContext,
    pub(super) event_buffer: EventBufferManager<DEFAULT_EVENTS_BUFFER_CAPACITY>,
}

#[derive(Clone, Copy)]
pub(super) enum BufferDecodeMode {
    Connectionless,
    Connected,
}

impl BufferDecodeMode {
    pub(super) fn is_eof(self, bytes_read: usize) -> bool {
        match self {
            Self::Connectionless => true,
            Self::Connected => bytes_read == 0,
        }
    }

    fn should_stop_on_eof(self, eof: bool) -> bool {
        matches!(self, Self::Connected) && eof
    }

    fn should_stop_on_framing_error(self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// Decode state of one connection, or of one datagram.
pub(super) struct BufferDecodeContext<'a> {
    pub(super) listener: &'a ListenerState,
    pub(super) mode: BufferDecodeMode,
    pub(super) framer: DsdFramer,

    /// The last PID seen on this connection, so that captured frames keep it on reads that carry no credentials.
    last_pid: Option<i32>,
}

impl<'a> BufferDecodeContext<'a> {
    pub(super) fn new(listener: &'a ListenerState, mode: BufferDecodeMode) -> Self {
        Self {
            listener,
            mode,
            framer: get_framer(&listener.listen_addr, listener.eol_required),
            last_pid: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DecodeOutcome {
    Continue,
    Stop,
}

pub(super) fn origin_detection_error_for_telemetry(
    origin_detection_enabled: bool, bytes_read: usize, peer_addr: &ConnectionAddress,
) -> Option<ProcessCredentialsError> {
    match peer_addr {
        ConnectionAddress::ProcessLike(identity @ ProcessIdentity::Error(error))
            if origin_detection_enabled && bytes_read > 0 && identity.is_telemetry_error() =>
        {
            Some(*error)
        }
        _ => None,
    }
}

impl DogStatsDDecoder {
    pub(super) fn new(source_context: SourceContext, context: DecoderContext) -> Self {
        Self {
            source_context,
            context,
            event_buffer: EventBufferManager::default(),
        }
    }

    pub(super) async fn decode_buffer(
        &mut self, context: &mut BufferDecodeContext<'_>, io_buffer: &mut BytesBuffer, bytes_read: usize,
        peer_addr: &ConnectionAddress, process_origin: Option<&ProcessOrigin>,
    ) -> DecodeOutcome {
        let listener = context.listener;
        let listen_addr = &listener.listen_addr;
        let metrics = &listener.metrics;
        let packet_forwarder = listener.packet_forwarder.as_ref();
        let mode = context.mode;

        let payload = received_payload(io_buffer, bytes_read);
        let traffic_capture = &self.context.traffic_capture;
        capture_unixgram_traffic(listen_addr, traffic_capture, peer_addr, process_origin, payload);

        let capture_stream_frames = matches!(listen_addr, ListenAddress::Unix(_)) && traffic_capture.is_ongoing();
        if capture_stream_frames {
            if let Some(process_id) = process_id_from_peer_addr(peer_addr) {
                context.last_pid = Some(process_id);
            }
        }

        metrics.bytes_received().increment(bytes_read as u64);
        metrics.bytes_received_size().record(bytes_read as f64);
        let origin_detection_errors =
            origin_detection_error_for_telemetry(self.context.origin_detection_enabled, bytes_read, peer_addr)
                .and_then(|error| metrics.origin_detection_errors(error));

        if matches!(mode, BufferDecodeMode::Connectionless) {
            metrics.packet_receive_success().increment(1);
            if let Some(origin_detection_errors) = origin_detection_errors {
                origin_detection_errors.increment(1);
            }
        }

        let eof = mode.is_eof(bytes_read);
        trace!(
            buffer_len = io_buffer.remaining(),
            buffer_cap = io_buffer.remaining_mut(),
            %listen_addr,
            %peer_addr,
            eof,
            "Received {} bytes from socket.",
            bytes_read
        );

        if should_drop_oversized_named_pipe_frame(listen_addr, io_buffer) {
            metrics.framing_errors().increment(1);
            debug!(%listen_addr, %peer_addr, "DogStatsD named pipe frame exceeded the configured buffer size. Dropping frame.");
            io_buffer.clear();
            return DecodeOutcome::Continue;
        }

        let mut frames = context.framer.frames(io_buffer.cursor(), eof, capture_stream_frames);
        loop {
            let frame_result = frames.next();
            let completed_outer_frames = frames.take_completed_outer_frames();
            if completed_outer_frames > 0 {
                metrics
                    .packet_receive_success()
                    .increment(completed_outer_frames as u64);
                if let Some(origin_detection_errors) = origin_detection_errors {
                    origin_detection_errors.increment(completed_outer_frames as u64);
                }
            }

            match frame_result {
                Some(Ok(Frame::Outer(payload))) => {
                    let _ = self.context.traffic_capture.enqueue(build_capture_record(
                        context.last_pid,
                        process_origin,
                        payload,
                    ));
                }
                Some(Ok(Frame::Message(frame))) => {
                    if matches!(listen_addr, ListenAddress::NamedPipe { .. }) {
                        capture_named_pipe_frame(&self.context.traffic_capture, frame);
                        metrics.packet_receive_success().increment(1);
                    }
                    self.decode_frame(frame, listen_addr, peer_addr, process_origin, metrics, packet_forwarder)
                        .await;
                }
                None => {
                    if mode.should_stop_on_eof(eof) {
                        debug!(%listen_addr, %peer_addr, "Stream received EOF. Shutting down handler.");
                        return DecodeOutcome::Stop;
                    }
                    return DecodeOutcome::Continue;
                }
                Some(Err(error)) => {
                    metrics.framing_errors().increment(1);
                    if should_warn_stream_log_too_big(listen_addr, &error, self.context.stream_log_too_big) {
                        warn!(
                            %listen_addr,
                            %peer_addr,
                            error = %error,
                            "DogStatsD stream frame exceeded the configured buffer size."
                        );
                    }

                    if mode.should_stop_on_framing_error() {
                        debug!(%listen_addr, %peer_addr, %error, "Error decoding frame. Stopping stream.");
                        return DecodeOutcome::Stop;
                    }

                    debug!(%listen_addr, %peer_addr, %error, "Error decoding datagram frame. Continuing listener.");
                    return DecodeOutcome::Continue;
                }
            }
        }
    }

    async fn decode_frame(
        &mut self, frame: &[u8], listen_addr: &ListenAddress, peer_addr: &ConnectionAddress,
        process_origin: Option<&ProcessOrigin>, metrics: &Metrics, packet_forwarder: Option<&PacketForwarder>,
    ) {
        trace!(%listen_addr, %peer_addr, frame = %frame.escape_ascii(), "Decoded frame.");
        if let Some(forwarder) = packet_forwarder {
            forwarder.forward(frame).await;
        }

        match self.handle_frame(frame, metrics, process_origin) {
            Ok(Some(event)) => {
                if let Some(event_buffer) = self.event_buffer.try_push(event) {
                    debug!(%listen_addr, %peer_addr, "Event buffer is full. Forwarding events.");
                    dispatch_events(event_buffer, &self.source_context).await;
                }
            }
            Ok(None) => {}
            Err(error) => {
                log_parse_failure(self.context.disable_verbose_logs, listen_addr, peer_addr, frame, &error);
            }
        }
    }

    pub(super) async fn flush_events(&mut self) {
        if let Some(event_buffer) = self.event_buffer.consume() {
            dispatch_events(event_buffer, &self.source_context).await;
        }
    }

    pub(super) fn handle_frame(
        &mut self, frame: &[u8], source_metrics: &Metrics, process_origin: Option<&ProcessOrigin>,
    ) -> Result<Option<Event>, ParseError> {
        let DecoderContext {
            codec,
            context_resolvers,
            default_hostname,
            enable_payloads,
            origin_detection_enabled,
            additional_tags,
            ..
        } = &mut self.context;

        let resolve_telemetry_origin = || {
            (source_metrics.origin_telemetry_enabled() && *origin_detection_enabled)
                .then(|| {
                    process_origin
                        .and_then(ProcessOrigin::container_entity_id)
                        .map(ToString::to_string)
                })
                .flatten()
        };

        let parsed = match codec.decode_packet(frame) {
            Ok(parsed) => parsed,
            Err(e) => {
                // Try and determine what the message type was, if possible, to increment the correct error counter.
                match parse_message_type(frame) {
                    MessageType::MetricSample => {
                        source_metrics.record_metric_parse_failed(resolve_telemetry_origin().as_deref())
                    }
                    MessageType::Event => source_metrics.event_decode_failed().increment(1),
                    MessageType::ServiceCheck => source_metrics.service_check_decode_failed().increment(1),
                }

                return Err(e);
            }
        };

        let event = match parsed {
            ParsedPacket::Metric(metric_packet) => {
                if metric_packet.num_points == 0 {
                    return Ok(None);
                }
                let events_len = metric_packet.num_points;
                if !enable_payloads.allow_metric(&metric_packet) {
                    trace!(
                        metric.name = metric_packet.metric_name,
                        "Skipping metric due to filter configuration."
                    );
                    return Ok(None);
                }

                match handle_metric_packet(
                    metric_packet,
                    context_resolvers,
                    process_origin,
                    additional_tags,
                    default_hostname,
                ) {
                    Some(metric) => {
                        source_metrics.record_metrics_received(events_len, resolve_telemetry_origin().as_deref());
                        Event::Metric(metric)
                    }
                    None => {
                        // We can only fail to get a metric back if we failed to resolve the context.
                        source_metrics.failed_context_resolve_total().increment(1);
                        return Ok(None);
                    }
                }
            }
            ParsedPacket::Event(event) => {
                if !enable_payloads.allow_event(&event) {
                    trace!("Skipping event {} due to filter configuration.", event.title);
                    return Ok(None);
                }
                match handle_event_packet(event, context_resolvers, process_origin, additional_tags) {
                    Some(event) => {
                        source_metrics.events_received().increment(1);
                        Event::EventD(event)
                    }
                    None => {
                        source_metrics.failed_context_resolve_total().increment(1);
                        return Ok(None);
                    }
                }
            }
            ParsedPacket::ServiceCheck(service_check) => {
                if !enable_payloads.allow_service_check(&service_check) {
                    trace!(
                        "Skipping service check {} due to filter configuration.",
                        service_check.name
                    );
                    return Ok(None);
                }
                match handle_service_check_packet(service_check, context_resolvers, process_origin, additional_tags) {
                    Some(service_check) => {
                        source_metrics.service_checks_received().increment(1);
                        Event::ServiceCheck(service_check)
                    }
                    None => {
                        source_metrics.failed_context_resolve_total().increment(1);
                        return Ok(None);
                    }
                }
            }
        };

        Ok(Some(event))
    }
}

pub(super) fn should_drop_oversized_named_pipe_frame(listen_addr: &ListenAddress, buffer: &BytesBuffer) -> bool {
    matches!(listen_addr, ListenAddress::NamedPipe { .. })
        && buffer.remaining_mut() == 0
        && memchr::memchr(b'\n', buffer.chunk()).is_none()
}

pub(super) fn should_warn_stream_log_too_big(
    listen_addr: &ListenAddress, error: &FramingError, stream_log_too_big: bool,
) -> bool {
    stream_log_too_big
        && matches!(listen_addr, ListenAddress::Unix(_))
        && matches!(error, FramingError::InvalidFrame { .. })
}

fn log_parse_failure(
    disable_verbose_logs: bool, listen_addr: &ListenAddress, peer_addr: &ConnectionAddress, frame: &[u8],
    error: &ParseError,
) {
    let frame = String::from_utf8_lossy(frame);
    if disable_verbose_logs {
        debug!(%listen_addr, %peer_addr, %frame, %error, "Failed to parse frame.");
    } else {
        warn!(%listen_addr, %peer_addr, %frame, %error, "Failed to parse frame.");
    }
}

pub(super) fn capture_named_pipe_frame(traffic_capture: &DogStatsDCaptureControl, frame: &[u8]) {
    if !frame.is_empty() && traffic_capture.is_ongoing() {
        let _ = traffic_capture.enqueue(build_capture_record(None, None, frame));
    }
}

fn capture_unixgram_traffic(
    listen_addr: &ListenAddress, traffic_capture: &DogStatsDCaptureControl, peer_addr: &ConnectionAddress,
    process_origin: Option<&ProcessOrigin>, payload: &[u8],
) {
    if payload.is_empty() || !matches!(listen_addr, ListenAddress::Unixgram(_)) || !traffic_capture.is_ongoing() {
        return;
    }

    let _ = traffic_capture.enqueue(build_capture_record(
        process_id_from_peer_addr(peer_addr),
        process_origin,
        payload,
    ));
}

pub(super) fn build_capture_record(
    process_id: Option<i32>, process_origin: Option<&ProcessOrigin>, payload: &[u8],
) -> CaptureRecord {
    CaptureRecord {
        timestamp_ns: capture_timestamp_ns(),
        payload: payload.to_vec(),
        pid: process_id,
        container_id: process_origin
            .and_then(ProcessOrigin::container_entity_id)
            .map(ToString::to_string),
    }
}

pub(super) fn received_payload(buffer: &BytesBuffer, bytes_read: usize) -> &[u8] {
    let chunk = buffer.chunk();
    let start = chunk.len().saturating_sub(bytes_read);
    &chunk[start..]
}

fn capture_timestamp_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}
