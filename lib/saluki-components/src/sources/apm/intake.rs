//! Native trace intake.
//!
//! Serves the trace endpoints the way the trace-agent's receiver does: it rejects cross-site requests, bounds the body
//! size, decodes at most `decoders` payloads at once, and answers with the trace-agent's status codes. Each request's
//! receiver metadata (client flags, the tracer's container and process tags, the connection) is resolved once and
//! shared by every trace of the payload.
//!
//! Status codes, in the order a request can meet them:
//!
//! | Condition                                        | Status                    |
//! |--------------------------------------------------|---------------------------|
//! | `Sec-Fetch-Site: cross-site`                     | 403                       |
//! | `receiver_timeout` elapses waiting for a decoder | 429, no body              |
//! | `decoder_timeout` elapses waiting for a decoder  | 200, or 429 when opted in |
//! | `Content-Length` over `max_request_bytes`        | 413                       |
//! | Body reaches `max_request_bytes`                 | 413                       |
//! | `receiver_timeout` elapses reading the body      | 408                       |
//! | The body ends early                              | 400 `unexpected-eof`      |
//! | The payload doesn't decode                       | 400                       |
//! | Accepted                                         | 200                       |
//!
//! A request opts in to 429 with the `429` APM feature or the `Datadog-Send-Real-Http-Status` header. Either way, the
//! body is drained first.

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
use crate::decoders::datadog::{decode_v1_payload, first_span, resolve_process_tags, resolve_sdk_otlp_export};

const SEC_FETCH_SITE_HEADER: HeaderName = HeaderName::from_static("sec-fetch-site");
const PROCESS_TAGS_HEADER: HeaderName = HeaderName::from_static("x-datadog-process-tags");
const LANG_HEADER: HeaderName = HeaderName::from_static("datadog-meta-lang");
const LANG_INTERPRETER_HEADER: HeaderName = HeaderName::from_static("datadog-meta-lang-interpreter");
const LANG_INTERPRETER_VENDOR_HEADER: HeaderName = HeaderName::from_static("datadog-meta-lang-interpreter-vendor");
const COMPUTED_TOP_LEVEL_HEADER: HeaderName = HeaderName::from_static("datadog-client-computed-top-level");
const COMPUTED_STATS_HEADER: HeaderName = HeaderName::from_static("datadog-client-computed-stats");
const DROPPED_P0_TRACES_HEADER: HeaderName = HeaderName::from_static("datadog-client-dropped-p0-traces");
const SEND_REAL_HTTP_STATUS_HEADER: HeaderName = HeaderName::from_static("datadog-send-real-http-status");

/// Longest `Datadog-Meta-*` value kept, in bytes, as the trace-agent bounds them.
const MAX_META_VALUE_LEN: usize = 200;

/// Body of an accepted payload until the receiver reports sampling rates.
///
/// Encoded as the trace-agent writes it, with a trailing newline.
const RATE_BY_SERVICE_BODY: &str = "{\"rate_by_service\":{}}\n";

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
}

/// An intake endpoint version, as the trace-agent's `Version`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EndpointVersion {
    /// `/v1.0/traces`.
    V10,
}

impl EndpointVersion {
    fn as_meta_string(self) -> MetaString {
        match self {
            Self::V10 => MetaString::from_static("v1.0"),
        }
    }
}

/// Handles trace intake requests, and hands their traces to the source.
#[derive(Clone)]
pub(super) struct TraceIntake {
    config: Arc<TraceIntakeConfiguration>,
    decoders: Arc<Semaphore>,
    traces: mpsc::Sender<Vec<Trace>>,
}

impl TraceIntake {
    pub(super) fn new(config: TraceIntakeConfiguration, traces: mpsc::Sender<Vec<Trace>>) -> Self {
        Self {
            decoders: Arc::new(Semaphore::new(config.decoders.get())),
            config: Arc::new(config),
            traces,
        }
    }

    /// Handles a trace payload sent to the endpoint of `version`.
    pub(super) async fn handle(
        &self, version: EndpointVersion, request: AxumRequest, container_tags: &ContainerTagsHasher,
    ) -> Response {
        // As in the trace-agent, the read timeout runs from the arrival of the request.
        let deadline = Instant::now() + self.config.receiver_timeout;
        let (parts, body) = request.into_parts();
        let headers = &parts.headers;

        if header_str(headers, &SEC_FETCH_SITE_HEADER) == Some("cross-site") {
            return http_error(StatusCode::FORBIDDEN, "cross-site request rejected");
        }

        let _permit = select! {
            biased;
            permit = self.decoders.clone().acquire_owned() => permit.expect("the decoder semaphore is never closed"),
            _ = sleep_until(deadline) => {
                debug!("Request timed out waiting for a decoder. Payload dropped.");
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
                return rate_by_service(status);
            },
        };

        let body = match read_body(headers, body, self.config.max_request_bytes, deadline, true).await {
            Ok(body) => body,
            Err(error) => return error.into_response(),
        };

        let decoded = match version {
            EndpointVersion::V10 => decode_v1_payload(&body),
        };
        // The decoded traces own their data, so the body is released before they wait for the pipeline, as the
        // trace-agent releases its buffer before sending the payload on.
        drop(body);
        let decoded = match decoded {
            Ok(decoded) => decoded,
            Err(error) => {
                debug!(%error, ?version, "Cannot decode traces payload.");
                return http_error(StatusCode::BAD_REQUEST, &error.to_string());
            }
        };
        let mut traces = decoded.traces;
        let connection = parts.extensions.get::<ConnectInfo<ConnectionAddress>>();

        // A v1.0 payload's own container ID wins over the request's.
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
        for trace in &mut traces {
            if let Some(container_id) = &container_id {
                trace.payload.container_id = container_id.clone();
            }
            trace.payload.receiver = Some(Arc::clone(&receiver));
        }

        if !traces.is_empty() && self.traces.send(traces).await.is_err() {
            debug!("APM source stopped. Payload dropped.");
        }

        rate_by_service(StatusCode::OK)
    }
}

