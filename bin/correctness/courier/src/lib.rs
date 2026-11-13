//! Probes, sends, HTTP requests and file follows, run from where the software runs.
//!
//! Panoramic copies the `courier` binary into each container and pod and runs it there through exec, so it reaches a
//! listener the way a peer would. A probe from outside proves nothing: the Docker port proxy accepts a connection even
//! when nothing listens inside. For host process groups, panoramic calls this library in-process instead.
//!
//! The courier has four operations, and only four:
//!
//! - [`probe`]: whether something accepts a connection at a `tcp`, `http` or `grpc` address, whether a Unix socket
//!   exists and accepts, or whether a named pipe exists. A probe cannot confirm a UDP listener, so it refuses `udp`.
//! - [`send`]: opens one connection, or sends one datagram, and writes the bytes as given.
//! - [`http_get`]: makes one plain-HTTP `GET` request and returns the status and body. The courier does not speak TLS.
//! - [`follow`]: streams a log file's contents as they are written, so an image needs neither `sh` nor `tail`.
//!
//! # Addresses
//!
//! An [`Address`] is written as the URL a consumer receives: `tcp://<host>:<port>`, `udp://<host>:<port>`,
//! `http://<host>:<port>`, `grpc://<host>:<port>`, `unix://<path>` for a stream socket, `unixgram://<path>` for a
//! datagram socket, and `pipe://<name>` for the named pipe `\\.\pipe\<name>` on Windows.
//!
//! # Command line
//!
//! ```text
//! courier --deadline <duration> probe <address>
//! courier --deadline <duration> send <address>         < payload
//! courier --deadline <duration> http <address> [--path <path>]
//! courier --deadline <duration> follow <path>
//! ```
//!
//! `--deadline` is required, and is a whole number with a unit: `ms`, `s` or `m`, as in `500ms`, `10s` or `2m`. It
//! bounds the whole operation, including name resolution, but not reading the input of `send` or writing the output of
//! `http`. For `follow`, it is how long to follow: reaching it after the file was found is the normal end, even while
//! output is being written.
//!
//! - `probe` writes nothing to standard output.
//! - `send` reads its payload from standard input until end of file, then writes it. It writes nothing to standard
//!   output.
//! - `http` requests `--path` (default `/`) and writes the status code on one line, then the body exactly as
//!   received.
//! - `follow` waits for the file to exist, then writes its contents from the start, as they arrive.
//!
//! The library's `limit` is the same deadline, with one gap. Name resolution runs `getaddrinfo` on a blocking thread,
//! and the deadline abandons the lookup without stopping it, so the thread stays busy until the resolver answers. The
//! binary exits without waiting for it.
//!
//! On failure, the courier writes one line to standard error, starting with `courier: `. Bad arguments instead get an
//! error and usage text on standard error.
//!
//! # Exit codes
//!
//! - `0`: the operation completed. For `http`, a response arrived, whatever its status.
//! - `1`: the software refused or is not there yet. A name does not resolve, nothing accepted, a read or write failed,
//!   the deadline passed, or an HTTP response was malformed. A peer's name may not resolve until the peer starts.
//! - `2`: a harness problem. The arguments were bad, the operation does not support the address, or reading input or
//!   writing output failed.
//! - Any other status, such as `101` from a panic or none from a signal, is a harness problem.
//!
//! [`Error::fault`] makes the same split for library callers.
#![deny(missing_docs)]

use std::{io, time::Duration};

use snafu::Snafu;

mod address;
mod follow;
mod http;
mod net;
mod probe;
mod send;

pub use self::{
    address::{Address, HostPort},
    follow::follow,
    http::{http_get, HttpResponse},
    probe::probe,
    send::send,
};

/// Who an [`Error`] blames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    /// The courier worked, and the software refused, answered badly or is not there yet.
    Software,

    /// The courier could not do what was asked: the request was invalid or a local resource failed.
    Harness,
}

/// An error from a courier operation.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    /// The text is not an address the courier understands.
    #[snafu(display("invalid address '{address}': {reason}"))]
    InvalidAddress {
        /// The text as given.
        address: String,
        /// What is wrong with it.
        reason: &'static str,
    },

    /// The operation cannot be done on this kind of address, or on this OS.
    #[snafu(display("{operation} cannot reach '{address}': {reason}"))]
    Unsupported {
        /// The operation asked for.
        operation: &'static str,
        /// The address it was asked to reach.
        address: Address,
        /// Why not.
        reason: &'static str,
    },

    /// The HTTP request path is not one the courier can send.
    #[snafu(display("invalid HTTP path '{path}': it must start with '/' and hold no whitespace"))]
    InvalidPath {
        /// The path as given.
        path: String,
    },

    /// The host name in the address does not resolve.
    #[snafu(display("failed to resolve '{host}': {source}"))]
    Resolve {
        /// The host name.
        host: String,
        /// The error from the resolver.
        source: io::Error,
    },

    /// Reaching or talking to the target failed.
    #[snafu(display("failed to {action} '{target}': {source}"))]
    Target {
        /// What the courier was doing, such as `connect to`.
        action: &'static str,
        /// The address or file path.
        target: String,
        /// The I/O error.
        source: io::Error,
    },

    /// The target answered with something that is not an HTTP/1 response.
    #[snafu(display("'{address}' sent a malformed HTTP response: {reason}"))]
    MalformedResponse {
        /// The address the request went to.
        address: Address,
        /// What is wrong with the response.
        reason: String,
    },

    /// The operation did not finish before its deadline.
    #[snafu(display("{operation} on '{target}' did not finish within {limit:?}"))]
    TimedOut {
        /// The operation.
        operation: &'static str,
        /// The address or file path.
        target: String,
        /// The deadline it was given.
        limit: Duration,
    },

    /// Reading the operation's input failed.
    #[snafu(display("failed to read input: {source}"))]
    Input {
        /// The I/O error.
        source: io::Error,
    },

    /// Writing the operation's output failed.
    #[snafu(display("failed to write output: {source}"))]
    Output {
        /// The I/O error.
        source: io::Error,
    },
}

