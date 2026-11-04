//! A stand-in `datadog-intake` server for unit tests.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Installs the process-wide crypto provider, which building a `reqwest` client needs and `main` installs at startup.
pub(crate) fn install_crypto_provider() {
    let _ = crate::default_crypto_provider().install_default();
}

/// A local port with no listener, so every connection to it is refused.
///
/// The port stays bound, without listening, until this is dropped. Releasing it at once would let a test running in
/// parallel bind the same ephemeral port and start answering on it.
///
/// Windows retries a refused connection for about two seconds before failing.
pub(crate) struct ClosedPort {
    port: u16,
    _socket: TcpSocket,
}

impl ClosedPort {
    /// Returns the port number.
    pub(crate) fn port(&self) -> u16 {
        self.port
    }
}

/// Reserves a local port with no listener. See [`ClosedPort`].
pub(crate) fn closed_port() -> ClosedPort {
    let socket = TcpSocket::new_v4().expect("should create a socket");
    socket
        .bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .expect("should bind an ephemeral port");
    let port = socket.local_addr().expect("should have a local address").port();
    ClosedPort { port, _socket: socket }
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
