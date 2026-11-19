use std::{future::Future, io, net::SocketAddr, time::Duration};

use snafu::ResultExt as _;
use tokio::net::TcpStream;

use crate::{Error, HostPort, ResolveSnafu, TargetSnafu, TimedOutSnafu};

/// Runs `operation` on `target`, failing with [`Error::TimedOut`] once `limit` passes.
pub(crate) async fn within<T>(
    operation: &'static str, target: &(dyn std::fmt::Display + Sync), limit: Duration,
    work: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    match tokio::time::timeout(limit, work).await {
        Ok(result) => result,
        Err(_) => TimedOutSnafu {
            operation,
            target: target.to_string(),
            limit,
        }
        .fail(),
    }
}

/// Resolves `host_port`, IPv4 addresses first. A participant's name can resolve to a link-local IPv6 address before its
/// IPv4 one, as on a Windows `nat` network, where the software listens on IPv4 only. A datagram sent to the first
/// address would be lost without an error.
pub(crate) async fn resolve(host_port: &HostPort) -> Result<Vec<SocketAddr>, Error> {
    let mut addrs: Vec<_> = tokio::net::lookup_host((host_port.host.as_str(), host_port.port))
        .await
        .context(ResolveSnafu { host: &host_port.host })?
        .collect();
    addrs.sort_by_key(SocketAddr::is_ipv6);
    if addrs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no addresses"))
            .context(ResolveSnafu { host: &host_port.host });
    }
    Ok(addrs)
}

/// Connects to the first resolved address that accepts.
pub(crate) async fn connect_tcp(host_port: &HostPort) -> Result<TcpStream, Error> {
    let mut last_error = None;
    for addr in resolve(host_port).await? {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_error = Some(e),
        }
    }
    let error = last_error.expect("resolve returns at least one address");
    Err(error).context(TargetSnafu {
        action: "connect to",
        target: host_port.to_string(),
    })
}

/// Opens the named pipe `\\.\pipe\<name>` as a client that only writes, so a server that accepts inbound data only
/// still admits it.
#[cfg(windows)]
pub(crate) fn open_pipe(name: &str) -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    tokio::net::windows::named_pipe::ClientOptions::new()
        .read(false)
        .open(format!(r"\\.\pipe\{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolved_addresses_list_ipv4_before_ipv6() {
        let host_port = HostPort {
            host: "localhost".to_string(),
            port: 9,
        };
        let addrs = resolve(&host_port).await.unwrap();
        let first_ipv6 = addrs.iter().position(SocketAddr::is_ipv6).unwrap_or(addrs.len());
        assert!(addrs[first_ipv6..].iter().all(SocketAddr::is_ipv6), "{addrs:?}");
    }
}
