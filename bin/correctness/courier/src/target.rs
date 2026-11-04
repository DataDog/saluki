use std::{fmt, net::Ipv6Addr};

use crate::UsageError;

/// An endpoint as seen from inside the container running the courier.
///
/// Directly constructed values must follow [`Target::parse`]'s rules: a nonempty hostname or IP literal
/// with IPv6 brackets omitted, a port in `1..=65535`, a socket path starting with `/`, and a nonempty
/// pipe name without separators or NUL.
/// Use [`Target::parse`] to validate a target URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    /// A TCP endpoint.
    Tcp {
        /// Hostname or IP literal, without IPv6 brackets.
        host: String,
        /// Destination port, from 1 through 65535.
        port: u16,
    },
    /// A UDP endpoint.
    Udp {
        /// Hostname or IP literal, without IPv6 brackets.
        host: String,
        /// Destination port, from 1 through 65535.
        port: u16,
    },
    /// A Windows named pipe.
    NamedPipe {
        /// Name below the local pipe namespace, without separators or NUL.
        name: String,
    },
    /// A Unix stream socket.
    UnixStream {
        /// Absolute path in the container's filesystem.
        path: String,
    },
    /// A Unix datagram socket.
    UnixDatagram {
        /// Absolute path in the container's filesystem.
        path: String,
    },
}

impl Target {
    /// Parses a `tcp://`, `udp://`, `unix://`, `unixgram://` or `npipe://` URL.
    ///
    /// Socket paths are preserved verbatim, independent of the caller's operating system.
    ///
    /// # Errors
    ///
    /// Returns a usage error naming the input if its scheme, host, port, socket path or pipe name is invalid.
    pub fn parse(input: &str) -> Result<Self, UsageError> {
        let invalid = |problem| UsageError {
            reason: format!("invalid target {input:?}: {problem}"),
        };
        let (scheme, address) = input
            .split_once("://")
            .ok_or_else(|| invalid("expected tcp://, udp://, unix://, unixgram:// or npipe://"))?;
        match scheme {
            "npipe" => {
                if address.is_empty() || address.contains(['\\', '/', '\0']) {
                    return Err(invalid("pipe name must be nonempty with no separators or NUL"));
                }
                Ok(Self::NamedPipe {
                    name: address.to_owned(),
                })
            }
            "unix" | "unixgram" => {
                if !address.starts_with('/') {
                    return Err(invalid("socket path must be absolute"));
                }
                let path = address.to_owned();
                Ok(if scheme == "unix" {
                    Self::UnixStream { path }
                } else {
                    Self::UnixDatagram { path }
                })
            }
            "tcp" | "udp" => {
                let (host, port) = if let Some(address) = address.strip_prefix('[') {
                    let (host, rest) = address
                        .split_once(']')
                        .ok_or_else(|| invalid("IPv6 host needs a closing bracket"))?;
                    host.parse::<Ipv6Addr>()
                        .map_err(|_| invalid("bracketed host must be an IPv6 literal"))?;
                    let port = rest.strip_prefix(':').ok_or_else(|| invalid("missing port"))?;
                    (host, port)
                } else {
                    let (host, port) = address.rsplit_once(':').ok_or_else(|| invalid("missing port"))?;
                    if host.is_empty()
                        || !host
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
                    {
                        return Err(invalid(
                            "host must be a nonempty name, IPv4 literal or bracketed IPv6 literal",
                        ));
                    }
                    (host, port)
                };
                let port = port
                    .parse::<u16>()
                    .ok()
                    .filter(|number| *number > 0 && port.bytes().all(|byte| byte.is_ascii_digit()))
                    .ok_or_else(|| invalid("port must be a decimal integer from 1 through 65535"))?;
                let host = host.to_owned();
                Ok(if scheme == "tcp" {
                    Self::Tcp { host, port }
                } else {
                    Self::Udp { host, port }
                })
            }
            _ => Err(invalid("unknown scheme; use tcp, udp, unix, unixgram or npipe")),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp { host, port } | Self::Udp { host, port } => {
                let scheme = if matches!(self, Self::Tcp { .. }) { "tcp" } else { "udp" };
                if host.contains(':') {
                    write!(f, "{scheme}://[{host}]:{port}")
                } else {
                    write!(f, "{scheme}://{host}:{port}")
                }
            }
            Self::NamedPipe { name } => write!(f, "npipe://{name}"),
            Self::UnixStream { path } => write!(f, "unix://{path}"),
            Self::UnixDatagram { path } => write!(f, "unixgram://{path}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_render_with_the_url_schemes_participant_addresses_use() {
        for (target, expected) in [
            (
                Target::Tcp {
                    host: "h".into(),
                    port: 1,
                },
                "tcp://h:1",
            ),
            (
                Target::Udp {
                    host: "h".into(),
                    port: 1,
                },
                "udp://h:1",
            ),
            (
                Target::UnixStream {
                    path: "/run/a.sock".into(),
                },
                "unix:///run/a.sock",
            ),
            (
                Target::UnixDatagram {
                    path: "/run/a.sock".into(),
                },
                "unixgram:///run/a.sock",
            ),
            (
                Target::Tcp {
                    host: "::1".into(),
                    port: 8125,
                },
                "tcp://[::1]:8125",
            ),
        ] {
            assert_eq!(target.to_string(), expected);
            assert_eq!(Target::parse(expected).unwrap(), target);
        }
    }

    #[test]
    fn socket_paths_are_preserved_verbatim() {
        let input = "unix:///run/a/../b with spaces.sock";
        assert_eq!(
            Target::parse(input).unwrap(),
            Target::UnixStream {
                path: "/run/a/../b with spaces.sock".into()
            }
        );
    }

    #[test]
    fn named_pipe_targets_round_trip_and_reject_empty_names_separators_and_nul() {
        let target = Target::NamedPipe {
            name: "listener with spaces".into(),
        };
        assert_eq!(target.to_string(), "npipe://listener with spaces");
        assert_eq!(Target::parse(&target.to_string()).unwrap(), target);
        for name in ["", r"a\b", "a/b", "a\0b", r"\\.\pipe\listener"] {
            assert!(Target::parse(&format!("npipe://{name}"))
                .unwrap_err()
                .to_string()
                .contains("pipe name"));
        }
    }
}
