use std::{fmt, net::SocketAddr, path::PathBuf};
use url::Url;

#[derive(Clone, Debug)]
pub enum ListenAddress {
    Tcp(SocketAddr),
    Udp(SocketAddr),
    #[cfg(unix)]
    Unixgram(PathBuf),
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

#[cfg(unix)]
#[derive(Clone)]
pub struct ProcessCredentials {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone)]
pub enum ConnectionAddress {
    SocketLike(SocketAddr),
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
