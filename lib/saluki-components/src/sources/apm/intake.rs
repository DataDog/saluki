//! Native trace intake.
//!
//! Serves the trace endpoints the way the trace-agent's receiver does (`handleWithVersion` and `handleTracesV1` in
//! `pkg/trace/api/api.go`): it rejects cross-site requests, bounds the body size, decodes at most `decoders` payloads
//! at once, and answers with the status codes of `responses.go`. Each request's receiver metadata (client flags, the
//! tracer's container and process tags, the connection) is resolved once and shared by every trace of the payload.
//!
//! Status codes, in the order a request can meet them:
//!
//! | Condition | Status | Go |
//! |---|---|---|
//! | A MessagePack body sent to `/v0.2/traces` | 415 | `handleWithVersion`, `httpFormatError` |
//! | `Sec-Fetch-Site: cross-site` | 403 | `handleWithVersion` |
//! | `receiver_timeout` elapses waiting for a decoder | 429, no body | `handleTracesV1`, `req.Context().Done()` |
//! | `decoder_timeout` elapses waiting for a decoder | 200, or 429 with feature `429` or `Datadog-Send-Real-Http-Status`, after draining the body | `handleTracesV1`, `time.After(DecoderTimeout)` |
//! | `Content-Length` over `max_request_bytes` | 413 | `reserveBodySize` |
//! | Body reaches `max_request_bytes` | 413 | `LimitedReader` |
//! | `receiver_timeout` elapses reading the body | 408 | `ReadTimeout`, `httpDecodingError` |
//! | The body ends early, or ends before its JSON value does | 400 `unexpected-eof` | `httpDecodingError` |
//! | The payload doesn't decode | 400 | `httpDecodingError` |
//! | Accepted | 200 | `replyOK` |
//!
//! Accepted payloads, and those refused for want of a decoder, are answered as `replyOK` answers them: `OK` on
//! `/v0.2/traces` and `/v0.3/traces`, and the [sampling rates by service](crate::common::datadog::traces::rates) the
//! priority sampler publishes on the other endpoints.
//!
//! # Media types
//!
//! `/v1.0/traces` decodes any body as v1.0. The v0.x endpoints decode a body by its [format](super::media_type), as
//! `decodeConvertedTracerPayload` does:
//!
//! | `Content-Type` | Decoded as | Go |
//! |---|---|---|
//! | `application/msgpack`, with any parameters | MessagePack, as `UnmarshalMsgConverted` | `getMediaType`, `case "application/msgpack"` |
//! | `application/json`, `text/json`, absent, or a value that doesn't parse | JSON, as `ConvertToIdx` | `getMediaType` defaults to `application/json` |
//! | Any other media type | JSON, or else MessagePack from where the JSON decoder stopped reading | `default: // do our best` |
//!
//! Each outcome but the cross-site rejection is counted in the [receiver telemetry](super::receiver_telemetry), as
//! `handleTracesV1` counts it.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request as AxumRequest};
use axum::response::{IntoResponse, Response};
use bytes::{Bytes, BytesMut};
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use http_body_util::BodyExt as _;
use saluki_core::data_model::event::trace::{ReceiverMetadata, Trace};
use saluki_io::net::ConnectionAddress;
use stringtheory::MetaString;
use tokio::select;
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, sleep_until, Instant};
use tracing::debug;

use super::container_tags::ContainerTagsHasher;
use super::media_type::{payload_format, PayloadFormat};
use super::receiver_telemetry::{DropReason, ReceiverTags, ReceiverTelemetry};
use crate::common::datadog::traces::normalize::{truncate_utf8_str, MAX_SERVICE_LEN};
use crate::decoders::datadog::{
    decode_v04_any, decode_v04_json, decode_v04_payload, decode_v1_payload, first_span, resolve_process_tags,
    resolve_sdk_otlp_export, DecodeError,
};
use crate::transforms::{HistogramConfiguration, ServiceRates};

const SEC_FETCH_SITE_HEADER: HeaderName = HeaderName::from_static("sec-fetch-site");
const PROCESS_TAGS_HEADER: HeaderName = HeaderName::from_static("x-datadog-process-tags");
const LANG_HEADER: HeaderName = HeaderName::from_static("datadog-meta-lang");
const LANG_VERSION_HEADER: HeaderName = HeaderName::from_static("datadog-meta-lang-version");
const TRACER_VERSION_HEADER: HeaderName = HeaderName::from_static("datadog-meta-tracer-version");
const LANG_INTERPRETER_HEADER: HeaderName = HeaderName::from_static("datadog-meta-lang-interpreter");
const LANG_INTERPRETER_VENDOR_HEADER: HeaderName = HeaderName::from_static("datadog-meta-lang-interpreter-vendor");
const COMPUTED_TOP_LEVEL_HEADER: HeaderName = HeaderName::from_static("datadog-client-computed-top-level");
const COMPUTED_STATS_HEADER: HeaderName = HeaderName::from_static("datadog-client-computed-stats");
const DROPPED_P0_TRACES_HEADER: HeaderName = HeaderName::from_static("datadog-client-dropped-p0-traces");
const DROPPED_P0_SPANS_HEADER: HeaderName = HeaderName::from_static("datadog-client-dropped-p0-spans");
const TRACE_COUNT_HEADER: HeaderName = HeaderName::from_static("x-datadog-trace-count");
const SEND_REAL_HTTP_STATUS_HEADER: HeaderName = HeaderName::from_static("datadog-send-real-http-status");

/// Longest `Datadog-Meta-*` value kept, in bytes, as the trace-agent's `truncateMetaValue` bounds them.
const MAX_META_VALUE_LEN: usize = 200;

const RATES_PAYLOAD_VERSION_HEADER: HeaderName = HeaderName::from_static("datadog-rates-payload-version");

/// Body sent to a client that already has the current rates.
const UNCHANGED_RATES_BODY: &[u8] = b"{}";

/// Settings of the native trace intake.
#[derive(Clone, Debug)]
pub struct TraceIntakeConfiguration {
    /// Largest request body accepted, in bytes (`apm_config.max_payload_size`).
    pub max_request_bytes: u64,

    /// Number of payloads decoded at once (`apm_config.decoders`, resolved).
    pub decoders: NonZeroUsize,

    /// Time a payload waits for a free decoder before it is rejected (`apm_config.decoder_timeout`).
    pub decoder_timeout: Duration,

    /// Time a request has to be read, from its arrival (`apm_config.receiver_timeout`, resolved).
    pub receiver_timeout: Duration,

    /// Whether a payload rejected for want of a decoder gets `429 Too Many Requests` rather than `200 OK`
    /// (`apm_config.features` contains `429`).
    pub reject_with_too_many_requests: bool,

    /// How the receiver telemetry reports its `serve_traces_ms` histogram, as DogStatsD reports histograms
    /// (`histogram_aggregates`, `histogram_percentiles`, `histogram_copy_to_distribution`).
    pub histogram: HistogramConfiguration,

    /// The sampling rates by service the trace sampler publishes, sent back to tracers.
    pub service_rates: ServiceRates,
}

/// An intake endpoint version, as the trace-agent's `Version`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EndpointVersion {
    /// `/v0.2/traces`.
    V02,
    /// `/v0.3/traces`.
    V03,
    /// `/v0.4/traces`.
    V04,
    /// `/v1.0/traces`.
    V10,
}

impl EndpointVersion {
    fn as_str(self) -> &'static str {
        match self {
            Self::V02 => "v0.2",
            Self::V03 => "v0.3",
            Self::V04 => "v0.4",
            Self::V10 => "v1.0",
        }
    }

    /// Returns whether the endpoint rejects MessagePack bodies, as `handleWithVersion` does below v0.3.
    fn rejects_msgpack(self) -> bool {
        matches!(self, Self::V02)
    }

    /// Returns whether the endpoint answers `OK` rather than the rates, as `replyOK` does below v0.4.
    fn replies_ok(self) -> bool {
        matches!(self, Self::V02 | Self::V03)
    }
}

/// Body of the `415 Unsupported Media Type` answer to a MessagePack body on an endpoint that doesn't take one, as
/// `httpFormatError` writes it.
const UNSUPPORTED_MSGPACK_MESSAGE: &str = "unsupported media type: \"application/msgpack\"";

