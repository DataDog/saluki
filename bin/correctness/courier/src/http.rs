use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{client::conn::http1, Request, Uri};
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};

use crate::{Report, Target, UsageError};

pub(crate) fn request(url: &str) -> Result<(Request<Empty<Bytes>>, Target), UsageError> {
    let invalid = |reason| UsageError {
        reason: format!("invalid HTTP URL {url:?}: {reason}"),
    };
    let uri: Uri = url
        .parse()
        .map_err(|_| invalid("expected http://<host>:<port><path>"))?;
    if uri.scheme_str() != Some("http") || url.contains('#') {
        return Err(invalid("only http:// URLs without fragments are supported"));
    }
    let authority = uri.authority().ok_or_else(|| invalid("missing host and port"))?;
    let target = Target::parse(&format!("tcp://{authority}"))?;
    let request = Request::get(uri.path_and_query().map_or("/", |path| path.as_str()))
        .header("Host", authority.as_str())
        .body(Empty::new())
        .map_err(|_| invalid("invalid request path or host"))?;
    Ok((request, target))
}

pub(crate) async fn get(url: &str, headers_only: bool, stdout: &mut (impl AsyncWrite + Unpin)) -> Report {
    let (request, target) = match request(url) {
        Ok(request) => request,
        Err(error) => {
            return Report::Usage {
                reason: error.to_string(),
            }
        }
    };
    let Target::Tcp { host, port } = target else {
        unreachable!()
    };
    let unreachable = |error| Report::Unreachable {
        reason: format!("{url}: {error}"),
    };
    let stream = match TcpStream::connect((host.as_str(), port)).await {
        Ok(stream) => stream,
        Err(error) => return unreachable(error.to_string()),
    };
    let (mut sender, connection) = match http1::handshake(TokioIo::new(stream)).await {
        Ok(connection) => connection,
        Err(error) => return unreachable(error.to_string()),
    };
    // Keep the connection in this future so cancellation closes it, including headers-only requests.
    tokio::select! {
        biased;
        report = async {
            let response = match sender.send_request(request).await {
                Ok(response) => response,
                Err(error) => return unreachable(error.to_string()),
            };
            let status = response.status().as_u16();
            let mut bytes = 0;
            if !headers_only {
                let mut body = response.into_body();
                while let Some(frame) = body.frame().await {
                    let frame = match frame {
                        Ok(frame) => frame,
                        Err(error) => return unreachable(error.to_string()),
                    };
                    if let Ok(data) = frame.into_data() {
                        if let Err(error) = async {
                            stdout.write_all(&data).await?;
                            stdout.flush().await
                        }.await {
                            return Report::Failed { reason: format!("writing stdout: {error}") };
                        }
                        bytes += data.len() as u64;
                    }
                }
            }
            Report::Responded { status, bytes }
        } => report,
        error = async {
            match connection.await {
                Err(error) => error,
                Ok(()) => std::future::pending().await,
            }
        } => unreachable(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{io::AsyncReadExt, net::TcpListener};

    use super::*;
    use crate::{execute, Command, Invocation};

    async fn server() -> (TcpListener, Invocation) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let invocation = Invocation {
            command: Command::Http {
                url: format!("http://{}/records?kind=all", listener.local_addr().unwrap()),
                headers_only: false,
            },
            timeout: Some(Duration::from_secs(10)),
        };
        (listener, invocation)
    }

    async fn accept(listener: &TcpListener) -> TcpStream {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
        }
        assert_eq!(
            request,
            format!(
                "GET /records?kind=all HTTP/1.1\r\nhost: {}\r\n\r\n",
                listener.local_addr().unwrap()
            )
            .as_bytes()
        );
        stream
    }

    #[tokio::test]
    async fn http_streams_complete_binary_bodies_and_large_chunked_bodies() {
        for chunked in [false, true] {
            let (listener, invocation) = server().await;
            let body = if chunked {
                b"\0\xff\r\n".repeat(300_000)
            } else {
                b"one\0\xff\r\ntwo".to_vec()
            };
            let mut stdout = Vec::new();
            let responder = async {
                let mut stream = accept(&listener).await;
                let framing = if chunked {
                    "Transfer-Encoding: chunked".into()
                } else {
                    format!("Content-Length: {}", body.len())
                };
                stream
                    .write_all(format!("HTTP/1.1 503 Unavailable\r\n{framing}\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                for chunk in body.chunks(8192) {
                    if chunked {
                        stream
                            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                            .await
                            .unwrap();
                    }
                    stream.write_all(chunk).await.unwrap();
                    if chunked {
                        stream.write_all(b"\r\n").await.unwrap();
                    }
                }
                if chunked {
                    stream.write_all(b"0\r\n\r\n").await.unwrap();
                }
                assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            };
            tokio::time::timeout(Duration::from_secs(15), async {
                let ((), report) = tokio::join!(responder, execute(&invocation, tokio::io::empty(), &mut stdout));
                assert_eq!(
                    report,
                    Report::Responded {
                        status: 503,
                        bytes: body.len() as u64
                    }
                );
                assert_eq!(stdout, body);
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn http_writes_body_frames_before_the_response_is_complete() {
        let (listener, invocation) = server().await;
        let (mut reader, writer) = tokio::io::duplex(64);
        tokio::time::timeout(Duration::from_secs(5), async {
            let responder = async {
                let mut stream = accept(&listener).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\none")
                    .await
                    .unwrap();
                let mut first = [0; 3];
                reader.read_exact(&mut first).await.unwrap();
                assert_eq!(&first, b"one");
                stream.write_all(b"two").await.unwrap();
                let mut rest = Vec::new();
                reader.read_to_end(&mut rest).await.unwrap();
                assert_eq!(rest, b"two");
            };
            let ((), report) = tokio::join!(responder, execute(&invocation, tokio::io::empty(), writer));
            assert_eq!(report, Report::Responded { status: 200, bytes: 6 });
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn headers_only_returns_without_waiting_for_the_body() {
        let (listener, mut invocation) = server().await;
        let Command::Http { headers_only, .. } = &mut invocation.command else {
            unreachable!()
        };
        *headers_only = true;
        let mut stdout = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            let responder = async {
                let mut stream = accept(&listener).await;
                stream
                    .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 100\r\n\r\n")
                    .await
                    .unwrap();
                assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            };
            let ((), report) = tokio::join!(responder, execute(&invocation, tokio::io::empty(), &mut stdout));
            assert_eq!(report, Report::Responded { status: 202, bytes: 0 });
            assert!(stdout.is_empty());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_closed_port_is_unreachable_and_stalled_headers_or_body_time_out() {
        let (listener, invocation) = server().await;
        drop(listener);
        assert!(matches!(
            execute(&invocation, tokio::io::empty(), tokio::io::sink()).await,
            Report::Unreachable { .. }
        ));
        for headers_received in [false, true] {
            let (listener, mut invocation) = server().await;
            invocation.timeout = Some(Duration::from_millis(100));
            tokio::time::timeout(Duration::from_secs(5), async {
                let responder = async {
                    let mut stream = accept(&listener).await;
                    if headers_received {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                            .await
                            .unwrap();
                    }
                    assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
                };
                let ((), report) = tokio::join!(responder, execute(&invocation, tokio::io::empty(), tokio::io::sink()));
                assert_eq!(report, Report::TimedOut { limit_ms: 100 });
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn incomplete_responses_are_unreachable_and_stdout_failures_are_local() {
        for broken_stdout in [false, true] {
            let (listener, invocation) = server().await;
            let (reader, mut stdout) = tokio::io::duplex(64);
            if broken_stdout {
                drop(reader);
            }
            let responder = async {
                let mut stream = accept(&listener).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\none")
                    .await
                    .unwrap();
                if broken_stdout {
                    assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
                }
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                let ((), report) = tokio::join!(responder, execute(&invocation, tokio::io::empty(), &mut stdout));
                if broken_stdout {
                    assert!(matches!(report, Report::Failed { reason } if reason.contains("stdout")));
                } else {
                    assert!(matches!(report, Report::Unreachable { .. }));
                }
            })
            .await
            .unwrap();
        }
    }
}
