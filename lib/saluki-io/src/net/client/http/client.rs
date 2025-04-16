use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use http::{Request, Response, Uri};
use hyper::body::{Body, Incoming};
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
use tokio::time::Instant;
use tower::{
    retry::Policy, timeout::TimeoutLayer, util::BoxCloneService, BoxError, Service, ServiceBuilder, ServiceExt as _,
};

use super::{
    conn::{check_connection_state, HttpsCapableConnectorBuilder},
    EndpointTelemetryLayer,
};
use crate::net::util::retry::NoopRetryPolicy;

/// An HTTP client.
#[derive(Clone)]
pub struct HttpClient<B = ()> {
    inner: BoxCloneService<Request<B>, Response<Incoming>, BoxError>,
}

impl HttpClient<()> {
    /// Creates a new builder for configuring an HTTP client.
    pub fn builder() -> HttpClientBuilder {
        HttpClientBuilder::default()
    }
}

impl<B> HttpClient<B>
where
    B: Body + Clone + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    /// Sends a request to the server, and waits for a response.
    ///
    /// # Errors
    ///
    /// If there was an error sending the request, an error will be returned.
    pub async fn send(&mut self, mut req: Request<B>) -> Result<Response<Incoming>, BoxError> {
        let captured_conn = capture_connection(&mut req);
        let result = self.inner.ready().await?.call(req).await;

        check_connection_state(captured_conn);

        result
    }
}

impl<B> Service<Request<B>> for HttpClient<B>
where
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    type Response = Response<Incoming>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let captured_conn = capture_connection(&mut req);
        let fut = self.inner.call(req);

        Box::pin(async move {
            let result = fut.await;

            check_connection_state(captured_conn);

            result
        })
    }
}

/// A wrapper around HttpClient that periodically resets connections
#[derive(Clone)]
#[allow(dead_code)]
pub struct ResetHttpClient<B = ()> {
    client: HttpClient<B>,
    client_builder: HttpClientBuilder,
    reset_interval: Duration,
    last_reset: Instant,
}

impl<B> ResetHttpClient<B>
where
    B: Body + Clone + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    /// Creates a new ResetHttpClient with the specified reset interval
    pub fn new(builder: HttpClientBuilder, reset_interval: Duration) -> Result<Self, GenericError> {
        let client = builder.clone().build()?;

        Ok(Self {
            client,
            client_builder: builder,
            reset_interval,
            last_reset: Instant::now(),
        })
    }

    fn reset_if_needed(&mut self) -> Result<(), BoxError> {
        // 0 means reset feature is disabled
        if self.reset_interval.is_zero() {
            return Ok(());
        }

        // If the last reset was less than the reset interval ago, do nothing
        if self.last_reset.elapsed() < self.reset_interval {
            return Ok(());
        }

        // Reset the client
        self.client = self.client_builder.clone().build()?;
        self.last_reset = Instant::now();

        Ok(())
    }
}

impl<B> Service<Request<B>> for ResetHttpClient<B>
where
    B: Body + Clone + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: std::error::Error + Send + Sync,
{
    type Response = Response<Incoming>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.client.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        if let Err(e) = self.reset_if_needed() {
            return Box::pin(async move { Err(e) });
        }
        self.client.call(req)
    }
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
#[derive(Clone)]
pub struct HttpClientBuilder<P = NoopRetryPolicy> {
    connector_builder: HttpsCapableConnectorBuilder,
    hyper_builder: Builder,
    tls_builder: ClientTLSConfigBuilder,
    retry_policy: P,
    request_timeout: Option<Duration>,
    endpoint_telemetry: Option<EndpointTelemetryLayer>,
    proxies: Option<Vec<Proxy>>,
}

impl<P> HttpClientBuilder<P> {
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

    /// Sets the retry policy to use when sending requests.
    ///
    /// When set, the client will automatically retry requests that are classified as having failed.
    ///
    /// Defaults to no retry policy. (i.e. requests are not retried)
    pub fn with_retry_policy<P2>(self, retry_policy: P2) -> HttpClientBuilder<P2> {
        HttpClientBuilder {
            connector_builder: self.connector_builder,
            hyper_builder: self.hyper_builder,
            tls_builder: self.tls_builder,
            request_timeout: self.request_timeout,
            retry_policy,
            endpoint_telemetry: self.endpoint_telemetry,
            proxies: self.proxies,
        }
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
    pub fn build<B>(self) -> Result<HttpClient<B>, GenericError>
    where
        B: Body + Clone + Unpin + Send + 'static,
        B::Data: Send,
        B::Error: std::error::Error + Send + Sync,
        P: Policy<Request<B>, Response<Incoming>, BoxError> + Send + Clone + 'static,
        P::Future: Send,
    {
        let tls_config = self.tls_builder.build()?;
        let connector = self.connector_builder.build(tls_config);
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
            .retry(self.retry_policy)
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
            retry_policy: NoopRetryPolicy,
            endpoint_telemetry: None,
            proxies: None,
        }
    }
}
