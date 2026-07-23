use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    task::{ready, Context, Poll},
};

use http::{uri::Authority, Request, Response, StatusCode, Uri, Version};
use http_body::Body;
use metrics::Counter;
use pin_project_lite::pin_project;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tower::{Layer, Service};

pub type EndpointNameFn = dyn Fn(&Uri) -> Option<MetaString> + Send + Sync;

const ERROR_TYPE_CLIENT: &str = "client_error";
pub(super) const ERROR_TYPE_CONNECTION: &str = "connection_error";
pub(super) const ERROR_TYPE_DNS: &str = "dns_error";
pub(super) const ERROR_TYPE_TLS: &str = "tls_error";
pub(super) const ERROR_TYPE_WROTE_REQUEST: &str = "wrote_request_error";
const ERROR_TYPE_CANT_SEND: &str = "cant_send";
const ERROR_TYPE_GT_400: &str = "gt_400";
const ERROR_SCOPE_PHASE: &str = "phase";
const ERROR_SCOPE_TRANSACTION: &str = "transaction";

/// Emits lifecycle and transaction error telemetry for HTTP requests.
#[derive(Clone)]
pub(crate) struct HttpTransactionErrorTelemetry {
    dns_errors: Counter,
    connection_errors: Counter,
    tls_errors: Counter,
    wrote_request_errors: Counter,
    send_errors: Counter,
    http_errors: Counter,
}

impl HttpTransactionErrorTelemetry {
    /// Creates a new `HttpTransactionErrorTelemetry` from a metrics builder.
    pub(crate) fn from_builder(builder: &MetricsBuilder) -> Self {
        // Mirror Core Agent forwarder buckets by counting lifecycle failures at their source. See
        // datadog-agent/comp/forwarder/defaultforwarder/transaction/transaction.go::GetClientTrace.
        Self {
            dns_errors: register_scoped_error(builder, ERROR_TYPE_DNS, ERROR_SCOPE_PHASE),
            connection_errors: register_scoped_error(builder, ERROR_TYPE_CONNECTION, ERROR_SCOPE_PHASE),
            tls_errors: register_scoped_error(builder, ERROR_TYPE_TLS, ERROR_SCOPE_PHASE),
            wrote_request_errors: register_scoped_error(builder, ERROR_TYPE_WROTE_REQUEST, ERROR_SCOPE_PHASE),
            send_errors: register_scoped_error(builder, ERROR_TYPE_CANT_SEND, ERROR_SCOPE_TRANSACTION),
            http_errors: register_scoped_error(builder, ERROR_TYPE_GT_400, ERROR_SCOPE_TRANSACTION),
        }
    }

    pub(crate) fn dns_errors(&self) -> Counter {
        self.dns_errors.clone()
    }

    pub(crate) fn increment_connection_error(&self) {
        self.connection_errors.increment(1);
    }

    pub(crate) fn increment_tls_error(&self) {
        self.tls_errors.increment(1);
    }

    pub(crate) fn increment_wrote_request_error(&self) {
        self.wrote_request_errors.increment(1);
    }

    fn increment_send_error(&self) {
        self.send_errors.increment(1);
    }

    fn increment_http_error(&self) {
        self.http_errors.increment(1);
    }
}

fn register_scoped_error(builder: &MetricsBuilder, error_type: &'static str, error_scope: &'static str) -> Counter {
    builder.register_counter_with_tags(
        "network_http_requests_errors_total",
        [("error_type", error_type), ("error_scope", error_scope)],
    )
}

