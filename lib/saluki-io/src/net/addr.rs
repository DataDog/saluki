use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
};

use axum::extract::connect_info::Connected;
use serde::{Deserialize, Serialize};
use url::Url;

use super::Connection;

/// A listen address.
///
/// Listen addresses are used to bind listeners to specific local addresses and ports, and multiple address families and
/// protocols are supported. In textual form, listen addresses are represented as URLs, with the scheme indicating the
/// protocol and the authority/path representing the address to listen on.
///
/// # Examples
///
/// - `tcp://127.0.0.1:6789` (listen on IPv4 loopback, TCP port 6789)
/// - `udp://[::1]:53` (listen on IPv6 loopback, UDP port 53)
/// - `unixgram:///tmp/app.socket` (listen on a Unix datagram socket at `/tmp/app.socket`)
/// - `unix:///tmp/app.socket` (listen on a Unix stream socket at `/tmp/app.socket`)
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "String")]
pub enum ListenAddress {
    /// A TCP listen address.
    Tcp(SocketAddr),

    /// A UDP listen address.
    Udp(SocketAddr),

    /// A Unix datagram listen address.
    Unixgram(PathBuf),

    /// A Unix stream listen address.
    Unix(PathBuf),
}

impl ListenAddress {
    /// Creates a TCP address for the given port that listens on all interfaces.
    pub const fn any_tcp(port: u16) -> Self {
        Self::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)))
    }

    /// Returns the socket type of the listen address.
    pub const fn listener_type(&self) -> &'static str {
        match self {
            Self::Tcp(_) => "tcp",
            Self::Udp(_) => "udp",
            Self::Unixgram(_) => "unixgram",
            Self::Unix(_) => "unix",
        }
    }

    /// Returns a socket address that can be used to connect to the configured listen address with a bias for local
    /// clients.
    ///
    /// When the listen address is a TCP or UDP address, this method returns a socket address that can be used to
    /// connect to the listener bound to this listen address, such that if the listen address is unspecified
    /// (`0.0.0.0`), the client will connect locally using `localhost`. When the listen address isn't unspecified or
    /// already uses `localhost`, this method returns the listen address as-is.
    ///
    /// If the address is a Unix domain socket, this method returns `None`.
    pub fn as_local_connect_addr(&self) -> Option<SocketAddr> {
        match self {
            Self::Tcp(addr) | Self::Udp(addr) => {
                let mut connect_addr = *addr;
                if connect_addr.ip().is_unspecified() {
                    let localhost_ip = match connect_addr.is_ipv4() {
                        true => IpAddr::V4(Ipv4Addr::LOCALHOST),
                        false => IpAddr::V6(Ipv6Addr::LOCALHOST),
                    };

                    connect_addr.set_ip(localhost_ip);
                }

                Some(connect_addr)
            }
            // TODO: why did i do this? it's totally possible to connect to a unix domain socket locally...
            // in fact, it's kind of the only way to connect to a unix domain socket :thonk:
            Self::Unixgram(_) => None,
            Self::Unix(_) => None,
        }
    }

    /// Returns the Unix domain socket path if the address is a Unix domain socket in SOCK_STREAM mode.
    ///
    /// Returns `None` otherwise.
    pub fn as_unix_stream_path(&self) -> Option<&Path> {
        match self {
            Self::Unix(path) => Some(path),
            _ => None,
        }
    }
}

impl fmt::Display for ListenAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(addr) => write!(f, "tcp://{}", addr),
            Self::Udp(addr) => write!(f, "udp://{}", addr),
            Self::Unixgram(path) => write!(f, "unixgram://{}", path.display()),
            Self::Unix(path) => write!(f, "unix://{}", path.display()),
        }
    }
}

impl TryFrom<String> for ListenAddress {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

impl<'a> TryFrom<&'a str> for ListenAddress {
    type Error = String;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        let url = match Url::parse(value) {
            Ok(url) => url,
            Err(e) => match e {
                url::ParseError::RelativeUrlWithoutBase => {
                    Url::parse(&format!("unixgram://{}", value)).map_err(|e| e.to_string())?
                }
                _ => return Err(e.to_string()),
            },
        };

