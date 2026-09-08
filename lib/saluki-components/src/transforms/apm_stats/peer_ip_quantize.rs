//! Quantization of IP addresses in peer tag values before stats aggregation.
//!
//! Peer tag values become APM stats dimensions, and a raw peer IP address turns every host into
//! its own aggregation key, so bucket cardinality grows with the fleet size and per-service views
//! fragment. The quantizer collapses the host portion of peer values that are IP addresses into a
//! single `blocked-ip-address` placeholder, while preserving prefixes (`ip-`, `scheme://`), ports,
//! and suffixes, and passing through anything that is not an IP address untouched.
//!
//! The behavior here is a direct port of the server-side `QuantizePeerIPAddresses` helper, and the
//! placeholder string is a wire-visible contract: mixed clusters only aggregate cleanly when both
//! produce byte-identical values, so the semantics—including quirks like lenient IPv4 field
//! separators—are pinned deliberately rather than replaced with a "nicer" parser.

use std::borrow::Cow;
use std::net::Ipv6Addr;

/// The placeholder that replaces non-allowlisted IP address hosts.
const BLOCKED_IP_ADDRESS: &str = "blocked-ip-address";

/// The schemes whose prefixes survive quantization, for example `http://` in `http://10.0.0.1:8080`.
const SCHEMES: [&str; 5] = ["dnspoll", "ftp", "file", "http", "https"];

/// IP addresses that survive quantization unchanged.
///
/// These are localhost (both loopback families) and link-local cloud provider metadata server
/// addresses. Unlike regular peer IP addresses, these values carry actual meaning: they identify
/// talking to the metadata endpoint or to localhost, both of which are low-cardinality and useful
/// as dimensions on their own.
const ALLOWED_IP_ADDRESSES: [&str; 5] = [
    // localhost
    "127.0.0.1",
    "::1",
    // link-local cloud provider metadata server addresses
    "169.254.169.254",
    "fd00:ec2::254",
    // ECS task metadata
    "169.254.170.2",
];

/// Quantizes the IP addresses in a comma-separated list of hosts.
///
/// Each entry that is an IP address (with an optional port, prefix, and suffix) is replaced using
/// [`quantize_ip`], and duplicate entries post-quantization are collapsed into a single unique
/// value. Entries that are not IP addresses are left unchanged. Comma-separated host lists are
/// common for peer tags like `db.cassandra.contact.points`, `db.couchbase.seed.nodes`, and
/// `messaging.kafka.bootstrap.servers`.
///
/// When no entry is rewritten and no duplicates are collapsed, the input is returned borrowed.
/// The common case—a single peer value that is not an IP address—takes the single-entry fast path
/// and does not allocate.
pub(crate) fn quantize_peer_ip_addresses(raw: &str) -> Cow<'_, str> {
    // Fast path: a single-entry value cannot contain duplicates, so the entry quantizer alone
    // decides whether anything changes. This is the dominant case, and it allocates nothing at all
    // when the value passes through unchanged.
    if !raw.contains(',') {
        return quantize_ip(raw);
    }

    let mut out: Vec<Cow<'_, str>> = Vec::new();
    let mut changed = false;

    for value in raw.split(',') {
        let quantized = quantize_ip(value);
        // An owned entry means the quantizer rewrote this value. (`Cow::is_owned` would say this
        // directly, but it is unstable, so check the variant instead.)
        if matches!(quantized, Cow::Owned(_)) {
            changed = true;
        }

        // Peer value lists are short (a handful of seed nodes or bootstrap servers), so a linear
        // scan is cheaper than a set and keeps the deduped entries borrowed from the input.
        let is_new = out.iter().all(|existing| existing.as_ref() != quantized.as_ref());
        if is_new {
            out.push(quantized);
        } else {
            changed = true;
        }
    }

    if !changed {
        Cow::Borrowed(raw)
    } else {
        let mut joined = String::with_capacity(raw.len());
        for (index, value) in out.iter().enumerate() {
            if index > 0 {
                joined.push(',');
            }
            joined.push_str(value.as_ref());
        }
        Cow::Owned(joined)
    }
}