/// Emit telemetry about the status of HTTP transactions.
///
/// This layer can be used with services that deal with `http::Request` and `http::Response`, and wraps them to provide
/// telemetry about the status of an HTTP "transaction": a full round-trip of request and response.
///
/// ## Metrics
///
/// The following metrics are emitted:
///
/// - `network_http_requests_failed_total`: The total number of HTTP requests that failed with a status code of 400,
///   403, or 413.
/// - `network_http_requests_success_total`: The total number of successful HTTP requests. (any response with a
///   non-4xx/5xx status code) This metric is additionally tagged with the response protocol as `proto_version`.
/// - `network_http_requests_success_sent_bytes_total`: The total number of body bytes sent in successful HTTP requests.
///   (see note below on how this is calculated)
/// - `network_http_requests_errors_total`: The total number of HTTP requests that had an error, either during the
///   sending of the request or in the response. This is further broken down by the `error_type` label.
///   - For all responses with a status code greater than 400, `error_type` will be `client_error` and `code` will be
///     the string version of the status code.
///   - When there is an error during the sending of the request, `error_type` classifies the request failure.
///
/// All metrics are emitted with two base tags:
///
/// - `domain`: The full domain of the request, including scheme and port, but excluding any credentials.
/// - `endpoint`: The endpoint name, which is derived from the URI path by default but can be customized. (See
///   [`EndpointTelemetryLayer::with_endpoint_name_fn`] for information on customization and how the endpoint name,
///   overall, is sanitized.)
///
/// Successful request counts also include `proto_version`, formatted as the response's HTTP version (for example,
/// `HTTP/1.1` or `HTTP/2.0`). Successful request bytes remain grouped only by the base tags.
///
/// ### Success bytes calculation
///
/// We calculate the number of bytes sent by examining the body length itself, which is done via [`Body::size_hint`].
/// This requires that an exact body size is known, which isn't always the case. If the body size isn't known, this
/// metric won't be emitted on a successful response.
///
/// For common body types, like [`FrozenChunkedBytesBuffer`][saluki_common::buf::FrozenChunkedBytesBuffer], the size
/// hint is always exact and so this functionality should work as intended.
#[derive(Clone, Default)]
pub struct EndpointTelemetryLayer {
    builder: MetricsBuilder,
    endpoint_name_fn: Option<Arc<EndpointNameFn>>,
    error_telemetry: Option<HttpTransactionErrorTelemetry>,
}

impl EndpointTelemetryLayer {
    /// Create a new `EndpointTelemetryLayer` with the given `ComponentContext`.
    ///
    /// The component context is used when creating metrics, which ensures they're tagged in a consistent way that
    /// attributes the metrics to the component issuing the HTTP requests.
    pub fn with_metrics_builder(mut self, builder: MetricsBuilder) -> Self {
        self.builder = builder;
        self
    }

    pub(super) fn with_error_telemetry(mut self, error_telemetry: HttpTransactionErrorTelemetry) -> Self {
        self.error_telemetry = Some(error_telemetry);
        self
    }

    /// Sets the function used to extract the "endpoint name" from a URI.
    ///
    /// The value returned by this function will be sanitized to ensure it can be used as a tag value, and is limited
    /// to: ASCII alphanumerics, hyphens, underscores, slashes, and periods. Any non-conforming character will be
    /// replaced with an underscore. Characters will be converted to lowercase.
    ///
    /// The value returned by this function is also cached for the given URI, and so the function shouldn't rely on
    /// non-deterministic behavior, or state, that could change the generated endpoint name for subsequent calls with
    /// the same input URI.
    pub fn with_endpoint_name_fn<F>(mut self, endpoint_name_fn: F) -> Self
    where
        F: Fn(&Uri) -> Option<MetaString> + Send + Sync + 'static,
    {
        self.endpoint_name_fn = Some(Arc::new(endpoint_name_fn));
        self
    }
}

impl<S> Layer<S> for EndpointTelemetryLayer {
    type Service = EndpointTelemetry<S>;

    fn layer(&self, service: S) -> Self::Service {
        EndpointTelemetry {
            service,
            builder: self.builder.clone(),
            endpoint_name_fn: self.endpoint_name_fn.clone(),
            error_telemetry: self.error_telemetry.clone(),
            domains: HashMap::new(),
            endpoint_name_cache: HashMap::new(),
        }
    }
}

#[derive(Default)]
struct SuccessCounters {
    http_09: OnceLock<Counter>,
    http_10: OnceLock<Counter>,
    http_11: OnceLock<Counter>,
    http_2: OnceLock<Counter>,
    http_3: OnceLock<Counter>,
    fallback: OnceLock<Mutex<HashMap<Version, Counter>>>,
}

impl SuccessCounters {
    fn slot_for_version(&self, version: Version) -> Option<(&OnceLock<Counter>, &'static str)> {
        match version {
            Version::HTTP_09 => Some((&self.http_09, "HTTP/0.9")),
            Version::HTTP_10 => Some((&self.http_10, "HTTP/1.0")),
            Version::HTTP_11 => Some((&self.http_11, "HTTP/1.1")),
            Version::HTTP_2 => Some((&self.http_2, "HTTP/2.0")),
            Version::HTTP_3 => Some((&self.http_3, "HTTP/3.0")),
            _ => None,
        }
    }

