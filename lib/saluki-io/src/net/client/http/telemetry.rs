use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{ready, Context, Poll},
};

use http::{uri::Authority, Request, Response, StatusCode, Uri};
use http_body::Body;
use metrics::{Counter, Label};
use pin_project_lite::pin_project;
use saluki_core::components::{ComponentContext, MetricsBuilder};
use tower::{Layer, Service};

pub type EndpointNameFn = dyn Fn(&Uri) -> Option<&'static str> + Send + Sync;

#[derive(Clone)]
pub struct EndpointTelemetryLayer {
    context: ComponentContext,
    endpoint_name_fn: Option<Arc<EndpointNameFn>>,
}

impl EndpointTelemetryLayer {
    pub fn from_component_context(context: ComponentContext) -> Self {
        Self {
            context,
            endpoint_name_fn: None,
        }
    }

    pub fn with_endpoint_name_fn<F>(mut self, endpoint_name_fn: F) -> Self
    where
        F: Fn(&Uri) -> Option<&'static str> + Send + Sync + 'static,
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
            context: self.context.clone(),
            endpoint_name_fn: self.endpoint_name_fn.clone(),
            domains: HashMap::new(),
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
    fn new(context: ComponentContext, uri: &Uri, endpoint_name: &'static str) -> Self {
        // Reconstruct the full domain from the URI, including scheme and port, but leaving out any credentials.
        let mut domain = format!("{}://{}", uri.scheme_str().unwrap(), uri.host().unwrap());
        if let Some(port) = uri.port() {
            domain.push(':');
            domain.push_str(port.as_str());
        }
        domain.push('/');

        let builder = MetricsBuilder::from_component_context(context).with_fixed_labels(vec![
            Label::new("domain", domain),
            Label::from_static_parts("endpoint", endpoint_name),
        ]);

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
            self.builder.register_debug_counter_with_labels(
                "network_http_requests_errors_total",
                [Label::from_static_parts("error_type", error_type)],
            )
        });
        counter.increment(1);
    }

    fn increment_http_error(&self, status: StatusCode) {
        let mut http_errors_map = self.http_errors_map.lock().unwrap();
        let counter = http_errors_map.entry(status).or_insert_with(move || {
            self.builder.register_debug_counter_with_labels(
                "network_http_requests_errors_total",
                [
                    Label::from_static_parts("error_type", "client_error"),
                    Label::new("code", status.to_string()),
                ],
            )
        });
        counter.increment(1);
    }
}

pub struct EndpointTelemetry<S> {
    service: S,
    context: ComponentContext,
    endpoint_name_fn: Option<Arc<EndpointNameFn>>,
    domains: HashMap<Authority, HashMap<&'static str, Arc<PerEndpointTelemetry>>>,
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

        let endpoint_name = self.endpoint_name_fn.as_ref().and_then(|f| f(req.uri())).unwrap_or("");
        let endpoint_telemetry = domain.entry(endpoint_name).or_insert_with(|| {
            Arc::new(PerEndpointTelemetry::new(
                self.context.clone(),
                req.uri(),
                endpoint_name,
            ))
        });

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

                        match status.as_u16() {
                            // There's some specific errors where we're not going to retry them, so we can reasonable
                            // classify these requests as being dropped: they won't be retried, etc.
                            400 | 403 | 413 => per_endpoint.increment_dropped(),

                            // For everything else, we just want to track that we had an error related to a status code
                            // greater than 400.
                            _ => per_endpoint.increment_error("gt_400"),
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