/// Body of the `400 Bad Request` answer to a body that ends early, as `httpDecodingError` writes it.
const UNEXPECTED_EOF_MESSAGE: &str = "unexpected-eof";

/// Body of the `400 Bad Request` answer to a MessagePack body that ends early, `msgp.ErrShortBytes`'s message.
const MSGP_SHORT_BYTES_MESSAGE: &str = "msgp: too few bytes left to read object";

/// Handles trace intake requests, and hands their traces to the source.
#[derive(Clone)]
pub(super) struct TraceIntake {
    config: Arc<TraceIntakeConfiguration>,
    decoders: Arc<Semaphore>,
    traces: mpsc::Sender<Vec<Trace>>,
    telemetry: Arc<ReceiverTelemetry>,
}

impl TraceIntake {
    pub(super) fn new(config: TraceIntakeConfiguration, traces: mpsc::Sender<Vec<Trace>>) -> Self {
        Self {
            decoders: Arc::new(Semaphore::new(config.decoders.get())),
            telemetry: Arc::new(ReceiverTelemetry::new(config.histogram.clone())),
            config: Arc::new(config),
            traces,
        }
    }

    /// Returns the receiver telemetry the intake counts into.
    pub(super) fn telemetry(&self) -> Arc<ReceiverTelemetry> {
        Arc::clone(&self.telemetry)
    }

    /// Handles a trace payload sent to the endpoint of `version`.
    pub(super) async fn handle(
        &self, version: EndpointVersion, request: AxumRequest, container_tags: &ContainerTagsHasher,
    ) -> Response {
        // Go's server read timeout runs from the arrival of the request.
        let deadline = Instant::now() + self.config.receiver_timeout;
        let (parts, body) = request.into_parts();
        let headers = &parts.headers;

        let format = payload_format(headers);
        if version.rejects_msgpack() && format == PayloadFormat::MessagePack {
            debug!(
                "Rejecting a MessagePack payload sent to the {} endpoint.",
                version.as_str()
            );
            self.telemetry.record_format_error(version.as_str());
            return http_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, UNSUPPORTED_MSGPACK_MESSAGE);
        }
        if header_str(headers, &SEC_FETCH_SITE_HEADER) == Some("cross-site") {
            return http_error(StatusCode::FORBIDDEN, "cross-site request rejected");
        }
        let connection = parts.extensions.get::<ConnectInfo<ConnectionAddress>>();

        let _permit = select! {
            biased;
            permit = self.decoders.clone().acquire_owned() => permit.expect("the decoder semaphore is never closed"),
            _ = sleep_until(deadline) => {
                debug!("Request timed out waiting for a decoder. Payload dropped.");
                self.telemetry.record(header_tags(version, headers, connection), |stats| stats.payload_timeout += 1);
                return StatusCode::TOO_MANY_REQUESTS.into_response();
            },
            _ = sleep(self.config.decoder_timeout) => {
                debug!("No decoder became free in time. Payload rejected.");
                let _ = read_body(headers, body, self.config.max_request_bytes, deadline, false).await;
                let status = if is_header_true(header_str(headers, &SEND_REAL_HTTP_STATUS_HEADER))
                    || self.config.reject_with_too_many_requests
                {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::OK
                };
                self.telemetry.record(header_tags(version, headers, connection), |stats| stats.payload_refused += 1);
                // The trace-agent writes the status before the rates, so the version header it sets after is dropped.
                return self.reply_ok(version, status, headers, false).0;
            },
        };

        // As `serve_traces_ms` in the trace-agent, the serve time runs from when a decoder is free.
        let start = Instant::now();
        let body = match read_body(headers, body, self.config.max_request_bytes, deadline, true).await {
            Ok(body) => body,
            Err(error) => {
                self.record_rejection(version, headers, connection, start, error.drop_reason());
                return error.into_response();
            }
        };
        let body_len = body.len();

        let decoded = match (version, format) {
            (EndpointVersion::V10, _) => decode_v1_payload(&body),
            // `/v0.2/traces` never gets here with MessagePack: it is rejected above.
            (_, PayloadFormat::MessagePack) => decode_v04_payload(&body),
            (_, PayloadFormat::Json) => decode_v04_json(&body),
            (_, PayloadFormat::Unknown) => decode_v04_any(&body),
        };
        // The decoded traces own their data, so the body is released before they wait for the pipeline, as the
        // trace-agent returns its buffer to the pool before `sendPayload`.
        drop(body);
        let decoded = match decoded {
            Ok(decoded) => decoded,
            Err(error) => {
                debug!(%error, "Cannot decode {version:?} traces payload.");
                // `msgp.ErrShortBytes` and the JSON decoder's `io.EOF` are counted apart, and answered with their own
                // messages.
                let (reason, message) = match error {
                    DecodeError::ShortBytes { .. } => (DropReason::MsgpShortBytes, MSGP_SHORT_BYTES_MESSAGE.into()),
                    DecodeError::UnexpectedEof => (DropReason::UnexpectedEof, UNEXPECTED_EOF_MESSAGE.into()),
                    error => (DropReason::DecodingError, error.to_string()),
                };
                self.record_rejection(version, headers, connection, start, reason);
                return http_error(StatusCode::BAD_REQUEST, &message);
            }
        };
        let mut traces = decoded.traces;

        // A v0.x payload has no tracer fields of its own: as `decodeConvertedTracerPayload`, they come from the
        // request headers, as sent.
        if version != EndpointVersion::V10 {
            let header = |name: &HeaderName| shared_meta(header_str(headers, name).unwrap_or_default());
            let (lang, lang_version, tracer_version) = (
                header(&LANG_HEADER),
                header(&LANG_VERSION_HEADER),
                header(&TRACER_VERSION_HEADER),
            );
            for trace in &mut traces {
                trace.payload.language_name = lang.clone();
                trace.payload.language_version = lang_version.clone();
                trace.payload.tracer_version = tracer_version.clone();
            }
        }

        // A v1.0 payload's own container ID wins over the request's. A v0.x payload has none, so its container is
        // the request's, as `decodeConvertedTracerPayload` sets it.
        let payload_container_id = traces.first().map(|trace| trace.payload.container_id.clone());
        let container = match payload_container_id {
            Some(container_id) if !container_id.is_empty() => {
                let tags = container_tags.container_tags(&container_id);
                Some((container_id, tags))
            }
            Some(_) => container_tags.container(headers, connection),
            None => None,
        };

        let mut receiver = receiver_metadata(version, headers, connection, traces.len());
        receiver.service = first_service(&traces);
        receiver.process_tags = resolve_process_tags(
            &decoded.payload_attributes,
            first_span(&traces),
            header_str(headers, &PROCESS_TAGS_HEADER),
        );
        receiver.sdk_otlp_export = resolve_sdk_otlp_export(&decoded.payload_attributes, first_span(&traces));
        receiver.payload_attributes = decoded.payload_attributes.into_iter().collect();

        let container_id = container.map(|(container_id, tags)| {
            receiver.container_tags = tags;
            container_id
        });

        let receiver = Arc::new(receiver);
        let mut spans = 0;
        for trace in &mut traces {
            if let Some(container_id) = &container_id {
                trace.payload.container_id = container_id.clone();
            }
            trace.payload.receiver = Some(Arc::clone(&receiver));
            spans += trace.spans().len();
        }
        let trace_count = traces.len();

        if !traces.is_empty() && self.traces.send(traces).await.is_err() {
            debug!("APM source stopped. Payload dropped.");
        }

        // The trace-agent counts the dropped P0s the client reports separately from the per-chunk weight.
        let dropped_p0_traces = header_count(headers, &DROPPED_P0_TRACES_HEADER);
        let dropped_p0_spans = header_count(headers, &DROPPED_P0_SPANS_HEADER);
        let (response, response_bytes) = self.reply_ok(version, StatusCode::OK, headers, true);
        self.telemetry.record(ReceiverTags::from_receiver(&receiver), |stats| {
            stats.traces_received += trace_count as i64;
            stats.traces_bytes += body_len as i64;
            stats.spans_received += spans as i64;
            stats.payload_accepted += 1;
            stats.client_dropped_p0_traces += dropped_p0_traces.unwrap_or(0);
            stats.client_dropped_p0_spans += dropped_p0_spans.unwrap_or(0);
            stats.rate_response_bytes(response_bytes);
            // Recorded once the traces are handed over, as the trace-agent's deferred timer is.
            stats.serve_time(start.elapsed(), true);
        });

