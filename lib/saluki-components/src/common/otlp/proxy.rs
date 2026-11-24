//! Streaming proxy from the OTLP servers to an upstream OTLP receiver.
//!
//! A proxied request isn't decoded. The proxy streams it to the upstream receiver with only the changes that forwarding
//! needs, and streams the upstream response back unchanged: status, headers, body, and trailers. When the upstream
//! receiver doesn't answer, the client gets an error. The proxy never retries a request.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use axum::extract::Request;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{any, MethodRouter};
use http::header::{self, HeaderMap, HeaderValue};
use http::uri::Uri;
use http::Version;
use metrics::Counter;
use rustls::RootCertStore;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::client::http::{HttpClient, HttpProtocol};
use saluki_metrics::MetricsBuilder;
use tonic::Status;
use tracing::{debug, info, warn};

use super::Signal;
use crate::common::proxy::{strip_hop_by_hop_headers, upstream_uri, Destination, ForwardError, UpstreamClient};

/// Bound on connecting to the upstream receiver.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the proxy forwards requests, for each protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OtlpProxyDestinations {
    /// Upstream OTLP/gRPC receiver. The proxy speaks HTTP/2 to it with prior knowledge.
    ///
    /// `None` if no gRPC server proxies requests.
    pub grpc: Option<Destination>,

    /// Upstream OTLP/HTTP receiver. The proxy speaks HTTP/1.1 to it.
    ///
    /// `None` if no HTTP server proxies requests.
    pub http: Option<Destination>,
}

/// Streams OTLP requests to the upstream receivers.
pub(crate) struct OtlpProxy {
    grpc: Option<Arc<Upstream>>,
    http: Option<Arc<Upstream>>,
    metrics_builder: MetricsBuilder,
}

impl OtlpProxy {
    /// Creates a proxy to `destinations`.
    ///
    /// # Errors
    ///
    /// If an HTTP client can't be built, an error is returned.
    pub(crate) fn new(
        destinations: &OtlpProxyDestinations, metrics_builder: MetricsBuilder,
    ) -> Result<Self, GenericError> {
        Self::with_connect_timeout(destinations, metrics_builder, CONNECT_TIMEOUT)
    }

    fn with_connect_timeout(
        destinations: &OtlpProxyDestinations, metrics_builder: MetricsBuilder, connect_timeout: Duration,
    ) -> Result<Self, GenericError> {
        let upstream = |protocol, destination: &Option<Destination>| {
            destination
                .as_ref()
                .map(|destination| Upstream::new(protocol, destination, connect_timeout).map(Arc::new))
                .transpose()
        };
        Ok(Self {
            grpc: upstream(Protocol::Grpc, &destinations.grpc)?,
            http: upstream(Protocol::Http, &destinations.http)?,
            metrics_builder,
        })
    }

    /// Returns the service that proxies the gRPC requests of `signal`.
    ///
    /// # Errors
    ///
    /// If the proxy has no gRPC destination, an error is returned.
    pub(crate) fn grpc_service(&self, signal: Signal) -> Result<MethodRouter, GenericError> {
        let upstream = self.grpc.as_ref().ok_or_else(|| {
            generic_error!("An OTLP signal is proxied over gRPC, but the OTLP proxy has no gRPC destination.")
        })?;
        Ok(SignalProxy::new(Arc::clone(upstream), signal, &self.metrics_builder).into_service())
    }

    /// Returns the service that proxies the HTTP requests of `signal`.
    ///
    /// # Errors
    ///
    /// If the proxy has no HTTP destination, an error is returned.
    pub(crate) fn http_service(&self, signal: Signal) -> Result<MethodRouter, GenericError> {
        let upstream = self.http.as_ref().ok_or_else(|| {
            generic_error!("An OTLP signal is proxied over HTTP, but the OTLP proxy has no HTTP destination.")
        })?;
        Ok(SignalProxy::new(Arc::clone(upstream), signal, &self.metrics_builder).into_service())
    }
}

/// Protocol of an upstream receiver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Protocol {
    Grpc,
    Http,
}

impl Protocol {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Grpc => "grpc",
            Self::Http => "http",
        }
    }

    /// Returns the response to a request that the upstream receiver didn't answer.
    fn error_response(self, error: ForwardError) -> Response {
        match (self, error) {
            // `into_http` makes a trailers-only response, which gRPC clients read as the status of the call.
            (Self::Grpc, ForwardError::Unreachable) => {
                Status::unavailable("The upstream OTLP receiver is unreachable.").into_http()
            }
            (Self::Grpc, _) => Status::internal("The request couldn't be forwarded.").into_http(),
            (Self::Http, error) => error.into_response(),
        }
    }
}

/// An upstream receiver, shared by every signal of one protocol.
struct Upstream {
    protocol: Protocol,
    client: UpstreamClient,

    /// Scheme and authority of the URI of each request. For a Unix domain socket destination the client ignores the
    /// authority.
    base_uri: Uri,

    destination: Destination,

    /// Whether the last request failed. Only the first failure after a success logs a warning.
    unreachable: AtomicBool,
}

