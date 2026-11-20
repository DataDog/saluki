//! Where a proxy forwards requests to, and the check that it doesn't forward them to itself.

use std::net::{IpAddr, SocketAddr};
#[cfg(unix)]
use std::path::PathBuf;

use http::{uri::Authority, Uri};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_io::net::{client::http::HttpClientBuilder, ListenAddress};

/// Where a proxy forwards requests to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Destination {
    /// A TCP host and port.
    Tcp {
        /// Host name or IP address, without brackets for IPv6.
        host: String,

        /// Port.
        port: u16,
    },

    /// A Unix domain socket path.
    #[cfg(unix)]
    Unix(PathBuf),
}

impl Destination {
    /// Creates a TCP destination from a `host:port` address, such as `127.0.0.1:4319` or `[::1]:4319`.
    ///
    /// # Errors
    ///
    /// If `address` isn't a host and a port, an error is returned.
    pub fn from_tcp_address(address: &str) -> Result<Self, GenericError> {
        if address.contains('@') {
            return Err(generic_error!(
                "Invalid address `{address}`: expected a host and a port, without credentials."
            ));
        }
        let authority = address
            .parse::<Authority>()
            .with_error_context(|| format!("Invalid address `{address}`: expected a host and a port."))?;
        let port = authority
            .port_u16()
            .ok_or_else(|| generic_error!("Invalid address `{address}`: expected a host and a port."))?;
        // An authority keeps an IPv6 host bracketed, which address lookups don't accept.
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        Ok(Self::Tcp { host, port })
    }

    /// Returns the URI with the scheme and authority of each request to this destination, and a root path.
    ///
    /// The client connects a Unix domain socket destination to its socket and ignores the authority, but a request URI
    /// still needs one, so it gets `localhost`.
    ///
    /// # Errors
    ///
    /// If the host and port don't make a valid authority, an error is returned.
    pub(crate) fn base_uri(&self) -> Result<Uri, http::Error> {
        match self {
            Self::Tcp { host, port } => {
                // An IPv6 host needs brackets in a URI.
                let authority = if host.contains(':') {
                    format!("[{host}]:{port}")
                } else {
                    format!("{host}:{port}")
                };
                Uri::builder()
                    .scheme("http")
                    .authority(authority)
                    .path_and_query("/")
                    .build()
            }
            #[cfg(unix)]
            Self::Unix(_) => Ok(Uri::from_static("http://localhost/")),
        }
    }

    /// Makes the client that `builder` builds connect to this destination.
    pub(crate) fn configure_client(&self, builder: HttpClientBuilder) -> HttpClientBuilder {
        match self {
            // The client connects to the host and port of each request URI.
            Self::Tcp { .. } => builder,
            #[cfg(unix)]
            Self::Unix(path) => builder.with_unix_socket_path(path.clone()),
        }
    }
}

/// Returns the listener in `listeners` that `destination` reaches, if any.
///
/// A TCP destination reaches a TCP listener, and a Unix domain socket destination reaches a Unix domain socket listener
/// on the same path. This catches the obvious misconfigurations, such as the same port used for both, not every route
/// back to the proxy: a listener on every interface is also reached through the host's other addresses, for example.
pub async fn listener_reached_by<'a, I>(destination: &Destination, listeners: I) -> Option<&'a ListenAddress>
where
    I: IntoIterator<Item = &'a ListenAddress>,
{
    match destination {
        Destination::Tcp { host, port } => {
            let tcp_listeners = listeners
                .into_iter()
                .filter_map(|endpoint| match endpoint {
                    ListenAddress::Tcp(listener) => Some((endpoint, *listener)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if tcp_listeners.is_empty() {
                return None;
            }

            // A destination that doesn't resolve yet can't be told apart from the listener, so it is let through.
            let destinations = tokio::net::lookup_host((host.as_str(), *port))
                .await
                .ok()?
                .collect::<Vec<_>>();
            tcp_listeners
                .into_iter()
                .find(|(_, listener)| {
                    destinations
                        .iter()
                        .any(|destination| tcp_reaches(*destination, *listener))
                })
                .map(|(endpoint, _)| endpoint)
        }
        #[cfg(unix)]
        Destination::Unix(path) => listeners
            .into_iter()
            .find(|endpoint| matches!(endpoint, ListenAddress::Unix(listener) if listener == path)),
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
    fn base_uri_brackets_an_ipv6_host() {
        let uri = |host: &str| {
            Destination::Tcp {
                host: host.to_string(),
                port: 8126,
            }
            .base_uri()
            .unwrap()
            .to_string()
        };

        assert_eq!(uri("127.0.0.1"), "http://127.0.0.1:8126/");
        assert_eq!(uri("localhost"), "http://localhost:8126/");
        assert_eq!(uri("::1"), "http://[::1]:8126/");
    }

    #[test]
    fn from_tcp_address_splits_the_host_and_port() {
        let tcp = |host: &str, port| Destination::Tcp {
            host: host.to_string(),
            port,
        };

        assert_eq!(
            Destination::from_tcp_address("127.0.0.1:4319").unwrap(),
            tcp("127.0.0.1", 4319)
        );
        assert_eq!(
            Destination::from_tcp_address("localhost:4320").unwrap(),
            tcp("localhost", 4320)
        );
        assert_eq!(Destination::from_tcp_address("[::1]:4319").unwrap(), tcp("::1", 4319));

        for address in [
            "127.0.0.1",
            "localhost:",
            "http://127.0.0.1:4319",
            "user@127.0.0.1:4319",
            "",
        ] {
            assert!(Destination::from_tcp_address(address).is_err(), "{address:?}");
        }
    }

    #[tokio::test]
    async fn listener_reached_by_matches_a_listener_of_the_same_transport() {
        let tcp = |host: &str, port| Destination::Tcp {
            host: host.to_string(),
            port,
        };
        let listen = |address: &str| ListenAddress::try_from(address).unwrap();
        let wildcard = listen("tcp://0.0.0.0:4319");
        let loopback = listen("tcp://127.0.0.1:4320");
        let socket = listen("unix:///tmp/otlp.sock");
        let listeners = [&wildcard, &loopback, &socket];

        assert_eq!(
            listener_reached_by(&tcp("127.0.0.1", 4319), listeners).await,
            Some(&wildcard)
        );
        assert_eq!(
            listener_reached_by(&tcp("localhost", 4320), listeners).await,
            Some(&loopback)
        );
        assert_eq!(listener_reached_by(&tcp("127.0.0.1", 4321), listeners).await, None);
        // A TCP destination never reaches a Unix domain socket listener, wherever it is in the list.
        assert_eq!(listener_reached_by(&tcp("127.0.0.1", 4320), [&socket]).await, None);
        assert_eq!(
            listener_reached_by(&tcp("127.0.0.1", 4320), [&socket, &loopback]).await,
            Some(&loopback)
        );

        #[cfg(unix)]
        {
            let unix = |path: &str| Destination::Unix(PathBuf::from(path));
            assert_eq!(
                listener_reached_by(&unix("/tmp/otlp.sock"), listeners).await,
                Some(&socket)
            );
            assert_eq!(listener_reached_by(&unix("/tmp/other.sock"), listeners).await, None);
        }
    }

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
