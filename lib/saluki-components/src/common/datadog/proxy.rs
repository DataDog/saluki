use std::net::IpAddr;
use std::sync::Arc;

use headers::Authorization;
use hyper_http_proxy::{Intercept, Proxy};
use saluki_error::GenericError;
use serde::{Deserialize, Deserializer};
use url::Url;

#[derive(Clone, Deserialize)]
pub struct ProxyConfiguration {
    /// The proxy server for HTTP requests.
    #[serde(rename = "proxy_http")]
    http_server: Option<String>,

    /// The proxy server for HTTPS requests.
    #[serde(rename = "proxy_https")]
    https_server: Option<String>,

    /// List of hosts that should bypass the proxy.
    ///
    /// In YAML this is a sequence under `proxy.no_proxy`. As an environment variable (`DD_PROXY_NO_PROXY`),
    /// values are space-separated.
    #[serde(
        default,
        rename = "proxy_no_proxy",
        deserialize_with = "deserialize_space_separated_or_seq"
    )]
    no_proxy: Vec<String>,

    /// When true, `no_proxy` uses full domain/CIDR/wildcard matching, mirroring the behavior of Go's
    /// [`golang.org/x/net/http/httpproxy`](https://pkg.go.dev/golang.org/x/net/http/httpproxy) package.
    /// When false (the default), only exact host matches are applied; suffix and CIDR entries are ignored.
    #[serde(default)]
    no_proxy_nonexact_match: bool,

    /// When true, proxy settings apply to requests for cloud provider metadata endpoints.
    ///
    /// When false (the default), the well-known cloud metadata addresses are automatically added to
    /// the `no_proxy` list so they always bypass the proxy.
    #[serde(default)]
    use_proxy_for_cloud_metadata: bool,
}

/// Well-known cloud provider metadata endpoint addresses that bypass the proxy by default.
const CLOUD_METADATA_ADDRS: &[&str] = &[
    "169.254.169.254", // Azure, EC2, GCE
    "100.100.100.200", // Alibaba
];

impl ProxyConfiguration {
    /// Builds the configured proxies.
    ///
    /// Each proxy uses a custom intercept that forwards only requests matching the proxy's scheme
    /// (HTTP or HTTPS) and not excluded by `no_proxy`. Cloud metadata addresses are appended to
    /// the exclusion list unless `use_proxy_for_cloud_metadata` is true. Matching mode (exact vs.
    /// full domain/CIDR/wildcard) is controlled by `no_proxy_nonexact_match`.
    ///
    /// # Errors
    ///
    /// If the configured proxy URLs are invalid, an error is returned.
    pub fn build(&self) -> Result<Vec<Proxy>, GenericError> {
        // Build the effective no_proxy list, injecting cloud metadata addresses unless the caller
        // has explicitly opted in to proxying them.
        let mut effective_no_proxy = self.no_proxy.clone();
        if !self.use_proxy_for_cloud_metadata {
            effective_no_proxy.extend(CLOUD_METADATA_ADDRS.iter().map(|s| s.to_string()));
        }

        let matcher = Arc::new(NoProxyMatcher::new(&effective_no_proxy, self.no_proxy_nonexact_match));

        let mut proxies = Vec::new();

        if let Some(url) = &self.http_server {
            let m = Arc::clone(&matcher);
            let intercept = Intercept::from(move |scheme: Option<&str>, host: Option<&str>, port: Option<u16>| {
                scheme == Some("http") && !host.map_or(false, |h| m.matches(h, port))
            });
            proxies.push(new_proxy(url, intercept)?);
        }

        if let Some(url) = &self.https_server {
            let m = Arc::clone(&matcher);
            let intercept = Intercept::from(move |scheme: Option<&str>, host: Option<&str>, port: Option<u16>| {
                scheme == Some("https") && !host.map_or(false, |h| m.matches(h, port))
            });
            proxies.push(new_proxy(url, intercept)?);
        }

        Ok(proxies)
    }
}