impl Upstream {
    fn new(protocol: Protocol, destination: &Destination, connect_timeout: Duration) -> Result<Self, GenericError> {
        let builder = HttpClient::builder()
            // The upstream receiver serves plaintext, so an empty root store avoids loading the platform's certificates.
            .with_tls_config(|builder| builder.with_root_cert_store(RootCertStore::empty()))
            .with_http_protocol(match protocol {
                Protocol::Grpc => HttpProtocol::Http2,
                Protocol::Http => HttpProtocol::Http1,
            })
            .with_connect_timeout(connect_timeout)
            // Clients set their own deadlines, such as `grpc-timeout`, and the upstream receiver enforces them.
            .without_request_timeout()
            // The proxy retries no request itself. This setting resends only a request that never started on a reused
            // connection, so the upstream receiver can't have seen it.
            .with_hyper_config(|builder| {
                builder.retry_canceled_requests(true);
            });

        let base_uri = destination
            .base_uri()
            .with_error_context(|| format!("Invalid OTLP proxy destination {destination:?}."))?;
        let builder = destination.configure_client(builder);

        Ok(Self {
            protocol,
            client: UpstreamClient::new(builder.build()?),
            base_uri,
            destination: destination.clone(),
            unreachable: AtomicBool::new(false),
        })
    }

    /// Forwards `request` and returns the upstream response.
    ///
    /// # Errors
    ///
    /// If the request's path can't be carried over, or the upstream receiver doesn't answer, an error is returned.
    async fn forward(&self, signal: Signal, request: Request) -> Result<Response, ForwardError> {
        let (mut parts, body) = request.into_parts();

        parts.uri = match upstream_uri(&self.base_uri, parts.uri.path_and_query()) {
            Ok(uri) => uri,
            Err(e) => {
                debug!(error = %e, signal = signal.as_str(), "Failed to build the upstream URI of an OTLP request.");
                return Err(ForwardError::InvalidUri);
            }
        };

        // HTTP/2 allows `TE` with the value `trailers` only, and gRPC uses it to say that the client reads trailers.
        let keep_te = self.protocol == Protocol::Grpc && accepts_trailers(&parts.headers);
        strip_hop_by_hop_headers(&mut parts.headers);
        if keep_te {
            parts.headers.insert(header::TE, HeaderValue::from_static("trailers"));
        }
        // The client derives `Host`, or `:authority` over HTTP/2, from the upstream URI.
        parts.headers.remove(header::HOST);
        // The OTLP/HTTP server also accepts HTTP/2, but the upstream client refuses an HTTP/2 request over HTTP/1.1.
        parts.version = match self.protocol {
            Protocol::Grpc => Version::HTTP_2,
            Protocol::Http => Version::HTTP_11,
        };

        match self.client.send(http::Request::from_parts(parts, body)).await {
            Ok(response) => {
                if self.unreachable.swap(false, Ordering::Relaxed) {
                    info!(
                        destination = ?self.destination,
                        protocol = self.protocol.as_str(),
                        "The upstream OTLP receiver is reachable again."
                    );
                }
                Ok(response)
            }
            Err(e) => {
                if self.unreachable.swap(true, Ordering::Relaxed) {
                    debug!(
                        error = %e,
                        destination = ?self.destination,
                        protocol = self.protocol.as_str(),
                        signal = signal.as_str(),
                        "Failed to forward an OTLP request to the upstream receiver."
                    );
                } else {
                    warn!(
                        error = %e,
                        destination = ?self.destination,
                        protocol = self.protocol.as_str(),
                        signal = signal.as_str(),
                        "Failed to forward an OTLP request to the upstream receiver. Proxied requests fail until it is reachable again."
                    );
                }
                Err(ForwardError::Unreachable)
            }
        }
    }
}

/// Returns `true` if the `TE` header lists `trailers`.
fn accepts_trailers(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::TE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|coding| coding.trim().eq_ignore_ascii_case("trailers"))
}

/// Proxies the requests of one signal over one protocol.
#[derive(Clone)]
struct SignalProxy {
    upstream: Arc<Upstream>,
    signal: Signal,

    /// Requests that the upstream receiver answered, whatever the status of its answer.
    requests_relayed: Counter,

    /// Requests that the upstream receiver never answered, such as when it was unreachable.
    requests_failed: Counter,
}

impl SignalProxy {
    fn new(upstream: Arc<Upstream>, signal: Signal, metrics_builder: &MetricsBuilder) -> Self {
        let tags = [("signal", signal.as_str()), ("protocol", upstream.protocol.as_str())];
        Self {
            requests_relayed: metrics_builder.register_counter_with_tags("component_requests_relayed_total", tags),
            requests_failed: metrics_builder.register_counter_with_tags("component_requests_failed_total", tags),
            upstream,
            signal,
        }
    }

    fn into_service(self) -> MethodRouter {
        any(move |request: Request| {
            let proxy = self.clone();
            async move { proxy.forward(request).await }
        })
    }

    async fn forward(&self, request: Request) -> Response {
        match self.upstream.forward(self.signal, request).await {
            Ok(response) => {
                self.requests_relayed.increment(1);
                response
            }
            Err(error) => {
                self.requests_failed.increment(1);
                self.upstream.protocol.error_response(error)
            }
        }
    }
}

#[cfg(test)]
mod tests;
