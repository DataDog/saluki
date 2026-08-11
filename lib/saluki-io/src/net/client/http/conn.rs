#[cfg(unix)]
use std::path::PathBuf;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use http::{Extensions, Uri};
use hyper_rustls::MaybeHttpsStream;
use hyper_util::{
    client::legacy::connect::{CaptureConnection, Connected, Connection, HttpConnector},
    rt::TokioIo,
};
use metrics::Counter;
use pin_project_lite::pin_project;
use rustls::{pki_types::ServerName, ClientConfig};
use saluki_error::GenericError;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
#[cfg(target_os = "linux")]
use tokio_vsock::{VsockAddr, VsockStream};
use tower::{BoxError, Service};
use tracing::debug;

use super::telemetry::HttpTransactionErrorTelemetry;
use crate::net::dns::{DnsError, SystemHttpConnector, SystemResolver};

/// Imposes a limit on the age of a connection.
///
/// In many cases, it's undesirable to hold onto a connection indefinitely, even if it can be theoretically reused.
/// Doing so can make it more difficult to perform maintenance on infrastructure, as the expectation of old connections
/// being eventually closed and replaced isn't upheld.
///
/// This extension allows tracking the age of a connection (based on when the connector creates the connection) and
/// checking if it's expired, or past the configured limit. Callers can then decide how to handle the expiration, such
/// as by closing the connection.
#[derive(Clone)]
struct ConnectionAgeLimit {
    limit: Duration,
    created: Instant,
}