fn new_proxy(proxy_url: &str, intercept: Intercept) -> Result<Proxy, GenericError> {
    let url = Url::parse(proxy_url)?;
    let mut proxy = Proxy::new(intercept, url.as_str().parse()?);
    if let Some(password) = url.password() {
        let username = url.username();
        proxy.set_authorization(Authorization::basic(username, password));
    }
    Ok(proxy)
}

/// Parsed representation of a single `no_proxy` entry.
enum NoProxyEntry {
    /// `*` — bypass proxy for all destinations. Only used in nonexact mode.
    Wildcard,
    /// An IP address in CIDR notation (e.g. `192.168.0.0/24`). Only used in nonexact mode.
    IpCidr { addr: IpAddr, prefix_len: u8 },
    /// An exact IP address, with an optional port constraint (e.g. `192.168.1.1` or `192.168.1.1:80`).
    IpExact { addr: IpAddr, port: Option<u16> },
    /// A domain name, with optional port constraint and a flag for suffix-only matching.
    ///
    /// When `suffix_only` is true (entry had a leading `.`), only subdomains match, not the domain
    /// itself. When false, both the domain and its subdomains match (in nonexact mode).
    Domain {
        name: String,
        port: Option<u16>,
        suffix_only: bool,
    },
}

impl NoProxyEntry {
    fn matches(&self, host: &str, port: Option<u16>, nonexact: bool) -> bool {
        match self {
            NoProxyEntry::Wildcard => nonexact,

            NoProxyEntry::IpExact { addr, port: entry_port } => {
                let host_matches = host.parse::<IpAddr>().map_or(false, |h| h == *addr);
                let port_matches = entry_port.map_or(true, |ep| port == Some(ep));
                host_matches && port_matches
            }

            NoProxyEntry::IpCidr { addr, prefix_len } => {
                if !nonexact {
                    return false;
                }
                host.parse::<IpAddr>()
                    .map_or(false, |h| ip_in_cidr(*addr, *prefix_len, h))
            }

            NoProxyEntry::Domain {
                name,
                port: entry_port,
                suffix_only,
            } => {
                let port_matches = entry_port.map_or(true, |ep| port == Some(ep));
                if !port_matches {
                    return false;
                }
                let host_lower = host.to_lowercase();
                if nonexact {
                    if *suffix_only {
                        // Leading-dot entry: only matches subdomains, not the domain itself.
                        // e.g. ".y.com" matches "x.y.com" but not "y.com".
                        host_lower.ends_with(&format!(".{}", name))
                    } else {
                        // Plain domain: matches the domain and all subdomains.
                        // e.g. "foo.com" matches "foo.com" and "bar.foo.com".
                        host_lower == *name || host_lower.ends_with(&format!(".{}", name))
                    }
                } else {
                    // Exact mode: suffix-only entries are ignored entirely; plain domains match
                    // only the literal hostname.
                    if *suffix_only {
                        return false;
                    }
                    host_lower == *name
                }
            }
        }
    }
}

struct NoProxyMatcher {
    entries: Vec<NoProxyEntry>,
    nonexact: bool,
}

impl NoProxyMatcher {
    fn new(no_proxy: &[String], nonexact: bool) -> Self {
        let entries = no_proxy.iter().filter_map(|s| parse_no_proxy_entry(s)).collect();
        Self { entries, nonexact }
    }

    fn matches(&self, host: &str, port: Option<u16>) -> bool {
        self.entries.iter().any(|e| e.matches(host, port, self.nonexact))
    }
}

