//! Tracer flare proxy.
//!
//! Tracers upload flares, a multipart form holding a zip archive and the support case it belongs to, to
//! `/tracer_flare/v1`. Like the trace-agent, the relay forwards them as they are to the Datadog serverless flare
//! endpoint, adding the API key.

use std::sync::LazyLock;

use axum::extract::{ConnectInfo, Request as AxumRequest};
use axum::response::{IntoResponse, Response};
use http::header::{self, HeaderMap, HeaderName};
use http::{HeaderValue, StatusCode, Uri, Version};
use regex::Regex;
use saluki_common::sync::live::Live;
use saluki_error::{generic_error, GenericError};
use saluki_io::net::client::http::HttpClientBuilder;
use saluki_io::net::ConnectionAddress;
use tracing::{debug, warn};

use super::forwarding::{strip_hop_by_hop_headers, UpstreamClient};
use crate::common::datadog::api_key::ApiKeyCell;
use crate::common::datadog::config::ForwarderConfiguration;
use crate::common::datadog::endpoints::is_datadog_host;
use crate::common::datadog::io::intake_http_client_builder;

/// Path of the Datadog endpoint flares are forwarded to.
const FLARE_ENDPOINT_PATH: &str = "/api/ui/support/serverless/flare";

/// Site flares are sent to when none is configured, as in the trace-agent.
const DEFAULT_SITE: &str = "datadoghq.com";

/// Name the API key source is logged under.
const API_KEY_SOURCE: &str = "tracer_flare";

const DD_API_KEY_HEADER: HeaderName = HeaderName::from_static("dd-api-key");
const FORWARDED_HEADER: HeaderName = HeaderName::from_static("forwarded");
const X_FORWARDED_FOR_HEADER: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_HOST_HEADER: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PROTO_HEADER: HeaderName = HeaderName::from_static("x-forwarded-proto");

/// An official Datadog site with no subdomain, such as `datadoghq.com`.
static DD_BARE_SITE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(datad(oghq|0g)\.(com|eu)|ddog-gov\.com)$").expect("bare site regex should compile")
});

/// The `MAJOR.MINOR.PATCH` part of a version string.
static VERSION_NUMBERS_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+)\.(\d+)\.(\d+)").expect("version regex should compile"));

/// Settings of the tracer flare proxy.
#[derive(Clone)]
pub struct TracerFlareConfiguration {
    /// Datadog site flares are sent to, such as `datadoghq.com`.
    ///
    /// Empty means `datadoghq.com`.
    pub site: String,

    /// API key flares are sent with.
    pub api_key: Live<String>,

    /// Settings of the Datadog intake forwarder.
    ///
    /// Only the settings of its HTTP client are used: the request timeout, the TLS settings (handshake timeout, key log
    /// file, and certificate validation, including `skip_ssl_validation`), the proxy, the HTTP protocol, and the
    /// connection reset interval. The destination comes from `site`, and the API key from `api_key`.
    pub forwarder: ForwarderConfiguration,
}

/// Forwards tracer flares to Datadog.
#[derive(Clone)]
pub(super) struct TracerFlareProxy {
    client: UpstreamClient,

    /// Where flares are forwarded to.
    endpoint: Uri,

    /// API key flares are sent with.
    api_key: ApiKeyCell,
}

/// Error from forwarding a flare to Datadog.
#[derive(Debug)]
pub(super) struct ForwardError;

impl IntoResponse for ForwardError {
    fn into_response(self) -> Response {
        StatusCode::BAD_GATEWAY.into_response()
    }
}

impl TracerFlareProxy {
    /// Creates a proxy forwarding flares to the flare endpoint of the configured site.
    ///
    /// # Errors
    ///
    /// If the site isn't a valid host, the API key isn't a valid header value, or the HTTP client can't be built, an
    /// error is returned.
    pub(super) fn from_config(config: &TracerFlareConfiguration) -> Result<Self, GenericError> {
        let site = match config.site.trim() {
            "" => DEFAULT_SITE,
            site => site,
        };
        let endpoint = flare_endpoint(site, &flare_version_label(agent_version_string()))?;
        let client_builder =
            intake_http_client_builder(&config.forwarder, config.forwarder.effective_min_tls_version())?;
        Self::new(client_builder, endpoint, config.api_key.as_str())
    }

