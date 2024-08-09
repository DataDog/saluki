use std::{fmt, net::SocketAddr, path::PathBuf};

use serde::Deserialize;
use url::Url;

/// A listen address.
///
/// Listen addresses are used to bind listeners to specific local addresses and ports, and multiple address families and
/// protocols are supported. In textual form, listen addresses are represented as URLs, with the scheme indicating the
/// protocol and the authority/path representing the address to listen on.
///
/// ## Examples
///
/// - `tcp://127.0.0.1:6789` (listen on IPv4 loopback, TCP port 6789)
/// - `udp://[::1]:53` (listen on IPv6 loopback, UDP port 53)
/// - `unixgram:///tmp/app.socket` (listen on a Unix datagram socket at `/tmp/app.socket`)
/// - `unix:///tmp/app.socket` (listen on a Unix stream socket at `/tmp/app.socket`)
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "String")]
pub enum ListenAddress {
    /// A TCP listen address.
    Tcp(SocketAddr),

    /// A UDP listen address.
    Udp(SocketAddr),

    /// A Unix datagram listen address.
    #[cfg(unix)]
    Unixgram(PathBuf),

    /// A Unix stream listen address.
    #[cfg(unix)]
    Unix(PathBuf),
}

impl fmt::Display for ListenAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(addr) => write!(f, "tcp://{}", addr),
            Self::Udp(addr) => write!(f, "udp://{}", addr),
            #[cfg(unix)]
            Self::Unixgram(path) => write!(f, "unixgram://{}", path.display()),
            #[cfg(unix)]
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
            #[cfg(unix)]
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
            #[cfg(unix)]
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
#[cfg(unix)]
#[derive(Clone)]
pub struct ProcessCredentials {
    /// Process ID of the remote peer.
    pub pid: i32,

    /// User ID of the remote peer process.
    pub uid: u32,

    /// Group ID of the remote peer process.
    pub gid: u32,
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
    #[cfg(unix)]
    ProcessLike(Option<ProcessCredentials>),
}

impl fmt::Display for ConnectionAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SocketLike(addr) => write!(f, "{}", addr),
            #[cfg(unix)]
            Self::ProcessLike(maybe_creds) => match maybe_creds {
                None => write!(f, "<unbound>"),
                Some(creds) => write!(f, "<pid={} uid={} gid={}>", creds.pid, creds.uid, creds.gid),
            },
        }
    }
}

impl From<SocketAddr> for ConnectionAddress {
    fn from(value: SocketAddr) -> Self {
        Self::SocketLike(value)
    }
}

#[cfg(unix)]
impl From<ProcessCredentials> for ConnectionAddress {
    fn from(creds: ProcessCredentials) -> Self {
        Self::ProcessLike(Some(creds))
    }
}