/// Quantizes the IP address in the provided value, if the value is exactly an IP address with an
/// optional port, prefix, and suffix.
///
/// If the value is not an IP address, it is returned unchanged. If the host is allowlisted, the
/// value is returned unchanged. Otherwise the host is replaced by `blocked-ip-address` while the
/// prefix, port, and suffix are preserved: ports are much lower cardinality than IP addresses and
/// tend to correspond to a protocol (for example, `443` is HTTPS), so keeping them in is both safe
/// and useful for aggregation.
fn quantize_ip(raw: &str) -> Cow<'_, str> {
    let (prefix, rest) = split_prefix(raw);
    let Some((host, port, suffix)) = parse_ip_and_port(rest) else {
        return Cow::Borrowed(raw);
    };

    if ALLOWED_IP_ADDRESSES.contains(&host) {
        return Cow::Borrowed(raw);
    }

    let port_len = if port.is_empty() { 0 } else { port.len() + 1 };
    let mut quantized = String::with_capacity(prefix.len() + BLOCKED_IP_ADDRESS.len() + port_len + suffix.len());
    quantized.push_str(prefix);
    quantized.push_str(BLOCKED_IP_ADDRESS);
    if !port.is_empty() {
        quantized.push(':');
        quantized.push_str(port);
    }
    quantized.push_str(suffix);

    Cow::Owned(quantized)
}

/// Splits a leading `ip-` or scheme prefix off of a value.
///
/// The `ip-` prefix covers AWS EC2 hostnames like `ip-10-123-4-567.ec2.internal`. The scheme
/// prefix covers values like `http://10.0.0.1:8080/health`, including the three-slash `file:///`
/// form; the scheme may appear anywhere in the value, matching the server-side behavior.
fn split_prefix(raw: &str) -> (&str, &str) {
    if let Some(rest) = raw.strip_prefix("ip-") {
        return ("ip-", rest);
    }

    for scheme in SCHEMES {
        if let Some(scheme_index) = raw.find(scheme) {
            let scheme_end = scheme_index + scheme.len() + 4;
            if scheme_end < raw.len() && &raw[scheme_index + scheme.len()..scheme_end] == ":///" {
                return (&raw[scheme_index..scheme_end], &raw[scheme_end..]);
            }
            let scheme_end = scheme_index + scheme.len() + 3;
            if scheme_end < raw.len() && &raw[scheme_index + scheme.len()..scheme_end] == "://" {
                return (&raw[scheme_index..scheme_end], &raw[scheme_end..]);
            }
        }
    }

    ("", raw)
}

/// Parses a value into an IP address host with an optional port and trailing suffix.
///
/// Returns `Some((host, port, suffix))` when the host portion is a valid IP address, where `suffix`
/// covers any trailing characters after the address (for example, `/health` in a URL) and is
/// preserved after quantization to keep paths distinct. Returns `None` when the value is not an IP
/// address.
fn parse_ip_and_port(value: &str) -> Option<(&str, &str, &str)> {
    let (mut host, mut port) = (value, "");
    if let Some((split_host, split_port)) = split_host_port(value) {
        host = split_host;
        port = split_port;
    }

    let end = is_parseable_ip(host)?;
    Some((&host[..end], port, &host[end..]))
}

/// Validates a string as an IP address and returns the index of the first character after the
/// address, which is the start of the suffix.
///
/// Returns `None` when the string is not an IP address.
fn is_parseable_ip(s: &str) -> Option<usize> {
    if s.is_empty() {
        return None;
    }

    // Must start with a hex digit, or IPv6 can have a preceding ':'.
    match s.as_bytes()[0] {
        b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' | b':' => {}
        _ => return None,
    }

    let bytes = s.as_bytes();
    for &b in bytes.iter() {
        match b {
            b'.' | b'_' | b'-' => return parse_ipv4(s, b),
            b':' => {
                // IPv6: the whole remaining string must parse.
                if s.parse::<Ipv6Addr>().is_ok() {
                    return Some(s.len());
                }
                return None;
            }
            b'%' => {
                // Assume that this was trying to be an IPv6 address with a zone specifier, but
                // the address is missing.
                return None;
            }
            _ => continue,
        }
    }

    None
}