    fn new(client_builder: HttpClientBuilder, endpoint: Uri, api_key: &str) -> Result<Self, GenericError> {
        let api_key = ApiKeyCell::new(api_key)
            .map_err(|e| generic_error!("The configured API key is not a valid HTTP header value: {e}"))?;
        debug!(%endpoint, "Tracer flares are forwarded to Datadog.");

        Ok(Self {
            client: UpstreamClient::new(client_builder.build()?),
            endpoint,
            api_key,
        })
    }

    /// Replaces the API key flares are sent with.
    pub(super) fn refresh_api_key(&self, api_key: &str) {
        self.api_key.refresh(API_KEY_SOURCE, api_key);
    }

    /// Forwards the flare in `request` to Datadog and returns its response.
    ///
    /// Any response from Datadog is returned as is, including an error status. The request keeps its method, headers,
    /// and body, but not its path or query, as the trace-agent does.
    ///
    /// # Errors
    ///
    /// If Datadog can't be reached, an error is returned, answered with `502 Bad Gateway`.
    pub(super) async fn forward(&self, request: AxumRequest) -> Result<Response, ForwardError> {
        let (mut parts, body) = request.into_parts();

        let forwarded_host = request_host(&parts.uri, &parts.headers);
        let peer_ip =
            parts
                .extensions
                .get::<ConnectInfo<ConnectionAddress>>()
                .and_then(|ConnectInfo(addr)| match addr {
                    ConnectionAddress::SocketLike(addr) => Some(addr.ip().to_string()),
                    ConnectionAddress::ProcessLike(_) => None,
                });

        parts.uri = self.endpoint.clone();
        // The relay's server also accepts HTTP/2: the client picks the protocol of its own connection.
        parts.version = Version::default();
        set_upstream_headers(
            &mut parts.headers,
            peer_ip.as_deref(),
            forwarded_host.as_ref(),
            &self.api_key.load(),
        );

        self.client
            .send(http::Request::from_parts(parts, body))
            .await
            .map_err(|e| {
                warn!(endpoint = %self.endpoint, error = %e, "Failed to forward tracer flare to Datadog.");
                ForwardError
            })
    }
}

/// Returns the host a request was sent to, from its URI for HTTP/2 or its `Host` header for HTTP/1.1.
fn request_host(uri: &Uri, headers: &HeaderMap) -> Option<HeaderValue> {
    match uri.authority() {
        Some(authority) => HeaderValue::from_str(authority.as_str()).ok(),
        None => headers.get(header::HOST).cloned(),
    }
}

/// Rewrites the headers of a flare request for Datadog, as Go's `httputil.ReverseProxy` does for the trace-agent.
///
/// Headers set by an earlier proxy are replaced rather than added to, so the client can't pass itself off as another.
fn set_upstream_headers(
    headers: &mut HeaderMap, peer_ip: Option<&str>, forwarded_host: Option<&HeaderValue>, api_key: &str,
) {
    strip_hop_by_hop_headers(headers);
    // The client sets `Host` from the upstream URI.
    headers.remove(header::HOST);
    headers.remove(FORWARDED_HEADER);
    headers.remove(X_FORWARDED_FOR_HEADER);
    headers.remove(X_FORWARDED_HOST_HEADER);

    if let Some(peer_ip) = peer_ip.and_then(|ip| HeaderValue::from_str(ip).ok()) {
        headers.insert(X_FORWARDED_FOR_HEADER, peer_ip);
    }
    if let Some(host) = forwarded_host {
        headers.insert(X_FORWARDED_HOST_HEADER, host.clone());
    }
    // The relay only serves plain HTTP.
    headers.insert(X_FORWARDED_PROTO_HEADER, HeaderValue::from_static("http"));

    match HeaderValue::from_str(api_key) {
        Ok(mut value) => {
            value.set_sensitive(true);
            headers.insert(DD_API_KEY_HEADER, value);
        }
        // `ApiKeyCell` only holds valid header values.
        Err(_) => {
            headers.remove(DD_API_KEY_HEADER);
        }
    }
}

/// Returns the version string of the Agent ADP runs alongside, if it was known at build time.
fn agent_version_string() -> Option<&'static str> {
    datadog_agent_commons::agent_version::version_string()
}

