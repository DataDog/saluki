//! Reverse proxy to the trace-agent.
//!
//! Every request the APM relay does not handle itself is forwarded, unchanged apart from hop-by-hop headers, to the
//! trace-agent at the configured destination, and the trace-agent's response is streamed back to the client.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[cfg(unix)]
use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _, path::PathBuf};

use axum::extract::Request as AxumRequest;
use axum::response::{IntoResponse, Response};
use http::header::{self, HeaderMap};
use http::uri::{PathAndQuery, Uri};
use http::{HeaderValue, StatusCode, Version};
use rustls::RootCertStore;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::client::http::{HttpClient, HttpProtocol};
use tracing::{debug, info, warn};
use url::Url;

use super::forwarding::{strip_hop_by_hop_headers, UpstreamClient};

/// Name the relay adds itself to the `Via` header under, so that a request proxied back to the relay is recognized.
/// See RFC 9110, section 7.6.3.
const VIA_PSEUDONYM: &str = "agent-data-plane";

/// `Via` entry the relay adds to every request it forwards.
const VIA_ENTRY: HeaderValue = HeaderValue::from_static("1.1 agent-data-plane");

/// Forwards requests to the trace-agent.
#[derive(Clone)]
pub(super) struct TraceAgentProxy {
    client: UpstreamClient,

    /// Scheme and authority requests are rewritten to. For a Unix domain socket destination the client ignores the
    /// authority and connects to the socket, but a request URI still needs one.
    base_uri: Uri,

    /// Where requests are forwarded to.
    destination: Destination,

    /// Whether the last attempt to reach the trace-agent failed.
    ///
    /// Only the first failure after a success is logged as a warning, so a trace-agent that is down logs once rather
    /// than once per request.
    unreachable: Arc<AtomicBool>,

    /// Whether a request proxied back to the relay was seen.
    ///
    /// Only the first one is logged as a warning, as every request loops the same way.
    loop_detected: Arc<AtomicBool>,
}

/// Where the proxy forwards requests to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Destination {
    /// A TCP host and port.
    Tcp { host: String, port: u16 },

    /// A Unix domain socket path.
    #[cfg(unix)]
    Unix(PathBuf),
}

/// Error from forwarding a request to the trace-agent.
#[derive(Debug)]
pub(super) enum ForwardError {
    /// The request's path couldn't be carried over to the trace-agent.
    InvalidUri,

    /// The trace-agent couldn't be reached, or didn't answer.
    Unreachable,

    /// The request was already forwarded by the relay, so the destination leads back to the relay.
    Loop,
}

impl IntoResponse for ForwardError {
    fn into_response(self) -> Response {
        match self {
            Self::InvalidUri => StatusCode::BAD_REQUEST.into_response(),
            Self::Unreachable => StatusCode::BAD_GATEWAY.into_response(),
            Self::Loop => StatusCode::LOOP_DETECTED.into_response(),
        }
    }
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

