use snafu::{ResultExt as _, Snafu};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ResolveHostError {
    #[snafu(display("failed to look up host '{}': {}", host, source))]
    LookupFailed { host: String, source: std::io::Error },

    #[snafu(display("host '{}' resolved to zero IP addresses", host))]
    NoAddresses { host: String },
}

/// Resolves a host to an `IpAddr`.
///
/// Accepts either an IP literal (no DNS required) or a hostname (resolved via async DNS). Returns `LookupFailed` if the
/// lookup fails, or `NoAddresses` if it succeeds but returns no addresses.
pub async fn resolve_host(host: &str) -> Result<std::net::IpAddr, ResolveHostError> {
    let mut addrs = tokio::net::lookup_host((host, 0u16))
        .await
        .context(LookupFailed { host })?;
    addrs
        .next()
        .map(|sa| sa.ip())
        .ok_or_else(|| ResolveHostError::NoAddresses { host: host.to_string() })
}
