//! Where a proxy forwards requests to, and the check that it doesn't forward them to itself.

use std::net::{IpAddr, SocketAddr};
#[cfg(unix)]
use std::path::PathBuf;

use saluki_io::net::ListenAddress;

/// Where a proxy forwards requests to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Destination {
    /// A TCP host and port.
    Tcp { host: String, port: u16 },

    /// A Unix domain socket path.
    #[cfg(unix)]
    Unix(PathBuf),
}

/// Returns the listener of the proxy that `destination` reaches, if any.
///
/// This catches the obvious misconfigurations, such as the same port used for both, not every route back to the proxy:
/// a listener on every interface is also reached through the host's other addresses, for example.
pub(crate) async fn listener_reached_by<'a>(
    destination: &Destination, tcp_endpoint: Option<&'a ListenAddress>, uds_endpoint: Option<&'a ListenAddress>,
) -> Option<&'a ListenAddress> {
    // Only Unix domain socket destinations can reach the Unix domain socket listener.
    #[cfg(not(unix))]
    let _ = uds_endpoint;

    match destination {
        Destination::Tcp { host, port } => {
            let endpoint = tcp_endpoint?;
            let ListenAddress::Tcp(listener) = endpoint else {
                return None;
            };
            // A destination that doesn't resolve yet can't be told apart from the listener, so it is let through.
            let mut destinations = tokio::net::lookup_host((host.as_str(), *port)).await.ok()?;
            destinations
                .any(|destination| tcp_reaches(destination, *listener))
                .then_some(endpoint)
        }
        #[cfg(unix)]
        Destination::Unix(path) => {
            uds_endpoint.filter(|endpoint| matches!(endpoint, ListenAddress::Unix(listener) if listener == path))
        }
    }
}

/// Returns whether connecting to `destination` reaches a TCP listener bound to `listener`.
///
/// A listener on the unspecified address accepts connections to any local address, and connecting to the unspecified
/// address connects to the local host, so either side being unspecified matches a loopback address of the same family.
fn tcp_reaches(destination: SocketAddr, listener: SocketAddr) -> bool {
    let is_local = |ip: IpAddr| ip.is_loopback() || ip.is_unspecified();
    let (destination_ip, listener_ip) = (destination.ip(), listener.ip());

    destination.port() == listener.port()
        && (destination_ip == listener_ip
            || ((destination_ip.is_unspecified() || listener_ip.is_unspecified())
                && destination_ip.is_ipv4() == listener_ip.is_ipv4()
                && is_local(destination_ip)
                && is_local(listener_ip)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_reaches_only_a_listener_on_the_same_port_and_address() {
        let addr = |s: &str| s.parse::<SocketAddr>().unwrap();

        assert!(tcp_reaches(addr("127.0.0.1:8126"), addr("127.0.0.1:8126")));
        assert!(tcp_reaches(addr("127.0.0.1:8126"), addr("0.0.0.0:8126")));
        assert!(tcp_reaches(addr("0.0.0.0:8126"), addr("127.0.0.1:8126")));
        assert!(tcp_reaches(addr("[::1]:8126"), addr("[::]:8126")));

        assert!(!tcp_reaches(addr("127.0.0.1:8127"), addr("127.0.0.1:8126")));
        assert!(!tcp_reaches(addr("10.0.0.2:8126"), addr("10.0.0.1:8126")));
        assert!(!tcp_reaches(addr("[::1]:8126"), addr("0.0.0.0:8126")));
        assert!(!tcp_reaches(addr("127.0.0.1:8126"), addr("10.0.0.1:8126")));
    }
}