/// Parses a single `no_proxy` entry string. Returns `None` for empty or malformed entries (best-effort).
fn parse_no_proxy_entry(entry: &str) -> Option<NoProxyEntry> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    if entry == "*" {
        return Some(NoProxyEntry::Wildcard);
    }

    // CIDR notation: `1.2.3.4/8`. A CIDR entry does not carry a port.
    if let Some((ip_str, prefix_str)) = entry.split_once('/') {
        let addr = ip_str.parse::<IpAddr>().ok()?;
        let prefix_len = prefix_str.parse::<u8>().ok()?;
        return Some(NoProxyEntry::IpCidr { addr, prefix_len });
    }

    // Everything else may carry an optional port suffix.
    let (host_str, port) = split_host_port(entry);

    // Try parsing as a bare IP address.
    if let Ok(addr) = host_str.parse::<IpAddr>() {
        return Some(NoProxyEntry::IpExact { addr, port });
    }

    // Treat as a domain name.
    let suffix_only = host_str.starts_with('.');
    let name = host_str.trim_start_matches('.').to_lowercase();
    if name.is_empty() {
        return None;
    }
    Some(NoProxyEntry::Domain {
        name,
        port,
        suffix_only,
    })
}

/// Splits a host string into a (host, port) pair.
///
/// Handles IPv6 addresses in brackets (`[::1]` or `[::1]:80`), bare IPv6 addresses (multiple
/// colons, treated as having no port), and standard `host:port` or `ip:port` forms.
fn split_host_port(s: &str) -> (&str, Option<u16>) {
    // Bracketed IPv6: [::1] or [::1]:80
    if s.starts_with('[') {
        if let Some(end) = s.find(']') {
            let host = &s[1..end];
            let port = s
                .get(end + 1..)
                .and_then(|r| r.strip_prefix(':'))
                .and_then(|p| p.parse().ok());
            return (host, port);
        }
    }
    // Bare IPv6 (multiple colons) has no port component.
    if s.chars().filter(|&c| c == ':').count() > 1 {
        return (s, None);
    }
    // Single colon: try host:port.
    if let Some(pos) = s.rfind(':') {
        if let Ok(port) = s[pos + 1..].parse::<u16>() {
            return (&s[..pos], Some(port));
        }
    }
    (s, None)
}

/// Returns true if `addr` falls within the CIDR block defined by `network`/`prefix_len`.
fn ip_in_cidr(network: IpAddr, prefix_len: u8, addr: IpAddr) -> bool {
    match (network, addr) {
        (IpAddr::V4(net), IpAddr::V4(tgt)) => {
            if prefix_len == 0 {
                return true;
            }
            if prefix_len > 32 {
                return false;
            }
            let shift = 32 - prefix_len;
            (u32::from(net) >> shift) == (u32::from(tgt) >> shift)
        }
        (IpAddr::V6(net), IpAddr::V6(tgt)) => {
            if prefix_len == 0 {
                return true;
            }
            if prefix_len > 128 {
                return false;
            }
            let shift = 128 - prefix_len;
            (u128::from(net) >> shift) == (u128::from(tgt) >> shift)
        }
        // IPv4 vs IPv6 mismatch — never matches.
        _ => false,
    }
}

