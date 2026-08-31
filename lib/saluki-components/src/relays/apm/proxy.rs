//! Reverse proxy to the trace-agent.
//!
//! Every request the APM relay does not handle itself is forwarded, unchanged apart from hop-by-hop headers, to the
//! trace-agent at the configured destination, and the trace-agent's response is streamed back to the client.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::Request as AxumRequest;
use axum::response::{IntoResponse, Response};
use http::header::{self, HeaderMap, HeaderName};
use http::uri::{PathAndQuery, Uri};
use http::{StatusCode, Version};
use http_body::{Body as HttpBody, Frame, SizeHint};
use rustls::RootCertStore;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::client::http::{HttpClient, HttpProtocol};
use tracing::debug;
use url::Url;

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

/// Forwards requests to the trace-agent.
#[derive(Clone)]
pub(super) struct TraceAgentProxy {
    /// The client is not `Sync`, and the proxy is shared by every request handler, so each request clones a handle
    /// out from behind the lock. Cloning is cheap, and the lock is held only for the clone.
    client: Arc<Mutex<HttpClient>>,

    /// Scheme and authority requests are rewritten to. For a Unix domain socket destination the client ignores the
    /// authority and connects to the socket, but a request URI still needs one.
    base_uri: Uri,
}

impl TraceAgentProxy {
    /// Creates a proxy forwarding to `destination`, an `http://host:port` or `unix:///path` URL.
    ///
    /// # Errors
    ///
    /// Returns an error if `destination` is not a valid URL of either form, or the HTTP client cannot be built.
    pub(super) fn from_destination(destination: &str) -> Result<Self, GenericError> {
        let url =
            Url::parse(destination).with_error_context(|| format!("Invalid APM proxy destination `{destination}`."))?;

        // Requests keep their own path and query, so anything else the URL carries would be silently dropped.
        if url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
            return Err(generic_error!(
                "APM proxy destination `{destination}` must not carry a query, fragment, or credentials."
            ));
        }

        let builder = HttpClient::builder()
            // The trace-agent serves plain HTTP/1.1, so TLS is never used: an empty root store avoids loading the
            // platform's certificates for nothing.
            .with_tls_config(|builder| builder.with_root_cert_store(RootCertStore::empty()))
            .with_http_protocol(HttpProtocol::Http1)
            // Each trace-agent route enforces its own timeout, some of them long (profiling, the EVP proxy), so the
            // proxy must not impose a shorter one of its own.
            .without_request_timeout();

        let (builder, base_uri) = match url.scheme() {
            "http" => {
                if url.path() != "/" {
                    return Err(generic_error!(
                        "APM proxy destination `{destination}` must not carry a path; requests keep their own."
                    ));
                }
                let host = url
                    .host_str()
                    .ok_or_else(|| generic_error!("APM proxy destination `{destination}` has no host."))?;
                // `port()` hides a port equal to the scheme's default, so ask for it explicitly.
                let port = url
                    .port_or_known_default()
                    .ok_or_else(|| generic_error!("APM proxy destination `{destination}` has no port."))?;
                // An IPv6 host comes back already bracketed.
                let base_uri = Uri::builder()
                    .scheme("http")
                    .authority(format!("{host}:{port}"))
                    .path_and_query("/")
                    .build()
                    .with_error_context(|| format!("Invalid APM proxy destination `{destination}`."))?;
                (builder, base_uri)
            }
            #[cfg(unix)]
            "unix" => {
                // `unix://relative/path` parses with `relative` as the host, so a host means the path was not
                // absolute.
                let path = PathBuf::from(url.path());
                if url.host_str().is_some_and(|host| !host.is_empty()) || !path.is_absolute() {
                    return Err(generic_error!(
                        "APM proxy destination `{destination}` must name an absolute socket path."
                    ));
                }
                (
                    builder.with_unix_socket_path(path),
                    Uri::from_static("http://localhost/"),
                )
            }
            scheme => {
                return Err(generic_error!(
                "APM proxy destination `{destination}` has unsupported scheme `{scheme}`; expected `http` or `unix`."
            ))
            }
        };

