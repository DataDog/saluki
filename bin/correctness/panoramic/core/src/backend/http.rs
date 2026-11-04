//! HTTP requests from the runner host.

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{body::Incoming, client::conn::http1, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_util::task::AbortOnDropHandle;

use crate::endpoint::ResolvedAddress;

/// Distinguishes invalid HTTP requests from connection failures.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HttpError {
    /// The request cannot be built, so retrying cannot help.
    Request(String),
    /// Connecting, the handshake or the exchange failed.
    Connection(String),
}

/// Returns the status of a plain HTTP GET as soon as the response headers arrive.
pub(crate) async fn get_status(address: &ResolvedAddress, path: &str) -> Result<u16, HttpError> {
    let (response, _connection) = request(address, path).await?;
    Ok(response.status().as_u16())
}

/// Returns the status and complete body of a plain HTTP GET.
pub(crate) async fn get(address: &ResolvedAddress, path: &str) -> Result<(u16, Bytes), HttpError> {
    let (response, _connection) = request(address, path).await?;
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|error| HttpError::Connection(error.to_string()))?
        .to_bytes();
    Ok((status, body))
}

async fn request(
    address: &ResolvedAddress, path: &str,
) -> Result<(Response<Incoming>, AbortOnDropHandle<Result<(), hyper::Error>>), HttpError> {
    let host = address
        .host
        .as_deref()
        .ok_or_else(|| HttpError::Request("HTTP address has no host".into()))?;
    let port = address
        .port
        .ok_or_else(|| HttpError::Request("HTTP address has no port".into()))?;
    let request = Request::get(path)
        .header("Host", format!("{host}:{port}"))
        .body(Empty::<Bytes>::new())
        .map_err(|error| HttpError::Request(error.to_string()))?;
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|error| HttpError::Connection(error.to_string()))?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|error| HttpError::Connection(error.to_string()))?;
    let connection = AbortOnDropHandle::new(tokio::spawn(connection));
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| HttpError::Connection(error.to_string()))?;
    Ok((response, connection))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::timeout,
    };

    use super::*;
    use crate::endpoint::EndpointKind;

    fn address(port: u16) -> ResolvedAddress {
        ResolvedAddress {
            kind: EndpointKind::Http,
            url: format!("http://127.0.0.1:{port}"),
            host: Some("127.0.0.1".into()),
            port: Some(port),
            path: None,
        }
    }

    #[tokio::test]
    async fn an_http_check_returns_the_status_of_a_get_to_the_path() {
        for status in [200, 503] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    request.push(stream.read_u8().await.unwrap());
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                assert_eq!(
                    String::from_utf8(request).unwrap(),
                    format!("GET /ready?probe=1 HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n\r\n")
                );
                // The status check completes while the server withholds the body.
                stream
                    .write_all(format!("HTTP/1.1 {status} Status\r\nContent-Length: 100\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            });
            timeout(Duration::from_secs(5), async {
                assert_eq!(get_status(&address(port), "/ready?probe=1").await, Ok(status));
                server.await.unwrap();
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn an_incomplete_http_body_is_a_connection_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = address(listener.local_addr().unwrap().port());
        let responder = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\none")
                .await
                .unwrap();
        };
        timeout(Duration::from_secs(5), async {
            let (_, result) = tokio::join!(responder, get(&address, "/dump"));
            assert!(matches!(result, Err(HttpError::Connection(_))), "{result:?}");
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_http_check_of_a_closed_port_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = timeout(Duration::from_secs(5), get_status(&address(port), "/ready"))
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(error, HttpError::Connection(ref message) if message.to_lowercase().contains("refused")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_invalid_http_request_is_distinct_from_a_connection_failure() {
        for path in ["/a b", "/a\tb"] {
            assert_eq!(
                get_status(&address(1), path).await,
                Err(HttpError::Request("invalid uri character".into()))
            );
        }
        for (address, message) in [
            (
                ResolvedAddress {
                    host: None,
                    ..address(1)
                },
                "HTTP address has no host",
            ),
            (
                ResolvedAddress {
                    port: None,
                    ..address(1)
                },
                "HTTP address has no port",
            ),
        ] {
            assert_eq!(
                get_status(&address, "/ready").await,
                Err(HttpError::Request(message.into()))
            );
        }
    }
}
