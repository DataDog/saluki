//! A stand-in `datadog-intake` server for unit tests.

use std::net::TcpListener as StdTcpListener;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rand::RngExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Installs the process-wide crypto provider, which building a `reqwest` client needs and `main` installs at startup.
pub(crate) fn install_crypto_provider() {
    let _ = crate::default_crypto_provider().install_default();
}

/// Ports that no default ephemeral range includes: Linux uses 32768-60999, and macOS and Windows use 49152-65535.
const NON_EPHEMERAL_PORTS: Range<u16> = 1024..32768;

/// Returns a local port with no listener, so every request to it fails to connect.
///
/// The port comes from outside the ephemeral range, so a test running in parallel can't get it by binding port 0 and
/// start answering on it. Holding the port with a bound socket instead doesn't work: macOS drops connections to a
/// bound socket that isn't listening, rather than refusing them.
///
/// Windows retries a refused connection for about two seconds before failing.
pub(crate) fn closed_port() -> u16 {
    let mut rng = rand::rng();
    (0..100)
        .map(|_| rng.random_range(NON_EPHEMERAL_PORTS))
        .find(|&port| StdTcpListener::bind(("127.0.0.1", port)).is_ok())
        .expect("should find a free port outside the ephemeral range")
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
        Self::unresponsive_with_connection_signal().await.0
    }

    /// Holds connections without answering and signals when the first connection is accepted.
    pub(crate) async fn unresponsive_with_connection_signal() -> (Self, oneshot::Receiver<()>) {
        let (connected_tx, connected_rx) = oneshot::channel();
        let server = Self::spawn(move |listener, connections| async move {
            let mut connected_tx = Some(connected_tx);
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                held.push(stream);
                if let Some(connected_tx) = connected_tx.take() {
                    let _ = connected_tx.send(());
                }
            }
        })
        .await;
        (server, connected_rx)
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
