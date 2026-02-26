use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use bytes::{Buf, Bytes};
use http::{Request, Response, Uri};
use http_body::Body;
use http_body_util::{combinators::BoxBody, BodyExt as _};
use hyper::body::Incoming;
use hyper_http_proxy::Proxy;
use hyper_util::{
    client::legacy::{connect::capture_connection, Builder},
    rt::{TokioExecutor, TokioTimer},
};
use metrics::Counter;
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use saluki_tls::ClientTLSConfigBuilder;
use stringtheory::MetaString;
use tower::{timeout::TimeoutLayer, util::BoxCloneService, BoxError, Service, ServiceBuilder, ServiceExt as _};

use super::{
    conn::{check_connection_state, HttpsCapableConnectorBuilder},
    EndpointTelemetryLayer,
};

/// The type-erased body type used internally by [`HttpClient`].
///
/// All request bodies are converted to this type before being sent over the wire, which ensures a single
/// monomorphization of the underlying HTTP/2 and TLS stacks regardless of the caller's body type.
pub type ClientBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

/// An HTTP client.
#[derive(Clone)]
pub struct HttpClient {
    inner: BoxCloneService<Request<ClientBody>, Response<Incoming>, BoxError>,
}

impl HttpClient {
    /// Creates a new builder for configuring an HTTP client.
    pub fn builder() -> HttpClientBuilder {
        HttpClientBuilder::default()
    }

    /// Sends a request to the server, and waits for a response.
    ///
    /// The request body is type-erased internally, so callers can use any body type that implements
    /// [`Body`] with `Data` types that implement [`Buf`].
    ///
    /// # Errors
    ///
    /// If there was an error sending the request, an error will be returned.
    pub async fn send<B>(&mut self, req: Request<B>) -> Result<Response<Incoming>, BoxError>
    where
        B: Body + Send + Sync + 'static,
        B::Data: Buf + Send,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let mut req = req.map(into_client_body);
        let captured_conn = capture_connection(&mut req);
        let result = self.inner.ready().await?.call(req).await;

        check_connection_state(captured_conn);

        result
    }
}

impl Service<Request<ClientBody>> for HttpClient {
    type Response = Response<Incoming>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ClientBody>) -> Self::Future {
        let captured_conn = capture_connection(&mut req);
        let fut = self.inner.call(req);

        Box::pin(async move {
            let result = fut.await;

            check_connection_state(captured_conn);

            result
        })
    }
}

/// Converts an arbitrary body into the type-erased [`ClientBody`].
///
/// This uses `Buf::copy_to_bytes` for the data conversion, which is zero-copy when the underlying
/// data is already `Bytes`.
pub fn into_client_body<B>(body: B) -> ClientBody
where
    B: Body + Send + Sync + 'static,
    B::Data: Buf + Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    BoxBody::new(
        body.map_frame(|frame| frame.map_data(|mut data| data.copy_to_bytes(data.remaining())))
            .map_err(Into::into),
    )
}

/// An HTTP client builder.
///
/// Provides an ergonomic builder API for configuring an HTTP client.
///
/// # Defaults
///
/// A number of sensible defaults are provided:
///
/// - support for both HTTP and HTTPS (uses platform's root certificates for server certificate validation)
/// - support for both HTTP/1.1 and HTTP/2 (automatically negotiated via ALPN)
/// - non-infinite timeouts for various stages of the request lifecycle (30 second connect timeout, 60 second per-request timeout)
/// - connection pool for reusing connections (45 second idle connection timeout, and a maximum of 5 idle connections
///   per host)
/// - support for FIPS-compliant cryptography (if the `fips` feature is enabled in the `saluki-tls` crate) via [AWS-LC][aws-lc]
///
/// [aws-lc]: https://github.com/aws/aws-lc-rs
pub struct HttpClientBuilder {
    connector_builder: HttpsCapableConnectorBuilder,
    hyper_builder: Builder,
    tls_builder: ClientTLSConfigBuilder,
    request_timeout: Option<Duration>,
    endpoint_telemetry: Option<EndpointTelemetryLayer>,
    proxies: Option<Vec<Proxy>>,
}