/// Builds the receiver metadata that comes from the request alone.
fn receiver_metadata(
    version: EndpointVersion, headers: &HeaderMap, connection: Option<&ConnectInfo<ConnectionAddress>>,
    chunk_count: usize,
) -> ReceiverMetadata {
    let meta = |name: &HeaderName| MetaString::from(truncate_utf8(header_str(headers, name).unwrap_or_default()));

    // Go divides by the payload's chunk count when it processes the payload, after dropping invalid chunks; dividing
    // by the decoded count spreads the weight over slightly more chunks when some are dropped.
    let dropped_p0s = header_str(headers, &DROPPED_P0_TRACES_HEADER)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    let client_dropped_p0s_weight = if chunk_count == 0 {
        0.0
    } else {
        dropped_p0s as f64 / chunk_count as f64
    };

    ReceiverMetadata {
        client_dropped_p0s_weight,
        client_computed_stats: is_header_true(header_str(headers, &COMPUTED_STATS_HEADER)),
        client_computed_top_level: is_header_true(header_str(headers, &COMPUTED_TOP_LEVEL_HEADER)),
        lang: meta(&LANG_HEADER),
        interpreter: meta(&LANG_INTERPRETER_HEADER),
        lang_vendor: meta(&LANG_INTERPRETER_VENDOR_HEADER),
        endpoint_version: version.as_meta_string(),
        connection_type: connection_type(connection),
        ..ReceiverMetadata::new()
    }
}

/// Returns the connection type the trace-agent reports for a connection.
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

/// Returns whether a header value is set and not false, as the trace-agent reads boolean headers.
///
/// The false values are the ones the trace-agent parses as false. A value it can't parse is true.
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

impl IntoResponse for BodyError {
    fn into_response(self) -> Response {
        match self {
            Self::TooLarge => http_error(StatusCode::PAYLOAD_TOO_LARGE, "payload-too-large"),
            Self::UnexpectedEof => http_error(StatusCode::BAD_REQUEST, "unexpected-eof"),
            Self::Timeout => http_error(StatusCode::REQUEST_TIMEOUT, "timeout"),
        }
    }
}

/// Reads a request body of fewer than `limit` bytes before `deadline`, keeping it only if `keep` is set.
///
/// As in the trace-agent, a body is too large once it reaches the limit, so a body of exactly `limit`
/// bytes is rejected too; a `Content-Length` over the limit is rejected before reading.
async fn read_body(
    headers: &HeaderMap, body: Body, limit: u64, deadline: Instant, keep: bool,
) -> Result<Bytes, BodyError> {
    // As in the trace-agent, an unparseable `Content-Length` is ignored.
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

/// Answers with the sampling rates by service, as the trace-agent does.
///
/// shortcut: the rates are always empty; APMSP-4286 reports the sampler's rates.
fn rate_by_service(status: StatusCode) -> Response {
    (
        status,
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        RATE_BY_SERVICE_BODY,
    )
        .into_response()
}

/// Answers with an error, as the trace-agent does.
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
    use saluki_core::data_model::origin::OriginTagCardinality;
    use saluki_core::data_model::tags::{SharedTagSet, Tag, TagSet};
    use saluki_env::workload::origin::{ResolvedExternalData, ResolvedOrigin};
    use saluki_env::workload::{EntityId, WorkloadProvider};
    use saluki_io::net::ProcessCredentials;

    use super::*;

    const LIMIT: u64 = 4096;

    fn config() -> TraceIntakeConfiguration {
        TraceIntakeConfiguration {
            max_request_bytes: LIMIT,
            decoders: NonZeroUsize::MIN,
            decoder_timeout: Duration::from_millis(50),
            receiver_timeout: Duration::from_secs(5),
            reject_with_too_many_requests: false,
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
        let response = intake
            .handle(EndpointVersion::V10, request, &ContainerTagsHasher::default())
            .await;
        let (parts, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        (parts.status, parts.headers, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn accepts_a_payload_and_hands_its_traces_to_the_source() {
        let (intake, mut traces) = intake(config());

        let (status, headers, body) = send(&intake, sized_request(&[], v1_payload(None, 2))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[CONTENT_TYPE], "application/json");
        assert_eq!(body, RATE_BY_SERVICE_BODY);
        let traces = traces.try_recv().expect("the traces should be handed over");
        assert_eq!(traces.len(), 2);
        assert!(Arc::ptr_eq(
            traces[0].payload.receiver.as_ref().unwrap(),
            traces[1].payload.receiver.as_ref().unwrap()
        ));
        let receiver = traces[0].payload.receiver();
        assert_eq!(receiver.endpoint_version, "v1.0");
        // Resolved from the first span, as in the trace-agent.
        assert_eq!(receiver.process_tags, "entrypoint.name:app");
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

        // As in the trace-agent, a body of exactly the limit is too large.
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
            assert_eq!(body, RATE_BY_SERVICE_BODY);
            assert!(traces.try_recv().is_err(), "a refused payload is dropped");
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