/// Deserializes a `Vec<String>` from either a sequence or a space-separated string.
///
/// This handles the dual representation of `no_proxy`: a YAML sequence (`proxy.no_proxy`) and an
/// environment variable (`DD_PROXY_NO_PROXY`) where values are space-separated.
fn deserialize_space_separated_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{SeqAccess, Visitor};
    use std::fmt;

    struct SpaceSeparatedOrSeq;

    impl<'de> Visitor<'de> for SpaceSeparatedOrSeq {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a sequence or a space-separated string")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(v.split_whitespace().map(str::to_owned).collect())
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
            let mut values = Vec::new();
            while let Some(v) = seq.next_element::<String>()? {
                values.push(v);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(SpaceSeparatedOrSeq)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deserialize_no_proxy(json: serde_json::Value) -> Vec<String> {
        serde_json::from_value::<ProxyConfiguration>(json)
            .expect("should deserialize")
            .no_proxy
    }

    fn matcher(entries: &[&str], nonexact: bool) -> NoProxyMatcher {
        NoProxyMatcher::new(&entries.iter().map(|s| s.to_string()).collect::<Vec<_>>(), nonexact)
    }

    // --- deserializer tests ---

    #[test]
    fn no_proxy_from_space_separated_string() {
        let config = serde_json::json!({ "proxy_no_proxy": "host1.example.com host2.example.com" });
        assert_eq!(
            deserialize_no_proxy(config),
            vec!["host1.example.com", "host2.example.com"]
        );
    }

    #[test]
    fn no_proxy_from_sequence() {
        let config = serde_json::json!({ "proxy_no_proxy": ["host1.example.com", "host2.example.com"] });
        assert_eq!(
            deserialize_no_proxy(config),
            vec!["host1.example.com", "host2.example.com"]
        );
    }

    #[test]
    fn no_proxy_empty_string_gives_empty_vec() {
        let config = serde_json::json!({ "proxy_no_proxy": "" });
        assert_eq!(deserialize_no_proxy(config), Vec::<String>::new());
    }

    #[test]
    fn no_proxy_empty_sequence_gives_empty_vec() {
        let config = serde_json::json!({ "proxy_no_proxy": [] });
        assert_eq!(deserialize_no_proxy(config), Vec::<String>::new());
    }

    #[test]
    fn no_proxy_absent_gives_empty_vec() {
        let config = serde_json::json!({});
        assert_eq!(deserialize_no_proxy(config), Vec::<String>::new());
    }

    #[test]
    fn no_proxy_string_trims_extra_whitespace() {
        let config = serde_json::json!({ "proxy_no_proxy": "  host1.example.com   host2.example.com  " });
        assert_eq!(
            deserialize_no_proxy(config),
            vec!["host1.example.com", "host2.example.com"]
        );
    }

    // --- wildcard ---

    #[test]
    fn wildcard_matches_any_host_in_nonexact_mode() {
        let m = matcher(&["*"], true);
        assert!(m.matches("anything.example.com", None));
        assert!(m.matches("192.168.1.1", None));
    }

    #[test]
    fn wildcard_ignored_in_exact_mode() {
        let m = matcher(&["*"], false);
        assert!(!m.matches("anything.example.com", None));
        assert!(!m.matches("192.168.1.1", None));
    }

    // --- exact IP ---

    #[test]
    fn exact_ip_matches() {
        let m = matcher(&["192.168.1.1"], true);
        assert!(m.matches("192.168.1.1", None));
        assert!(!m.matches("192.168.1.2", None));
        assert!(!m.matches("example.com", None));
    }

    #[test]
    fn exact_ip_with_port_requires_port_match() {
        let m = matcher(&["192.168.1.1:8080"], true);
        assert!(m.matches("192.168.1.1", Some(8080)));
        assert!(!m.matches("192.168.1.1", Some(9090)));
        assert!(!m.matches("192.168.1.1", None));
    }

    #[test]
    fn exact_ip_without_port_matches_any_port() {
        let m = matcher(&["192.168.1.1"], true);
        assert!(m.matches("192.168.1.1", Some(80)));
        assert!(m.matches("192.168.1.1", None));
    }

    // --- CIDR ---

    #[test]
    fn cidr_matches_address_in_range() {
        let m = matcher(&["192.168.0.0/24"], true);
        assert!(m.matches("192.168.0.1", None));
        assert!(m.matches("192.168.0.254", None));
        assert!(!m.matches("192.168.1.1", None));
    }

    #[test]
    fn cidr_ignored_in_exact_mode() {
        let m = matcher(&["192.168.0.0/24"], false);
        assert!(!m.matches("192.168.0.1", None));
    }

    #[test]
    fn cidr_prefix_zero_matches_any_address() {
        let m = matcher(&["0.0.0.0/0"], true);
        assert!(m.matches("1.2.3.4", None));
        assert!(m.matches("192.168.1.1", None));
    }

    #[test]
    fn cidr_ipv4_does_not_match_ipv6_address() {
        let m = matcher(&["192.168.0.0/24"], true);
        assert!(!m.matches("::1", None));
    }

    #[test]
    fn cidr_ipv6_does_not_match_ipv4_address() {
        let m = matcher(&["2001:db8::/32"], true);
        assert!(!m.matches("192.168.0.1", None));
    }

    // --- IPv6 ---

    #[test]
    fn exact_ipv6_matches() {
        let m = matcher(&["::1"], true);
        assert!(m.matches("::1", None));
        assert!(!m.matches("::2", None));
        assert!(!m.matches("192.168.1.1", None));
    }

    #[test]
    fn exact_ipv6_with_port_requires_port_match() {
        let m = matcher(&["[::1]:8080"], true);
        assert!(m.matches("::1", Some(8080)));
        assert!(!m.matches("::1", Some(9090)));
        assert!(!m.matches("::1", None));
    }

    #[test]
    fn exact_ipv6_without_port_matches_any_port() {
        let m = matcher(&["::1"], true);
        assert!(m.matches("::1", Some(80)));
        assert!(m.matches("::1", None));
    }

    #[test]
    fn ipv6_cidr_matches_address_in_range() {
        let m = matcher(&["2001:db8::/32"], true);
        assert!(m.matches("2001:db8::1", None));
        assert!(m.matches("2001:db8:ffff::1", None));
        assert!(!m.matches("2001:db9::1", None));
    }

    #[test]
    fn ipv6_cidr_ignored_in_exact_mode() {
        let m = matcher(&["2001:db8::/32"], false);
        assert!(!m.matches("2001:db8::1", None));
    }

    // --- domain exact mode ---

    #[test]
    fn domain_exact_mode_matches_literal() {
        let m = matcher(&["localhost"], false);
        assert!(m.matches("localhost", None));
        assert!(!m.matches("other.localhost", None));
    }

    #[test]
    fn domain_exact_mode_does_not_match_subdomains() {
        let m = matcher(&["example.com"], false);
        assert!(m.matches("example.com", None));
        assert!(!m.matches("sub.example.com", None));
    }

    #[test]
    fn domain_suffix_entry_ignored_in_exact_mode() {
        // Leading-dot entries are suffix-only and should be ignored in exact mode.
        let m = matcher(&[".example.com"], false);
        assert!(!m.matches("sub.example.com", None));
        assert!(!m.matches("example.com", None));
    }

    // --- domain nonexact mode ---

    #[test]
    fn domain_nonexact_matches_domain_and_subdomains() {
        let m = matcher(&["foo.com"], true);
        assert!(m.matches("foo.com", None));
        assert!(m.matches("bar.foo.com", None));
        assert!(!m.matches("notfoo.com", None));
    }

    #[test]
    fn domain_suffix_only_matches_subdomains_not_domain() {
        // ".y.com" matches "x.y.com" but not "y.com"
        let m = matcher(&[".y.com"], true);
        assert!(m.matches("x.y.com", None));
        assert!(!m.matches("y.com", None));
    }

    #[test]
    fn domain_matching_is_case_insensitive() {
        let m = matcher(&["Example.COM"], true);
        assert!(m.matches("example.com", None));
        assert!(m.matches("SUB.EXAMPLE.COM", None));
    }

    #[test]
    fn domain_with_port_requires_port_match() {
        let m = matcher(&["example.com:443"], true);
        assert!(m.matches("example.com", Some(443)));
        assert!(!m.matches("example.com", Some(80)));
        assert!(!m.matches("example.com", None));
    }

    // --- cloud metadata defaults ---

    fn proxy_config_with_cloud_flag(use_proxy: bool) -> ProxyConfiguration {
        serde_json::from_value::<ProxyConfiguration>(serde_json::json!({
            "proxy_http": "http://proxy.example.com:3128",
            "use_proxy_for_cloud_metadata": use_proxy,
        }))
        .expect("should deserialize")
    }

    #[test]
    fn cloud_metadata_bypasses_proxy_by_default() {
        let config = proxy_config_with_cloud_flag(false);
        let proxies = config.build().expect("should build");
        // With a custom intercept the proxy list still has one entry.
        assert_eq!(proxies.len(), 1);
        // The intercept should not proxy cloud metadata addresses.
        assert!(!proxies[0]
            .intercept()
            .matches(&"http://169.254.169.254/latest/meta-data".parse::<hyper::Uri>().unwrap()));
        assert!(!proxies[0]
            .intercept()
            .matches(&"http://100.100.100.200/".parse::<hyper::Uri>().unwrap()));
        // Normal hosts should still be proxied.
        assert!(proxies[0]
            .intercept()
            .matches(&"http://example.com/".parse::<hyper::Uri>().unwrap()));
    }

    #[test]
    fn cloud_metadata_proxied_when_flag_enabled() {
        let config = proxy_config_with_cloud_flag(true);
        let proxies = config.build().expect("should build");
        assert_eq!(proxies.len(), 1);
        // With no no_proxy list and the flag enabled, nothing is excluded — proxies everything.
        assert!(proxies[0]
            .intercept()
            .matches(&"http://169.254.169.254/latest/meta-data".parse::<hyper::Uri>().unwrap()));
        assert!(proxies[0]
            .intercept()
            .matches(&"http://100.100.100.200/".parse::<hyper::Uri>().unwrap()));
    }

    // --- end-to-end routing tests ---
    //
    // These tests spin up a mock HTTP server acting as a proxy and verify that the
    // HttpClient actually routes (or bypasses) traffic correctly based on ProxyConfiguration.

    fn init_tls() {
        static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        INIT.get_or_init(|| {
            saluki_tls::initialize_default_crypto_provider()
                .expect("failed to initialize TLS crypto provider");
            saluki_tls::load_platform_root_certificates()
                .expect("failed to load platform root certificates");
        });
    }

    async fn start_mock_http_server(status: http::StatusCode) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = axum::Router::new().fallback(move || async move { status });
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        port
    }

    fn proxy_config_for_routing(proxy_http: &str, no_proxy: &[&str]) -> ProxyConfiguration {
        serde_json::from_value::<ProxyConfiguration>(serde_json::json!({
            "proxy_http": proxy_http,
            "proxy_no_proxy": no_proxy,
        }))
        .expect("should deserialize")
    }

    #[tokio::test]
    async fn http_request_routed_through_proxy() {
        init_tls();
        // The mock proxy returns 418; a direct connection to doesnotexist.example.com would fail.
        // Getting 418 back confirms the request reached our mock proxy.
        let proxy_port = start_mock_http_server(http::StatusCode::IM_A_TEAPOT).await;
        let config = proxy_config_for_routing(&format!("http://127.0.0.1:{proxy_port}"), &[]);
        let proxies = config.build().unwrap();

        let mut client = saluki_io::net::client::http::HttpClient::builder()
            .with_proxies(proxies)
            .build()
            .unwrap();

        let req = http::Request::builder()
            .uri("http://doesnotexist.example.com/")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let resp = client.send(req).await.unwrap();
        assert_eq!(resp.status(), http::StatusCode::IM_A_TEAPOT);
    }

    #[tokio::test]
    async fn no_proxy_host_bypasses_proxy() {
        init_tls();
        // The mock proxy returns 418; the direct server returns 200.
        // Getting 200 back confirms the request bypassed the proxy and went directly.
        let proxy_port = start_mock_http_server(http::StatusCode::IM_A_TEAPOT).await;
        let direct_port = start_mock_http_server(http::StatusCode::OK).await;
        let config = proxy_config_for_routing(
            &format!("http://127.0.0.1:{proxy_port}"),
            &[&format!("127.0.0.1:{direct_port}")],
        );
        let proxies = config.build().unwrap();

        let mut client = saluki_io::net::client::http::HttpClient::builder()
            .with_proxies(proxies)
            .build()
            .unwrap();

        let req = http::Request::builder()
            .uri(format!("http://127.0.0.1:{direct_port}/"))
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let resp = client.send(req).await.unwrap();
        assert_eq!(resp.status(), http::StatusCode::OK);
    }

    /// Starts a minimal raw TCP server that handles a single HTTP CONNECT request per connection.
    ///
    /// Returns the listening port and a channel receiver that yields the `host:port` target from
    /// each CONNECT request received. The server runs until the tokio test runtime is dropped.
    async fn start_mock_connect_proxy() -> (u16, tokio::sync::mpsc::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::mpsc::channel(10);

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    if let Ok(n) = stream.read(&mut buf).await {
                        let req = String::from_utf8_lossy(&buf[..n]);
                        // "CONNECT host:port HTTP/1.1\r\n..."
                        if let Some(target) =
                            req.strip_prefix("CONNECT ").and_then(|s| s.split_whitespace().next())
                        {
                            let _ = tx.send(target.to_string()).await;
                        }
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                            .await;
                    }
                    // Hold the connection open briefly so the client can read the 200 before
                    // the stream is dropped and the connection closes.
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                });
            }
        });

        (port, rx)
    }

    #[tokio::test]
    async fn https_request_routed_through_proxy() {
        init_tls();
        // The mock CONNECT proxy records the target from each CONNECT request. The TLS
        // handshake will fail after the tunnel is established (no real server behind it), but
        // receiving the CONNECT confirms the request was routed through the proxy.
        let (proxy_port, mut rx) = start_mock_connect_proxy().await;
        let config = serde_json::from_value::<ProxyConfiguration>(serde_json::json!({
            "proxy_https": format!("http://127.0.0.1:{proxy_port}"),
        }))
        .expect("should deserialize");
        let proxies = config.build().unwrap();

        let mut client = saluki_io::net::client::http::HttpClient::builder()
            .with_proxies(proxies)
            .with_tls_config(|b| b.danger_accept_invalid_certs())
            .build()
            .unwrap();

        let req = http::Request::builder()
            .uri("https://doesnotexist.example.com/")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        // TLS will fail after the CONNECT tunnel is established; ignore the error.
        let _ = client.send(req).await;

        // Verify the proxy received a CONNECT request for our target.
        let connect_target = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for CONNECT request")
            .expect("channel closed unexpectedly");
        assert!(
            connect_target.starts_with("doesnotexist.example.com:"),
            "unexpected CONNECT target: {connect_target}"
        );
    }

    #[tokio::test]
    async fn https_no_proxy_host_bypasses_proxy() {
        init_tls();
        // For a no_proxy host, the client connects directly rather than sending a CONNECT to
        // the proxy. We verify this by asserting the proxy receives no CONNECT request.
        let (proxy_port, mut rx) = start_mock_connect_proxy().await;
        let direct_port = start_mock_http_server(http::StatusCode::OK).await;
        let config = serde_json::from_value::<ProxyConfiguration>(serde_json::json!({
            "proxy_https": format!("http://127.0.0.1:{proxy_port}"),
            "proxy_no_proxy": [format!("127.0.0.1:{direct_port}")],
        }))
        .expect("should deserialize");
        let proxies = config.build().unwrap();

        let mut client = saluki_io::net::client::http::HttpClient::builder()
            .with_proxies(proxies)
            .with_tls_config(|b| b.danger_accept_invalid_certs())
            .build()
            .unwrap();

        let req = http::Request::builder()
            .uri(format!("https://127.0.0.1:{direct_port}/"))
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        // Direct connection: TLS fails (plain HTTP server), but the proxy should not be
        // contacted. Ignore the error.
        let _ = client.send(req).await;

        // Verify the proxy received no CONNECT requests.
        assert!(
            rx.try_recv().is_err(),
            "proxy should not have been contacted for a no_proxy host"
        );
    }
}