        Ok(Self {
            client: Arc::new(Mutex::new(builder.build()?)),
            base_uri,
        })
    }

    /// Forwards `request` to the trace-agent and returns its response.
    ///
    /// A failure to reach the trace-agent is answered with `502 Bad Gateway`.
    pub(super) async fn forward(&self, request: AxumRequest) -> Response {
        let (mut parts, body) = request.into_parts();

        parts.uri = match self.upstream_uri(parts.uri.path_and_query()) {
            Ok(uri) => uri,
            Err(e) => {
                debug!(error = %e, "Failed to build the upstream URI for a proxied APM request.");
                return StatusCode::BAD_REQUEST.into_response();
            }
        };
        strip_hop_by_hop_headers(&mut parts.headers);
        // The client sets `Host` from the upstream URI.
        parts.headers.remove(header::HOST);
        // The relay's server also accepts HTTP/2, but the upstream client speaks only HTTP/1.1 and refuses a request
        // still marked as HTTP/2.
        parts.version = Version::HTTP_11;

        // TODO: The trace-agent derives a tracer's container ID from the connection's Unix domain socket peer
        // credentials when the tracer sends no container headers. Behind this proxy, those credentials are ADP's. Set
        // `Datadog-Container-ID` here, from ADP's own peer lookup, when the tracer sent neither it nor
        // `Datadog-Entity-ID`.

        let upstream_request = http::Request::from_parts(parts, SyncBody::new(body));

        let mut client = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match client.send(upstream_request).await {
            Ok(response) => {
                let (mut parts, body) = response.into_parts();
                strip_hop_by_hop_headers(&mut parts.headers);
                Response::from_parts(parts, Body::new(body))
            }
            Err(e) => {
                debug!(error = %e, "Failed to forward APM request to the trace-agent.");
                StatusCode::BAD_GATEWAY.into_response()
            }
        }
    }

    fn upstream_uri(&self, path_and_query: Option<&PathAndQuery>) -> Result<Uri, http::Error> {
        let mut parts = self.base_uri.clone().into_parts();
        parts.path_and_query = Some(
            path_and_query
                .cloned()
                .unwrap_or_else(|| PathAndQuery::from_static("/")),
        );
        Ok(Uri::from_parts(parts)?)
    }
}

