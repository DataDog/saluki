//! A configurable Docker HTTP server for unit tests.

use std::sync::{Arc, Mutex};

use bollard::Docker;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// A fake Docker daemon with configurable responses and recorded requests.
pub(crate) struct FakeDaemon {
    pub(crate) docker: Docker,
    /// Each request's method and path.
    pub(crate) requests: Arc<Mutex<Vec<String>>>,
    /// Request bodies in the same order as `requests`.
    pub(crate) bodies: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl FakeDaemon {
    /// Starts a daemon using `respond` to map request methods and paths to HTTP statuses and JSON bodies.
    pub(crate) async fn start(respond: fn(&str, &str) -> (u16, &'static str)) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the fake daemon should bind");
        let address = listener.local_addr().expect("the listener should have an address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let recorded_bodies = bodies.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("the fake daemon should accept");
                let mut head = Vec::new();
                let mut buf = [0; 1024];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buf).await.expect("the request should be readable");
                    if read == 0 {
                        break;
                    }
                    head.extend_from_slice(&buf[..read]);
                }
                let header_end = head.windows(4).position(|window| window == b"\r\n\r\n").unwrap() + 4;
                let headers = String::from_utf8_lossy(&head[..header_end]);
                let body_len = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, length)| length.trim().parse::<usize>().unwrap());
                let mut request_line = headers.lines().next().unwrap_or_default().split(' ');
                let method = request_line.next().unwrap_or_default().to_string();
                let path = request_line.next().unwrap_or_default().to_string();
                while head.len() < header_end + body_len {
                    let read = stream
                        .read(&mut buf)
                        .await
                        .expect("the request body should be readable");
                    assert_ne!(read, 0, "the request body should be complete");
                    head.extend_from_slice(&buf[..read]);
                }
                recorded_bodies
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head[header_end..header_end + body_len]).into_owned());
                let (status, body) = respond(&method, &path);
                recorded.lock().unwrap().push(format!("{} {}", method, path));
                let response = format!(
                    "HTTP/1.1 {} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        let docker = Docker::connect_with_http(&format!("http://{}", address), 5, bollard::API_DEFAULT_VERSION)
            .expect("bollard should accept the fake daemon's address");
        Self {
            docker,
            requests,
            bodies,
            server,
        }
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}