/// Returns the version label of the flare host, such as `7-50-0-flare`.
///
/// The trace-agent uses its own version. When the Agent version wasn't known at build time, or carries no
/// `MAJOR.MINOR.PATCH`, ADP's version is used instead.
fn flare_version_label(agent_version: Option<&str>) -> String {
    let numbers = agent_version
        .and_then(|version| VERSION_NUMBERS_REGEX.captures(version))
        .map(|captures| format!("{}-{}-{}", &captures[1], &captures[2], &captures[3]))
        .unwrap_or_else(|| {
            let version = saluki_metadata::get_app_details().version();
            format!("{}-{}-{}", version.major(), version.minor(), version.patch())
        });
    format!("{numbers}-flare")
}

/// Returns the flare endpoint of `site`, a Datadog site such as `datadoghq.com` or a URL.
///
/// This follows the trace-agent's `getServerlessFlareEndpoint`: an official Datadog application host has its leftmost
/// label replaced with `version_label` (a bare official site is treated as its `app.` host), and any other host is
/// kept. The scheme is always `https`, and the path is always that of the flare endpoint.
///
/// It differs from the trace-agent in two ways:
///
/// - A site with a subdomain but no scheme, such as `us3.datadoghq.com`, is taken as the host, where the trace-agent
///   leaves the host empty.
/// - Official hosts are recognized as the forwarder recognizes them, so a Multi-Region Failover host such as
///   `app.mrf.datadoghq.com` is versioned too, where the trace-agent keeps it.
///
/// # Errors
///
/// If `site` isn't a valid URI with a host, an error is returned.
fn flare_endpoint(site: &str, version_label: &str) -> Result<Uri, GenericError> {
    let site = site.trim();
    // A bare site such as `datadoghq.com` parses as a URI with only an authority, and a URL as a URI with a scheme.
    let uri: Uri = site
        .parse()
        .map_err(|e| generic_error!("Datadog site `{site}` is not a valid host to send tracer flares to: {e}"))?;
    let Some(host) = uri.host().filter(|host| !host.is_empty()) else {
        return Err(generic_error!(
            "Datadog site `{site}` has no host to send tracer flares to."
        ));
    };

    let mut host = host.to_ascii_lowercase();
    if DD_BARE_SITE_REGEX.is_match(&host) {
        host = format!("app.{host}");
    }
    if is_datadog_host(&host) {
        let (_, domain) = host
            .split_once('.')
            .expect("an app host has a domain after its first label");
        host = format!("{version_label}.{domain}");
    }
    let authority = match uri.port_u16() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    };

    Uri::builder()
        .scheme("https")
        .authority(authority)
        .path_and_query(FLARE_ENDPOINT_PATH)
        .build()
        .map_err(|e| generic_error!("Datadog site `{site}` is not a valid host to send tracer flares to: {e}"))
}