impl Error {
    /// Returns who this error blames.
    pub fn fault(&self) -> Fault {
        match self {
            Self::Resolve { .. } | Self::Target { .. } | Self::MalformedResponse { .. } | Self::TimedOut { .. } => {
                Fault::Software
            }
            Self::InvalidAddress { .. }
            | Self::Unsupported { .. }
            | Self::InvalidPath { .. }
            | Self::Input { .. }
            | Self::Output { .. } => Fault::Harness,
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    use super::*;

    const LIMIT: Duration = Duration::from_secs(5);

    /// Returns an `http` address whose server answers one request with `response`, then closes.
    async fn serve_once(response: Vec<u8>) -> Address {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read the request first: closing with it unread resets the connection.
            assert!(stream.read(&mut [0; 1024]).await.unwrap() > 0);
            stream.write_all(&response).await.unwrap();
        });
        address
    }

    /// Returns an address on which nothing listens.
    async fn closed_port(scheme: &str) -> Address {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("{scheme}://127.0.0.1:{port}").parse().unwrap()
    }

    #[track_caller]
    fn assert_software<T: std::fmt::Debug>(result: Result<T, Error>) -> Error {
        let error = result.unwrap_err();
        assert_eq!(error.fault(), Fault::Software, "{error}");
        error
    }

    #[tokio::test]
    async fn closed_port_is_refused_by_the_software() {
        for scheme in ["tcp", "http", "grpc"] {
            let error = assert_software(probe(&closed_port(scheme).await, LIMIT).await);
            assert!(matches!(error, Error::Target { .. }), "{error}");
        }
        let error = assert_software(send(&closed_port("tcp").await, b"x", LIMIT).await);
        assert!(matches!(error, Error::Target { .. }), "{error}");
        let error = assert_software(http_get(&closed_port("http").await, "/", LIMIT).await);
        assert!(matches!(error, Error::Target { .. }), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_socket_is_refused_by_the_software() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sock").display().to_string();
        for scheme in ["unix", "unixgram"] {
            let address: Address = format!("{scheme}://{path}").parse().unwrap();
            let error = assert_software(probe(&address, LIMIT).await);
            assert!(matches!(error, Error::Target { .. }), "{error}");
            let error = assert_software(send(&address, b"x", LIMIT).await);
            assert!(matches!(error, Error::Target { .. }), "{error}");
        }
    }

    #[tokio::test]
    async fn silent_server_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: Address = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
        // The listener's backlog accepts the connection, and nothing ever answers.
        let error = assert_software(http_get(&address, "/", Duration::from_millis(200)).await);
        assert!(matches!(error, Error::TimedOut { .. }), "{error}");
    }

    #[tokio::test]
    async fn file_that_never_appears_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let result = follow(&dir.path().join("never.log"), Duration::from_millis(200), &mut out).await;
        let error = assert_software(result);
        assert!(matches!(error, Error::TimedOut { .. }), "{error}");
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn listening_port_accepts_a_probe() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: Address = format!("tcp://{}", listener.local_addr().unwrap()).parse().unwrap();
        probe(&address, LIMIT).await.unwrap();
    }

    #[tokio::test]
    async fn body_ends_at_content_length() {
        let address = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nready, and more".to_vec()).await;
        let response = http_get(&address, "/ready", LIMIT).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ready");
    }

    #[tokio::test]
    async fn body_without_content_length_ends_at_close() {
        let address = serve_once(b"HTTP/1.0 503 Service Unavailable\r\n\r\nnot yet".to_vec()).await;
        let response = http_get(&address, "/", LIMIT).await.unwrap();
        assert_eq!(response.status, 503);
        assert_eq!(response.body, b"not yet");
    }

    #[tokio::test]
    async fn malformed_response_is_the_softwares_fault() {
        let too_long = [&b"HTTP/1.1 200 OK\r\n\r\n"[..], &vec![b'x'; http::MAX_BODY_LEN + 1]].concat();
        for response in [
            b"SSH-2.0-OpenSSH\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort".to_vec(),
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", http::MAX_BODY_LEN + 1).into_bytes(),
            too_long,
        ] {
            let address = serve_once(response).await;
            let error = assert_software(http_get(&address, "/", LIMIT).await);
            assert!(matches!(error, Error::MalformedResponse { .. }), "{error}");
        }
    }

    #[tokio::test]
    async fn unsupported_request_is_a_harness_fault() {
        let udp: Address = "udp://127.0.0.1:9".parse().unwrap();
        let http: Address = "http://127.0.0.1:9".parse().unwrap();
        let cases = [
            probe(&udp, LIMIT).await,
            send(&http, b"x", LIMIT).await,
            http_get(&udp, "/", LIMIT).await.map(drop),
            http_get(&http, "ready", LIMIT).await.map(drop),
            http_get(&http, "/a b", LIMIT).await.map(drop),
        ];
        for result in cases {
            let error = result.unwrap_err();
            assert_eq!(error.fault(), Fault::Harness, "{error}");
        }
    }
}
