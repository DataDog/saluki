use std::{fmt, net::SocketAddr, path::PathBuf};
use tokio::net::unix::SocketAddr as UnixSocketAddr;
use url::Url;

#[derive(Clone)]
pub enum ListenAddress {
    Tcp(SocketAddr),
    Udp(SocketAddr),
    Unix(PathBuf),
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
        let url = Url::parse(value).map_err(|e| e.to_string())?;

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

#[derive(Clone)]
pub enum ConnectionAddress {
    SocketLike(SocketAddr),
    PathLike(PathBuf),
}

impl fmt::Display for ConnectionAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SocketLike(addr) => write!(f, "{}", addr),
            Self::PathLike(path) => write!(f, "{}", path.display()),
        }
    }
}

impl From<SocketAddr> for ConnectionAddress {
    fn from(value: SocketAddr) -> Self {
        Self::SocketLike(value)
    }
}

impl From<UnixSocketAddr> for ConnectionAddress {
    fn from(value: UnixSocketAddr) -> Self {
        Self::PathLike(value.as_pathname().map(|p| p.to_path_buf()).unwrap_or_default())
    }
}
