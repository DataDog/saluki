#[cfg(unix)]
use std::path::PathBuf;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use bytes::{Buf, Bytes};
use http::{Request, Response, Uri};
use http_body::{Body, Frame, SizeHint};
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper_http_proxy::Proxy;
use hyper_util::{
    client::legacy::{connect::capture_connection, Builder},
    rt::{TokioExecutor, TokioTimer},
};
use metrics::Counter;
use pin_project::pin_project;
use rustls::ClientConfig;
use saluki_error::GenericError;
use saluki_metrics::MetricsBuilder;
use saluki_tls::{ensure_client_config_fips_compliant, ClientTLSConfigBuilder, TlsMinimumVersion};
use stringtheory::MetaString;
use tower::{timeout::TimeoutLayer, util::BoxCloneService, BoxError, Service, ServiceBuilder, ServiceExt as _};

use super::{
    conn::{check_connection_state, HttpProtocol, HttpsCapableConnectorBuilder},
    telemetry::HttpTransactionErrorTelemetry,
    EndpointTelemetryLayer,
};

/// The type-erased body type used internally by [`HttpClient`].
///
/// All request bodies are converted to this type before being sent over the wire, which ensures a single
/// monomorphization of the underlying HTTP/2 and TLS stacks regardless of the caller's body type.
pub type ClientBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

#[pin_project]
struct ClientBodyAdapter<B> {
    #[pin]
    inner: B,
    size_hint: SizeHint,
}

impl<B> Body for ClientBodyAdapter<B>
where
    B: Body,
    B::Data: Buf,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.project().inner.poll_frame(cx).map(|maybe_frame| {
            maybe_frame.map(|result| {
                result
                    .map(|frame| frame.map_data(|mut data| data.copy_to_bytes(data.remaining())))
                    .map_err(Into::into)
            })
        })
    }

    fn size_hint(&self) -> SizeHint {
        self.size_hint.clone()
    }
}

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
    pub async fn send<B>(&mut self, req: Request<B>) -> Result<Response<Incoming>, GenericError>
    where
        B: Body + Send + Sync + 'static,
        B::Data: Buf + Send,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let mut req = req.map(into_client_body);
        let captured_conn = capture_connection(&mut req);
        let result = self
            .inner
            .ready()
            .await
            .map_err(GenericError::from_boxed)?
            .call(req)
            .await;

        check_connection_state(captured_conn);

        result.map_err(GenericError::from_boxed)
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
    let size_hint = body.size_hint();
    BoxBody::new(ClientBodyAdapter { inner: body, size_hint })
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
/// - support for FIPS-compliant cryptography if the `fips` feature is enabled in the `saluki-tls` crate
pub struct HttpClientBuilder {
    connector_builder: HttpsCapableConnectorBuilder,
    hyper_builder: Builder,
    tls_builder: ClientTLSConfigBuilder,
    client_tls_config: Option<ClientConfig>,
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

    /// Sets the HTTP protocol selection for client connections.
    ///
    /// Defaults to [`HttpProtocol::Auto`], which automatically negotiates HTTP/2 with HTTP/1.1 fallback.
    pub fn with_http_protocol(mut self, protocol: HttpProtocol) -> Self {
        self.connector_builder = self.connector_builder.with_http_protocol(protocol);
        self
    }

    /// Sets the maximum age of a connection before it's closed.
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
        let error_telemetry = HttpTransactionErrorTelemetry::from_builder(&metrics_builder);
        self.connector_builder = self.connector_builder.with_error_telemetry(error_telemetry.clone());

        let mut layer = EndpointTelemetryLayer::default()
            .with_metrics_builder(metrics_builder)
            .with_error_telemetry(error_telemetry);

        if let Some(endpoint_name_fn) = endpoint_name_fn {
            layer = layer.with_endpoint_name_fn(endpoint_name_fn);
        }