/// A proxy forwarding to `base` over plain HTTP, standing in for the flare endpoint, as the trace-agent's test does.
#[cfg(test)]
pub(super) fn plain_http_proxy(base: &str, api_key: &str) -> TracerFlareProxy {
    let builder = saluki_io::net::client::http::HttpClient::builder()
        .with_tls_config(|builder| builder.with_root_cert_store(rustls::RootCertStore::empty()))
        .with_http_protocol(saluki_io::net::client::http::HttpProtocol::Http1);
    let endpoint = format!("{base}{FLARE_ENDPOINT_PATH}")
        .parse()
        .expect("endpoint should parse");
    TracerFlareProxy::new(builder, endpoint, api_key).expect("proxy should build")
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::body::{to_bytes, Body};
    use axum::routing::any;
    use axum::Router;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;

    const VERSION_LABEL: &str = "7-50-0-flare";

    fn endpoint(site: &str) -> String {
        flare_endpoint(site, VERSION_LABEL)
            .expect("site should give an endpoint")
            .to_string()
    }

    #[test]
    fn flare_endpoint_matches_the_trace_agent() {
        // The trace-agent's own test cases.
        assert_eq!(
            endpoint("datadoghq.com"),
            "https://7-50-0-flare.datadoghq.com/api/ui/support/serverless/flare"
        );
        assert_eq!(
            endpoint("https://app.datadoghq.com/test"),
            "https://7-50-0-flare.datadoghq.com/api/ui/support/serverless/flare"
        );
        assert_eq!(
            endpoint("https://us3.datadoghq.com/test"),
            "https://us3.datadoghq.com/api/ui/support/serverless/flare"
        );
        assert_eq!(
            endpoint("https://datadog.random.com/test"),
            "https://datadog.random.com/api/ui/support/serverless/flare"
        );
    }

    #[test]
    fn flare_endpoint_versions_every_official_app_host() {
        for (site, expected) in [
            ("datadoghq.eu", "7-50-0-flare.datadoghq.eu"),
            ("datad0g.com", "7-50-0-flare.datad0g.com"),
            ("ddog-gov.com", "7-50-0-flare.ddog-gov.com"),
            ("app.datadoghq.com", "7-50-0-flare.datadoghq.com"),
            ("app.us5.datadoghq.com", "7-50-0-flare.us5.datadoghq.com"),
            // The trace-agent keeps a Multi-Region Failover host; ADP versions it as the forwarder does.
            ("app.mrf.datadoghq.com", "7-50-0-flare.mrf.datadoghq.com"),
            ("http://app.datadoghq.eu", "7-50-0-flare.datadoghq.eu"),
            (" DATADOGHQ.COM ", "7-50-0-flare.datadoghq.com"),
        ] {
            assert_eq!(
                endpoint(site),
                format!("https://{expected}/api/ui/support/serverless/flare"),
                "{site}"
            );
        }
    }

    #[test]
    fn flare_endpoint_keeps_other_hosts() {
        // The trace-agent leaves the host of a bare `us3.datadoghq.com` empty; ADP uses the site as the host.
        assert_eq!(
            endpoint("us3.datadoghq.com"),
            "https://us3.datadoghq.com/api/ui/support/serverless/flare"
        );
        assert_eq!(
            endpoint("localhost:8080"),
            "https://localhost:8080/api/ui/support/serverless/flare"
        );
        assert_eq!(
            endpoint("http://[::1]:8443/flare"),
            "https://[::1]:8443/api/ui/support/serverless/flare"
        );
    }

    #[test]
    fn flare_endpoint_drops_credentials_and_keeps_the_port() {
        assert_eq!(
            endpoint("https://user:secret@app.datadoghq.com"),
            "https://7-50-0-flare.datadoghq.com/api/ui/support/serverless/flare"
        );
        assert_eq!(
            endpoint("https://app.datadoghq.com:8443"),
            "https://7-50-0-flare.datadoghq.com:8443/api/ui/support/serverless/flare"
        );
    }

    #[test]
    fn flare_endpoint_rejects_a_site_without_a_host() {
        assert!(flare_endpoint("", VERSION_LABEL).is_err());
        assert!(flare_endpoint("https://", VERSION_LABEL).is_err());
        assert!(flare_endpoint("not a host", VERSION_LABEL).is_err());
    }

    #[test]
    fn flare_version_label_uses_the_agent_version() {
        assert_eq!(
            flare_version_label(Some("7.50.0-devel+git.488.be5861b")),
            "7-50-0-flare"
        );
        assert_eq!(flare_version_label(Some("7.81.2-full")), "7-81-2-flare");
    }

    #[test]
    fn flare_version_label_falls_back_to_the_data_plane_version() {
        let version = saluki_metadata::get_app_details().version();
        let expected = format!("{}-{}-{}-flare", version.major(), version.minor(), version.patch());

        assert_eq!(flare_version_label(None), expected);
        assert_eq!(flare_version_label(Some("nightly")), expected);
    }

    /// What the fake intake saw of the last request it received.
    #[derive(Clone, Debug, Default)]
    struct Seen {
        method: String,
        uri: String,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    /// A stand-in flare intake: records each request and answers with `status`.
    async fn spawn_intake(status: StatusCode) -> (String, Arc<AsyncMutex<Seen>>) {
        let seen = Arc::new(AsyncMutex::new(Seen::default()));
        let router = Router::new().fallback(any({
            let seen = Arc::clone(&seen);
            move |request: AxumRequest| {
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
                    (status, [("x-intake", "yes"), ("connection", "close")], "from intake")
                }
            }
        }));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind an ephemeral port");
        let addr = listener.local_addr().expect("listener should have a local address");
        tokio::spawn(async move { axum::serve(listener, router).await });
        (format!("http://{addr}"), seen)
    }

    const MULTIPART_BODY: &str = "--boundary\r\n\
        Content-Disposition: form-data; name=\"case_id\"\r\n\r\ncase_id\r\n\
        --boundary\r\n\
        Content-Disposition: form-data; name=\"flare_file\"; filename=\"flare.zip\"\r\n\
        Content-Type: application/octet-stream\r\n\r\nzip bytes\r\n\
        --boundary--\r\n";

    fn flare_request(uri: &str, headers: &[(&str, &str)], peer: Option<ConnectionAddress>) -> AxumRequest {
        let mut builder = http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "multipart/form-data; boundary=boundary")
            .header("host", "localhost:8126");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut request = builder.body(Body::from(MULTIPART_BODY)).expect("request should build");
        if let Some(peer) = peer {
            request.extensions_mut().insert(ConnectInfo(peer));
        }
        request
    }

    #[tokio::test]
    async fn forwards_the_flare_to_the_flare_endpoint() {
        let (base, seen) = spawn_intake(StatusCode::OK).await;
        let proxy = plain_http_proxy(&base, "test-api-key");

        let response = proxy
            .forward(flare_request(
                "/tracer_flare/v1?foo=bar",
                &[],
                Some(SocketAddr::from(([192, 0, 2, 7], 4242)).into()),
            ))
            .await
            .expect("flare should be forwarded");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-intake"], "yes");
        assert!(!response.headers().contains_key("connection"));
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"from intake");

        let seen = seen.lock().await.clone();
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.uri, FLARE_ENDPOINT_PATH, "the path and query should be replaced");
        assert_eq!(seen.headers["dd-api-key"], "test-api-key");
        assert_eq!(seen.headers["x-forwarded-for"], "192.0.2.7");
        assert_eq!(seen.headers["x-forwarded-host"], "localhost:8126");
        assert_eq!(seen.headers["x-forwarded-proto"], "http");
        assert_eq!(seen.headers["content-type"], "multipart/form-data; boundary=boundary");
        assert_ne!(seen.headers["host"], "localhost:8126");
        assert_eq!(seen.body, MULTIPART_BODY.as_bytes());
    }

    #[tokio::test]
    async fn replaces_the_headers_a_client_sets_for_the_proxy() {
        let (base, seen) = spawn_intake(StatusCode::OK).await;
        let proxy = plain_http_proxy(&base, "test-api-key");

        proxy
            .forward(flare_request(
                "/tracer_flare/v1",
                &[
                    ("dd-api-key", "client-key"),
                    ("x-forwarded-for", "203.0.113.1"),
                    ("x-forwarded-host", "spoofed"),
                    ("forwarded", "for=203.0.113.1"),
                ],
                None,
            ))
            .await
            .expect("flare should be forwarded");

        let seen = seen.lock().await.clone();
        assert_eq!(seen.headers["dd-api-key"], "test-api-key");
        // Without a TCP peer, there is no client IP to forward.
        assert!(!seen.headers.contains_key("x-forwarded-for"));
        assert_eq!(seen.headers["x-forwarded-host"], "localhost:8126");
        assert!(!seen.headers.contains_key("forwarded"));
    }

    #[tokio::test]
    async fn passes_an_error_status_through() {
        let (base, _seen) = spawn_intake(StatusCode::FORBIDDEN).await;
        let proxy = plain_http_proxy(&base, "test-api-key");

        let response = proxy
            .forward(flare_request("/tracer_flare/v1", &[], None))
            .await
            .expect("a response from Datadog should be returned as is");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_unreachable_intake_is_a_bad_gateway() {
        let proxy = plain_http_proxy("http://127.0.0.1:1", "test-api-key");

        let error = proxy
            .forward(flare_request("/tracer_flare/v1", &[], None))
            .await
            .expect_err("forwarding should fail");

        assert_eq!(error.into_response().status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn sends_a_refreshed_api_key() {
        let (base, seen) = spawn_intake(StatusCode::OK).await;
        let proxy = plain_http_proxy(&base, "old-key");

        proxy.refresh_api_key("new-key");
        // A blank key is rejected, keeping the last usable one.
        proxy.refresh_api_key("  ");
        proxy
            .forward(flare_request("/tracer_flare/v1", &[], None))
            .await
            .expect("flare should be forwarded");

        assert_eq!(seen.lock().await.headers["dd-api-key"], "new-key");
    }
}
