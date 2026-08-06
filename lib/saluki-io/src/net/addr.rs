use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
};

use axum::extract::connect_info::Connected;
use serde::Deserialize;
use url::Url;

use super::Connection;

/// vsock context identifier accepting connections addressed to any local CID (`VMADDR_CID_ANY`).
pub const VSOCK_CID_ANY: u32 = 0xFFFF_FFFF;

/// vsock context identifier of the hypervisor (`VMADDR_CID_HYPERVISOR`).
pub const VSOCK_CID_HYPERVISOR: u32 = 0;

/// vsock context identifier for same-host loopback (`VMADDR_CID_LOCAL`).
pub const VSOCK_CID_LOCAL: u32 = 1;

/// vsock context identifier of the host (`VMADDR_CID_HOST`).
pub const VSOCK_CID_HOST: u32 = 2;

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
/// - `vsock://:8125` (listen on any local vsock CID, port 8125)
/// - `vsock://host:8125` (listen on the host vsock CID, port 8125)
/// - `vsock://42:8125` (listen on vsock CID 42, port 8125)
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
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

    /// A vsock (`AF_VSOCK`) stream listen address.
    ///
    /// vsock is a connection-oriented, host/guest transport: an address is a 32-bit context
    /// identifier (CID) naming a VM, paired with a 32-bit port. Both values are wider than their
    /// IP counterparts, so neither fits a [`SocketAddr`].
    Vsock {
        /// Context identifier (CID) to bind to.
        ///
        /// [`VSOCK_CID_ANY`] accepts connections addressed to any of the local context
        /// identifiers.
        cid: u32,

        /// Port to bind to.
        port: u32,
    },

    /// A Windows named pipe listen address.
    NamedPipe {
        /// Named pipe name without the `\\.\pipe\` prefix.
        name: String,

        /// Security descriptor string applied when creating the pipe.
        security_descriptor: String,

        /// Input buffer size to request from Windows when creating the pipe.
        input_buffer_size: Option<u32>,
    },
}