    fn fallback_counter(&self, builder: &MetricsBuilder, version: Version) -> Counter {
        let fallback = self.fallback.get_or_init(|| Mutex::new(HashMap::new()));
        let mut counters = fallback.lock().unwrap();
        counters
            .entry(version)
            .or_insert_with(|| {
                builder.register_counter_with_tags(
                    "network_http_requests_success_total",
                    [("proto_version", format!("{version:?}"))],
                )
            })
            .clone()
    }
}

struct PerEndpointTelemetry {
    builder: MetricsBuilder,
    dropped: Counter,
    success: SuccessCounters,
    success_bytes: Counter,
    http_errors_map: Mutex<HashMap<StatusCode, Counter>>,
}

impl PerEndpointTelemetry {
    fn new(builder: MetricsBuilder, uri: &Uri, endpoint_name: &str) -> Self {
        // Reconstruct the full domain from the URI, including scheme and port, but leaving out any credentials.
        let mut domain = format!("{}://{}", uri.scheme_str().unwrap(), uri.host().unwrap());
        if let Some(port) = uri.port() {
            domain.push(':');
            domain.push_str(port.as_str());
        }

        let builder = builder
            .add_default_tag(("domain", domain))
            .add_default_tag(("endpoint", endpoint_name.to_string()));

        let dropped = builder.register_counter("network_http_requests_failed_total");
        let success = SuccessCounters::default();
        let success_bytes = builder.register_counter("network_http_requests_success_sent_bytes_total");
        let http_errors_map = Mutex::new(HashMap::new());

        Self {
            builder,
            dropped,
            success,
            success_bytes,
            http_errors_map,
        }
    }

    fn increment_dropped(&self) {
        self.dropped.increment(1);
    }

    fn increment_success(&self, version: Version) {
        if let Some((slot, proto_version)) = self.success.slot_for_version(version) {
            slot.get_or_init(|| {
                self.builder.register_counter_with_tags(
                    "network_http_requests_success_total",
                    [("proto_version", proto_version)],
                )
            })
            .increment(1);
        } else {
            // A dependency upgrade may add HTTP versions. The lazy fallback prevents an unfamiliar version from
            // turning telemetry into a process-level failure without adding overhead to known-version requests.
            self.success.fallback_counter(&self.builder, version).increment(1);
        }
    }

    fn increment_success_bytes(&self, len: u64) {
        self.success_bytes.increment(len);
    }

    fn increment_http_error(&self, status: StatusCode) {
        let mut http_errors_map = self.http_errors_map.lock().unwrap();
        let counter = http_errors_map.entry(status).or_insert_with(move || {
            self.builder.register_counter_with_tags(
                "network_http_requests_errors_total",
                [
                    ("error_type", ERROR_TYPE_CLIENT.to_string()),
                    ("code", status.as_str().to_string()),
                ],
            )
        });
        counter.increment(1);
    }
}

/// Emit telemetry about the status of HTTP transactions.
#[derive(Clone)]
pub struct EndpointTelemetry<S> {
    service: S,
    builder: MetricsBuilder,
    endpoint_name_fn: Option<Arc<EndpointNameFn>>,
    error_telemetry: Option<HttpTransactionErrorTelemetry>,
    domains: HashMap<Authority, HashMap<MetaString, Arc<PerEndpointTelemetry>>>,
    endpoint_name_cache: HashMap<Uri, MetaString>,
}

impl<S> EndpointTelemetry<S> {
    fn get_telemetry_handle<B>(&mut self, req: &Request<B>) -> Option<Arc<PerEndpointTelemetry>>
    where
        B: Body,
    {
        // We require a scheme and a host in the URI to emit telemetry.
        if req.uri().scheme().is_none() || req.uri().host().is_none() {
            return None;
        }

        // `Authority` is underpinned by `Bytes` so cloning is cheap.
        let authority = req.uri().authority()?.clone();
        let domain = self.domains.entry(authority).or_default();

        // Look up the per-endpoint telemetry handle, or create a new one if it doesn't exist.
        //
        // We do some caching of the endpoint name to avoid repeatedly calling the endpoint name function, which could
        // be expensive due to the sanitization we perform on it.
        let endpoint_telemetry = match self.endpoint_name_cache.get(req.uri()) {
            Some(endpoint_name) => domain
                .get(endpoint_name)
                .expect("per-endpoint telemetry must exist if name is cached"),
            None => {
                // Generate our endpoint name, and then cache it.
                let endpoint_name = self
                    .endpoint_name_fn
                    .as_ref()
                    .and_then(|f| f(req.uri()))
                    .map(sanitize_endpoint_name)
                    .unwrap_or_else(|| sanitize_endpoint_name(req.uri().path().into()));

                self.endpoint_name_cache
                    .insert(req.uri().clone(), endpoint_name.clone());

                // Now we'll create the per-endpoint telemetry.
                domain.entry(endpoint_name).or_insert_with_key(|endpoint_name| {
                    Arc::new(PerEndpointTelemetry::new(
                        self.builder.clone(),
                        req.uri(),
                        endpoint_name,
                    ))
                })
            }
        };

        Some(Arc::clone(endpoint_telemetry))
    }
}

