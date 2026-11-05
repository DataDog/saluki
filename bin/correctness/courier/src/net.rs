use std::{fmt, io, net::SocketAddr};

use tokio::net::{lookup_host, TcpSocket, TcpStream, UdpSocket};

use crate::{Report, Target};

// Separate socket creation keeps local failures distinct from resolution and connection failures.
pub(crate) async fn connect_tcp(host: &str, port: u16, to: &Target) -> Result<TcpStream, Report> {
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
pub(crate) async fn connect_unix(path: &str, to: &Target) -> Result<tokio::net::UnixStream, Report> {
    let socket = tokio::net::UnixSocket::new_stream().map_err(|error| Report::Failed {
        reason: format!("{to}: {error}"),
    })?;
    socket.connect(path).await.map_err(|error| Report::Unreachable {
        reason: format!("{to}: {error}"),
    })
}

// Connecting first separates a missing or stale socket from a datagram the kernel refuses.
#[cfg(unix)]
pub(crate) fn connect_unix_datagram(path: &str, to: &Target) -> Result<tokio::net::UnixDatagram, Report> {
    let socket = tokio::net::UnixDatagram::unbound().map_err(|error| Report::Failed {
        reason: format!("{to}: {error}"),
    })?;
    socket.connect(path).map_err(|error| Report::Unreachable {
        reason: format!("{to}: {error}"),
    })?;
    Ok(socket)
}

#[cfg(windows)]
pub(crate) async fn connect_named_pipe(
    name: &str, to: &Target,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, Report> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let path = format!(r"\\.\pipe\{name}");
    loop {
        match ClientOptions::new().read(false).open(&path) {
            Ok(pipe) => return Ok(pipe),
            Err(error) if error.raw_os_error() == Some(231) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(Report::Unreachable {
                    reason: format!("{to}: {error}"),
                })
            }
        }
    }
}

#[cfg(not(windows))]
pub(crate) const UNSUPPORTED_PIPE_REASON: &str = "named pipes are supported only on Windows";

#[cfg(not(unix))]
pub(crate) const UNSUPPORTED_UNIX_REASON: &str = "Unix sockets are not supported on this platform";

pub(crate) struct UdpDestination {
    resolved: Vec<SocketAddr>,
    pub(crate) selected: SocketAddr,
}

impl UdpDestination {
    fn select(resolved: Vec<SocketAddr>, to: &Target) -> Result<Self, Report> {
        // A datagram to an unbound address is silently lost. Prefer IPv4 for the IPv4-wildcard listeners we target.
        let selected = resolved
            .iter()
            .find(|address| address.is_ipv4())
            .or_else(|| resolved.first())
            .copied()
            .ok_or_else(|| Report::Unreachable {
                reason: format!("{to}: host resolved to no addresses"),
            })?;
        Ok(Self { resolved, selected })
    }
}

impl fmt::Display for UdpDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "resolved addresses {:?}; selected {}", self.resolved, self.selected)
    }
}

pub(crate) async fn bind_udp(host: &str, port: u16, to: &Target) -> Result<(UdpSocket, UdpDestination), Report> {
    let resolved = lookup_host((host, port)).await.map_err(|error| Report::Unreachable {
        reason: format!("{to}: {error}"),
    })?;
    let destination = UdpDestination::select(resolved.collect(), to)?;
    let bind = if destination.selected.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).await.map_err(|error| Report::Failed {
        reason: format!("{to}: {destination}; bind failed: {error}"),
    })?;
    Ok((socket, destination))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn udp_prefers_the_first_ipv4_address_and_retains_every_resolution_result() {
        // An IPv6-first hostname result silently misses a listener bound only to an IPv4 wildcard.
        let to = Target::parse("udp://listener:8125").unwrap();
        for (resolved, selected) in [
            (vec!["[::1]:8125", "127.0.0.2:8125", "127.0.0.1:8125"], "127.0.0.2:8125"),
            (vec!["127.0.0.1:8125", "[::1]:8125", "127.0.0.2:8125"], "127.0.0.1:8125"),
            (vec!["[::1]:8125", "[::2]:8125"], "[::1]:8125"),
            (vec!["127.0.0.1:8125"], "127.0.0.1:8125"),
            (vec!["[::1]:8125"], "[::1]:8125"),
        ] {
            let expected = format!("resolved addresses [{}]; selected {selected}", resolved.join(", "));
            let resolved = resolved.iter().map(|address| address.parse().unwrap()).collect();
            let destination = UdpDestination::select(resolved, &to).unwrap();
            assert_eq!(destination.selected, selected.parse::<SocketAddr>().unwrap());
            assert_eq!(destination.to_string(), expected);
        }
        assert!(matches!(
            UdpDestination::select(vec![], &to),
            Err(Report::Unreachable { reason }) if reason == "udp://listener:8125: host resolved to no addresses"
        ));
    }

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