        self.endpoint_telemetry = Some(layer);
        self
    }

    /// Sets a Unix domain socket path to route all connections through.
    ///
    /// When set, the client will connect to this Unix socket instead of performing DNS resolution
    /// and TCP connection. The URI host is ignored—all requests are sent through the configured
    /// socket.
    ///
    /// Defaults to unset (TCP connections via DNS).
    #[cfg(unix)]
    pub fn with_unix_socket_path<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.connector_builder = self.connector_builder.with_unix_socket_path(path);
        self
    }

    /// Sets the TLS configuration.
    ///
    /// A TLS configuration builder is provided to allow for more advanced configuration of the TLS connection.
    /// [`Self::with_client_tls_config`] overrides these settings regardless of call order.
    pub fn with_tls_config<F>(mut self, f: F) -> Self
    where
        F: FnOnce(ClientTLSConfigBuilder) -> ClientTLSConfigBuilder,
    {
        self.tls_builder = f(self.tls_builder);
        self
    }

    /// Sets a complete Rustls client TLS configuration.
    ///
    /// This configuration takes precedence over all option-based settings made through [`Self::with_tls_config`] or
    /// [`Self::with_min_tls_version`], regardless of call order. The configuration is otherwise preserved, including
    /// its certificate verifier, client identity, enabled TLS protocol versions, and other security settings.
    ///
    /// Any ALPN protocols already present in `config` are discarded before the configuration is passed to
    /// `hyper-rustls`. [`Self::with_http_protocol`] remains the sole source of HTTP protocol selection:
    /// [`HttpProtocol::Auto`] advertises HTTP/2 with HTTP/1.1 fallback, while [`HttpProtocol::Http1`] enables only
    /// HTTP/1.1 and does not advertise ALPN.
    ///
    /// The supplied configuration is validated for FIPS compliance during [`Self::build`].
    pub fn with_client_tls_config(mut self, config: ClientConfig) -> Self {
        self.client_tls_config = Some(config);
        self
    }

    /// Sets the minimum TLS protocol version for HTTPS connections.
    ///
    /// Defaults to TLS 1.2.
    ///
    /// This updates the same TLS builder configured by [`Self::with_tls_config`], so call order matters when both
    /// methods change the minimum TLS version. [`Self::with_client_tls_config`] overrides this setting regardless of
    /// call order.
    pub fn with_min_tls_version(mut self, version: TlsMinimumVersion) -> Self {
        self.tls_builder = self.tls_builder.with_min_tls_version(version);
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
    /// This tracks bytes sent at the HTTP client level, which includes headers and body but doesn't include underlying
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
    /// If there was an error building the TLS configuration for the client, or if a supplied complete TLS
    /// configuration fails FIPS validation in a FIPS build, an error will be returned.
    pub fn build(self) -> Result<HttpClient, GenericError> {
        let tls_config = match self.client_tls_config {
            Some(mut config) => {
                ensure_client_config_fips_compliant(&config)?;
                config.alpn_protocols.clear();
                config
            }
            None => self.tls_builder.build()?,
        };
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
            client_tls_config: None,
            request_timeout: Some(Duration::from_secs(20)),
            endpoint_telemetry: None,
            proxies: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use http_body::Body as _;
    use http_body_util::{Empty, Full};
    use rustls::{server::WebPkiClientVerifier, ClientConfig, RootCertStore, ServerConfig};
    use saluki_tls::test_util::SelfSignedCert;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
        time::timeout,
    };
    use tokio_rustls::TlsAcceptor;

    use super::*;

    #[test]
    fn into_client_body_preserves_exact_size_hint() {
        let body = Full::new(Bytes::from_static(b"hello"));
        let converted = into_client_body(body);

        assert_eq!(Some(5), converted.size_hint().exact());
    }

    fn initialize_crypto_provider() {
        let _ = saluki_tls::initialize_default_crypto_provider();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "default crypto provider should be installed"
        );
    }

    #[tokio::test]
    async fn complete_tls_config_takes_precedence_when_set_last() {
        initialize_crypto_provider();
        let (server_config, client_config, option_root_store) = mutual_tls_configs();
        let builder = HttpClient::builder()
            .with_tls_config(|builder| builder.with_root_cert_store(option_root_store))
            .with_client_tls_config(client_config);

        send_request_to_tls_server(builder, server_config)
            .await
            .expect("supplied client identity should authenticate");
    }

    #[tokio::test]
    async fn complete_tls_config_takes_precedence_when_set_first() {
        initialize_crypto_provider();
        let (server_config, client_config, option_root_store) = mutual_tls_configs();
        let builder = HttpClient::builder()
            .with_client_tls_config(client_config)
            .with_tls_config(|builder| builder.with_root_cert_store(option_root_store));

        send_request_to_tls_server(builder, server_config)
            .await
            .expect("supplied client identity should authenticate");
    }

    #[tokio::test]
    async fn supplied_alpn_is_replaced_by_auto_protocol_selection() {
        initialize_crypto_provider();
        let negotiated_alpn = negotiate_alpn_with_supplied_config(HttpProtocol::Auto).await;

        assert_eq!(negotiated_alpn.as_deref(), Some(b"h2".as_slice()));
    }

    #[tokio::test]
    async fn supplied_alpn_is_removed_for_http1_protocol_selection() {
        initialize_crypto_provider();
        let negotiated_alpn = negotiate_alpn_with_supplied_config(HttpProtocol::Http1).await;

        assert_eq!(negotiated_alpn, None);
    }

    #[tokio::test]
    async fn option_based_tls_config_remains_in_use_without_complete_config() {
        initialize_crypto_provider();
        let server_cert = SelfSignedCert::localhost();
        let mut root_store = RootCertStore::empty();
        root_store
            .add(
                server_cert
                    .cert_chain()
                    .into_iter()
                    .next()
                    .expect("certificate chain should not be empty"),
            )
            .expect("server certificate should be a valid trust anchor");
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(server_cert.cert_chain(), server_cert.private_key())
            .expect("server TLS config should build");
        let builder = HttpClient::builder().with_tls_config(|builder| builder.with_root_cert_store(root_store));

        send_request_to_tls_server(builder, server_config)
            .await
            .expect("option-based TLS configuration should authenticate the server");
    }

    fn mutual_tls_configs() -> (ServerConfig, ClientConfig, RootCertStore) {
        let server_cert = SelfSignedCert::localhost();
        let client_cert = SelfSignedCert::new(["saluki-client"]);

        let mut client_root_store = RootCertStore::empty();
        client_root_store
            .add(
                client_cert
                    .cert_chain()
                    .into_iter()
                    .next()
                    .expect("certificate chain should not be empty"),
            )
            .expect("client certificate should be a valid trust anchor");
        let client_verifier = WebPkiClientVerifier::builder(Arc::new(client_root_store))
            .build()
            .expect("client certificate verifier should build");
        let server_config = ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(server_cert.cert_chain(), server_cert.private_key())
            .expect("server TLS config should build");

        let mut server_root_store = RootCertStore::empty();
        server_root_store
            .add(
                server_cert
                    .cert_chain()
                    .into_iter()
                    .next()
                    .expect("certificate chain should not be empty"),
            )
            .expect("server certificate should be a valid trust anchor");
        let option_root_store = RootCertStore::empty();
        let client_config = ClientConfig::builder()
            .with_root_certificates(server_root_store)
            .with_client_auth_cert(client_cert.cert_chain(), client_cert.private_key())
            .expect("client TLS config should build");

        (server_config, client_config, option_root_store)
    }

    async fn send_request_to_tls_server(
        builder: HttpClientBuilder, server_config: ServerConfig,
    ) -> Result<(), GenericError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("server should accept a connection");
            let mut stream = TlsAcceptor::from(Arc::new(server_config))
                .accept(stream)
                .await
                .expect("TLS handshake should succeed");
            let mut request = [0; 4096];
            let bytes_read = stream.read(&mut request).await.expect("server should read the request");
            assert!(bytes_read > 0, "server should receive an HTTP request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .expect("server should write the response");
        });

        let mut client = builder.with_http_protocol(HttpProtocol::Http1).build()?;
        let request = Request::get(format!("https://localhost:{port}/"))
            .body(Empty::<Bytes>::new())
            .expect("request should build");
        let response = timeout(Duration::from_secs(5), client.send(request))
            .await
            .map_err(|_| GenericError::msg("HTTP request timed out"))??;
        assert_eq!(response.status(), http::StatusCode::OK);
        timeout(Duration::from_secs(5), server_task)
            .await
            .expect("TLS server should finish")
            .expect("TLS server task should not panic");

        Ok(())
    }

    async fn negotiate_alpn_with_supplied_config(protocol: HttpProtocol) -> Option<Vec<u8>> {
        let server_cert = SelfSignedCert::localhost();
        let mut root_store = RootCertStore::empty();
        root_store
            .add(
                server_cert
                    .cert_chain()
                    .into_iter()
                    .next()
                    .expect("certificate chain should not be empty"),
            )
            .expect("server certificate should be a valid trust anchor");

        let mut server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(server_cert.cert_chain(), server_cert.private_key())
            .expect("server TLS config should build");
        server_config.alpn_protocols = vec![b"custom".to_vec(), b"h2".to_vec(), b"http/1.1".to_vec()];

        let mut client_config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"custom".to_vec()];

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("TLS server should bind");
        let port = listener.local_addr().expect("TLS server should have an address").port();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("server should accept a connection");
            let stream = TlsAcceptor::from(Arc::new(server_config))
                .accept(stream)
                .await
                .expect("TLS handshake should succeed");
            stream.get_ref().1.alpn_protocol().map(ToOwned::to_owned)
        });

        let mut client = HttpClient::builder()
            .with_http_protocol(protocol)
            .with_client_tls_config(client_config)
            .build()
            .expect("client should accept an ALPN-bearing TLS config");
        let request = Request::get(format!("https://localhost:{port}/"))
            .body(Empty::<Bytes>::new())
            .expect("request should build");
        let _ = timeout(Duration::from_secs(5), client.send(request)).await;

        timeout(Duration::from_secs(5), server_task)
            .await
            .expect("TLS server should finish")
            .expect("TLS server task should not panic")
    }
}
