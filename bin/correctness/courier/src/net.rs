use std::{io, net::SocketAddr};

use panoramic_courier::{Report, Target};
use tokio::net::{lookup_host, TcpSocket, TcpStream, UdpSocket};

// Separate socket creation keeps local failures distinct from resolution and connection failures.
pub(super) async fn connect_tcp(host: &str, port: u16, to: &Target) -> Result<TcpStream, Report> {
    let addresses = lookup_host((host, port)).await.map_err(|error| Report::Unreachable {
        reason: format!("{to}: {error}"),
    })?;
    let sockets = addresses.map(|address| {
        let socket = if address.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        };
        (address, socket)
    });
    connect_tcp_addresses(sockets, to).await
}

async fn connect_tcp_addresses(
    sockets: impl IntoIterator<Item = (SocketAddr, io::Result<TcpSocket>)>, to: &Target,
) -> Result<TcpStream, Report> {
    let mut local_failure = None;
    let mut connect_failure = None;
    for (address, socket) in sockets {
        let socket = match socket {
            Ok(socket) => socket,
            Err(error) => {
                local_failure = Some(error);
                continue;
            }
        };
        match socket.connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => connect_failure = Some(error),
        }
    }
    Err(match (connect_failure, local_failure) {
        (Some(error), _) => Report::Unreachable {
            reason: format!("{to}: {error}"),
        },
        (None, Some(error)) => Report::Failed {
            reason: format!("{to}: {error}"),
        },
        (None, None) => Report::Unreachable {
            reason: format!("{to}: host resolved to no addresses"),
        },
    })
}

// Separate socket creation keeps local failures distinct from connection failures.
#[cfg(unix)]
pub(super) async fn connect_unix(path: &str, to: &Target) -> Result<tokio::net::UnixStream, Report> {
    let socket = tokio::net::UnixSocket::new_stream().map_err(|error| Report::Failed {
        reason: format!("{to}: {error}"),
    })?;
    socket.connect(path).await.map_err(|error| Report::Unreachable {
        reason: format!("{to}: {error}"),
    })
}

// Connecting first separates a missing or stale socket from a datagram the kernel refuses.
#[cfg(unix)]
pub(super) fn connect_unix_datagram(path: &str, to: &Target) -> Result<tokio::net::UnixDatagram, Report> {
    let socket = tokio::net::UnixDatagram::unbound().map_err(|error| Report::Failed {
        reason: format!("{to}: {error}"),
    })?;
    socket.connect(path).map_err(|error| Report::Unreachable {
        reason: format!("{to}: {error}"),
    })?;
    Ok(socket)
}

#[cfg(not(unix))]
pub(super) const UNSUPPORTED_UNIX_REASON: &str = "Unix sockets are not supported on this platform";

// UDP uses the first resolved address: it has no handshake to test the address family.
// Use an IP literal when a name resolves to both IPv4 and IPv6.
pub(super) async fn bind_udp(host: &str, port: u16, to: &Target) -> Result<(UdpSocket, SocketAddr), Report> {
    let address = lookup_host((host, port))
        .await
        .map_err(|error| Report::Unreachable {
            reason: format!("{to}: {error}"),
        })?
        .next()
        .ok_or_else(|| Report::Unreachable {
            reason: format!("{to}: host resolved to no addresses"),
        })?;
    let bind = if address.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let socket = UdpSocket::bind(bind).await.map_err(|error| Report::Failed {
        reason: format!("{to}: {error}"),
    })?;
    Ok((socket, address))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn tcp_socket_creation_failure_does_not_hide_a_reachable_address() {
        // A kernel without IPv6 must still reach the IPv4 address returned for the same host.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let to = Target::parse(&format!("tcp://localhost:{}", address.port())).unwrap();
        let sockets = [
            (
                SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], address.port())),
                Err(io::Error::new(io::ErrorKind::Unsupported, "IPv6 unavailable")),
            ),
            (address, TcpSocket::new_v4()),
        ];
        let stream = tokio::time::timeout(Duration::from_secs(5), connect_tcp_addresses(sockets, &to))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), address);
    }

    #[tokio::test]
    async fn tcp_reports_local_failure_only_when_every_socket_creation_fails() {
        let to = Target::parse("tcp://localhost:8125").unwrap();
        let sockets = [
            (
                "[::1]:8125".parse().unwrap(),
                Err(io::Error::new(io::ErrorKind::Unsupported, "IPv6 unavailable")),
            ),
            (
                "127.0.0.1:8125".parse().unwrap(),
                Err(io::Error::other("socket limit reached")),
            ),
        ];
        assert_eq!(
            connect_tcp_addresses(sockets, &to).await.unwrap_err(),
            Report::Failed {
                reason: format!("{to}: socket limit reached")
            }
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let to = Target::parse(&format!("tcp://localhost:{}", address.port())).unwrap();
        for local_first in [true, false] {
            let mut sockets = [
                (address, Err(io::Error::other("local socket failure"))),
                (address, TcpSocket::new_v4()),
            ];
            if !local_first {
                sockets.reverse();
            }
            let report = tokio::time::timeout(Duration::from_secs(5), connect_tcp_addresses(sockets, &to))
                .await
                .unwrap()
                .unwrap_err();
            match report {
                Report::Unreachable { reason } => {
                    assert!(reason.starts_with(&format!("{to}: ")));
                    assert!(!reason.contains("local socket failure"));
                }
                report => panic!("expected an unreachable report, got {report:?}"),
            }
        }
    }

    #[tokio::test]
    async fn tcp_without_resolved_addresses_is_unreachable() {
        let to = Target::parse("tcp://h:1").unwrap();
        assert_eq!(
            connect_tcp_addresses([], &to).await.unwrap_err(),
            Report::Unreachable {
                reason: format!("{to}: host resolved to no addresses")
            }
        );
    }
}
