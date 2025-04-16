use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use http::{Extensions, Uri};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder, MaybeHttpsStream};
use hyper_util::{
    client::legacy::connect::{CaptureConnection, Connected, Connection, HttpConnector},
    rt::TokioIo,
};
use metrics::Counter;
use pin_project_lite::pin_project;
use rustls::ClientConfig;
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

pin_project! {
    /// A connection that supports both HTTP and HTTPS.
    pub struct HttpsCapableConnection {
        #[pin]
        inner: MaybeHttpsStream<TokioIo<TcpStream>>,
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

/// A connector that supports HTTP or HTTPS.
#[derive(Clone)]
pub struct HttpsCapableConnector {
    inner: HttpsConnector<HttpConnector>,
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
#[derive(Clone, Default)]
pub struct HttpsCapableConnectorBuilder {
    connect_timeout: Option<Duration>,
    bytes_sent: Option<Counter>,
    conn_age_limit: Option<Duration>,
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

    /// Builds the `HttpsCapableConnector` from the given TLS configuration.
    pub fn build(self, tls_config: ClientConfig) -> HttpsCapableConnector {
        let connect_timeout = self.connect_timeout.unwrap_or(Duration::from_secs(30));

        // Create the HTTP connector, and ensure that we don't enforce _only_ HTTP, since that will break being able to
        // wrap this in an HTTPS connector.
        let mut http_connector = HttpConnector::new();
        http_connector.set_connect_timeout(Some(connect_timeout));
        http_connector.enforce_http(false);

        // Create the HTTPS connector.
        let https_connector = HttpsConnectorBuilder::new()
            .with_tls_config(tls_config)
            .https_or_http()
            .enable_all_versions()
            .wrap_connector(http_connector);

        HttpsCapableConnector {
            inner: https_connector,
            bytes_sent: self.bytes_sent,
            conn_age_limit: self.conn_age_limit,
        }
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