        response
    }

    /// Answers a payload as `replyOK` does, with `OK` or the rates by version, and returns the body's length.
    fn reply_ok(
        &self, version: EndpointVersion, status: StatusCode, headers: &HeaderMap, send_version: bool,
    ) -> (Response, usize) {
        if version.replies_ok() {
            // As `httpOK`, with the `Content-Type` Go's server sniffs from the body.
            const OK_BODY: &str = "OK\n";
            return ((status, OK_BODY).into_response(), OK_BODY.len());
        }
        self.rate_by_service(status, headers, send_version)
    }

    /// Answers with the sampling rates by service, as `httpRateByService`, and returns the body's length.
    ///
    /// A client that sends the version of the rates it has gets the current version back, if `send_version`, and `{}`
    /// rather than the rates when it already has them.
    fn rate_by_service(&self, status: StatusCode, headers: &HeaderMap, send_version: bool) -> (Response, usize) {
        let rates = self.config.service_rates.load();
        let client_version = headers
            .get(&RATES_PAYLOAD_VERSION_HEADER)
            .map(HeaderValue::as_bytes)
            .unwrap_or_default();
        let mut response_headers = HeaderMap::with_capacity(2);
        response_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let body = if client_version.is_empty() {
            rates.body.clone()
        } else {
            if send_version {
                response_headers.insert(RATES_PAYLOAD_VERSION_HEADER, rates.version.clone());
            }
            if client_version == rates.version.as_bytes() {
                Bytes::from_static(UNCHANGED_RATES_BODY)
            } else {
                rates.body.clone()
            }
        };
        let len = body.len();
        ((status, response_headers, body).into_response(), len)
    }

    /// Counts a payload that couldn't be read or decoded, as `handleTracesV1` does.
    fn record_rejection(
        &self, version: EndpointVersion, headers: &HeaderMap, connection: Option<&ConnectInfo<ConnectionAddress>>,
        start: Instant, reason: DropReason,
    ) {
        self.telemetry.record_error(version.as_str(), reason);
        // As `traceCount`, a missing or unparseable `X-Datadog-Trace-Count` counts no trace.
        let dropped = header_count(headers, &TRACE_COUNT_HEADER).unwrap_or(0);
        self.telemetry
            .record(header_tags(version, headers, connection), |stats| {
                stats.drop_traces(reason, dropped);
                stats.serve_time(start.elapsed(), false);
            });
    }
}

/// Returns the receiver telemetry tags that come from the request alone, as the trace-agent's `tagStats` with no
/// service.
fn header_tags(
    version: EndpointVersion, headers: &HeaderMap, connection: Option<&ConnectInfo<ConnectionAddress>>,
) -> ReceiverTags {
    let meta = |name: &HeaderName| shared_meta(truncate_utf8(header_str(headers, name).unwrap_or_default()));
    ReceiverTags {
        lang: meta(&LANG_HEADER),
        lang_version: meta(&LANG_VERSION_HEADER),
        lang_vendor: meta(&LANG_INTERPRETER_VENDOR_HEADER),
        interpreter: meta(&LANG_INTERPRETER_HEADER),
        tracer_version: meta(&TRACER_VERSION_HEADER),
        endpoint_version: MetaString::from_static(version.as_str()),
        connection_type: connection_type(connection),
        service: MetaString::empty(),
    }
}

/// Returns the service of the payload's first span, truncated as the trace-agent's `tagStats` truncates it.
fn first_service(traces: &[Trace]) -> MetaString {
    let service = first_span(traces).map(|span| span.service()).unwrap_or_default();
    shared_meta(truncate_utf8_str(service, MAX_SERVICE_LEN))
}

/// Returns a string that is cheap to clone, so that the telemetry can key on it without copying it again.
fn shared_meta(value: &str) -> MetaString {
    if value.is_empty() {
        return MetaString::empty();
    }
    MetaString::try_inline(value).unwrap_or_else(|| MetaString::from(Arc::<str>::from(value)))
}

/// Returns a header's value parsed as a count, as `strconv.ParseInt`, if it parses.
fn header_count(headers: &HeaderMap, name: &HeaderName) -> Option<i64> {
    header_str(headers, name).and_then(|value| value.parse::<i64>().ok())
}

/// Builds the receiver metadata that comes from the request alone.
fn receiver_metadata(
    version: EndpointVersion, headers: &HeaderMap, connection: Option<&ConnectInfo<ConnectionAddress>>,
    chunk_count: usize,
) -> ReceiverMetadata {
    let tags = header_tags(version, headers, connection);

    // Go divides by the payload's chunk count when it processes the payload, after dropping invalid chunks; dividing
    // by the decoded count spreads the weight over slightly more chunks when some are dropped.
    let dropped_p0s = header_count(headers, &DROPPED_P0_TRACES_HEADER).unwrap_or(0);
    let client_dropped_p0s_weight = if chunk_count == 0 {
        0.0
    } else {
        dropped_p0s as f64 / chunk_count as f64
    };

    ReceiverMetadata {
        client_dropped_p0s_weight,
        client_computed_stats: is_header_true(header_str(headers, &COMPUTED_STATS_HEADER)),
        client_computed_top_level: is_header_true(header_str(headers, &COMPUTED_TOP_LEVEL_HEADER)),
        lang: tags.lang,
        lang_version: tags.lang_version,
        tracer_version: tags.tracer_version,
        interpreter: tags.interpreter,
        lang_vendor: tags.lang_vendor,
        endpoint_version: tags.endpoint_version,
        connection_type: tags.connection_type,
        ..ReceiverMetadata::new()
    }
}

/// Returns the connection type of the trace-agent's `GetConnectionType`.
fn connection_type(connection: Option<&ConnectInfo<ConnectionAddress>>) -> MetaString {
    MetaString::from_static(match connection {
        Some(ConnectInfo(ConnectionAddress::SocketLike(_))) => "tcp",
        Some(ConnectInfo(ConnectionAddress::ProcessLike(_))) => "uds",
        None => "unknown",
    })
}

/// Returns a header's value, if it is valid UTF-8.
///
/// Go reads header values as raw bytes, so a value with non-ASCII bytes isn't rejected as `HeaderValue::to_str` would.
fn header_str<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| simdutf8::basic::from_utf8(value.as_bytes()).ok())
}

/// Returns whether a header value is set and not false, as the trace-agent's `isHeaderTrue`.
///
/// The false values are those Go's `strconv.ParseBool` reads as false. A value it can't parse is true.
fn is_header_true(value: Option<&str>) -> bool {
    match value {
        None | Some("") => false,
        Some("0" | "f" | "F" | "false" | "FALSE" | "False") => false,
        Some(_) => true,
    }
}