impl ListenAddress {
    /// Creates a TCP address for the given port that listens on all interfaces.
    pub const fn any_tcp(port: u16) -> Self {
        Self::Tcp(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)))
    }

    /// Creates a Windows named pipe listen address.
    pub fn named_pipe(name: impl Into<String>, security_descriptor: impl Into<String>) -> Self {
        Self::named_pipe_with_input_buffer_size(name, security_descriptor, None)
    }

    /// Creates a Windows named pipe listen address with a requested input buffer size.
    pub fn named_pipe_with_input_buffer_size(
        name: impl Into<String>, security_descriptor: impl Into<String>, input_buffer_size: impl Into<Option<u32>>,
    ) -> Self {
        Self::NamedPipe {
            name: normalize_windows_named_pipe_name(name.into()),
            security_descriptor: security_descriptor.into(),
            input_buffer_size: input_buffer_size.into(),
        }
    }

    /// Creates a vsock listen address from a `<cid>:<port>` address, given without the `vsock://` scheme.
    ///
    /// The CID may be a bare 32-bit number, one of the well-known names `any`, `hypervisor`, `local`, or `host`, or
    /// empty to mean `any`. The port must be a bare 32-bit number: vsock ports are twice as wide as IP ports, and have
    /// no named equivalents.
    ///
    /// # Errors
    ///
    /// Returns an error describing the problem if the address has no `:` separator, or if either half fails to parse.
    pub fn try_from_vsock_address(address: &str) -> Result<Self, String> {
        let (raw_cid, raw_port) = address
            .split_once(':')
            .ok_or_else(|| format!("'{}' is missing the ':' separating the CID from the port", address))?;

        let cid = match raw_cid {
            "" | "any" => VSOCK_CID_ANY,
            "hypervisor" => VSOCK_CID_HYPERVISOR,
            "local" => VSOCK_CID_LOCAL,
            "host" => VSOCK_CID_HOST,
            other => other.parse::<u32>().map_err(|_| {
                format!(
                    "'{other}' is not a valid CID; expected a 32-bit number or one of: any, hypervisor, local, host"
                )
            })?,
        };

        let port = raw_port
            .parse::<u32>()
            .map_err(|_| format!("'{raw_port}' is not a valid port; expected a 32-bit number"))?;

        Ok(Self::Vsock { cid, port })
    }

    /// Returns the socket type of the listen address.
    pub const fn listener_type(&self) -> &'static str {
        match self {
            Self::Tcp(_) => "tcp",
            Self::Udp(_) => "udp",
            Self::Unixgram(_) => "unixgram",
            Self::Unix(_) => "unix",
            Self::Vsock { .. } => "vsock",
            Self::NamedPipe { .. } => "named_pipe",
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
            Self::Vsock { .. } => None,
            Self::NamedPipe { .. } => None,
        }
    }

    /// Returns the fully qualified Windows named pipe path, if this is a named pipe address.
    pub fn as_windows_named_pipe_path(&self) -> Option<String> {
        match self {
            Self::NamedPipe { name, .. } => Some(format!(r"\\.\pipe\{name}")),
            _ => None,
        }
    }

    /// Returns the Windows named pipe security descriptor, if this is a named pipe address.
    pub fn as_windows_named_pipe_security_descriptor(&self) -> Option<&str> {
        match self {
            Self::NamedPipe {
                security_descriptor, ..
            } => Some(security_descriptor.as_str()),
            _ => None,
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

fn normalize_windows_named_pipe_name(name: String) -> String {
    name.strip_prefix(r"\\.\pipe\")
        .or_else(|| name.strip_prefix(r"//./pipe/"))
        .or_else(|| name.strip_prefix("pipe/"))
        .unwrap_or(&name)
        .to_string()
}

impl fmt::Display for ListenAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(addr) => write!(f, "tcp://{}", addr),
            Self::Udp(addr) => write!(f, "udp://{}", addr),
            Self::Unixgram(path) => write!(f, "unixgram://{}", path.display()),
            Self::Unix(path) => write!(f, "unix://{}", path.display()),
            Self::Vsock { cid, port } => write!(f, "vsock://{}:{}", cid, port),
            Self::NamedPipe { name, .. } => write!(f, "npipe://{}", name),
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
        // vsock addresses are parsed by hand, ahead of `Url`: a vsock port is 32 bits wide, while URL
        // ports are capped at 16 bits, so `Url` rejects any address using the upper range.
        if let Some(address) = value.strip_prefix("vsock://") {
            return Self::try_from_vsock_address(address)
                .map_err(|reason| format!("invalid vsock listen address: {}", reason));
        }

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
            "npipe" => {
                let name = url.host_str().unwrap_or_else(|| url.path().trim_start_matches('/'));
                if name.is_empty() {
                    return Err("named pipe name cannot be empty".to_string());
                }

                Ok(Self::named_pipe(name, String::new()))
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

    /// Returns `true` if process credential detection failed for a per-message reason.
    pub const fn is_telemetry_error(&self) -> bool {
        matches!(
            self,
            Self::Error(ProcessCredentialsError::InvalidCredentials | ProcessCredentialsError::ZeroPid)
        )
    }
}

/// Connection address.
///
/// A generic representation of the address of a remote peer. This can either be a typical socket address (used for
/// IPv4/IPv6), a vsock address, or potentially the process credentials of a Unix domain socket connection.
#[derive(Clone)]
pub enum ConnectionAddress {
    /// A socket-like address.
    SocketLike(SocketAddr),

    /// A vsock address.
    Vsock {
        /// Context identifier (CID) of the remote peer.
        cid: u32,

        /// Port of the remote peer.
        port: u32,
    },

    /// A process-like address.
    ProcessLike(ProcessIdentity),
}

impl fmt::Display for ConnectionAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SocketLike(addr) => write!(f, "{}", addr),
            Self::Vsock { cid, port } => write!(f, "vsock://{}:{}", cid, port),
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
            Self::SocketLike(_) | Self::Vsock { .. } => None,
        }
    }

    /// Returns `true` if Unix domain socket process credential detection failed.
    pub const fn has_process_credential_error(&self) -> bool {
        match self {
            Self::ProcessLike(identity) => identity.is_error(),
            Self::SocketLike(_) | Self::Vsock { .. } => false,
        }
    }

    /// Returns `true` if Unix domain socket process credential detection failed for a per-message reason.
    pub const fn has_process_credential_telemetry_error(&self) -> bool {
        match self {
            Self::ProcessLike(identity) => identity.is_telemetry_error(),
            Self::SocketLike(_) | Self::Vsock { .. } => false,
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
    fn named_pipe_listen_address_formats_with_windows_pipe_prefix() {
        let address = ListenAddress::named_pipe("datadog-dogstatsd", "D:AI(A;;GA;;;WD)");

        assert_eq!(address.listener_type(), "named_pipe");
        assert_eq!(address.to_string(), r"npipe://datadog-dogstatsd");
        assert_eq!(
            address.as_windows_named_pipe_path().as_deref(),
            Some(r"\\.\pipe\datadog-dogstatsd")
        );
    }

    #[test]
    fn named_pipe_listen_address_accepts_full_windows_pipe_path() {
        let address = ListenAddress::named_pipe(r"\\.\pipe\datadog-dogstatsd", "D:AI(A;;GA;;;WD)");

        assert_eq!(address.to_string(), r"npipe://datadog-dogstatsd");
        assert_eq!(
            address.as_windows_named_pipe_path().as_deref(),
            Some(r"\\.\pipe\datadog-dogstatsd")
        );
    }

    #[test]
    fn npipe_url_parses_full_windows_pipe_path() {
        let address = ListenAddress::try_from("npipe:////./pipe/datadog-dogstatsd").unwrap();

        assert_eq!(address.to_string(), r"npipe://datadog-dogstatsd");
        assert_eq!(
            address.as_windows_named_pipe_path().as_deref(),
            Some(r"\\.\pipe\datadog-dogstatsd")
        );
    }

    #[test]
    fn vsock_listen_address_round_trips_through_its_url_form() {
        let address = ListenAddress::try_from("vsock://2:8125").unwrap();

        assert_eq!(address, ListenAddress::Vsock { cid: 2, port: 8125 });
        assert_eq!(address.listener_type(), "vsock");
        assert_eq!(address.to_string(), "vsock://2:8125");
        assert_eq!(ListenAddress::try_from(address.to_string()).unwrap(), address);
    }

    #[test]
    fn vsock_listen_address_accepts_full_32_bit_cid_and_port() {
        // Both halves of a vsock address are 32 bits wide, unlike a URL port, so the upper range has to
        // survive parsing.
        let address = ListenAddress::try_from("vsock://4294967295:4294967295").unwrap();

        assert_eq!(
            address,
            ListenAddress::Vsock {
                cid: VSOCK_CID_ANY,
                port: u32::MAX,
            }
        );
    }

    #[test]
    fn vsock_address_resolves_named_and_omitted_cids() {
        let cases = [
            (":8125", VSOCK_CID_ANY),
            ("any:8125", VSOCK_CID_ANY),
            ("hypervisor:8125", VSOCK_CID_HYPERVISOR),
            ("local:8125", VSOCK_CID_LOCAL),
            ("host:8125", VSOCK_CID_HOST),
            ("42:8125", 42),
        ];

        for (address, expected_cid) in cases {
            assert_eq!(
                ListenAddress::try_from_vsock_address(address).unwrap(),
                ListenAddress::Vsock {
                    cid: expected_cid,
                    port: 8125
                },
                "unexpected result for '{address}'"
            );

            // The scheme-qualified URL form must resolve identically.
            assert_eq!(
                ListenAddress::try_from(format!("vsock://{address}")).unwrap(),
                ListenAddress::try_from_vsock_address(address).unwrap(),
            );
        }
    }

    #[test]
    fn malformed_vsock_addresses_are_rejected() {
        for address in ["2", "", "guest:8125", "2:-1", "2:", "2:8125:9", "-1:8125"] {
            assert!(
                ListenAddress::try_from_vsock_address(address).is_err(),
                "expected '{address}' to be rejected"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn vsock_cid_constants_match_the_kernel() {
        assert_eq!(VSOCK_CID_ANY, tokio_vsock::VMADDR_CID_ANY);
        assert_eq!(VSOCK_CID_HYPERVISOR, tokio_vsock::VMADDR_CID_HYPERVISOR);
        assert_eq!(VSOCK_CID_LOCAL, tokio_vsock::VMADDR_CID_LOCAL);
        assert_eq!(VSOCK_CID_HOST, tokio_vsock::VMADDR_CID_HOST);
    }

    #[test]
    fn vsock_connection_address_carries_no_process_credentials() {
        let peer_addr = ConnectionAddress::Vsock { cid: 3, port: 41235 };

        assert_eq!(peer_addr.to_string(), "vsock://3:41235");
        assert!(peer_addr.process_credentials().is_none());
        assert!(!peer_addr.has_process_credential_error());
        assert!(!peer_addr.has_process_credential_telemetry_error());
    }

    #[test]
    fn unsupported_platform_process_identity_is_not_a_telemetry_error() {
        let peer_addr =
            ConnectionAddress::ProcessLike(ProcessIdentity::Error(ProcessCredentialsError::UnsupportedPlatform));

        assert!(peer_addr.has_process_credential_error());
        assert!(!peer_addr.has_process_credential_telemetry_error());
    }

    #[test]
    fn invalid_process_credentials_are_telemetry_errors() {
        let peer_addr =
            ConnectionAddress::ProcessLike(ProcessIdentity::Error(ProcessCredentialsError::InvalidCredentials));

        assert!(peer_addr.has_process_credential_error());
        assert!(peer_addr.has_process_credential_telemetry_error());
    }

    #[test]
    fn zero_pid_process_credentials_are_telemetry_errors() {
        let peer_addr = ConnectionAddress::ProcessLike(ProcessIdentity::Error(ProcessCredentialsError::ZeroPid));

        assert!(peer_addr.has_process_credential_error());
        assert!(peer_addr.has_process_credential_telemetry_error());
    }

    #[test]
    fn as_local_connect_addr() {
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
