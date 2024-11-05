use std::{future::Future, pin::Pin, task::Poll, time::Duration};

use http::{Request, Response};
use hyper::body::{Body, Incoming};
use hyper_util::{
    client::legacy::{
        connect::{capture_connection, Connect},
        Builder, Client, Error,
    },
    rt::{TokioExecutor, TokioTimer},
};
use saluki_error::GenericError;
use saluki_tls::ClientTLSConfigBuilder;
use tower::{BoxError, Service};

use super::{
    conn::{check_connection_state, HttpsCapableConnectorBuilder},
    HttpsCapableConnector,
};

/// An HTTP client.
#[derive(Clone)]
pub struct HttpClient<C = (), B = ()> {
    inner: Client<C, B>,
}

impl HttpClient<(), ()> {
    /// Creates a new builder for configuring an HTTP client.
    pub fn builder() -> HttpClientBuilder {
        HttpClientBuilder::default()
    }
}

impl<C, B> HttpClient<C, B>
where
    C: Connect + Clone + Send + Sync + 'static,
    B: Body + Clone + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    /// Sends a request to the server, and waits for a response.
    ///
    /// # Errors
    ///
    /// If there was an error sending the request, an error will be returned.
    pub async fn send(&self, mut req: Request<B>) -> Result<Response<Incoming>, Error> {
        let captured_conn = capture_connection(&mut req);
        let result = self.inner.request(req).await;

        check_connection_state(captured_conn);

        result
    }
}

impl<C, B> Service<Request<B>> for HttpClient<C, B>
where
    C: Connect + Clone + Send + Sync + 'static,
    B: Body + Send + Unpin + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    type Response = hyper::Response<Incoming>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
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
/// - 30 second timeout when connecting to the remote host
/// - connection pool for reusing connections (45 second idle connection timeout, and a maximum of 5 idle connections
///   per host)
/// - support for FIPS-compliant cryptography (if the `fips` feature is enabled in the `saluki-tls` crate) via [AWS-LC][aws-lc]
///
/// [aws-lc]: https://github.com/aws/aws-lc-rs
pub struct HttpClientBuilder {
    connector_builder: HttpsCapableConnectorBuilder,
    hyper_builder: Builder,
    tls_builder: ClientTLSConfigBuilder,
}

impl HttpClientBuilder {
    /// Sets the timeout when connecting to the remote host.
    ///
    /// Defaults to 30 seconds.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connector_builder = self.connector_builder.with_connect_timeout(timeout);
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

    /// Builds the `HttpClient`.
    ///
    /// # Errors
    ///
    /// If there was an error building the TLS configuration for the client, an error will be returned.
    pub fn build<B>(self) -> Result<HttpClient<HttpsCapableConnector, B>, GenericError>
    where
        B: Body + Clone + Unpin + Send + 'static,
        B::Data: Send,
        B::Error: std::error::Error + Send + Sync,
    {
        let tls_config = self.tls_builder.build()?;
        let connector = self.connector_builder.build(tls_config);
        let client = self.hyper_builder.build(connector);

        Ok(HttpClient { inner: client })
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
        }
    }
}
