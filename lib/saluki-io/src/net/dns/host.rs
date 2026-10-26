//! Resolution of hosts to IP addresses.

use std::net::IpAddr;

use snafu::{ResultExt as _, Snafu};

/// An error produced while resolving a host to an IP address.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ResolveHostError {
    /// The host lookup failed.
    #[snafu(display("failed to look up host '{}': {}", host, source))]
    LookupFailed {
        /// Host that was looked up.
        host: String,

        /// Source of the error.
        source: std::io::Error,
    },

    /// The host lookup succeeded but returned no addresses.
    #[snafu(display("host '{}' resolved to zero IP addresses", host))]
    NoAddresses {
        /// Host that was looked up.
        host: String,
    },
}

/// Resolves a host to an IP address.
///
/// `host` can be an IP literal, which is parsed without a DNS lookup, or a hostname, which is resolved using the system
/// resolver. When a hostname resolves to several addresses, the first one is returned.
///
/// # Errors
///
/// If the lookup fails, [`ResolveHostError::LookupFailed`] is returned. If the lookup succeeds but returns no
/// addresses, [`ResolveHostError::NoAddresses`] is returned.
pub async fn resolve_host(host: &str) -> Result<IpAddr, ResolveHostError> {
    let mut addrs = tokio::net::lookup_host((host, 0u16))
        .await
        .context(LookupFailed { host })?;
    addrs
        .next()
        .map(|sa| sa.ip())
        .ok_or_else(|| ResolveHostError::NoAddresses { host: host.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_ip_literals_without_lookup() {
        assert_eq!(resolve_host("127.0.0.1").await.unwrap(), IpAddr::from([127, 0, 0, 1]));
        assert_eq!(
            resolve_host("192.168.1.50").await.unwrap(),
            IpAddr::from([192, 168, 1, 50])
        );
        assert_eq!(
            resolve_host("::1").await.unwrap(),
            IpAddr::from(std::net::Ipv6Addr::LOCALHOST)
        );
    }
}