impl<B, B2, S> Service<Request<B>> for EndpointTelemetry<S>
where
    S: Service<Request<B>, Response = http::Response<B2>>,
    B: Body,
    B2: Body,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = EndpointTelemetryFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let maybe_body_len = req.body().size_hint().exact();
        let per_endpoint = self.get_telemetry_handle(&req);
        let fut = self.service.call(req);

        EndpointTelemetryFuture {
            per_endpoint,
            error_telemetry: self.error_telemetry.clone(),
            maybe_body_len,
            fut,
        }
    }
}

pin_project! {
    /// Response future from [`EndpointTelemetry`] services.
    pub struct EndpointTelemetryFuture<F> {
        per_endpoint: Option<Arc<PerEndpointTelemetry>>,
        error_telemetry: Option<HttpTransactionErrorTelemetry>,
        maybe_body_len: Option<u64>,

        #[pin]
        fut: F,
    }
}

impl<F, B, E> Future for EndpointTelemetryFuture<F>
where
    F: Future<Output = Result<Response<B>, E>>,
    B: Body,
{
    type Output = Result<Response<B>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match ready!(this.fut.poll(cx)) {
            Ok(response) => {
                if let Some(per_endpoint) = this.per_endpoint.as_ref() {
                    let status = response.status();
                    if status.is_client_error() || status.is_server_error() {
                        // Always increment the HTTP error total by grouped over the actual status code.
                        per_endpoint.increment_http_error(status);

                        let status_code = status.as_u16();
                        if status_code == 400 || status_code == 403 || status_code == 413 {
                            // There's some specific errors where we're not going to retry them, so we can reasonable
                            // classify these requests as being dropped: they won't be retried, etc.
                            per_endpoint.increment_dropped()
                        } else if let Some(error_telemetry) = this.error_telemetry.as_ref() {
                            error_telemetry.increment_http_error();
                        }
                    } else {
                        per_endpoint.increment_success(response.version());
                        if let Some(body_len) = this.maybe_body_len {
                            per_endpoint.increment_success_bytes(*body_len);
                        }
                    }
                }

                Poll::Ready(Ok(response))
            }
            Err(e) => {
                if let Some(error_telemetry) = this.error_telemetry.as_ref() {
                    error_telemetry.increment_send_error();
                }

                Poll::Ready(Err(e))
            }
        }
    }
}

fn is_sanitized_endpoint_name(s: &str) -> bool {
    s.chars().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '/' || c == '\\' || c == '.'
    })
}