/// Removes hop-by-hop headers, including any the `Connection` header names.
fn strip_hop_by_hop_headers(headers: &mut HeaderMap) {
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
    use axum::body::to_bytes;
    use axum::routing::any;
    use axum::Router;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;

    /// What the fake trace-agent saw of the last request it received.
    #[derive(Clone, Debug, Default)]
    struct Seen {
        method: String,
        uri: String,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    /// A stand-in trace-agent: records each request and answers `201` with a fixed body and headers.
    fn fake_trace_agent(seen: Arc<AsyncMutex<Seen>>) -> Router {
        Router::new().fallback(any(move |request: AxumRequest| {
            let seen = Arc::clone(&seen);
            async move {
                let (parts, body) = request.into_parts();
                let body = to_bytes(body, usize::MAX)
                    .await
                    .expect("request body should be readable");
                *seen.lock().await = Seen {
                    method: parts.method.to_string(),
                    uri: parts.uri.to_string(),
                    headers: parts.headers,
                    body: body.to_vec(),
                };
                (
                    StatusCode::CREATED,
                    [("x-trace-agent", "yes"), ("connection", "close")],
                    "from trace-agent",
                )
            }
        }))
    }

    async fn spawn_tcp_trace_agent() -> (String, Arc<AsyncMutex<Seen>>) {
        let seen = Arc::new(AsyncMutex::new(Seen::default()));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind an ephemeral port");
        let addr = listener.local_addr().expect("listener should have a local address");
        let router = fake_trace_agent(Arc::clone(&seen));
        tokio::spawn(async move { axum::serve(listener, router).await });
        (format!("http://{addr}"), seen)
    }

    fn request(method: &str, uri: &str, headers: &[(&str, &str)], body: &'static str) -> AxumRequest {
        let mut builder = http::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::from(body)).expect("request should build")
    }

    async fn body_string(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        String::from_utf8(bytes.to_vec()).expect("response body should be UTF-8")
    }

    #[tokio::test]
    async fn forwards_the_request_and_relays_the_response() {
        let (destination, seen) = spawn_tcp_trace_agent().await;
        let proxy = TraceAgentProxy::from_destination(&destination).expect("destination should parse");

        let response = proxy
            .forward(request(
                "PUT",
                "/v0.4/traces?foo=bar",
                &[("datadog-meta-lang", "python"), ("content-type", "application/msgpack")],
                "payload",
            ))
            .await;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers().get("x-trace-agent").unwrap(), "yes");
        assert_eq!(body_string(response).await, "from trace-agent");

        let seen = seen.lock().await;
        assert_eq!(seen.method, "PUT");
        assert_eq!(seen.uri, "/v0.4/traces?foo=bar");
        assert_eq!(seen.headers.get("datadog-meta-lang").unwrap(), "python");
        assert_eq!(seen.headers.get("content-type").unwrap(), "application/msgpack");
        assert_eq!(seen.body, b"payload");
    }

    #[tokio::test]
    async fn strips_hop_by_hop_headers_in_both_directions() {
        let (destination, seen) = spawn_tcp_trace_agent().await;
        let proxy = TraceAgentProxy::from_destination(&destination).expect("destination should parse");

        let response = proxy
            .forward(request(
                "POST",
                "/info",
                &[
                    ("connection", "x-per-hop"),
                    ("x-per-hop", "drop me"),
                    ("proxy-authorization", "secret"),
                    ("x-end-to-end", "keep me"),
                ],
                "",
            ))
            .await;

        assert!(response.headers().get("connection").is_none());

        let seen = seen.lock().await;
        assert!(seen.headers.get("x-per-hop").is_none());
        assert!(seen.headers.get("proxy-authorization").is_none());
        assert_eq!(seen.headers.get("x-end-to-end").unwrap(), "keep me");
    }

    #[tokio::test]
    async fn forwards_an_http2_request_over_http1() {
        let (destination, seen) = spawn_tcp_trace_agent().await;
        let proxy = TraceAgentProxy::from_destination(&destination).expect("destination should parse");

        let mut http2_request = request("GET", "/info", &[], "");
        *http2_request.version_mut() = Version::HTTP_2;
        let response = proxy.forward(http2_request).await;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(seen.lock().await.uri, "/info");
    }

    #[test]
    fn accepts_a_destination_with_a_trailing_slash() {
        assert!(TraceAgentProxy::from_destination("http://127.0.0.1:8127/").is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forwards_over_a_unix_domain_socket() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        let socket_path = dir.path().join("trace-agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("should bind the socket");
        let seen = Arc::new(AsyncMutex::new(Seen::default()));
        let router = fake_trace_agent(Arc::clone(&seen));
        tokio::spawn(async move { axum::serve(listener, router).await });

        let proxy = TraceAgentProxy::from_destination(&format!("unix://{}", socket_path.display()))
            .expect("destination should parse");
        let response = proxy.forward(request("GET", "/info", &[], "")).await;

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(seen.lock().await.uri, "/info");
    }

    #[tokio::test]
    async fn an_unreachable_trace_agent_is_a_bad_gateway() {
        // Bind and immediately drop a listener, so the port is known to be closed.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind an ephemeral port");
        let addr = listener.local_addr().expect("listener should have a local address");
        drop(listener);

        let proxy = TraceAgentProxy::from_destination(&format!("http://{addr}")).expect("destination should parse");
        let response = proxy.forward(request("GET", "/info", &[], "")).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn rejects_invalid_destinations() {
        for destination in [
            "127.0.0.1:8127",
            "https://127.0.0.1:8127",
            "http://",
            "unix://relative/path",
            "not a url",
            "http://127.0.0.1:8127/trace-agent",
            "http://127.0.0.1:8127/?key=value",
            "http://127.0.0.1:8127/#fragment",
            "http://user:secret@127.0.0.1:8127",
            "unix:///var/run/datadog/apm.socket?key=value",
        ] {
            assert!(
                TraceAgentProxy::from_destination(destination).is_err(),
                "expected `{destination}` to be rejected"
            );
        }
    }

    #[test]
    fn accepts_ipv6_destinations() {
        let proxy = TraceAgentProxy::from_destination("http://[::1]:8127").expect("destination should parse");
        assert_eq!(proxy.base_uri.authority().unwrap().as_str(), "[::1]:8127");
    }
}