impl ConnectionAgeLimit {
    fn new(limit: Duration) -> Self {
        ConnectionAgeLimit {
            limit,
            created: Instant::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.created.elapsed() >= self.limit
    }
}

/// An inner transport that abstracts over TCP, Unix domain socket, and vsock connections.
///
/// This allows using a single monomorphization of the HTTP/2 and TLS stacks regardless of the
/// underlying transport, avoiding duplicate code generation for each transport type.
enum Transport {
    Tcp(TokioIo<TcpStream>),
    #[cfg(unix)]
    Unix(TokioIo<tokio::net::UnixStream>),
    #[cfg(target_os = "linux")]
    Vsock(TokioIo<VsockStream>),
}

impl Connection for Transport {
    fn connected(&self) -> Connected {
        match self {
            Self::Tcp(s) => s.connected(),
            #[cfg(unix)]
            Self::Unix(_) => Connected::new(),
            #[cfg(target_os = "linux")]
            Self::Vsock(_) => Connected::new(),
        }
    }
}

impl hyper::rt::Read for Transport {
    fn poll_read(
        self: Pin<&mut Self>, cx: &mut Context<'_>, buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        match Pin::get_mut(self) {
            Self::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(target_os = "linux")]
            Self::Vsock(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl hyper::rt::Write for Transport {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match Pin::get_mut(self) {
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(target_os = "linux")]
            Self::Vsock(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::get_mut(self) {
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_flush(cx),
            #[cfg(target_os = "linux")]
            Self::Vsock(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::get_mut(self) {
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(target_os = "linux")]
            Self::Vsock(s) => Pin::new(s).poll_shutdown(cx),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Tcp(s) => s.is_write_vectored(),
            #[cfg(unix)]
            Self::Unix(s) => s.is_write_vectored(),
            #[cfg(target_os = "linux")]
            Self::Vsock(s) => s.is_write_vectored(),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::get_mut(self) {
            Self::Tcp(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            #[cfg(target_os = "linux")]
            Self::Vsock(s) => Pin::new(s).poll_write_vectored(cx, bufs),
        }
    }
}

pin_project! {
    /// A connection that supports both HTTP and HTTPS.
    pub struct HttpsCapableConnection {
        #[pin]
        inner: MaybeHttpsStream<Transport>,
        bytes_sent: Option<Counter>,
        error_telemetry: Option<HttpTransactionErrorTelemetry>,
        conn_age_limit: Option<Duration>,
    }
}

impl Connection for HttpsCapableConnection {
    fn connected(&self) -> Connected {
        let connected = self.inner.connected();

        if let Some(conn_age_limit) = self.conn_age_limit {
            debug!("setting connection age limit to {:?}", conn_age_limit);
            connected.extra(ConnectionAgeLimit::new(conn_age_limit))
        } else {
            connected
        }
    }
}

impl hyper::rt::Read for HttpsCapableConnection {
    fn poll_read(
        self: Pin<&mut Self>, cx: &mut Context<'_>, buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.project();
        this.inner.poll_read(cx, buf)
    }
}

impl hyper::rt::Write for HttpsCapableConnection {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.project();
        match this.inner.poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                if let Some(bytes_sent) = this.bytes_sent {
                    bytes_sent.increment(n as u64);
                }
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(error)) => {
                if let Some(error_telemetry) = this.error_telemetry.as_ref() {
                    error_telemetry.increment_wrote_request_error();
                }
                Poll::Ready(Err(error))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.project();
        match this.inner.poll_flush(cx) {
            Poll::Ready(Err(error)) => {
                if let Some(error_telemetry) = this.error_telemetry.as_ref() {
                    error_telemetry.increment_wrote_request_error();
                }
                Poll::Ready(Err(error))
            }
            other => other,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.project();
        this.inner.poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        match this.inner.poll_write_vectored(cx, bufs) {
            Poll::Ready(Ok(n)) => {
                if let Some(bytes_sent) = this.bytes_sent {
                    bytes_sent.increment(n as u64);
                }
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(error)) => {
                if let Some(error_telemetry) = this.error_telemetry.as_ref() {
                    error_telemetry.increment_wrote_request_error();
                }
                Poll::Ready(Err(error))
            }
            other => other,
        }
    }
}

/// An inner connector that routes to TCP (via DNS), a Unix domain socket, or a vsock socket.
///
/// When a Unix socket path is configured, all connections are routed through that socket regardless
/// of the URI host. When a vsock CID is configured, all connections are routed through that vsock
/// socket using the port from the destination URI. Otherwise, connections use the standard DNS +
/// TCP path.
#[derive(Clone)]
struct InnerConnector {
    http: SystemHttpConnector,
    #[cfg(unix)]
    connect_timeout: Duration,
    error_telemetry: Option<HttpTransactionErrorTelemetry>,
    #[cfg(unix)]
    unix_socket_path: Option<Arc<std::path::Path>>,
    #[cfg(target_os = "linux")]
    vsock_addr: Option<VsockAddr>,
}

impl Service<Uri> for InnerConnector {
    type Response = Transport;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Transport, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // When routing via vsock or a Unix domain socket, the TCP/DNS connector is not used, so we
        // consider the service immediately ready. vsock takes priority over Unix (matching Agent
        // behavior) when both are configured.
        #[cfg(target_os = "linux")]
        if self.vsock_addr.is_some() {
            return Poll::Ready(Ok(()));
        }

        #[cfg(unix)]
        if self.unix_socket_path.is_some() {
            return Poll::Ready(Ok(()));
        }

        self.http.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        #[cfg(target_os = "linux")]
        if let Some(addr) = self.vsock_addr {
            let connect_timeout = self.connect_timeout;
            let error_telemetry = self.error_telemetry.clone();
            return Box::pin(async move {
                let stream = tokio::time::timeout(connect_timeout, VsockStream::connect(addr))
                    .await
                    .map_err(|_| -> BoxError {
                        if let Some(error_telemetry) = &error_telemetry {
                            error_telemetry.increment_connection_error();
                        }
                        Box::new(io::Error::new(io::ErrorKind::TimedOut, "vsock connect timed out"))
                    })?
                    .map_err(|e| -> BoxError {
                        if let Some(error_telemetry) = &error_telemetry {
                            error_telemetry.increment_connection_error();
                        }
                        Box::new(e)
                    })?;
                Ok(Transport::Vsock(TokioIo::new(stream)))
            });
        }

        #[cfg(unix)]
        if let Some(path) = self.unix_socket_path.clone() {
            let connect_timeout = self.connect_timeout;
            let error_telemetry = self.error_telemetry.clone();
            return Box::pin(async move {
                let stream = tokio::time::timeout(connect_timeout, tokio::net::UnixStream::connect(&*path))
                    .await
                    .map_err(|_| -> BoxError {
                        if let Some(error_telemetry) = &error_telemetry {
                            error_telemetry.increment_connection_error();
                        }
                        Box::new(io::Error::new(io::ErrorKind::TimedOut, "unix socket connect timed out"))
                    })?
                    .map_err(|e| -> BoxError {
                        if let Some(error_telemetry) = &error_telemetry {
                            error_telemetry.increment_connection_error();
                        }
                        Box::new(e)
                    })?;
                Ok(Transport::Unix(TokioIo::new(stream)))
            });
        }

        let fut = self.http.call(dst);
        let error_telemetry = self.error_telemetry.clone();
        Box::pin(async move {
            let tcp = fut.await.map_err(|error| {
                if !is_dns_error(&error) {
                    if let Some(error_telemetry) = &error_telemetry {
                        error_telemetry.increment_connection_error();
                    }
                }
                BoxError::from(error)
            })?;
            Ok(Transport::Tcp(tcp))
        })
    }
}

/// HTTP protocol selection for client connections.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HttpProtocol {
    /// Automatically negotiate HTTP/2 with HTTP/1.1 fallback.
    #[default]
    Auto,

    /// Use HTTP/1.1 only.
    Http1,
}

/// A connector that supports HTTP or HTTPS.
///
/// Unlike [`hyper_rustls::HttpsConnector`], which fuses the transport connect and TLS handshake into a single
/// opaque future, this connector performs them as two distinct steps. That split allows a timeout to be scoped to
/// just the handshake, rather than the combined connect-and-handshake duration.
#[derive(Clone)]
pub struct HttpsCapableConnector {
    inner: InnerConnector,
    tls_config: Arc<ClientConfig>,
    tls_handshake_timeout: Duration,
    bytes_sent: Option<Counter>,
    error_telemetry: Option<HttpTransactionErrorTelemetry>,
    conn_age_limit: Option<Duration>,
}

impl Service<Uri> for HttpsCapableConnector {
    type Response = HttpsCapableConnection;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpsCapableConnection, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let is_https = match dst.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            scheme => {
                let scheme = scheme.map(str::to_owned);
                return Box::pin(async move {
                    Err(Box::new(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unsupported URI scheme: {scheme:?}"),
                    )) as BoxError)
                });
            }
        };
        let transport_fut = self.inner.call(dst.clone());
        let tls_config = Arc::clone(&self.tls_config);
        let tls_handshake_timeout = self.tls_handshake_timeout;
        let bytes_sent = self.bytes_sent.clone();
        let error_telemetry = self.error_telemetry.clone();
        let conn_age_limit = self.conn_age_limit;

        Box::pin(async move {
            let transport = transport_fut.await?;

            let inner = if is_https {
                let host = dst.host().ok_or_else(|| -> BoxError {
                    Box::new(io::Error::new(io::ErrorKind::InvalidInput, "URI has no host"))
                })?;
                let host = strip_ipv6_brackets(host);
                let server_name = ServerName::try_from(host)
                    .map_err(|error| -> BoxError { Box::new(error) })?
                    .to_owned();

                let handshake = TlsConnector::from(tls_config).connect(server_name, TokioIo::new(transport));

                match await_handshake_with_deadline(tls_handshake_timeout, handshake).await {
                    Ok(stream) => MaybeHttpsStream::from(stream),
                    Err(error) => {
                        if let Some(error_telemetry) = &error_telemetry {
                            error_telemetry.increment_tls_error();
                        }
                        return Err(error);
                    }
                }
            } else {
                MaybeHttpsStream::from(transport)
            };

            Ok(HttpsCapableConnection {
                inner,
                bytes_sent,
                error_telemetry,
                conn_age_limit,
            })
        })
    }
}

/// Strips the surrounding brackets from a bracketed IPv6 host, as found in a URI authority.
///
/// [`rustls::pki_types::ServerName`] accepts unbracketed IPv6 addresses but rejects the bracketed form that
/// [`http::Uri::host`] returns (for example, `[::1]`), so this normalizes the host before constructing the server name.
fn strip_ipv6_brackets(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

/// Awaits a TLS handshake future, bounding it by `timeout` unless `timeout` is zero.
///
/// A zero duration means the handshake deadline is disabled.
/// `tokio::time::timeout` with a zero duration fires immediately rather than never, so that case is handled by
/// awaiting the handshake directly instead of wrapping it in a timeout.
async fn await_handshake_with_deadline<F, T, E>(timeout: Duration, handshake: F) -> Result<T, BoxError>
where
    F: Future<Output = Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    if timeout.is_zero() {
        return handshake.await.map_err(|error| Box::new(error) as BoxError);
    }

    match tokio::time::timeout(timeout, handshake).await {
        Ok(result) => result.map_err(|error| Box::new(error) as BoxError),
        Err(_) => Err(Box::new(io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out")) as BoxError),
    }
}

fn build_dns_resolver(error_telemetry: &Option<HttpTransactionErrorTelemetry>) -> SystemResolver {
    let mut r = SystemResolver::new();
    if let Some(et) = error_telemetry {
        r = r.with_lookup_errors_counter(et.dns_errors());
    }
    r
}

/// A builder for `HttpsCapableConnector`.
#[derive(Default)]
pub struct HttpsCapableConnectorBuilder {
    connect_timeout: Option<Duration>,
    tls_handshake_timeout: Option<Duration>,
    bytes_sent: Option<Counter>,
    error_telemetry: Option<HttpTransactionErrorTelemetry>,
    conn_age_limit: Option<Duration>,
    http_protocol: HttpProtocol,
    #[cfg(unix)]
    unix_socket_path: Option<PathBuf>,
    #[cfg(target_os = "linux")]
    vsock_addr: Option<VsockAddr>,
}

impl HttpsCapableConnectorBuilder {
    /// Sets the timeout when connecting to the remote host.
    ///
    /// Defaults to 30 seconds.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// Sets the timeout for completing the TLS handshake after a connection is established.
    ///
    /// Defaults to 10 seconds.
    pub fn with_tls_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.tls_handshake_timeout = Some(timeout);
        self
    }

    /// Sets the HTTP protocol selection for client connections.
    ///
    /// Defaults to [`HttpProtocol::Auto`].
    pub fn with_http_protocol(mut self, protocol: HttpProtocol) -> Self {
        self.http_protocol = protocol;
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
        self.conn_age_limit = limit.into();
        self
    }

    /// Sets a counter that gets incremented with the number of bytes sent over the connection.
    ///
    /// This tracks bytes sent at the HTTP client level, which includes headers and body but doesn't include underlying
    /// transport overhead, such as TLS handshaking, and so on.
    ///
    /// Defaults to unset.
    pub fn with_bytes_sent_counter(mut self, counter: Counter) -> Self {
        self.bytes_sent = Some(counter);
        self
    }

    /// Sets the telemetry counters used to track HTTP request lifecycle failures.
    pub(super) fn with_error_telemetry(mut self, error_telemetry: HttpTransactionErrorTelemetry) -> Self {
        self.error_telemetry = Some(error_telemetry);
        self
    }

    /// Sets a Unix domain socket path to route all connections through.
    ///
    /// When set, the connector will connect to this Unix socket instead of performing DNS resolution
    /// and TCP connection. The URI host is ignored in this case—all requests are sent through the
    /// configured socket.
    ///
    /// Defaults to unset (TCP connections via DNS).
    #[cfg(unix)]
    pub fn with_unix_socket_path<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.unix_socket_path = Some(path.into());
        self
    }

    /// Sets a vsock address to route all connections through.
    ///
    /// When set, the connector will connect via AF_VSOCK using the given address, bypassing
    /// DNS and TCP. This allows connecting to a server process running in a host or hypervisor
    /// context from within a guest VM (for example, Nitro Enclaves).
    ///
    /// Defaults to unset (TCP connections via DNS).
    #[cfg(target_os = "linux")]
    pub fn with_vsock_addr(mut self, addr: VsockAddr) -> Self {
        self.vsock_addr = Some(addr);
        self
    }

    /// Builds the `HttpsCapableConnector` from the given TLS configuration.
    pub fn build(self, mut tls_config: ClientConfig) -> Result<HttpsCapableConnector, GenericError> {
        let connect_timeout = self.connect_timeout.unwrap_or(Duration::from_secs(30));
        let tls_handshake_timeout = self.tls_handshake_timeout.unwrap_or(Duration::from_secs(10));

        // Create the HTTP connector, and ensure that we don't enforce _only_ HTTP, since that will break being able to
        // wrap this in an HTTPS connector.
        let mut http_connector = HttpConnector::new_with_resolver(build_dns_resolver(&self.error_telemetry));
        http_connector.set_connect_timeout(Some(connect_timeout));
        http_connector.enforce_http(false);

        let inner_connector = InnerConnector {
            http: http_connector,
            #[cfg(unix)]
            connect_timeout,
            error_telemetry: self.error_telemetry.clone(),
            #[cfg(unix)]
            unix_socket_path: self.unix_socket_path.map(PathBuf::into_boxed_path).map(Arc::from),
            #[cfg(target_os = "linux")]
            vsock_addr: self.vsock_addr,
        };

        tls_config.alpn_protocols = http_protocol_alpns(self.http_protocol);

        Ok(HttpsCapableConnector {
            inner: inner_connector,
            tls_config: Arc::new(tls_config),
            tls_handshake_timeout,
            bytes_sent: self.bytes_sent,
            error_telemetry: self.error_telemetry,
            conn_age_limit: self.conn_age_limit,
        })
    }
}

/// Selects the ALPN protocols to advertise for the given HTTP protocol.
fn http_protocol_alpns(protocol: HttpProtocol) -> Vec<Vec<u8>> {
    match protocol {
        HttpProtocol::Auto => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        HttpProtocol::Http1 => Vec::new(),
    }
}

fn is_dns_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.downcast_ref::<DnsError>().is_some() {
            return true;
        }
        current = error.source();
    }
    false
}