/// Parses `s` as an IPv4 address and returns the index of the first character after the address.
///
/// This is a modified port of the standard library's IPv4 parsing that accepts alternate field
/// separators besides `.` (for example, `10-1-2-3` or `10_1_2_3`), because instrumentation libraries
/// sometimes mangle the separator, and also returns the index of trailing characters after the
/// address so that suffixes (for example, `.ec2.internal`) can be preserved.
///
/// Returns `None` when the string is not an IPv4 address.
fn parse_ipv4(s: &str, sep: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut field_value = 0u32;
    let mut field_count = 0;
    let mut digit_len = 0;

    for (i, &b) in bytes.iter().enumerate() {
        if b.is_ascii_digit() {
            // No leading zeros.
            if digit_len == 1 && field_value == 0 {
                return None;
            }
            field_value = field_value * 10 + (b - b'0') as u32;
            digit_len += 1;
            if field_value > 255 {
                return None;
            }
        } else if b == sep {
            // Reject `.1.2.3`, `1.2.3.`, and `1..2.3`-style forms.
            if i == 0 || i == bytes.len() - 1 || bytes[i - 1] == sep {
                return None;
            }
            // `1.2.3.4.5`—the fifth field is the start of the suffix.
            if field_count == 3 {
                return Some(i);
            }
            field_count += 1;
            field_value = 0;
            digit_len = 0;
        } else if field_count == 3 && digit_len > 0 {
            // First non-digit character after a complete four-field address: the suffix begins
            // here (for example, the `.` in `10.0.0.1.ec2.internal`).
            return Some(i);
        } else {
            return None;
        }
    }

    if field_count < 3 {
        return None;
    }

    Some(s.len())
}

