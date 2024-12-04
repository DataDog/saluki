use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{ready, Context, Poll},
};

use http::{uri::Authority, Request, Response, StatusCode, Uri};
use http_body::Body;
use metrics::Counter;
use pin_project_lite::pin_project;
use saluki_metrics::MetricsBuilder;
use stringtheory::MetaString;
use tower::{Layer, Service};

pub type EndpointNameFn = dyn Fn(&Uri) -> Option<MetaString> + Send + Sync;

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
///   non-4xx/5xx status code)
/// - `network_http_requests_success_sent_bytes_total`: The total number of body bytes sent in successful HTTP requests.
///   (see note below on how this is calculated)
/// - `network_http_requests_errors_total`: The total number of HTTP requests that had an error, either during the
///   sending of the request or in the response. This is further broken down by the `error_type` label.
///   - For all responses with a status code greater than 400, `error_type` will be `client_error` and `code` will be a
///     stringified version of the status code.
///   - When there is an error during the sending of the request, `error_type` will be `send_failed`.
///
/// All metrics are emitted with two base tags:
///
/// - `domain`: The full domain of the request, including scheme and port, but excluding any credentials.
/// - `endpoint`: The endpoint name, which is derived from the URI path by default but can be customized. (See
///   [`EndpointTelemetryLayer::with_endpoint_name_fn`] for information on customization and how the endpoint name,
///   overall, is sanitized.)
///
/// ### Success bytes calculation
///
/// We calculate the number of bytes sent by examining the body length itself, which is done via [`Body::size_hint`].
/// This requires that an exact body size is known, which is not always the case. If the body size is not known, this
/// metric will not be emitted on a successful response.
///
/// For common body types, like [`FrozenChunkedBytesBuffer`][crate::buf::chunked::FrozenChunkedBytesBuffer], the size
/// hint is always exact and so this functionality should work as intended.
#[derive(Clone, Default)]
pub struct EndpointTelemetryLayer {
    builder: MetricsBuilder,
    endpoint_name_fn: Option<Arc<EndpointNameFn>>,
}

impl EndpointTelemetryLayer {
    /// Create a new `EndpointTelemetryLayer` with the given `ComponentContext`.
    ///
    /// The component context is used when creating metrics, which ensures they are tagged in a consistent way that
    /// attributes the metrics to the component issuing the HTTP requests.
    pub fn with_metrics_builder(mut self, builder: MetricsBuilder) -> Self {
        self.builder = builder;
        self
    }

    /// Sets the function used to extract the "endpoint name" from a URI.
    ///
    /// The value returned by this function will be sanitized to ensure it can be used as a tag value, and is limited
    /// to: ASCII alphanumerics, hyphens, underscores, slashes, and periods. Any non-conforming character will be
    /// replaced with an underscore. Characters will be converted to lowercase.
    ///
    /// The value returned by this function is also cached for the given URI, and so the function should not rely on
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
            domains: HashMap::new(),
            endpoint_name_cache: HashMap::new(),
        }
    }
}

struct PerEndpointTelemetry {
    builder: MetricsBuilder,
    dropped: Counter,
    success: Counter,
    success_bytes: Counter,
    errors_map: Mutex<HashMap<&'static str, Counter>>,
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
        domain.push('/');

        let builder = builder
            .add_default_tag(("domain", domain))
            .add_default_tag(("endpoint", endpoint_name.to_string()));

        let dropped = builder.register_debug_counter("network_http_requests_failed_total");
        let success = builder.register_debug_counter("network_http_requests_success_total");
        let success_bytes = builder.register_debug_counter("network_http_requests_success_sent_bytes_total");
        let errors_map = Mutex::new(HashMap::new());
        let http_errors_map = Mutex::new(HashMap::new());

        Self {
            builder,
            dropped,
            success,
            success_bytes,
            errors_map,
            http_errors_map,
        }
    }

    fn increment_dropped(&self) {
        self.dropped.increment(1);
    }

    fn increment_success(&self) {
        self.success.increment(1);
    }

    fn increment_success_bytes(&self, len: u64) {
        self.success_bytes.increment(len);
    }

    fn increment_error(&self, error_type: &'static str) {
        let mut errors_map = self.errors_map.lock().unwrap();
        let counter = errors_map.entry(error_type).or_insert_with(|| {
            self.builder
                .register_debug_counter_with_tags("network_http_requests_errors_total", [("error_type", error_type)])
        });
        counter.increment(1);
    }

    fn increment_http_error(&self, status: StatusCode) {
        let mut http_errors_map = self.http_errors_map.lock().unwrap();
        let counter = http_errors_map.entry(status).or_insert_with(move || {
            self.builder.register_debug_counter_with_tags(
                "network_http_requests_errors_total",
                [
                    ("error_type", "client_error".to_string()),
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
            maybe_body_len,
            fut,
        }
    }
}

pin_project! {
    /// Response future from [`EndpointTelemetry`] services.
    pub struct EndpointTelemetryFuture<F> {
        per_endpoint: Option<Arc<PerEndpointTelemetry>>,
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
                        }
                    } else {
                        per_endpoint.increment_success();
                        if let Some(body_len) = this.maybe_body_len {
                            per_endpoint.increment_success_bytes(*body_len);
                        }
                    }
                }

                Poll::Ready(Ok(response))
            }
            Err(e) => {
                if let Some(per_endpoint) = this.per_endpoint.as_ref() {
                    per_endpoint.increment_error("send_failed");
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
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn property_test_sanitize_endpoint_name(input in ".*") {
            let sanitized = sanitize_endpoint_name(input.into());
            prop_assert!(is_sanitized_endpoint_name(&sanitized));
        }
    }
}
