//! HTTP forwarding shared by the relay's proxies.
//!
//! Both the trace-agent proxy and the tracer flare proxy stream a request to an upstream server and stream its response
//! back, dropping the hop-by-hop headers on the way in each direction.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::response::Response;
use http::header::{self, HeaderMap, HeaderName};
use http_body::{Body as HttpBody, Frame, SizeHint};
use saluki_error::GenericError;
use saluki_io::net::client::http::HttpClient;

/// Headers that describe a single connection rather than the request, and so must not be forwarded across a proxy
/// hop. See RFC 9110, section 7.6.1.
const HOP_BY_HOP_HEADERS: [HeaderName; 8] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// An HTTP client shared by every request handler of a proxy.
#[derive(Clone)]
pub(super) struct UpstreamClient {
    /// The client is not `Sync`, and the proxy is shared by every request handler, so each request clones a handle
    /// out from behind the lock. Cloning is cheap, and the lock is held only for the clone.
    client: Arc<Mutex<HttpClient>>,
}

impl UpstreamClient {
    pub(super) fn new(client: HttpClient) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
        }
    }

    /// Sends `request` upstream and returns the upstream response, without its hop-by-hop headers.
    ///
    /// The request is sent as is: the caller rewrites its URI and headers. Both bodies are streamed, not buffered.
    ///
    /// # Errors
    ///
    /// If the upstream server can't be reached, an error is returned.
    pub(super) async fn send(&self, request: http::Request<Body>) -> Result<Response, GenericError> {
        let mut client = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let response = client.send(request.map(SyncBody::new)).await?;
        let (mut parts, body) = response.into_parts();
        strip_hop_by_hop_headers(&mut parts.headers);
        Ok(Response::from_parts(parts, Body::new(body)))
    }
}

/// Removes hop-by-hop headers, including any the `Connection` header names.
pub(super) fn strip_hop_by_hop_headers(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::try_from(name.trim()).ok())
        .collect();

    for name in HOP_BY_HOP_HEADERS.iter().chain(named.iter()) {
        headers.remove(name);
    }
}

/// Adapts an incoming request body to the `Sync` bound the HTTP client requires, so it can be streamed upstream
/// without buffering.
///
/// The body is only ever polled through `&mut self`, so the mutex is never contended: it exists only to make the
/// wrapper `Sync`.
struct SyncBody {
    inner: Mutex<Body>,
    size_hint: SizeHint,
}

impl SyncBody {
    fn new(body: Body) -> Self {
        let size_hint = body.size_hint();
        Self {
            inner: Mutex::new(body),
            size_hint,
        }
    }
}

impl HttpBody for SyncBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let body = self
            .get_mut()
            .inner
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Pin::new(body).poll_frame(cx)
    }

    fn size_hint(&self) -> SizeHint {
        self.size_hint
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    #[test]
    fn strips_hop_by_hop_headers_and_those_connection_names() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive, x-custom-hop"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert(header::TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        headers.insert("x-custom-hop", HeaderValue::from_static("1"));
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/msgpack"));

        strip_hop_by_hop_headers(&mut headers);

        assert_eq!(headers.len(), 1);
        assert_eq!(headers[header::CONTENT_TYPE], "application/msgpack");
    }
}