fn sanitize_endpoint_name(endpoint_name: MetaString) -> MetaString {
    // Check if the endpoint name is already sanitized, and if so, just return it as-is.
    if is_sanitized_endpoint_name(&endpoint_name) {
        return endpoint_name;
    }

    endpoint_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/' || c == '\\' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .into()
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use bytes::Bytes;
    use http_body_util::{Empty, Full};
    use metrics::{Key, Label};
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder},
        CompositeKey, MetricKind,
    };
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn success_counter_slots_cover_supported_http_versions() {
        let counters = SuccessCounters::default();
        let cases = [
            (Version::HTTP_09, &counters.http_09, "HTTP/0.9"),
            (Version::HTTP_10, &counters.http_10, "HTTP/1.0"),
            (Version::HTTP_11, &counters.http_11, "HTTP/1.1"),
            (Version::HTTP_2, &counters.http_2, "HTTP/2.0"),
            (Version::HTTP_3, &counters.http_3, "HTTP/3.0"),
        ];

        for (version, expected_slot, expected_label) in cases {
            let (slot, label) = counters
                .slot_for_version(version)
                .expect("known HTTP version should have a dedicated counter slot");
            assert!(std::ptr::eq(slot, expected_slot));
            assert_eq!(label, expected_label);
        }
    }

    #[test]
    fn fallback_success_counters_are_registered_lazily_and_reused() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let builder = MetricsBuilder::default();
        let counters = SuccessCounters::default();
        let version = Version::HTTP_11;
        let expected_label = "HTTP/1.1";

        assert!(counters.fallback.get().is_none());
        assert_eq!(format!("{version:?}"), expected_label);

        metrics::with_local_recorder(&recorder, || {
            counters.fallback_counter(&builder, version).increment(2);
            counters.fallback_counter(&builder, version).increment(3);
        });

        let fallback = counters
            .fallback
            .get()
            .expect("fallback cache should be initialized after first use");
        assert_eq!(fallback.lock().unwrap().len(), 1);

        let snapshot = snapshotter.snapshot().into_hashmap();
        let success_keys = snapshot
            .keys()
            .filter(|key| key.key().name() == "network_http_requests_success_total")
            .count();
        assert_eq!(success_keys, 1);
        let key = CompositeKey::new(
            MetricKind::Counter,
            Key::from_parts(
                "network_http_requests_success_total",
                vec![Label::new("proto_version", expected_label)],
            ),
        );
        let (_, _, value) = snapshot
            .get(&key)
            .expect("fallback success counter should use the version's Debug label");
        assert_eq!(value, &DebugValue::Counter(5));
    }

    #[test]
    fn successful_requests_are_counted_by_response_protocol_without_splitting_bytes() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let mut response_versions = [http::Version::HTTP_11, http::Version::HTTP_2].into_iter();
        let inner = tower::service_fn(move |_request: Request<Full<Bytes>>| {
            let version = response_versions.next().expect("response version should be available");
            async move {
                Ok::<_, Infallible>(
                    Response::builder()
                        .version(version)
                        .body(Empty::<Bytes>::new())
                        .expect("response should be valid"),
                )
            }
        });
        let mut service = EndpointTelemetryLayer::default().layer(inner);

        let first_request = Request::builder()
            .uri("https://example.com/api/v1/series")
            .version(http::Version::HTTP_10)
            .body(Full::new(Bytes::from_static(b"hello")))
            .expect("request should be valid");
        metrics::with_local_recorder(&recorder, || {
            tokio_test::block_on(service.call(first_request)).expect("request should succeed")
        });

        let second_request = Request::builder()
            .uri("https://example.com/api/v1/series")
            .version(http::Version::HTTP_10)
            .body(Full::new(Bytes::from_static(b"goodbye")))
            .expect("request should be valid");
        metrics::with_local_recorder(&recorder, || {
            tokio_test::block_on(service.call(second_request)).expect("request should succeed")
        });

        let snapshot = snapshotter.snapshot().into_hashmap();
        let success_keys = snapshot
            .keys()
            .filter(|key| key.key().name() == "network_http_requests_success_total")
            .count();
        assert_eq!(success_keys, 2);
        for proto_version in ["HTTP/1.1", "HTTP/2.0"] {
            let key = CompositeKey::new(
                MetricKind::Counter,
                Key::from_parts(
                    "network_http_requests_success_total",
                    vec![
                        Label::new("domain", "https://example.com"),
                        Label::new("endpoint", "/api/v1/series"),
                        Label::new("proto_version", proto_version),
                    ],
                ),
            );
            let (_, _, value) = snapshot
                .get(&key)
                .expect("protocol-specific success counter should exist");
            assert_eq!(value, &DebugValue::Counter(1));
        }

        let success_bytes_keys = snapshot
            .keys()
            .filter(|key| key.key().name() == "network_http_requests_success_sent_bytes_total")
            .count();
        assert_eq!(success_bytes_keys, 1);
        let success_bytes_key = CompositeKey::new(
            MetricKind::Counter,
            Key::from_parts(
                "network_http_requests_success_sent_bytes_total",
                vec![
                    Label::new("domain", "https://example.com"),
                    Label::new("endpoint", "/api/v1/series"),
                ],
            ),
        );
        let (_, _, value) = snapshot
            .get(&success_bytes_key)
            .expect("protocol-agnostic success bytes counter should exist");
        assert_eq!(value, &DebugValue::Counter(12));
    }

    proptest! {
        #[test]
        fn property_test_sanitize_endpoint_name(input in ".*") {
            let sanitized = sanitize_endpoint_name(input.into());
            prop_assert!(is_sanitized_endpoint_name(&sanitized));
        }
    }
}