        let (builder, base_uri, destination) = match url.scheme() {
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
                // The URL keeps an IPv6 host bracketed, which address lookups don't accept.
                let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
                (builder, base_uri, Destination::Tcp { host, port })
            }
            #[cfg(unix)]
            "unix" => {
                // `unix://relative/path` parses with `relative` as the host, so a host means the path was not
                // absolute. The URL path is percent-encoded, so a socket path with, for example, a space in it has to
                // be decoded first.
                let decoded = percent_encoding::percent_decode_str(url.path()).collect::<Vec<u8>>();
                let path = PathBuf::from(OsStr::from_bytes(&decoded));
                if url.host_str().is_some_and(|host| !host.is_empty()) || !path.is_absolute() {
                    return Err(generic_error!(
                        "APM proxy destination `{destination}` must name an absolute socket path."
                    ));
                }
                (
                    builder.with_unix_socket_path(path.clone()),
                    Uri::from_static("http://localhost/"),
                    Destination::Unix(path),
                )
            }
            scheme => {
                return Err(generic_error!(
                "APM proxy destination `{destination}` has unsupported scheme `{scheme}`; expected `http` or `unix`."
            ))
            }
        };

        Ok(Self {
            client: UpstreamClient::new(builder.build()?),
            base_uri,
            destination,
            unreachable: Arc::new(AtomicBool::new(false)),
            loop_detected: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Returns where the proxy forwards requests to.
    pub(super) fn destination(&self) -> &Destination {
        &self.destination
    }

    /// Forwards `request` to the trace-agent and returns its response.
    ///
    /// Any response from the trace-agent is returned as is, including an error status. Converted into a response, a
    /// [`ForwardError`] is answered with `400 Bad Request` if the request couldn't be carried over, `502 Bad Gateway`
    /// if the trace-agent couldn't be reached, or `508 Loop Detected` if the request was already forwarded by the relay.
    ///
    /// # Errors
    ///
    /// If the request was already forwarded by the relay, its path can't be carried over, or the trace-agent can't be
    /// reached, an error is returned.
    pub(super) async fn forward(&self, request: AxumRequest) -> Result<Response, ForwardError> {
        let (mut parts, body) = request.into_parts();

        // Forwarding a request the relay already forwarded would only send it around again, each time holding another
        // connection and request open until the process runs out of either.
        if was_forwarded_by_relay(&parts.headers) {
            if self.loop_detected.swap(true, Ordering::Relaxed) {
                debug!("Rejected an APM request proxied back to the relay.");
            } else {
                warn!("Rejected an APM request proxied back to the relay. The APM proxy destination leads back to the relay rather than to the trace-agent.");
            }
            return Err(ForwardError::Loop);
        }

        parts.uri = match self.upstream_uri(parts.uri.path_and_query()) {
            Ok(uri) => uri,
            Err(e) => {
                debug!(error = %e, "Failed to build the upstream URI for a proxied APM request.");
                return Err(ForwardError::InvalidUri);
            }
        };
        strip_hop_by_hop_headers(&mut parts.headers);
        // The client sets `Host` from the upstream URI.
        parts.headers.remove(header::HOST);
        // The relay's server also accepts HTTP/2, but the upstream client speaks only HTTP/1.1 and refuses a request
        // still marked as HTTP/2.
        parts.version = Version::HTTP_11;
        parts.headers.append(header::VIA, VIA_ENTRY);

        match self.client.send(http::Request::from_parts(parts, body)).await {
            Ok(response) => {
                if self.unreachable.swap(false, Ordering::Relaxed) {
                    info!("The trace-agent is reachable again; APM requests are being relayed.");
                }
                Ok(response)
            }
            Err(e) => {
                if self.unreachable.swap(true, Ordering::Relaxed) {
                    debug!(error = %e, "Failed to forward APM request to the trace-agent.");
                } else {
                    warn!(error = %e, "Failed to forward APM request to the trace-agent. Requests are dropped until it is reachable again");
                }
                Err(ForwardError::Unreachable)
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

/// Returns whether the relay is listed in the `Via` header of a request.
fn was_forwarded_by_relay(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::VIA)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        // An entry is the protocol, then the pseudonym, then an optional comment.
        .any(|entry| entry.split_whitespace().nth(1) == Some(VIA_PSEUDONYM))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
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
            .await
            .into_response();

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
            .await
            .into_response();

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
        let response = proxy.forward(http2_request).await.into_response();

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(seen.lock().await.uri, "/info");
    }

    #[tokio::test]
    async fn adds_the_relay_to_the_via_header() {
        let (destination, seen) = spawn_tcp_trace_agent().await;
        let proxy = TraceAgentProxy::from_destination(&destination).expect("destination should parse");

        let response = proxy
            .forward(request("GET", "/info", &[("via", "1.1 some-proxy")], ""))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::CREATED);
        let seen = seen.lock().await;
        let via: Vec<_> = seen.headers.get_all("via").iter().collect();
        assert_eq!(via, ["1.1 some-proxy", "1.1 agent-data-plane"]);
    }

    #[tokio::test]
    async fn rejects_a_request_the_relay_already_forwarded() {
        let (destination, seen) = spawn_tcp_trace_agent().await;
        let proxy = TraceAgentProxy::from_destination(&destination).expect("destination should parse");

        for via in [
            "1.1 agent-data-plane",
            "1.0 some-proxy, HTTP/1.1 agent-data-plane (comment)",
        ] {
            let response = proxy
                .forward(request("GET", "/info", &[("via", via)], ""))
                .await
                .into_response();
            assert_eq!(response.status(), StatusCode::LOOP_DETECTED, "for `Via: {via}`");
        }

        assert!(
            seen.lock().await.uri.is_empty(),
            "the trace-agent should not receive the request"
        );
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
        let response = proxy.forward(request("GET", "/info", &[], "")).await.into_response();

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(seen.lock().await.uri, "/info");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn decodes_a_percent_encoded_unix_domain_socket_path() {
        let dir = tempfile::tempdir().expect("temp dir should be created");
        let socket_path = dir.path().join("trace agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("should bind the socket");
        let seen = Arc::new(AsyncMutex::new(Seen::default()));
        let router = fake_trace_agent(Arc::clone(&seen));
        tokio::spawn(async move { axum::serve(listener, router).await });

        let destination = format!("unix://{}", socket_path.display()).replace(' ', "%20");
        let proxy = TraceAgentProxy::from_destination(&destination).expect("destination should parse");
        let response = proxy.forward(request("GET", "/info", &[], "")).await.into_response();

        assert_eq!(response.status(), StatusCode::CREATED);
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
        let response = proxy.forward(request("GET", "/info", &[], "")).await.into_response();

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
