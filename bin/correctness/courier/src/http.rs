use std::time::Duration;

use snafu::ResultExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::{net, Address, Error, InvalidPathSnafu, MalformedResponseSnafu, TargetSnafu, UnsupportedSnafu};

/// The largest response head the courier reads before giving up on finding its end.
const MAX_HEAD_LEN: usize = 64 * 1024;

/// The largest response body the courier reads.
///
/// Readiness and status checks answer with far less. The cap bounds the memory a server that never stops sending can
/// take.
pub(crate) const MAX_BODY_LEN: usize = 4 * 1024 * 1024;

/// A response to [`http_get`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,

    /// The body, exactly as received.
    pub body: Vec<u8>,
}

/// Makes one plain-HTTP `GET` request for `path` at an `http` address.
///
/// The request is HTTP/1.0, so the server answers without chunked encoding and closes the connection after the
/// response. The body ends at `Content-Length` when the response has one, at the head for a 204 or 304, and at the end
/// of the connection otherwise. A response with a `Transfer-Encoding` other than `identity`, or with a body over the courier's limit of a few MiB,
/// is malformed.
///
/// `limit` bounds the call, but a name lookup it cuts short keeps running on its blocking thread until the resolver
/// answers.
///
/// # Errors
///
/// Fails with [`Error::Unsupported`] for any address but `http`, and [`Error::InvalidPath`] for a path that does not
/// start with `/` or holds whitespace. Fails with [`Error::Target`] when the connection, the write or a read fails,
/// [`Error::MalformedResponse`] when the answer is not an HTTP/1 response, and [`Error::TimedOut`] when `limit`
/// passes.
pub async fn http_get(address: &Address, path: &str, limit: Duration) -> Result<HttpResponse, Error> {
    let Address::Http(host_port) = address else {
        return UnsupportedSnafu {
            operation: "http",
            address: address.clone(),
            reason: "http requests go to http addresses only",
        }
        .fail();
    };
    if !path.starts_with('/') || path.contains(char::is_whitespace) {
        return InvalidPathSnafu { path }.fail();
    }
    let context = |action| TargetSnafu {
        action,
        target: address.to_string(),
    };
    net::within("http", address, limit, async {
        let mut stream = net::connect_tcp(host_port).await?;
        let request =
            format!("GET {path} HTTP/1.0\r\nHost: {host_port}\r\nUser-Agent: panoramic-courier\r\nAccept: */*\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .with_context(|_| context("send a request to"))?;

        let mut received = Vec::new();
        let mut head: Option<Head> = None;
        loop {
            received.reserve(8 * 1024);
            let n = stream
                .read_buf(&mut received)
                .await
                .with_context(|_| context("read a response from"))?;
            if head.is_none() {
                head = Head::parse(&received).map_err(|reason| malformed(address, reason))?;
            }
            match &head {
                None if n == 0 => return Err(malformed(address, "the connection closed inside the response head")),
                None if received.len() > MAX_HEAD_LEN => {
                    return Err(malformed(address, "the response head is longer than 64 KiB"))
                }
                None => {}
                Some(head) if matches!(head.status, 204 | 304) => {
                    received.truncate(head.len);
                    break;
                }
                Some(head) => {
                    let body_len = received.len() - head.len;
                    match head.content_length {
                        Some(content_length) if content_length > MAX_BODY_LEN => {
                            return Err(malformed(
                                address,
                                format!(
                                    "Content-Length {content_length} is over the {} MiB limit",
                                    MAX_BODY_LEN >> 20
                                ),
                            ))
                        }
                        Some(content_length) if body_len >= content_length => {
                            received.truncate(head.len + content_length);
                            break;
                        }
                        Some(content_length) if n == 0 => {
                            return Err(malformed(
                                address,
                                format!("the body ended after {body_len} of {content_length} bytes"),
                            ))
                        }
                        None if body_len > MAX_BODY_LEN => {
                            return Err(malformed(
                                address,
                                format!("the body is over the {} MiB limit", MAX_BODY_LEN >> 20),
                            ))
                        }
                        None if n == 0 => break,
                        Some(_) | None => {}
                    }
                }
            }
        }
        let head = head.expect("the loop ends only once the head is parsed");
        Ok(HttpResponse {
            status: head.status,
            body: received.split_off(head.len),
        })
    })
    .await
}

fn malformed(address: &Address, reason: impl Into<String>) -> Error {
    MalformedResponseSnafu {
        address: address.clone(),
        reason: reason.into(),
    }
    .build()
}

/// The parts of a response head the courier uses.
struct Head {
    /// The length of the head, including the blank line that ends it.
    len: usize,
    status: u16,
    content_length: Option<usize>,
}

impl Head {
    /// Parses the head at the start of `received`, or returns `None` if it has not ended yet.
    fn parse(received: &[u8]) -> Result<Option<Self>, String> {
        let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") else {
            return Ok(None);
        };
        let text = String::from_utf8_lossy(&received[..end]);
        let mut lines = text.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        let mut parts = status_line.split(' ');
        let status = match (parts.next(), parts.next()) {
            (Some(version), Some(code)) if version.starts_with("HTTP/1.") && code.len() == 3 => code.parse().ok(),
            _ => None,
        }
        .ok_or_else(|| format!("'{status_line}' is not an HTTP/1 status line"))?;

        let mut content_length = None;
        for line in lines {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| format!("'{line}' is not a header"))?;
            if name.eq_ignore_ascii_case("content-length") {
                let value = value.trim();
                let length = value
                    .parse::<usize>()
                    .map_err(|_| format!("Content-Length '{value}' is not a number"))?;
                if content_length.is_some_and(|previous| previous != length) {
                    return Err("conflicting Content-Length headers".to_string());
                }
                content_length = Some(length);
            } else if name.eq_ignore_ascii_case("transfer-encoding") && !value.trim().eq_ignore_ascii_case("identity") {
                return Err(format!("Transfer-Encoding '{}' is not supported", value.trim()));
            }
        }
        Ok(Some(Self {
            len: end + 4,
            status,
            content_length,
        }))
    }
}
