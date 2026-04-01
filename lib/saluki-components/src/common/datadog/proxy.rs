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
}