pub(super) fn check_connection_state(captured_conn: CaptureConnection) {
    let maybe_conn_metadata = captured_conn.connection_metadata();
    if let Some(conn_metadata) = maybe_conn_metadata.as_ref() {
        let mut extensions = Extensions::new();
        conn_metadata.get_extras(&mut extensions);

        // If the connection has an age limit, check to see if the connection is expired (i.e. too old) and "poison"
        // it if so. Poisoning indicates to `hyper` that the connection should be closed/dropped instead of
        // returning it back to the idle connection pool.
        if let Some(conn_age_limit) = extensions.get::<ConnectionAgeLimit>() {
            if conn_age_limit.is_expired() {
                debug!("connection is expired; poisoning it");
                conn_metadata.poison();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io, time::Duration};

    use super::{await_handshake_with_deadline, http_protocol_alpns, HttpProtocol};

    #[tokio::test(start_paused = true)]
    async fn handshake_deadline_of_zero_disables_the_timeout() {
        let handshake = async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok::<_, io::Error>(())
        };

        let result = await_handshake_with_deadline(Duration::ZERO, handshake).await;
        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn handshake_deadline_times_out_when_exceeded() {
        let handshake = async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok::<_, io::Error>(())
        };

        let result = await_handshake_with_deadline(Duration::from_secs(10), handshake).await;
        let error = result.expect_err("expected handshake to time out");
        assert!(error.to_string().contains("TLS handshake timed out"));
    }

    #[tokio::test]
    async fn handshake_deadline_propagates_success() {
        let handshake = async { Ok::<_, io::Error>(42) };

        let result = await_handshake_with_deadline(Duration::from_secs(10), handshake).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn strip_ipv6_brackets_unwraps_bracketed_addresses() {
        use super::strip_ipv6_brackets;

        assert_eq!(strip_ipv6_brackets("[::1]"), "::1");
        assert_eq!(strip_ipv6_brackets("[2001:db8::1]"), "2001:db8::1");
    }

    #[test]
    fn strip_ipv6_brackets_leaves_unbracketed_hosts_alone() {
        use super::strip_ipv6_brackets;

        assert_eq!(strip_ipv6_brackets("example.com"), "example.com");
        assert_eq!(strip_ipv6_brackets("::1"), "::1");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn call_rejects_unsupported_uri_scheme() {
        use std::sync::Arc;

        use rustls::{ClientConfig, RootCertStore};
        use tower::Service as _;

        use super::{HttpsCapableConnector, InnerConnector};
        use crate::net::dns::SystemResolver;

        let inner = InnerConnector {
            http: SystemResolver::new().into_http_connector(),
            connect_timeout: Duration::from_secs(1),
            error_telemetry: None,
            unix_socket_path: None,
            #[cfg(target_os = "linux")]
            vsock_addr: None,
        };

        let tls_config = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(RootCertStore::empty())
                .with_no_client_auth(),
        );

        let mut connector = HttpsCapableConnector {
            inner,
            tls_config,
            tls_handshake_timeout: Duration::from_secs(1),
            bytes_sent: None,
            error_telemetry: None,
            conn_age_limit: None,
        };

        let uri: http::Uri = "ftp://example.com/".parse().unwrap();
        let error = connector.call(uri).await.err().expect("expected scheme to be rejected");
        assert!(error.to_string().contains("unsupported URI scheme"));
    }

    #[test]
    fn auto_protocol_advertises_h2_and_http1_alpn() {
        let alpn_protocols = http_protocol_alpns(HttpProtocol::Auto);

        assert_eq!(alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    }

    #[test]
    fn http1_protocol_leaves_alpn_empty() {
        let alpn_protocols = http_protocol_alpns(HttpProtocol::Http1);

        assert!(alpn_protocols.is_empty());
    }

    // vsock takes priority over unix when both are configured, matching Agent behavior.
    // We verify by checking the error does not mention "unix" — if unix had priority it would
    // fail with a socket-path error; vsock produces a connection or device error instead.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn vsock_takes_priority_over_unix_when_both_set() {
        use std::sync::Arc;

        use tower::Service as _;

        use super::{InnerConnector, VsockAddr};
        use crate::net::dns::SystemResolver;

        let mut connector = InnerConnector {
            http: SystemResolver::new().into_http_connector(),
            connect_timeout: std::time::Duration::from_secs(1),
            error_telemetry: None,
            unix_socket_path: Some(Arc::from(std::path::Path::new("/tmp/test.sock"))),
            vsock_addr: Some(VsockAddr::new(2, 5001)),
        };

        // Verify vsock path was taken: if unix had priority the error would mention the socket
        // path or "unix"; a vsock attempt produces a connection or device error instead.
        let uri: http::Uri = "https://127.0.0.1:5001/".parse().unwrap();
        let err = connector.call(uri).await.err().expect("expected a connection error");
        assert!(
            !err.to_string().contains("unix"),
            "expected vsock error (not unix socket error), got: {err}"
        );
    }
}