/// Truncates a `Datadog-Meta-*` value to [`MAX_META_VALUE_LEN`] bytes, on a character boundary.
fn truncate_utf8(value: &str) -> &str {
    if value.len() <= MAX_META_VALUE_LEN {
        return value;
    }
    let mut end = MAX_META_VALUE_LEN;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Why a request body couldn't be read.
#[derive(Debug, PartialEq, Eq)]
enum BodyError {
    /// The body is at least as large as the limit.
    TooLarge,
    /// The body ended before it was complete.
    UnexpectedEof,
    /// The receiver timeout elapsed.
    Timeout,
}

impl BodyError {
    fn drop_reason(&self) -> DropReason {
        match self {
            Self::TooLarge => DropReason::PayloadTooLarge,
            Self::UnexpectedEof => DropReason::UnexpectedEof,
            Self::Timeout => DropReason::Timeout,
        }
    }
}

impl IntoResponse for BodyError {
    fn into_response(self) -> Response {
        match self {
            Self::TooLarge => http_error(StatusCode::PAYLOAD_TOO_LARGE, "payload-too-large"),
            Self::UnexpectedEof => http_error(StatusCode::BAD_REQUEST, UNEXPECTED_EOF_MESSAGE),
            Self::Timeout => http_error(StatusCode::REQUEST_TIMEOUT, "timeout"),
        }
    }
}

/// Reads a request body of fewer than `limit` bytes before `deadline`, keeping it only if `keep` is set.
///
/// As the trace-agent's `LimitedReader`, a body is too large once it reaches the limit, so a body of exactly `limit`
/// bytes is rejected too; a `Content-Length` over the limit is rejected before reading.
async fn read_body(
    headers: &HeaderMap, body: Body, limit: u64, deadline: Instant, keep: bool,
) -> Result<Bytes, BodyError> {
    // As `reserveBodySize`, an unparseable `Content-Length` is ignored.
    let content_length = header_str(headers, &CONTENT_LENGTH).and_then(|value| value.parse::<u64>().ok());
    if content_length.is_some_and(|length| length > limit) {
        return Err(BodyError::TooLarge);
    }

    let read = async {
        let mut buffer = BytesMut::with_capacity(if keep {
            content_length.unwrap_or(0).min(limit) as usize
        } else {
            0
        });
        let mut read = 0u64;
        let mut body = body;
        loop {
            if read >= limit {
                return Err(BodyError::TooLarge);
            }
            match body.frame().await {
                None => return Ok(buffer.freeze()),
                Some(Err(_)) => return Err(BodyError::UnexpectedEof),
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        read += data.len() as u64;
                        if keep && read < limit {
                            buffer.extend_from_slice(&data);
                        }
                    }
                }
            }
        }
    };
    tokio::time::timeout_at(deadline, read)
        .await
        .unwrap_or(Err(BodyError::Timeout))
}

