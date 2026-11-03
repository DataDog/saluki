//! A stand-in `datadog-intake` server for unit tests.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Installs the process-wide crypto provider, which building a `reqwest` client needs and `main` installs at startup.
pub(crate) fn install_crypto_provider() {
    let _ = crate::default_crypto_provider().install_default();
}

/// Returns a local port with no listener, so every request to it fails to connect.
///
/// The port lies below the default ephemeral range of Linux (32768), macOS and Windows (49152), so a concurrently
/// running test that binds port 0 can't be handed it. Taking the port from the ephemeral range instead would let such
/// a test start listening on it, and connections that should be refused would succeed.
///
/// Windows retries a refused connection for about two seconds before failing.
pub(crate) fn closed_port() -> u16 {
    use rand::RngExt as _;

    let mut rng = rand::rng();
    for _ in 0..100 {
        let port = rng.random_range(CLOSED_PORT_RANGE);
        if is_unbound(port) {
            return port;
        }
    }
    panic!("should find an unbound port in {CLOSED_PORT_RANGE:?}");
}

/// The ports `closed_port` picks from, all below every platform's default ephemeral range.
const CLOSED_PORT_RANGE: std::ops::Range<u16> = 20000..32768;

/// Returns whether no IPv4 socket is bound to `port`, so nothing would accept a connection to `127.0.0.1:port`.
///
/// The probe binds the wildcard address without `SO_REUSEADDR`, which conflicts with an existing socket on the port
/// whether it's bound to `127.0.0.1` or to the wildcard. Probing `127.0.0.1` instead would miss a wildcard listener on
/// Windows, which lets a socket bind a specific address alongside one, and on macOS if the probe set `SO_REUSEADDR`.
fn is_unbound(port: u16) -> bool {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).expect("should create a TCP socket");
    socket
        .bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())
        .is_ok()
}

/// A local HTTP server answering the `datadog-intake` dump endpoints. It stops when dropped.
pub(crate) struct MockIntake {
    port: u16,
    connections: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl MockIntake {
    /// Serves `metrics` from the metrics dump endpoint and an empty dump from every other endpoint.
    pub(crate) async fn serving(metrics: serde_json::Value) -> Self {
        let metrics = metrics.to_string();
        Self::spawn(move |listener, connections| async move {
            while let Ok((stream, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(respond(stream, metrics.clone()));
            }
        })
        .await
    }

    /// Answers the first connection like [`MockIntake::serving`], then accepts and closes subsequent connections.
    pub(crate) async fn serving_once(metrics: serde_json::Value) -> Self {
        let metrics = metrics.to_string();
        Self::spawn(move |listener, connections| async move {
            while let Ok((stream, _)) = listener.accept().await {
                if connections.fetch_add(1, Ordering::SeqCst) == 0 {
                    tokio::spawn(respond(stream, metrics.clone()));
                }
            }
        })
        .await
    }

    /// Accepts connections and closes them without answering.
    ///
    /// Requests fail at once on every platform, unlike a closed port, which Windows retries for seconds.
    pub(crate) async fn hanging_up() -> Self {
        Self::spawn(|listener, connections| async move {
            while let Ok((stream, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        })
        .await
    }

    /// Accepts connections but never answers a request.
    ///
    /// With Tokio's clock paused, requests stay pending as time advances to the next timer, making timeout tests
    /// independent of response timing.
    pub(crate) async fn unresponsive() -> Self {
        Self::spawn(|listener, connections| async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                held.push(stream);
            }
        })
        .await
    }

    async fn spawn<F, Fut>(serve: F) -> Self
    where
        F: FnOnce(TcpListener, Arc<AtomicUsize>) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        install_crypto_provider();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind an ephemeral port");
        let port = listener.local_addr().expect("should have a local address").port();
        let connections = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(serve(listener, connections.clone()));
        Self {
            port,
            connections,
            task,
        }
    }

    /// Returns the port the server listens on.
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Returns how many connections the server has accepted.
    pub(crate) fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for MockIntake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn respond(mut stream: TcpStream, metrics: String) {
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }

    let request = String::from_utf8_lossy(&request);
    let path = request.split_whitespace().nth(1).unwrap_or_default();
    let body = match path {
        "/metrics/dump" => metrics,
        "/traces/dump_stats" => r#"{"groups":{}}"#.to_string(),
        _ => "[]".to_string(),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::net::{TcpListener as StdTcpListener, TcpStream as StdTcpStream};

    use super::*;

    #[test]
    fn a_closed_port_is_outside_the_ephemeral_range_and_refuses_connections() {
        let port = closed_port();

        assert!(CLOSED_PORT_RANGE.contains(&port), "{port}");
        let connect =
            StdTcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect_err("nothing should accept a connection");
        assert_eq!(connect.kind(), ErrorKind::ConnectionRefused, "{connect}");
    }

    #[test]
    fn a_port_with_a_wildcard_listener_is_not_unbound() {
        // A standard listener sets `SO_REUSEADDR` on Unix, as most servers do.
        let listener = StdTcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("should bind an ephemeral port");
        let port = listener.local_addr().expect("should have a local address").port();

        assert!(!is_unbound(port));
    }

    #[test]
    fn a_port_with_a_loopback_listener_is_not_unbound() {
        let listener = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("should bind an ephemeral port");
        let port = listener.local_addr().expect("should have a local address").port();

        assert!(!is_unbound(port));
    }
}