impl HttpClientBuilder {
    /// Sets the timeout when connecting to the remote host.
    ///
    /// Defaults to 30 seconds.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connector_builder = self.connector_builder.with_connect_timeout(timeout);
        self
    }

    /// Sets the per-request timeout.
    ///
    /// The request timeout applies to each individual request made to the remote host, including each request made when
    /// retrying a failed request.
    ///
    /// Defaults to 20 seconds.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// Allow requests to run indefinitely.
    ///
    /// This means there will be no overall timeout for the request, but the request still may be subject to other
    /// configuration settings, such as the connect timeout or retry policy.
    pub fn without_request_timeout(mut self) -> Self {
        self.request_timeout = None;
        self
    }

    /// Sets the maximum age of a connection before it is closed.
    ///
    /// This is distinct from the maximum idle time: if any connection's age exceeds `limit`, it will be closed rather
    /// than being reused and added to the idle connection pool.
    ///
    /// Defaults to no limit.
    pub fn with_connection_age_limit<L>(mut self, limit: L) -> Self
    where
        L: Into<Option<Duration>>,
    {
        self.connector_builder = self.connector_builder.with_connection_age_limit(limit);
        self
    }

    /// Sets the maximum number of idle connections per host.
    ///
    /// Defaults to 5.
    pub fn with_max_idle_conns_per_host(mut self, max: usize) -> Self {
        self.hyper_builder.pool_max_idle_per_host(max);
        self
    }

    /// Sets the idle connection timeout.
    ///
    /// Once a connection has been idle in the pool for longer than this duration, it will be closed and removed from
    /// the pool.
    ///
    /// Defaults to 45 seconds.
    pub fn with_idle_conn_timeout(mut self, timeout: Duration) -> Self {
        self.hyper_builder.pool_idle_timeout(timeout);
        self
    }

    /// Sets the proxies to be used for outgoing requests.
    ///
    /// Defaults to no proxies. (i.e requests will be sent directly without using a proxy).
    pub fn with_proxies(mut self, proxies: Vec<Proxy>) -> Self {
        self.proxies = Some(proxies);
        self
    }

    /// Enables per-endpoint telemetry for HTTP transactions.
    ///
    /// See [`EndpointTelemetryLayer`] for more information.
    pub fn with_endpoint_telemetry<F>(mut self, metrics_builder: MetricsBuilder, endpoint_name_fn: Option<F>) -> Self
    where
        F: Fn(&Uri) -> Option<MetaString> + Send + Sync + 'static,
    {
        let mut layer = EndpointTelemetryLayer::default().with_metrics_builder(metrics_builder);

        if let Some(endpoint_name_fn) = endpoint_name_fn {
            layer = layer.with_endpoint_name_fn(endpoint_name_fn);
        }

        self.endpoint_telemetry = Some(layer);
        self
    }

    /// Sets the TLS configuration.
    ///
    /// A TLS configuration builder is provided to allow for more advanced configuration of the TLS connection.
    pub fn with_tls_config<F>(mut self, f: F) -> Self
    where
        F: FnOnce(ClientTLSConfigBuilder) -> ClientTLSConfigBuilder,
    {
        self.tls_builder = f(self.tls_builder);
        self
    }

    /// Sets the underlying Hyper client configuration.
    ///
    /// This is provided to allow for more advanced configuration of the Hyper client itself, and should generally be
    /// used sparingly.
    pub fn with_hyper_config<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut Builder),
    {
        f(&mut self.hyper_builder);
        self
    }

    /// Sets a counter that gets incremented with the number of bytes sent over the connection.
    ///
    /// This tracks bytes sent at the HTTP client level, which includes headers and body but does not include underlying
    /// transport overhead, such as TLS handshaking, and so on.
    ///
    /// Defaults to unset.
    pub fn with_bytes_sent_counter(mut self, counter: Counter) -> Self {
        self.connector_builder = self.connector_builder.with_bytes_sent_counter(counter);
        self
    }

    /// Builds the `HttpClient`.
    ///
    /// # Errors
    ///
    /// If there was an error building the TLS configuration for the client, an error will be returned.
    pub fn build(self) -> Result<HttpClient, GenericError> {
        let tls_config = self.tls_builder.build()?;
        let connector = self.connector_builder.build(tls_config)?;
        // TODO(fips): Look into updating `hyper-http-proxy` to use the provided connector for establishing the
        // connection to the proxy itself, even when the proxy is at an HTTPS URL, to ensure our desired TLS stack is
        // being used.
        let mut proxy_connector = hyper_http_proxy::ProxyConnector::new(connector)?;
        if let Some(proxies) = &self.proxies {
            for proxy in proxies {
                proxy_connector.add_proxy(proxy.to_owned());
            }
        }
        let client = self.hyper_builder.build(proxy_connector);

        let inner = ServiceBuilder::new()
            .option_layer(self.request_timeout.map(TimeoutLayer::new))
            .option_layer(self.endpoint_telemetry)
            .service(client.map_err(BoxError::from))
            .boxed_clone();

        Ok(HttpClient { inner })
    }
}

impl Default for HttpClientBuilder {
    fn default() -> Self {
        let mut hyper_builder = Builder::new(TokioExecutor::new());
        hyper_builder
            .pool_timer(TokioTimer::new())
            .pool_max_idle_per_host(5)
            .pool_idle_timeout(Duration::from_secs(45));

        Self {
            connector_builder: HttpsCapableConnectorBuilder::default(),
            hyper_builder,
            tls_builder: ClientTLSConfigBuilder::new(),
            request_timeout: Some(Duration::from_secs(20)),
            endpoint_telemetry: None,
            proxies: None,
        }
    }
}