        match url.scheme() {
            "tcp" => {
                let mut socket_addresses = url.socket_addrs(|| None).map_err(|e| e.to_string())?;
                if socket_addresses.is_empty() {
                    Err("listen address must resolve to at least one valid IP address/port pair".to_string())
                } else {
                    Ok(Self::Tcp(socket_addresses.swap_remove(0)))
                }
            }
            "udp" => {
                let mut socket_addresses = url.socket_addrs(|| None).map_err(|e| e.to_string())?;
                if socket_addresses.is_empty() {
                    Err("listen address must resolve to at least one valid IP address/port pair".to_string())
                } else {
                    Ok(Self::Udp(socket_addresses.swap_remove(0)))
                }
            }
            "unixgram" => {
                let path = url.path();
                if path.is_empty() {
                    return Err("socket path cannot be empty".to_string());
                }

                let path_buf = PathBuf::from(path);
                if !path_buf.is_absolute() {
                    return Err("socket path must be absolute".to_string());
                }

                Ok(Self::Unixgram(path_buf))
            }
            "unix" => {
                let path = url.path();
                if path.is_empty() {
                    return Err("socket path cannot be empty".to_string());
                }

                let path_buf = PathBuf::from(path);
                if !path_buf.is_absolute() {
                    return Err("socket path must be absolute".to_string());
                }

                Ok(Self::Unix(path_buf))
            }
            scheme => Err(format!("unknown/unsupported address scheme '{}'", scheme)),
        }
    }
}

/// Process credentials for a Unix domain socket connection.
///
/// When dealing with Unix domain sockets, they can be configured such that the "process credentials" of the remote peer
/// are sent as part of each received message. These "credentials" are the process ID of the remote peer, and the user
/// ID and group ID that the process is running as.
///
/// In some cases, this information can be useful for identifying the remote peer and enriching the received data in an
/// automatic way.
#[derive(Clone)]
pub struct ProcessCredentials {
    /// Process ID of the remote peer.
    pub pid: i32,

    /// User ID of the remote peer process.
    pub uid: u32,

    /// Group ID of the remote peer process.
    pub gid: u32,
}

/// Reason UDS process credential detection failed.
#[derive(Clone, Copy)]
pub enum ProcessCredentialsError {
    /// Ancillary data was present but didn't contain usable process credentials.
    InvalidCredentials,

    /// Process credentials were present, but the PID was zero.
    ZeroPid,

    /// UDS process credential detection isn't supported on this platform.
    UnsupportedPlatform,
}

impl ProcessCredentialsError {
    /// Returns a concise identifier for the failure reason.
    pub const fn identifier(&self) -> &'static str {
        match self {
            Self::InvalidCredentials => "invalid-credentials",
            Self::ZeroPid => "zero-pid",
            Self::UnsupportedPlatform => "unsupported-platform",
        }
    }
}

impl fmt::Display for ProcessCredentialsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCredentials => write!(f, "invalid process credentials"),
            Self::ZeroPid => write!(f, "process credential PID is zero"),
            Self::UnsupportedPlatform => write!(f, "process credentials are unsupported on this platform"),
        }
    }
}

/// Process identity associated with a Unix domain socket peer.
#[derive(Clone)]
pub enum ProcessIdentity {
    /// Process credentials were detected.
    Credentials(ProcessCredentials),

    /// Process credential detection failed.
    Error(ProcessCredentialsError),

    /// Process identity isn't available for this peer.
    Unavailable,
}

impl ProcessIdentity {
    /// Returns process credentials, if they were detected.
    pub fn credentials(&self) -> Option<&ProcessCredentials> {
        match self {
            Self::Credentials(creds) => Some(creds),
            Self::Error(_) | Self::Unavailable => None,
        }
    }

    /// Returns `true` if process credential detection failed.
    pub const fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

/// Connection address.
///
/// A generic representation of the address of a remote peer. This can either be a typical socket address (used for
/// IPv4/IPv6), or potentially the process credentials of a Unix domain socket connection.
#[derive(Clone)]
pub enum ConnectionAddress {
    /// A socket-like address.
    SocketLike(SocketAddr),

    /// A process-like address.
    ProcessLike(ProcessIdentity),
}

impl fmt::Display for ConnectionAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SocketLike(addr) => write!(f, "{}", addr),
            Self::ProcessLike(identity) => match identity {
                ProcessIdentity::Credentials(creds) => {
                    write!(f, "<pid={} uid={} gid={}>", creds.pid, creds.uid, creds.gid)
                }
                ProcessIdentity::Error(error) => write!(f, "<origin-detection-error: {}>", error.identifier()),
                ProcessIdentity::Unavailable => write!(f, "<no-origin>"),
            },
        }
    }
}

