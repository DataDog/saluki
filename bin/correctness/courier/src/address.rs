use std::{fmt, path::PathBuf, str::FromStr};

use crate::{Error, InvalidAddressSnafu};

/// A host name or IP address, and a port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostPort {
    /// The host name or IP address, without brackets around an IPv6 address.
    pub host: String,

    /// The port.
    pub port: u16,
}

impl fmt::Display for HostPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl FromStr for HostPort {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (host, port) = s.rsplit_once(':').ok_or("expected <host>:<port>")?;
        let host = match host.strip_prefix('[') {
            Some(rest) => rest.strip_suffix(']').ok_or("an IPv6 host must be in brackets")?,
            None if host.contains(':') => return Err("an IPv6 host must be in brackets"),
            None => host,
        };
        if host.is_empty() {
            return Err("the host is empty");
        }
        if host.contains(['/', '?', '#']) {
            return Err("only <host>:<port> may follow the scheme");
        }
        let port = port.parse().map_err(|_| "the port is not a number from 0 to 65535")?;
        Ok(Self {
            host: host.to_string(),
            port,
        })
    }
}

/// Where a courier operation goes, written as a URL.
///
/// See the [crate documentation](crate#addresses) for each form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Address {
    /// `tcp://<host>:<port>`.
    Tcp(HostPort),

    /// `udp://<host>:<port>`.
    Udp(HostPort),

    /// `http://<host>:<port>`.
    Http(HostPort),

    /// `grpc://<host>:<port>`.
    Grpc(HostPort),

    /// `unix://<path>`: a Unix stream socket.
    UnixStream(PathBuf),

    /// `unixgram://<path>`: a Unix datagram socket.
    UnixDatagram(PathBuf),

    /// `pipe://<name>`: the Windows named pipe `\\.\pipe\<name>`.
    NamedPipe(String),
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(host_port) => write!(f, "tcp://{host_port}"),
            Self::Udp(host_port) => write!(f, "udp://{host_port}"),
            Self::Http(host_port) => write!(f, "http://{host_port}"),
            Self::Grpc(host_port) => write!(f, "grpc://{host_port}"),
            Self::UnixStream(path) => write!(f, "unix://{}", path.display()),
            Self::UnixDatagram(path) => write!(f, "unixgram://{}", path.display()),
            Self::NamedPipe(name) => write!(f, "pipe://{name}"),
        }
    }
}

impl FromStr for Address {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = |reason| InvalidAddressSnafu { address: s, reason }.build();
        let (scheme, rest) = s.split_once("://").ok_or_else(|| invalid("expected <scheme>://"))?;
        let host_port = || rest.parse::<HostPort>().map_err(invalid);
        let path = || {
            if rest.is_empty() {
                Err(invalid("the socket path is empty"))
            } else {
                Ok(PathBuf::from(rest))
            }
        };
        match scheme {
            "tcp" => Ok(Self::Tcp(host_port()?)),
            "udp" => Ok(Self::Udp(host_port()?)),
            "http" => Ok(Self::Http(host_port()?)),
            "grpc" => Ok(Self::Grpc(host_port()?)),
            "https" => Err(invalid("the courier does not speak TLS")),
            "unix" => Ok(Self::UnixStream(path()?)),
            "unixgram" => Ok(Self::UnixDatagram(path()?)),
            "pipe" if rest.is_empty() || rest.contains(['/', '\\']) => {
                Err(invalid("a pipe name is not empty and holds no '/' or '\\'"))
            }
            "pipe" => Ok(Self::NamedPipe(rest.to_string())),
            _ => Err(invalid(
                "the scheme is not one of tcp, udp, http, grpc, unix, unixgram or pipe",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fault;

    #[test]
    fn rejects_bad_addresses_as_a_harness_fault() {
        for text in [
            "adp:9126",
            "https://adp:443",
            "ftp://adp:21",
            "tcp://adp",
            "tcp://adp:65536",
            "tcp://:80",
            "tcp://::1:80",
            "http://adp:80/ready",
            "unix://",
            "pipe://a\\b",
        ] {
            let error = text.parse::<Address>().unwrap_err();
            assert!(matches!(error, Error::InvalidAddress { .. }), "{text}: {error}");
            assert_eq!(error.fault(), Fault::Harness, "{text}");
        }
    }
}
