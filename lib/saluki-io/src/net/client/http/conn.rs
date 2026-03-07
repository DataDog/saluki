use std::{
    future::Future,
    io,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use http::{Extensions, Uri};
use hyper_hickory::{TokioHickoryHttpConnector, TokioHickoryResolver};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder, MaybeHttpsStream};
use hyper_util::{
    client::legacy::connect::{CaptureConnection, Connected, Connection, HttpConnector},
    rt::TokioIo,
};
use metrics::Counter;
use pin_project_lite::pin_project;
use rustls::ClientConfig;
use saluki_error::{ErrorContext as _, GenericError};
use tokio::net::TcpStream;
use tower::{BoxError, Service};
use tracing::debug;

/// Imposes a limit on the age of a connection.
///
/// In many cases, it is undesirable to hold onto a connection indefinitely, even if it can be theoretically reused.
/// Doing so can make it more difficult to perform maintenance on infrastructure, as the expectation of old connections
/// being eventually closed and replaced is not upheld.
///
/// This extension allows tracking the age of a connection (based on when the connector creates the connection) and
/// checking if it is expired, or past the configured limit. Callers can then decide how to handle the expiration, such
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

/// An inner transport that abstracts over TCP and Unix domain socket connections.
///
/// This allows using a single monomorphization of the HTTP/2 and TLS stacks regardless of the
/// underlying transport, avoiding duplicate code generation for each transport type.
enum Transport {
    Tcp(TokioIo<TcpStream>),
    #[cfg(unix)]
    Unix(TokioIo<tokio::net::UnixStream>),
}

impl Connection for Transport {
    fn connected(&self) -> Connected {
        match self {
            Self::Tcp(s) => s.connected(),
            #[cfg(unix)]
            Self::Unix(_) => Connected::new(),
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
        }
    }
}

impl hyper::rt::Write for Transport {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match Pin::get_mut(self) {
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::get_mut(self) {
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match Pin::get_mut(self) {
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_shutdown(cx),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Tcp(s) => s.is_write_vectored(),
            #[cfg(unix)]
            Self::Unix(s) => s.is_write_vectored(),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::get_mut(self) {
            Self::Tcp(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_write_vectored(cx, bufs),
        }
    }
}

pin_project! {
    /// A connection that supports both HTTP and HTTPS.
    pub struct HttpsCapableConnection {
        #[pin]
        inner: MaybeHttpsStream<Transport>,
        bytes_sent: Option<Counter>,
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
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.project();
        this.inner.poll_flush(cx)
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
            other => other,
        }
    }
}

/// An inner connector that routes to either TCP (via DNS) or a Unix domain socket.
///
/// When a Unix socket path is configured, all connections are routed through that socket regardless
/// of the URI host. Otherwise, connections are routed via the standard DNS + TCP path.
#[derive(Clone)]
struct InnerConnector {
    http: TokioHickoryHttpConnector,
    connect_timeout: Duration,
    #[cfg(unix)]
    unix_socket_path: Option<Arc<std::path::Path>>,
}

impl Service<Uri> for InnerConnector {
    type Response = Transport;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Transport, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // When routing via a Unix domain socket, the TCP/DNS connector is not used, so we consider
        // the service immediately ready.
        #[cfg(unix)]
        if self.unix_socket_path.is_some() {
            return Poll::Ready(Ok(()));
        }

        self.http.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        #[cfg(unix)]
        if let Some(path) = self.unix_socket_path.clone() {
            let connect_timeout = self.connect_timeout;
            return Box::pin(async move {
                let stream = tokio::time::timeout(connect_timeout, tokio::net::UnixStream::connect(&*path))
                    .await
                    .map_err(|_| -> BoxError {
                        Box::new(io::Error::new(io::ErrorKind::TimedOut, "unix socket connect timed out"))
                    })?
                    .map_err(|e| -> BoxError { Box::new(e) })?;
                Ok(Transport::Unix(TokioIo::new(stream)))
            });
        }

        let fut = self.http.call(dst);
        Box::pin(async move {
            let tcp = fut.await.map_err(BoxError::from)?;
            Ok(Transport::Tcp(tcp))
        })
    }
}

/// A connector that supports HTTP or HTTPS.
#[derive(Clone)]
pub struct HttpsCapableConnector {
    inner: HttpsConnector<InnerConnector>,
    bytes_sent: Option<Counter>,
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
        let inner = self.inner.call(dst);
        let bytes_sent = self.bytes_sent.clone();
        let conn_age_limit = self.conn_age_limit;
        Box::pin(async move {
            inner.await.map(|inner| HttpsCapableConnection {
                inner,
                bytes_sent,
                conn_age_limit,
            })
        })
    }
}

/// A builder for `HttpsCapableConnector`.
#[derive(Default)]
pub struct HttpsCapableConnectorBuilder {
    connect_timeout: Option<Duration>,
    bytes_sent: Option<Counter>,
    conn_age_limit: Option<Duration>,
    #[cfg(unix)]
    unix_socket_path: Option<PathBuf>,
}

impl HttpsCapableConnectorBuilder {
    /// Sets the timeout when connecting to the remote host.
    ///
    /// Defaults to 30 seconds.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
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
        self.conn_age_limit = limit.into();
        self
    }

    /// Sets a counter that gets incremented with the number of bytes sent over the connection.
    ///
    /// This tracks bytes sent at the HTTP client level, which includes headers and body but does not include underlying
    /// transport overhead, such as TLS handshaking, and so on.
    ///
    /// Defaults to unset.
    pub fn with_bytes_sent_counter(mut self, counter: Counter) -> Self {
        self.bytes_sent = Some(counter);
        self
    }

    /// Sets a Unix domain socket path to route all connections through.
    ///
    /// When set, the connector will connect to this Unix socket instead of performing DNS resolution
    /// and TCP connection. The URI host is ignored in this case — all requests are sent through the
    /// configured socket.
    ///
    /// Defaults to unset (TCP connections via DNS).
    #[cfg(unix)]
    pub fn with_unix_socket_path<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.unix_socket_path = Some(path.into());
        self
    }

    /// Builds the `HttpsCapableConnector` from the given TLS configuration.
    pub fn build(self, tls_config: ClientConfig) -> Result<HttpsCapableConnector, GenericError> {
        let connect_timeout = self.connect_timeout.unwrap_or(Duration::from_secs(30));

        let hickory_resolver = TokioHickoryResolver::from_system_conf()
            .error_context("Failed to load system DNS configuration when creating DNS resolver for HTTP client.")?;

        // Create the HTTP connector, and ensure that we don't enforce _only_ HTTP, since that will break being able to
        // wrap this in an HTTPS connector.
        let mut http_connector = HttpConnector::new_with_resolver(hickory_resolver);
        http_connector.set_connect_timeout(Some(connect_timeout));
        http_connector.enforce_http(false);

        let inner_connector = InnerConnector {
            http: http_connector,
            connect_timeout,
            #[cfg(unix)]
            unix_socket_path: self.unix_socket_path.map(PathBuf::into_boxed_path).map(Arc::from),
        };

        // Create the HTTPS connector.
        let https_connector = HttpsConnectorBuilder::new()
            .with_tls_config(tls_config)
            .https_or_http()
            .enable_all_versions()
            .wrap_connector(inner_connector);

        Ok(HttpsCapableConnector {
            inner: https_connector,
            bytes_sent: self.bytes_sent,
            conn_age_limit: self.conn_age_limit,
        })
    }
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
