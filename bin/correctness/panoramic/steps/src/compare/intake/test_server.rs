//! A stand-in `datadog-intake` server for unit tests.

use std::net::TcpListener as StdTcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Installs the process-wide crypto provider, which building a `reqwest` client needs and `main` installs at startup.
pub fn install_crypto_provider() {
    let _ = saluki_tls::initialize_default_crypto_provider();
}

/// Returns a local port with no listener, so every request to it fails to connect.
///
/// Windows retries a refused connection for about two seconds before failing.
pub fn closed_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("should bind an ephemeral port");
    listener.local_addr().expect("should have a local address").port()
}

/// A local HTTP server that holds requests open. It stops when dropped.
pub struct MockIntake {
    port: u16,
    connections: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl MockIntake {
    /// Accepts connections but never answers a request.
    ///
    /// With Tokio's clock paused, requests stay pending as time advances to the next timer, making timeout tests
    /// independent of response timing.
    pub async fn unresponsive() -> Self {
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
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Returns how many connections the server has accepted.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for MockIntake {
    fn drop(&mut self) {
        self.task.abort();
    }
}