/// Splits a network address of the form `host:port` or `[host]:port` into its host and port.
///
/// A literal IPv6 address in a host:port form must be enclosed in square brackets, as in
/// `[::1]:80`. Returns `None` when the value is not a valid host:port form; callers fall back to
/// treating the whole value as a bare host in that case.
fn split_host_port(hostport: &str) -> Option<(&str, &str)> {
    // The port starts after the last colon.
    let i = hostport.rfind(':')?;

    let (host, open_close) = if hostport.starts_with('[') {
        // Expect the first ']' just before the last ':'.
        let end = hostport.find(']')?;
        if end + 1 != i {
            // Either ']' isn't followed by a colon, or it is followed by a colon that is not the
            // last one.
            return None;
        }
        (&hostport[1..end], (1, end + 1))
    } else {
        let host = &hostport[..i];
        if host.contains(':') {
            return None;
        }
        (host, (0, 0))
    };

    // There can't be a '[' or ']' before/after these positions.
    if hostport[open_close.0..].contains('[') {
        return None;
    }
    if hostport[open_close.1..].contains(']') {
        return None;
    }

    Some((host, &hostport[i + 1..]))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn assert_q(raw: &str, expected: &str) {
        assert_eq!(quantize_peer_ip_addresses(raw), expected, "input: {}", raw);
    }

    #[test]
    fn plain_ipv4_is_blocked() {
        assert_q("10.0.13.42:5432", "blocked-ip-address:5432");
        assert_q("10.0.13.42", "blocked-ip-address");
        assert_q("52.14.144.171:8125", "blocked-ip-address:8125");
    }

    #[test]
    fn ipv6_is_blocked() {
        assert_q("fd00:ec2::16", "blocked-ip-address");
        assert_q("[2001:db8::1]:8080", "blocked-ip-address:8080");
    }

    #[test]
    fn allowlisted_addresses_pass_through() {
        assert_q("127.0.0.1", "127.0.0.1");
        assert_q("127.0.0.1:8080", "127.0.0.1:8080");
        assert_q("::1", "::1");
        assert_q("[::1]:8080", "[::1]:8080");
        assert_q("169.254.169.254", "169.254.169.254");
        assert_q("fd00:ec2::254", "fd00:ec2::254");
        assert_q("169.254.170.2", "169.254.170.2");
    }

    #[test]
    fn prefixes_survive_quantization() {
        assert_q("http://10.0.0.7:8080", "http://blocked-ip-address:8080");
        assert_q("https://10.0.0.7:443/health", "https://blocked-ip-address:443/health");
        assert_q("ftp://10.0.0.7", "ftp://blocked-ip-address");
        assert_q("file:///10.0.0.7", "file:///blocked-ip-address");
        assert_q("dnspoll://10.0.0.7", "dnspoll://blocked-ip-address");
        assert_q("ip-10-1-2-3.ec2.internal", "ip-blocked-ip-address.ec2.internal");
    }

    #[test]
    fn non_ip_values_pass_through() {
        assert_q("cassandra-01.prod", "cassandra-01.prod");
        assert_q("example.com:443", "example.com:443");
        // The Go parser treats the fifth field as the start of a suffix, so this quantizes.
        assert_q("10.0.0.7.5", "blocked-ip-address.5");
        assert_q("", "");
        assert_q("postgres", "postgres");
    }

    #[test]
    fn comma_lists_are_deduplicated_after_quantization() {
        // All entries collapse to one.
        assert_q("10.0.0.1:9000,10.0.0.2:9000,10.0.0.1:9000", "blocked-ip-address:9000");
        // Mixed IP and hostname: only the IP addresses collapse.
        assert_q(
            "10.0.0.1:9000,cassandra-01.prod,10.0.0.2:9000",
            "blocked-ip-address:9000,cassandra-01.prod",
        );
        // Allowlisted and blocked addresses stay distinct.
        assert_q("127.0.0.1,10.0.0.1", "127.0.0.1,blocked-ip-address");
        // Pre-existing duplicates are collapsed even without quantization.
        assert_q("db-a,db-a", "db-a");
    }

    #[test]
    fn suffixes_are_preserved() {
        assert_q("10.0.0.1/health", "blocked-ip-address/health");
        assert_q("10.0.0.1.ec2.internal", "blocked-ip-address.ec2.internal");
    }

    #[test]
    fn malformed_ip_like_values_pass_through() {
        // Leading zero.
        assert_q("010.0.0.1", "010.0.0.1");
        // Out of range octet.
        assert_q("999.0.0.1", "999.0.0.1");
        // Zone specifier without an address.
        assert_q("%lo0", "%lo0");
        // Bracketed IPv6 without a port.
        assert_q("[::1]", "[::1]");
    }

    /// The reference table from the server-side `TestQuantizePeerIpAddresses`, ported one-for-one.
    ///
    /// The placeholder and parser behavior are a wire-visible compatibility contract, so every
    /// case from the server-side test table must hold here unchanged. The comma-list cases from
    /// that table live in [`go_reference_comma_list_cases`] to overlap with the deduplication tests.
    #[test]
    fn go_reference_single_entry_cases() {
        let cases: &[(&str, &str)] = &[
            // Allowlisted addresses.
            ("127.0.0.1", "127.0.0.1"),
            ("::1", "::1"),
            ("169.254.169.254", "169.254.169.254"),
            ("fd00:ec2::254", "fd00:ec2::254"),
            ("169.254.170.2", "169.254.170.2"),
            // Blocking and pass-through cases.
            ("", ""),
            ("foo.dog", "foo.dog"),
            ("192.168.1.1", "blocked-ip-address"),
            ("192.168.1.1.foo", "blocked-ip-address.foo"),
            ("192.168.1.1.2.3.4.5", "blocked-ip-address.2.3.4.5"),
            ("192_168_1_1", "blocked-ip-address"),
            ("192-168-1-1", "blocked-ip-address"),
            ("192-168-1-1.foo", "blocked-ip-address.foo"),
            ("192-168-1-1-foo", "blocked-ip-address-foo"),
            ("2001:db8:3333:4444:CCCC:DDDD:EEEE:FFFF", "blocked-ip-address"),
            ("2001:db8:3c4d:15::1a2f:1a2b", "blocked-ip-address"),
            ("[fe80::1ff:fe23:4567:890a]:8080", "blocked-ip-address:8080"),
            ("192.168.1.1:1234", "blocked-ip-address:1234"),
            ("dnspoll:///10.21.120.145:6400", "dnspoll:///blocked-ip-address:6400"),
            (
                "dnspoll:///abc.cluster.local:50051",
                "dnspoll:///abc.cluster.local:50051",
            ),
            ("http://10.21.120.145:6400", "http://blocked-ip-address:6400"),
            ("https://10.21.120.145:6400", "https://blocked-ip-address:6400"),
            (
                "10-60-160-172.my-service.namespace.svc.abc.cluster.local",
                "blocked-ip-address.my-service.namespace.svc.abc.cluster.local",
            ),
            ("ip-10-152-4-129.ec2.internal", "ip-blocked-ip-address.ec2.internal"),
            ("1-foo", "1-foo"),
            ("1-2-foo", "1-2-foo"),
            ("1-2-3-foo", "1-2-3-foo"),
            ("1-2-3-999", "1-2-3-999"),
            ("1-2-999-foo", "1-2-999-foo"),
            ("1-2-3-999-foo", "1-2-3-999-foo"),
            ("1-2-3-4-foo", "blocked-ip-address-foo"),
            ("7-55-2-app.agent.datadoghq.com", "7-55-2-app.agent.datadoghq.com"),
        ];

        for (input, expected) in cases {
            assert_eq!(
                quantize_peer_ip_addresses(input),
                *expected,
                "server-side reference case mismatch for input: {}",
                input
            );
        }
    }

    /// The comma-list cases from the server-side `TestQuantizePeerIpAddresses`, ported one-for-one.
    #[test]
    fn go_reference_comma_list_cases() {
        let cases: &[(&str, &str)] = &[
            (
                "192.168.1.1:1234,10.23.1.1:53,10.23.1.1,fe80::1ff:fe23:4567:890a,foo.dog",
                "blocked-ip-address:1234,blocked-ip-address:53,blocked-ip-address,foo.dog",
            ),
            (
                "http://172.24.160.151:8091,172.24.163.33:8091,172.24.164.111:8091,172.24.165.203:8091,172.24.168.235:8091,172.24.170.130:8091",
                "http://blocked-ip-address:8091,blocked-ip-address:8091",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(
                quantize_peer_ip_addresses(input),
                *expected,
                "server-side reference case mismatch for input: {}",
                input
            );
        }
    }

    #[test]
    fn property_test_quantize_is_idempotent() {
        proptest!(|(value in "[A-Za-z0-9:\\.\\-_%/]{1,64}")| {
            let once = quantize_peer_ip_addresses(&value);
            let once_owned = once.clone().into_owned();
            let twice = quantize_peer_ip_addresses(&once_owned);
            prop_assert_eq!(once, twice, "quantizing a quantized value must be a no-op");
        });
    }

    #[test]
    fn property_test_quantize_never_grows_cardinality() {
        proptest!(|(hosts in proptest::collection::vec("[A-Za-z0-9:\\.\\-_%/]{1,32}", 1..8))| {
            let raw = hosts.join(",");
            let quantized = quantize_peer_ip_addresses(&raw);
            let input_count = raw.split(',').count();
            let output_count = quantized.split(',').count();
            prop_assert!(output_count <= input_count, "output cardinality must never exceed input");
        });
    }
}