impl ConnectionAddress {
    /// Returns process credentials for a Unix domain socket peer, if available.
    pub fn process_credentials(&self) -> Option<&ProcessCredentials> {
        match self {
            Self::ProcessLike(identity) => identity.credentials(),
            Self::SocketLike(_) => None,
        }
    }

    /// Returns `true` if Unix domain socket process credential detection failed.
    pub const fn has_process_credential_error(&self) -> bool {
        match self {
            Self::ProcessLike(identity) => identity.is_error(),
            Self::SocketLike(_) => false,
        }
    }
}

impl From<SocketAddr> for ConnectionAddress {
    fn from(value: SocketAddr) -> Self {
        Self::SocketLike(value)
    }
}

impl From<ProcessCredentials> for ConnectionAddress {
    fn from(creds: ProcessCredentials) -> Self {
        Self::ProcessLike(ProcessIdentity::Credentials(creds))
    }
}

impl<'a> Connected<&'a Connection> for ConnectionAddress {
    fn connect_info(target: &'a Connection) -> Self {
        target.remote_addr()
    }
}

/// A gRPC target address.
///
/// This represents the address of a gRPC server that can be connected to. `GrpcTargetAddress` exposes a `Display`
/// implementation that emits the target address following the rules of the [gRPC Name
/// Resolution][grpc_name_resolution_docs] documentation.
///
/// Only connection-oriented transports are supported: TCP and Unix domain sockets in SOCK_STREAM mode.
///
/// [grpc_name_resolution_docs]: https://github.com/grpc/grpc/blob/master/doc/naming.md
pub enum GrpcTargetAddress {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl GrpcTargetAddress {
    /// Creates a new `GrpcTargetAddress` from the given `ListenAddress`.
    ///
    /// For TCP addresses, this method converts unspecified addresses (`0.0.0.0` or `::`) to localhost
    /// (`127.0.0.1` or `::1`) to ensure the advertised address matches TLS certificates.
    ///
    /// Returns `None` if the listen address isn't a connection-oriented transport.
    pub fn try_from_listen_addr(listen_address: &ListenAddress) -> Option<Self> {
        match listen_address {
            ListenAddress::Tcp(_) => {
                // For TCP, convert 0.0.0.0 to 127.0.0.1 to match TLS certificate
                listen_address.as_local_connect_addr().map(GrpcTargetAddress::Tcp)
            }
            ListenAddress::Unix(path) => Some(GrpcTargetAddress::Unix(path.clone())),
            _ => None,
        }
    }
}

impl fmt::Display for GrpcTargetAddress {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            GrpcTargetAddress::Tcp(addr) => write!(f, "{}", addr),
            GrpcTargetAddress::Unix(path) => write!(f, "unix://{}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_as_local_connect_addr() {
        let tcp_any_addr = ListenAddress::try_from("tcp://0.0.0.0:1234").unwrap();
        assert_eq!(
            tcp_any_addr.as_local_connect_addr(),
            Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234)))
        );

        let tcp_localhost_addr = ListenAddress::try_from("tcp://127.0.0.1:2345").unwrap();
        assert_eq!(
            tcp_localhost_addr.as_local_connect_addr(),
            Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 2345)))
        );

        let tcp_private_addr = ListenAddress::try_from("tcp://192.168.10.2:3456").unwrap();
        assert_eq!(
            tcp_private_addr.as_local_connect_addr(),
            Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 10, 2), 3456)))
        );

        let udp_any_addr = ListenAddress::try_from("udp://0.0.0.0:4567").unwrap();
        assert_eq!(
            udp_any_addr.as_local_connect_addr(),
            Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 4567)))
        );

        let udp_localhost_addr = ListenAddress::try_from("udp://127.0.0.1:5678").unwrap();
        assert_eq!(
            udp_localhost_addr.as_local_connect_addr(),
            Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5678)))
        );

        let udp_private_addr = ListenAddress::try_from("udp://192.168.10.2:6789").unwrap();
        assert_eq!(
            udp_private_addr.as_local_connect_addr(),
            Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 10, 2), 6789)))
        );
    }
}