/// Answers with an error, as Go's `http.Error` does.
fn http_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [
            (CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8")),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
        ],
        format!("{message}\n"),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use futures::stream;
    use http::Request;
    use rmp::encode;
    use saluki_core::data_model::event::metric::MetricValues;
    use saluki_core::data_model::origin::OriginTagCardinality;
    use saluki_core::data_model::tags::{SharedTagSet, Tag, TagSet};
    use saluki_env::workload::origin::{ResolvedExternalData, ResolvedOrigin};
    use saluki_env::workload::{EntityId, WorkloadProvider};
    use saluki_io::net::ProcessCredentials;

    use super::*;
    use crate::common::datadog::traces::rates::RateByService;

    const LIMIT: u64 = 4096;

    fn config() -> TraceIntakeConfiguration {
        TraceIntakeConfiguration {
            max_request_bytes: LIMIT,
            decoders: NonZeroUsize::MIN,
            decoder_timeout: Duration::from_millis(50),
            receiver_timeout: Duration::from_secs(5),
            reject_with_too_many_requests: false,
            histogram: HistogramConfiguration::default(),
            service_rates: ServiceRates::default(),
        }
    }

    fn intake(config: TraceIntakeConfiguration) -> (TraceIntake, mpsc::Receiver<Vec<Trace>>) {
        let (tx, rx) = mpsc::channel(8);
        (TraceIntake::new(config, tx), rx)
    }

    fn write_string_attribute(buf: &mut Vec<u8>, key: &str, value: &str) {
        encode::write_array_len(buf, 3).unwrap();
        encode::write_str(buf, key).unwrap();
        encode::write_uint(buf, 1).unwrap(); // AnyValue string
        encode::write_str(buf, value).unwrap();
    }

    /// Builds a v1.0 tracer payload with `chunks` chunks of one span each, and the container ID, if any.
    fn v1_payload(container_id: Option<&str>, chunks: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        encode::write_map_len(&mut buf, if container_id.is_some() { 2 } else { 1 }).unwrap();
        if let Some(container_id) = container_id {
            encode::write_uint(&mut buf, 2).unwrap();
            encode::write_str(&mut buf, container_id).unwrap();
        }
        encode::write_uint(&mut buf, 11).unwrap();
        encode::write_array_len(&mut buf, chunks).unwrap();
        for _ in 0..chunks {
            // Trace chunk: one span (field 4) with a service (field 1) and attributes (field 9).
            encode::write_map_len(&mut buf, 1).unwrap();
            encode::write_uint(&mut buf, 4).unwrap();
            encode::write_array_len(&mut buf, 1).unwrap();
            encode::write_map_len(&mut buf, 2).unwrap();
            encode::write_uint(&mut buf, 1).unwrap();
            encode::write_str(&mut buf, "checkout-service").unwrap();
            encode::write_uint(&mut buf, 9).unwrap();
            write_string_attribute(&mut buf, "_dd.tags.process", "entrypoint.name:app");
        }
        buf
    }

    fn request(headers: &[(&str, &str)], body: Body) -> AxumRequest {
        let mut builder = Request::builder().method("PUT").uri("/v1.0/traces");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(body).unwrap()
    }

    fn sized_request(headers: &[(&str, &str)], body: Vec<u8>) -> AxumRequest {
        let length = body.len().to_string();
        let mut headers = headers.to_vec();
        headers.push(("Content-Length", &length));
        request(&headers, Body::from(body))
    }

    /// A body sent in chunks, with no `Content-Length`, as a chunked transfer encoding does.
    fn streamed(chunks: Vec<Result<Bytes, std::io::Error>>) -> Body {
        Body::from_stream(stream::iter(chunks))
    }

    async fn send(intake: &TraceIntake, request: AxumRequest) -> (StatusCode, HeaderMap, String) {
        send_to(intake, EndpointVersion::V10, request).await
    }

    async fn send_to(
        intake: &TraceIntake, version: EndpointVersion, request: AxumRequest,
    ) -> (StatusCode, HeaderMap, String) {
        let response = intake.handle(version, request, &ContainerTagsHasher::default()).await;
        let (parts, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        (parts.status, parts.headers, String::from_utf8(body.to_vec()).unwrap())
    }

    /// Builds a v0.4 payload of `chunks` chunks of one root span each, from service `web` in env `prod`.
    fn v04_payload(chunks: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        encode::write_array_len(&mut buf, chunks).unwrap();
        for chunk in 0..chunks {
            encode::write_array_len(&mut buf, 1).unwrap();
            encode::write_map_len(&mut buf, 5).unwrap();
            encode::write_str(&mut buf, "service").unwrap();
            encode::write_str(&mut buf, "web").unwrap();
            encode::write_str(&mut buf, "name").unwrap();
            encode::write_str(&mut buf, "http.request").unwrap();
            encode::write_str(&mut buf, "trace_id").unwrap();
            encode::write_uint(&mut buf, u64::from(chunk) + 1).unwrap();
            encode::write_str(&mut buf, "span_id").unwrap();
            encode::write_uint(&mut buf, 1).unwrap();
            encode::write_str(&mut buf, "meta").unwrap();
            encode::write_map_len(&mut buf, 1).unwrap();
            encode::write_str(&mut buf, "env").unwrap();
            encode::write_str(&mut buf, "prod").unwrap();
        }
        buf
    }

    const MSGPACK: (&str, &str) = ("Content-Type", "application/msgpack");

    /// `testutil.GetTestTraces(1, 1, false)` as `json.Marshal` writes it.
    const GO_TEST_TRACES_JSON: &str = r#"[[{"service":"fennel_IS amazing!","name":"something &&<@# that should be a metric!","resource":"NOT touched because it is going to be hashed","trace_id":42,"span_id":52,"parent_id":42,"start":9223372036854775807,"duration":9223372036854775807,"error":0,"meta":{"http.host":"192.168.0.1"},"metrics":{"http.monitor":41.99},"type":"web","span_links":[{"trace_id":42,"trace_id_high":32,"span_id":52,"attributes":{"a1":"v1","a2":"v2"},"tracestate":"dd=s:2;o:rum,congo=baz123","flags":2147483649}]}]]"#;

    // `TestReceiverJSONDecoder` in `api_test.go`.
    #[tokio::test]
    async fn json_payloads_are_decoded_on_every_v0_endpoint() {
        for version in [EndpointVersion::V02, EndpointVersion::V03, EndpointVersion::V04] {
            for content_type in [None, Some(""), Some("application/json"), Some("text/json")] {
                let (intake, mut traces) = intake(config());
                let headers: Vec<_> = content_type.map(|value| ("Content-Type", value)).into_iter().collect();
                let request = sized_request(&headers, GO_TEST_TRACES_JSON.as_bytes().to_vec());
                let (status, _, _) = send_to(&intake, version, request).await;
                assert_eq!(status, StatusCode::OK, "{version:?} {content_type:?}");

                let traces = traces.try_recv().unwrap();
                assert_eq!(traces.len(), 1);
                let trace = &traces[0];
                assert_eq!((trace.trace_id_high, trace.trace_id_low), (0, 42));
                let span = &trace.spans()[0];
                assert_eq!(span.span_id(), 52);
                assert_eq!(span.service(), "fennel_IS amazing!");
                assert_eq!(span.name(), "something &&<@# that should be a metric!");
                assert_eq!(span.resource(), "NOT touched because it is going to be hashed");
                assert_eq!(
                    span.attributes
                        .get("http.host")
                        .and_then(|value| value.as_string())
                        .map(|value| value.as_ref()),
                    Some("192.168.0.1")
                );
                assert_eq!(
                    span.attributes.get("http.monitor").and_then(|value| value.as_float()),
                    Some(41.99)
                );
            }
        }
    }

    #[tokio::test]
    async fn a_body_of_another_media_type_is_decoded_as_json_or_else_as_msgpack() {
        let (intake, mut traces) = intake(config());
        let headers = [("Content-Type", "text/plain"), ("X-Datadog-Trace-Count", "1")];

        let (status, _, _) = send_to(
            &intake,
            EndpointVersion::V04,
            sized_request(&headers, GO_TEST_TRACES_JSON.as_bytes().to_vec()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(traces.try_recv().unwrap().len(), 1);

        // The MessagePack decoder only gets what the JSON decoder left, which for a short body is nothing.
        let (status, _, body) = send_to(&intake, EndpointVersion::V04, sized_request(&headers, v04_payload(1))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, format!("{MSGP_SHORT_BYTES_MESSAGE}\n"));
        assert!(traces.try_recv().is_err());
        let counters = counters(&intake);
        assert_eq!(
            counter(&counters, "datadog.trace_agent.normalizer.traces_dropped")
                .0
                .last()
                .map(String::as_str),
            Some("reason:msgp_short_bytes")
        );
    }

    #[tokio::test]
    async fn a_msgpack_content_type_with_parameters_is_msgpack() {
        let (intake, mut traces) = intake(config());
        let headers = [("Content-Type", "Application/MsgPack; charset=utf-8")];
        let (status, _, _) = send_to(&intake, EndpointVersion::V04, sized_request(&headers, v04_payload(1))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(traces.try_recv().unwrap().len(), 1);

        let (status, _, _) = send_to(&intake, EndpointVersion::V02, sized_request(&headers, v04_payload(1))).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    // `TestReceiverDecodingError` in `api_test.go`, and `io.ErrUnexpectedEOF` from the JSON decoder.
    #[tokio::test]
    async fn json_decoding_errors_are_classified_as_http_decoding_error_does() {
        let cases: [(&[u8], &str, &str, &str); 4] = [
            (b"} invalid json", "error:decoding-error", "reason:decoding_error", ""),
            (
                br#"[[{"span_id":"52"}]]"#,
                "error:decoding-error",
                "reason:decoding_error",
                "",
            ),
            (
                br#"[[{"service":"a""#,
                "error:unexpected-eof",
                "reason:unexpected_eof",
                "unexpected-eof\n",
            ),
            (b"", "error:unexpected-eof", "reason:unexpected_eof", "unexpected-eof\n"),
        ];
        for (body, error_tag, reason_tag, message) in cases {
            let (intake, _traces) = intake(config());
            let headers = [("Content-Type", "application/json"), ("X-Datadog-Trace-Count", "10")];
            let (status, _, response) =
                send_to(&intake, EndpointVersion::V04, sized_request(&headers, body.to_vec())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
            if !message.is_empty() {
                assert_eq!(response, message);
            }
            let counters = counters(&intake);
            let (tags, errors) = counter(&counters, "datadog.trace_agent.receiver.error");
            assert_eq!(tags, ["handler:traces", "v:v0.4", error_tag], "{body:?}");
            assert_eq!(errors, 1.0);
            let (tags, dropped) = counter(&counters, "datadog.trace_agent.normalizer.traces_dropped");
            assert_eq!(tags.last().map(String::as_str), Some(reason_tag), "{body:?}");
            assert_eq!(dropped, 10.0);
        }
    }

    // `TestHandleWithVersionRejectCrossSite` and the v0.2 case of `TestReceiverMsgpackDecoder` in `api_test.go`.
    #[tokio::test]
    async fn a_msgpack_payload_on_v02_is_an_unsupported_media_type() {
        let (intake, mut traces) = intake(config());

        // As `handleWithVersion`, the media type is checked before the cross-site header.
        let headers = [MSGPACK, ("Sec-Fetch-Site", "cross-site")];
        let (status, response_headers, body) =
            send_to(&intake, EndpointVersion::V02, sized_request(&headers, v04_payload(1))).await;

        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(response_headers[CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(response_headers[X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(body, "unsupported media type: \"application/msgpack\"\n");
        assert!(traces.try_recv().is_err());
        let counters = counters(&intake);
        let (tags, errors) = counter(&counters, "datadog.trace_agent.receiver.error");
        assert_eq!(tags, ["error:format-error", "version:v0.2"]);
        assert_eq!(errors, 1.0);
    }

    #[tokio::test]
    async fn v03_answers_ok_and_v04_the_rates() {
        let (intake, mut traces, version) = intake_with_rates(config());
        let headers = [MSGPACK, ("Datadog-Rates-Payload-Version", "stale")];

        let (status, response_headers, body) =
            send_to(&intake, EndpointVersion::V03, sized_request(&headers, v04_payload(1))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response_headers[CONTENT_TYPE], "text/plain; charset=utf-8");
        assert!(!response_headers.contains_key(&RATES_PAYLOAD_VERSION_HEADER));
        assert_eq!(body, "OK\n");
        assert_eq!(traces.try_recv().unwrap().len(), 1);

        let (status, response_headers, body) =
            send_to(&intake, EndpointVersion::V04, sized_request(&headers, v04_payload(1))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response_headers[CONTENT_TYPE], "application/json");
        assert_eq!(response_headers[&RATES_PAYLOAD_VERSION_HEADER], version.as_str());
        assert_eq!(body, PUBLISHED_RATES_BODY);
        assert_eq!(traces.try_recv().unwrap().len(), 1);

        // `replyOK` sizes `OK` too.
        let events = intake.telemetry.take_window_events();
        let endpoints: Vec<_> = events
            .iter()
            .filter_map(|event| event.try_as_metric())
            .filter(|metric| metric.context().name() == "datadog.trace_agent.receiver.rate_response_bytes.max")
            .map(|metric| {
                let endpoint = metric
                    .context()
                    .tags()
                    .into_iter()
                    .find_map(|tag| tag.as_str().strip_prefix("endpoint:").map(String::from));
                (endpoint.unwrap(), metric.values().clone())
            })
            .collect();
        assert_eq!(endpoints.len(), 2);
        assert!(endpoints.contains(&("traces_v0.3".to_string(), MetricValues::gauge(3.0))));
        assert!(endpoints.contains(&(
            "traces_v0.4".to_string(),
            MetricValues::gauge(PUBLISHED_RATES_BODY.len() as f64)
        )));
    }

    #[tokio::test]
    async fn a_refused_v03_payload_gets_ok() {
        let (intake, _traces) = intake(config());
        let _busy = intake.decoders.clone().acquire_owned().await.unwrap();

        let (status, response_headers, body) =
            send_to(&intake, EndpointVersion::V03, sized_request(&[MSGPACK], v04_payload(1))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(response_headers[CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(body, "OK\n");
    }

    #[tokio::test]
    async fn a_v04_payload_takes_its_tracer_fields_and_container_from_the_headers() {
        let container_tags = ContainerTagsHasher::new(None, Some(Arc::new(TaggingProvider)));
        let (intake, mut traces) = intake(config());
        let long_lang = "l".repeat(MAX_META_VALUE_LEN + 1);
        let headers = [
            MSGPACK,
            ("Datadog-Meta-Lang", long_lang.as_str()),
            ("Datadog-Meta-Lang-Version", "3.12"),
            ("Datadog-Meta-Tracer-Version", "2.0.0"),
            ("Datadog-Container-ID", "from-header"),
        ];

        let request = sized_request(&headers, v04_payload(2));
        let response = intake.handle(EndpointVersion::V04, request, &container_tags).await;
        assert_eq!(response.status(), StatusCode::OK);

        let traces = traces.try_recv().unwrap();
        assert_eq!(traces.len(), 2);
        for trace in &traces {
            // Set as sent, where the telemetry tags are truncated.
            assert_eq!(trace.payload.language_name, long_lang.as_str());
            assert_eq!(trace.payload.language_version, "3.12");
            assert_eq!(trace.payload.tracer_version, "2.0.0");
            assert_eq!(trace.payload.container_id, "from-header");
            // Promoted from the spans by the decoder.
            assert_eq!(trace.payload.env, "prod");
        }
        let receiver = traces[0].payload.receiver();
        assert_eq!(receiver.endpoint_version, "v0.4");
        assert_eq!(receiver.lang.len(), MAX_META_VALUE_LEN);
        assert_eq!(receiver.service, "web");
        assert!(receiver.container_tags.contains("container_id:from-header"));
    }

    #[tokio::test]
    async fn a_payload_that_ends_early_is_counted_as_msgp_short_bytes() {
        for (version, payload) in [
            (EndpointVersion::V04, v04_payload(2)),
            (EndpointVersion::V10, v1_payload(None, 2)),
        ] {
            let (intake, _traces) = intake(config());
            let truncated = payload[..payload.len() - 3].to_vec();
            let headers = [MSGPACK, ("X-Datadog-Trace-Count", "2")];

            let (status, _, body) = send_to(&intake, version, sized_request(&headers, truncated)).await;

            assert_eq!(status, StatusCode::BAD_REQUEST, "{version:?}");
            assert_eq!(body, "msgp: too few bytes left to read object\n", "{version:?}");
            let counters = counters(&intake);
            let (tags, errors) = counter(&counters, "datadog.trace_agent.receiver.error");
            let version_tag = format!("v:{}", version.as_str());
            assert_eq!(tags, ["handler:traces", version_tag.as_str(), "error:decoding-error"]);
            assert_eq!(errors, 1.0);
            let (tags, dropped) = counter(&counters, "datadog.trace_agent.normalizer.traces_dropped");
            assert_eq!(tags.last().unwrap(), "reason:msgp_short_bytes", "{version:?}");
            assert_eq!(dropped, 2.0);
        }
    }

    #[tokio::test]
    async fn accepts_a_payload_and_hands_its_traces_to_the_source() {
        let (intake, mut traces) = intake(config());

        let (status, headers, body) = send(&intake, sized_request(&[], v1_payload(None, 2))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[CONTENT_TYPE], "application/json");
        assert_eq!(body, "{\"rate_by_service\":{}}\n");
        let traces = traces.try_recv().expect("the traces should be handed over");
        assert_eq!(traces.len(), 2);
        assert!(Arc::ptr_eq(
            traces[0].payload.receiver.as_ref().unwrap(),
            traces[1].payload.receiver.as_ref().unwrap()
        ));
        let receiver = traces[0].payload.receiver();
        assert_eq!(receiver.endpoint_version, "v1.0");
        // Resolved from the first span, as `getProcessTagsV1` does.
        assert_eq!(receiver.process_tags, "entrypoint.name:app");
    }

    /// Returns an intake whose sampler has published one rate, and that rate's version.
    fn intake_with_rates(config: TraceIntakeConfiguration) -> (TraceIntake, mpsc::Receiver<Vec<Trace>>, String) {
        let service_rates = ServiceRates::default();
        RateByService::new(service_rates.clone()).set_all([("service:web,env:prod".to_string(), 0.5)]);
        let version = service_rates.load().version.to_str().unwrap().to_string();
        let (intake, traces) = intake(TraceIntakeConfiguration {
            service_rates,
            ..config
        });
        (intake, traces, version)
    }

    const PUBLISHED_RATES_BODY: &str = "{\"rate_by_service\":{\"service:web,env:prod\":0.5}}\n";

    #[tokio::test]
    async fn an_accepted_payload_gets_the_rates_unless_the_client_has_their_version() {
        let (intake, _traces, version) = intake_with_rates(config());
        let cases = [
            (None, None, PUBLISHED_RATES_BODY),
            (Some(""), None, PUBLISHED_RATES_BODY),
            (Some("stale"), Some(version.as_str()), PUBLISHED_RATES_BODY),
            (Some(version.as_str()), Some(version.as_str()), "{}"),
        ];
        for (sent, echoed, expected_body) in cases {
            let headers: Vec<_> = sent.map(|v| ("Datadog-Rates-Payload-Version", v)).into_iter().collect();

            let (status, response_headers, body) = send(&intake, sized_request(&headers, v1_payload(None, 1))).await;

            assert_eq!(status, StatusCode::OK, "sent {sent:?}");
            assert_eq!(response_headers[CONTENT_TYPE], "application/json");
            assert_eq!(
                response_headers
                    .get(&RATES_PAYLOAD_VERSION_HEADER)
                    .map(|v| v.to_str().unwrap()),
                echoed,
                "sent {sent:?}"
            );
            assert_eq!(body, expected_body, "sent {sent:?}");
        }
    }

    // `TestHTTPRateByService` in `responses_test.go`.
    #[tokio::test]
    async fn a_client_version_gets_the_empty_version_before_the_first_rates() {
        let (intake, _traces) = intake(config());
        let headers = [("Datadog-Rates-Payload-Version", "abc")];

        let (_, response_headers, body) = send(&intake, sized_request(&headers, v1_payload(None, 1))).await;

        assert_eq!(response_headers[&RATES_PAYLOAD_VERSION_HEADER], "");
        assert_eq!(body, "{\"rate_by_service\":{}}\n");
    }

    #[tokio::test]
    async fn a_refused_payload_gets_the_rates_without_their_version() {
        let (intake, _traces, version) = intake_with_rates(config());
        let _busy = intake.decoders.clone().acquire_owned().await.unwrap();

        for (sent, expected_body) in [("stale", PUBLISHED_RATES_BODY), (version.as_str(), "{}")] {
            let headers = [("Datadog-Rates-Payload-Version", sent)];
            let (status, response_headers, body) = send(&intake, sized_request(&headers, v1_payload(None, 1))).await;

            assert_eq!(status, StatusCode::OK);
            assert_eq!(response_headers[CONTENT_TYPE], "application/json");
            assert!(!response_headers.contains_key(&RATES_PAYLOAD_VERSION_HEADER));
            assert_eq!(body, expected_body, "sent {sent:?}");
        }
    }

    #[tokio::test]
    async fn the_size_of_the_rates_sent_is_reported_by_endpoint() {
        let (intake, _traces, version) = intake_with_rates(config());
        send(&intake, sized_request(&[], v1_payload(None, 1))).await;
        let headers = [("Datadog-Rates-Payload-Version", version.as_str())];
        send(&intake, sized_request(&headers, v1_payload(None, 1))).await;

        let events = intake.telemetry.take_window_events();
        let find = |name: &str| {
            events
                .iter()
                .filter_map(|event| event.try_as_metric())
                .find(|metric| metric.context().name() == name)
                .unwrap_or_else(|| panic!("{name} is missing"))
        };
        let max = find("datadog.trace_agent.receiver.rate_response_bytes.max");
        let tags: Vec<_> = max
            .context()
            .tags()
            .into_iter()
            .map(|tag| tag.as_str().to_string())
            .collect();
        assert_eq!(
            tags,
            [
                "endpoint_version:v1.0",
                "connection_type:unknown",
                "service:checkout-service",
                "endpoint:traces_v1.0"
            ]
        );
        assert_eq!(*max.values(), MetricValues::gauge(PUBLISHED_RATES_BODY.len() as f64));
        let count = find("datadog.trace_agent.receiver.rate_response_bytes.count");
        assert_eq!(*count.values(), MetricValues::rate(2.0, Duration::from_secs(1)));
    }

    /// Returns the value of each counter the intake reported, by name, with its tags in order.
    fn counters(intake: &TraceIntake) -> Vec<(String, Vec<String>, f64)> {
        intake
            .telemetry
            .take_window_events()
            .iter()
            .filter_map(|event| {
                let metric = event.try_as_metric()?;
                let MetricValues::Rate(points, _) = metric.values() else {
                    return None;
                };
                let tags = metric.context().tags().into_iter().map(|tag| tag.as_str().to_string());
                let value = points.into_iter().map(|(_, value)| value).sum();
                Some((metric.context().name().to_string(), tags.collect(), value))
            })
            .collect()
    }

    fn counter(counters: &[(String, Vec<String>, f64)], name: &str) -> (Vec<String>, f64) {
        counters
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, tags, value)| (tags.clone(), *value))
            .unwrap_or_else(|| panic!("{name} is missing: {counters:?}"))
    }

    #[tokio::test]
    async fn an_accepted_payload_is_counted_under_its_header_and_service_tags() {
        let (intake, mut traces) = intake(config());
        let headers = [
            ("Datadog-Meta-Lang", "python"),
            ("Datadog-Meta-Lang-Version", "3.12"),
            ("Datadog-Meta-Tracer-Version", "2.0.0"),
            ("Datadog-Client-Dropped-P0-Traces", "5"),
            ("Datadog-Client-Dropped-P0-Spans", "7"),
        ];
        let payload = v1_payload(None, 2);
        let payload_len = payload.len();

        let (status, _, _) = send(&intake, sized_request(&headers, payload)).await;

        assert_eq!(status, StatusCode::OK);
        let receiver = traces.try_recv().unwrap()[0].payload.receiver.clone().unwrap();
        assert_eq!(receiver.lang, "python");
        assert_eq!(receiver.service, "checkout-service");

        let counters = counters(&intake);
        let expected_tags = [
            "lang:python",
            "lang_version:3.12",
            "tracer_version:2.0.0",
            "endpoint_version:v1.0",
            "connection_type:unknown",
            "service:checkout-service",
        ];
        for (name, expected) in [
            ("datadog.trace_agent.receiver.traces_received", 2.0),
            ("datadog.trace_agent.receiver.traces_bytes", payload_len as f64),
            ("datadog.trace_agent.receiver.spans_received", 2.0),
            ("datadog.trace_agent.receiver.payload_accepted", 1.0),
            ("datadog.trace_agent.receiver.client_dropped_p0_traces", 5.0),
            ("datadog.trace_agent.receiver.client_dropped_p0_spans", 7.0),
        ] {
            assert_eq!(
                counter(&counters, name),
                (expected_tags.map(String::from).to_vec(), expected),
                "{name}"
            );
        }
    }

    #[test]
    fn the_service_tag_is_truncated_as_the_normalizer_truncates_services() {
        let long = "s".repeat(MAX_SERVICE_LEN + 10);
        let trace = Trace::new(
            vec![saluki_core::data_model::event::trace::Span::default().with_service(long.as_str())],
            saluki_core::data_model::event::trace::TraceIngestInfo::Datadog,
        );
        assert_eq!(first_service(&[trace]).len(), MAX_SERVICE_LEN);
        assert!(first_service(&[]).is_empty());
    }

    #[tokio::test]
    async fn a_payload_that_does_not_decode_counts_an_error_and_its_dropped_traces() {
        let (intake, _traces) = intake(config());
        let headers = [("Datadog-Meta-Lang", "go"), ("X-Datadog-Trace-Count", "3")];

        let (status, _, _) = send(&intake, sized_request(&headers, vec![0xc1])).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        let counters = counters(&intake);
        let (tags, errors) = counter(&counters, "datadog.trace_agent.receiver.error");
        assert_eq!(tags, ["handler:traces", "v:v1.0", "error:decoding-error"]);
        assert_eq!(errors, 1.0);
        let (tags, dropped) = counter(&counters, "datadog.trace_agent.normalizer.traces_dropped");
        assert_eq!(
            tags,
            [
                "lang:go",
                "endpoint_version:v1.0",
                "connection_type:unknown",
                "reason:decoding_error"
            ]
        );
        assert_eq!(dropped, 3.0);
        assert_eq!(
            counter(&counters, "datadog.trace_agent.receiver.payload_accepted").1,
            0.0
        );
    }

    #[tokio::test]
    async fn rejected_bodies_count_their_error() {
        let too_long = (LIMIT + 1).to_string();
        let cases = [
            (
                request(&[("Content-Length", &too_long)], Body::empty()),
                "error:payload-too-large",
            ),
            (
                request(&[], streamed(vec![Err(std::io::Error::other("closed"))])),
                "error:unexpected-eof",
            ),
        ];
        for (request, expected) in cases {
            let (intake, _traces) = intake(config());
            send(&intake, request).await;
            let (tags, errors) = counter(&counters(&intake), "datadog.trace_agent.receiver.error");
            assert_eq!(tags[2], expected);
            assert_eq!(errors, 1.0);
        }
    }

    #[tokio::test]
    async fn rejects_cross_site_requests() {
        let (intake, _traces) = intake(config());
        let (status, headers, body) = send(
            &intake,
            sized_request(&[("Sec-Fetch-Site", "cross-site")], v1_payload(None, 1)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(headers[CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(body, "cross-site request rejected\n");

        let (status, _, _) = send(
            &intake,
            sized_request(&[("Sec-Fetch-Site", "same-origin")], v1_payload(None, 1)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_a_declared_length_over_the_limit_before_reading() {
        let (intake, _traces) = intake(config());
        let too_long = (LIMIT + 1).to_string();
        // The body is never read: a stream that would fail is never polled.
        let body = streamed(vec![Err(std::io::Error::other("never read"))]);
        let (status, _, body) = send(&intake, request(&[("Content-Length", &too_long)], body)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body, "payload-too-large\n");
    }

    #[tokio::test]
    async fn rejects_a_body_that_reaches_the_limit_while_read() {
        let (intake, _traces) = intake(config());
        let half = Bytes::from(vec![0u8; LIMIT as usize / 2 + 1]);
        let body = streamed(vec![
            Ok(half.clone()),
            Ok(half),
            Err(std::io::Error::other("never read")),
        ]);
        let (status, _, _) = send(&intake, request(&[], body)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

        // As Go's `LimitedReader`, a body of exactly the limit is too large.
        let (status, _, _) = send(&intake, request(&[], Body::from(vec![0u8; LIMIT as usize]))).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_body_that_ends_early_is_an_unexpected_eof() {
        let (intake, _traces) = intake(config());
        let body = streamed(vec![
            Ok(Bytes::from_static(b"\x81")),
            Err(std::io::Error::other("closed")),
        ]);
        let (status, _, body) = send(&intake, request(&[], body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, "unexpected-eof\n");
    }

    #[tokio::test]
    async fn a_payload_that_does_not_decode_is_a_bad_request() {
        let (intake, mut traces) = intake(config());
        let (status, _, _) = send(&intake, sized_request(&[], vec![0xc1])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(traces.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_body_not_read_within_the_receiver_timeout_times_out() {
        let (intake, _traces) = intake(TraceIntakeConfiguration {
            receiver_timeout: Duration::from_millis(50),
            ..config()
        });
        let body = Body::from_stream(stream::pending::<Result<Bytes, Infallible>>());
        let (status, _, body) = send(&intake, request(&[], body)).await;
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(body, "timeout\n");
    }

    #[tokio::test]
    async fn a_payload_with_no_free_decoder_in_time_is_refused() {
        type Case<'a> = (bool, &'a [(&'a str, &'a str)], StatusCode);
        let cases: &[Case<'_>] = &[
            (false, &[], StatusCode::OK),
            (false, &[("Datadog-Send-Real-Http-Status", "false")], StatusCode::OK),
            (
                false,
                &[("Datadog-Send-Real-Http-Status", "true")],
                StatusCode::TOO_MANY_REQUESTS,
            ),
            (true, &[], StatusCode::TOO_MANY_REQUESTS),
        ];
        for (feature_429, headers, expected) in cases {
            let (intake, mut traces) = intake(TraceIntakeConfiguration {
                reject_with_too_many_requests: *feature_429,
                ..config()
            });
            let _busy = intake.decoders.clone().acquire_owned().await.unwrap();

            let (status, response_headers, body) = send(&intake, sized_request(headers, v1_payload(None, 1))).await;

            assert_eq!(status, *expected, "feature 429: {feature_429}, headers: {headers:?}");
            assert_eq!(response_headers[CONTENT_TYPE], "application/json");
            assert_eq!(body, "{\"rate_by_service\":{}}\n");
            assert!(traces.try_recv().is_err(), "a refused payload is dropped");
            let counters = counters(&intake);
            assert_eq!(
                counter(&counters, "datadog.trace_agent.receiver.payload_refused").1,
                1.0
            );
            assert_eq!(
                counter(&counters, "datadog.trace_agent.receiver.payload_accepted").1,
                0.0
            );
        }
    }

    #[tokio::test]
    async fn a_request_that_times_out_waiting_for_a_decoder_gets_too_many_requests() {
        let (intake, _traces) = intake(TraceIntakeConfiguration {
            receiver_timeout: Duration::from_millis(20),
            decoder_timeout: Duration::from_secs(5),
            ..config()
        });
        let _busy = intake.decoders.clone().acquire_owned().await.unwrap();

        let (status, _, body) = send(&intake, sized_request(&[], v1_payload(None, 1))).await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(body.is_empty());
        assert_eq!(
            counter(&counters(&intake), "datadog.trace_agent.receiver.payload_timeout").1,
            1.0
        );
    }

    #[test]
    fn header_truth_follows_go_parse_bool() {
        let cases = [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("f"), false),
            (Some("F"), false),
            (Some("false"), false),
            (Some("FALSE"), false),
            (Some("False"), false),
            (Some("1"), true),
            (Some("t"), true),
            (Some("true"), true),
            (Some("TRUE"), true),
            // Not a boolean to Go, so true.
            (Some("yes"), true),
            (Some("fAlSe"), true),
            (Some(" false"), true),
        ];
        for (value, expected) in cases {
            assert_eq!(is_header_true(value), expected, "{value:?}");
        }
    }

    #[test]
    fn receiver_metadata_comes_from_the_request_headers_and_connection() {
        let headers = HeaderMap::from_iter(
            [
                ("datadog-client-computed-stats", "yes"),
                ("datadog-client-computed-top-level", "false"),
                ("datadog-client-dropped-p0-traces", "6"),
                ("datadog-meta-lang", "python"),
                ("datadog-meta-lang-version", "3.12"),
                ("datadog-meta-tracer-version", "2.0.0"),
                ("datadog-meta-lang-interpreter", "CPython"),
                ("datadog-meta-lang-interpreter-vendor", "python-foundation"),
            ]
            .map(|(name, value)| (HeaderName::from_static(name), HeaderValue::from_static(value))),
        );
        let uds = ConnectInfo(ConnectionAddress::from(ProcessCredentials { pid: 1, uid: 0, gid: 0 }));

        let receiver = receiver_metadata(EndpointVersion::V10, &headers, Some(&uds), 4);

        assert!(receiver.client_computed_stats);
        assert!(!receiver.client_computed_top_level);
        assert_eq!(receiver.client_dropped_p0s_weight, 1.5);
        assert_eq!(receiver.lang, "python");
        assert_eq!(receiver.lang_version, "3.12");
        assert_eq!(receiver.tracer_version, "2.0.0");
        assert_eq!(receiver.interpreter, "CPython");
        assert_eq!(receiver.lang_vendor, "python-foundation");
        assert_eq!(receiver.endpoint_version, "v1.0");
        assert_eq!(receiver.connection_type, "uds");

        let tcp = ConnectInfo(ConnectionAddress::from(std::net::SocketAddr::from(([127, 0, 0, 1], 1))));
        let receiver = receiver_metadata(EndpointVersion::V10, &HeaderMap::new(), Some(&tcp), 0);
        assert_eq!(receiver.client_dropped_p0s_weight, 0.0);
        assert_eq!(receiver.connection_type, "tcp");
        assert_eq!(
            receiver_metadata(EndpointVersion::V10, &HeaderMap::new(), None, 0).connection_type,
            "unknown"
        );
    }

    #[test]
    fn long_meta_values_are_truncated_on_a_character_boundary() {
        assert_eq!(truncate_utf8("CPython"), "CPython");
        let long = "é".repeat(MAX_META_VALUE_LEN);
        assert_eq!(truncate_utf8(&long).len(), MAX_META_VALUE_LEN);
        let odd = format!("a{long}");
        assert_eq!(truncate_utf8(&odd).len(), MAX_META_VALUE_LEN - 1);
    }

    /// Tags every container with its own ID, tags cgroup inode 42 as the tagger tags its container `from-inode`, and
    /// resolves any External Data to the container `from-external-data`.
    struct TaggingProvider;

    impl WorkloadProvider for TaggingProvider {
        fn get_tags_for_entity(&self, entity_id: &EntityId, _: OriginTagCardinality) -> Option<SharedTagSet> {
            let container_id = match entity_id {
                EntityId::Container(container_id) => container_id.to_string(),
                EntityId::ContainerInode(42) => "from-inode".to_string(),
                _ => return None,
            };
            let mut tags = TagSet::default();
            tags.insert_tag(Tag::from(format!("container_id:{container_id}")));
            tags.insert_tag(Tag::from("kube_namespace:prod"));
            Some(tags.into_shared())
        }

        fn get_resolved_origin(&self, _: saluki_core::data_model::origin::RawOrigin<'_>) -> Option<ResolvedOrigin> {
            Some(ResolvedOrigin::from_parts(
                None,
                None,
                None,
                None,
                Some(ResolvedExternalData::new(
                    EntityId::PodUid("pod".into()),
                    EntityId::Container("from-external-data".into()),
                )),
            ))
        }
    }

    #[tokio::test]
    async fn the_request_container_is_found_from_its_inode_or_external_data() {
        let container_tags = ContainerTagsHasher::new(None, Some(Arc::new(TaggingProvider)));
        let (intake, mut traces) = intake(config());

        let cases = [
            (("Datadog-Entity-ID", "in-42"), "from-inode"),
            (("Datadog-External-Env", "it-false,cn-app,pu-pod"), "from-external-data"),
        ];
        for (header, expected) in cases {
            let request = sized_request(&[header], v1_payload(None, 1));
            let response = intake.handle(EndpointVersion::V10, request, &container_tags).await;
            assert_eq!(response.status(), StatusCode::OK);

            let traces = traces.try_recv().unwrap();
            assert_eq!(traces[0].payload.container_id, expected, "{header:?}");
            let mut tags: Vec<_> = traces[0].payload.receiver().container_tags.split(',').collect();
            tags.sort_unstable();
            assert_eq!(
                tags,
                [format!("container_id:{expected}").as_str(), "kube_namespace:prod"]
            );
        }
    }

    #[tokio::test]
    async fn the_payload_container_id_wins_over_the_request_container_id() {
        let container_tags = ContainerTagsHasher::new(None, Some(Arc::new(TaggingProvider)));
        let (intake, mut traces) = intake(config());
        let header = [("Datadog-Container-ID", "from-header")];

        for (payload_container_id, expected) in [(Some("from-payload"), "from-payload"), (None, "from-header")] {
            let request = sized_request(&header, v1_payload(payload_container_id, 1));
            let response = intake.handle(EndpointVersion::V10, request, &container_tags).await;
            assert_eq!(response.status(), StatusCode::OK);

            let traces = traces.try_recv().unwrap();
            assert_eq!(traces[0].payload.container_id, expected);
            let tags = &traces[0].payload.receiver().container_tags;
            let mut tags: Vec<_> = tags.split(',').collect();
            tags.sort_unstable();
            assert_eq!(
                tags,
                [format!("container_id:{expected}").as_str(), "kube_namespace:prod"]
            );
        }
    }
}
