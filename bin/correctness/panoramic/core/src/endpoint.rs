//! Served listeners, consumer-visible addresses and connection delivery declarations.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use crate::case::template::AddressPart;

/// How a served endpoint is reached.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EndpointKind {
    /// A TCP listener.
    Tcp,
    /// A UDP listener.
    Udp,
    /// An HTTP server.
    Http,
    /// A gRPC server.
    Grpc,
    /// A Unix datagram socket.
    UnixDatagram,
    /// A Unix stream socket.
    UnixStream,
    /// A named pipe.
    NamedPipe,
}

/// What a step does with an endpoint from the runner host.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointOperation {
    /// Tests whether the endpoint is listening or addressable.
    Probe,
    /// Sends raw bytes on a stream or in a datagram.
    Send,
    /// Makes an HTTP or gRPC request and observes the server's response.
    Request,
}

impl EndpointKind {
    /// Spelling used in diagnostics.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Http => "http",
            Self::Grpc => "grpc",
            Self::UnixDatagram => "unix-datagram",
            Self::UnixStream => "unix-stream",
            Self::NamedPipe => "named-pipe",
        }
    }

    pub(crate) fn runner_host_transport(self) -> Option<Transport> {
        match self {
            Self::Tcp | Self::Http | Self::Grpc => Some(Transport::Tcp),
            Self::Udp => Some(Transport::Udp),
            Self::UnixDatagram | Self::UnixStream | Self::NamedPipe => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Transport {
    Tcp,
    Udp,
}

impl Transport {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// An endpoint a participant serves.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub(crate) kind: EndpointKind,
    pub(crate) tls: bool,
    pub(crate) listener: Listener,
    pub(crate) ready: Option<Evidence>,
}

#[derive(Clone, Debug)]
pub(crate) enum Listener {
    Port(Port),
    Path(String),
    Pipe(String),
}

/// Where a network listener's port comes from.
#[derive(Clone, Copy, Debug)]
pub enum Port {
    /// A port the participant type derives from its configuration.
    Fixed(u16),
    /// A port only the case knows, supplied as `config.endpoints.<name>.port`.
    FromCase,
    /// A host-process-group port core assigns when configuration sets none.
    Assigned {
        /// Maps the assigned port to the listener's environment variables.
        deliver: fn(u16) -> BTreeMap<String, String>,
    },
}

impl Endpoint {
    /// The configured network port, excluding case-supplied and dynamically assigned ports.
    pub fn fixed_port(&self) -> Option<u16> {
        match self.listener {
            Listener::Port(Port::Fixed(port)) => Some(port),
            _ => None,
        }
    }

    /// Enables TLS for an HTTP listener. Invalid for other endpoint kinds.
    pub fn with_tls(mut self) -> Self {
        self.tls = true;
        self
    }

    /// Attaches readiness evidence to this endpoint.
    pub fn with_ready(mut self, evidence: Evidence) -> Self {
        self.ready = Some(evidence);
        self
    }

    /// Declares a TCP listener at the given port.
    pub fn tcp(port: Port) -> Self {
        Self {
            kind: EndpointKind::Tcp,
            listener: Listener::Port(port),
            ready: None,
            tls: false,
        }
    }

    /// Declares a UDP listener at the given port.
    pub fn udp(port: Port) -> Self {
        Self {
            kind: EndpointKind::Udp,
            listener: Listener::Port(port),
            ready: None,
            tls: false,
        }
    }

    /// Declares an HTTP listener at the given port.
    pub fn http(port: Port) -> Self {
        Self {
            kind: EndpointKind::Http,
            listener: Listener::Port(port),
            ready: None,
            tls: false,
        }
    }

    /// Declares a gRPC listener at the given port.
    pub fn grpc(port: Port) -> Self {
        Self {
            kind: EndpointKind::Grpc,
            listener: Listener::Port(port),
            ready: None,
            tls: false,
        }
    }

    /// Declares a Unix datagram socket at the given path.
    pub fn unix_datagram(path: impl Into<String>) -> Self {
        Self {
            kind: EndpointKind::UnixDatagram,
            listener: Listener::Path(path.into()),
            ready: None,
            tls: false,
        }
    }

    /// Declares a Unix stream socket at the given path.
    pub fn unix_stream(path: impl Into<String>) -> Self {
        Self {
            kind: EndpointKind::UnixStream,
            listener: Listener::Path(path.into()),
            ready: None,
            tls: false,
        }
    }

    /// Declares a named pipe with the given name.
    pub fn named_pipe(name: impl Into<String>) -> Self {
        Self {
            kind: EndpointKind::NamedPipe,
            listener: Listener::Pipe(name.into()),
            ready: None,
            tls: false,
        }
    }
}

/// Evidence that a participant, or one of its endpoints, is ready, checked within a bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    pub(crate) check: Check,
    pub(crate) within: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    TcpAccepts { endpoint: String },
    HttpOk { endpoint: String, path: String },
    SocketExists { endpoint: String },
    FileExists { path: String },
    OutputMatches { process: String, pattern: String },
}

/// Default time allowed to observe readiness evidence.
const DEFAULT_READINESS_BOUND: Duration = Duration::from_secs(60);

impl Evidence {
    /// Waits for a TCP connection to succeed at the named endpoint.
    pub fn tcp_accepts(endpoint: impl Into<String>) -> Self {
        Self {
            check: Check::TcpAccepts {
                endpoint: endpoint.into(),
            },
            within: DEFAULT_READINESS_BOUND,
        }
    }

    /// Waits for HTTP 200 at the named endpoint and path.
    pub fn http_ok(endpoint: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            check: Check::HttpOk {
                endpoint: endpoint.into(),
                path: path.into(),
            },
            within: DEFAULT_READINESS_BOUND,
        }
    }

    /// Waits for the named endpoint's socket to exist.
    pub fn socket_exists(endpoint: impl Into<String>) -> Self {
        Self {
            check: Check::SocketExists {
                endpoint: endpoint.into(),
            },
            within: DEFAULT_READINESS_BOUND,
        }
    }

    /// Waits for a file to exist at the given path.
    pub fn file_exists(path: impl Into<String>) -> Self {
        Self {
            check: Check::FileExists { path: path.into() },
            within: DEFAULT_READINESS_BOUND,
        }
    }

    /// Waits for a regex match in the named process's output.
    pub fn output_matches(process: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            check: Check::OutputMatches {
                process: process.into(),
                pattern: pattern.into(),
            },
            within: DEFAULT_READINESS_BOUND,
        }
    }

    /// Replaces the default sixty-second readiness bound.
    pub fn within(mut self, within: Duration) -> Self {
        self.within = within;
        self
    }
}

impl Evidence {
    pub(crate) fn describe(&self) -> String {
        match &self.check {
            Check::TcpAccepts { endpoint } => format!("tcp-accepts on '{endpoint}'"),
            Check::HttpOk { endpoint, path } => format!("http-ok on '{endpoint}' at '{path}'"),
            Check::SocketExists { endpoint } => format!("socket-exists on '{endpoint}'"),
            Check::FileExists { path } => format!("file-exists at '{path}'"),
            Check::OutputMatches { process, pattern } => format!("output-matches '{pattern}' on process '{process}'"),
        }
    }
}

/// An endpoint address as seen by the consuming participant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAddress {
    pub(crate) kind: EndpointKind,
    pub(crate) url: String,
    pub(crate) host: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) path: Option<String>,
}

impl ResolvedAddress {
    /// The endpoint's URL as seen by its consumer.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The network endpoint's host as seen by its consumer, or `None` for sockets and pipes.
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// The network endpoint's port as seen by its consumer, or `None` for sockets and pipes.
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    pub(crate) fn part(&self, part: AddressPart) -> Option<String> {
        match part {
            AddressPart::Url => Some(self.url.clone()),
            AddressPart::Host => self.host.clone(),
            AddressPart::Port => self.port.map(|port| port.to_string()),
            AddressPart::Path => self.path.clone(),
        }
    }
}

/// How the runner host performs one endpoint operation, directly or through the server participant's courier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RunnerHostRoute {
    Direct(ResolvedAddress),
    Courier(ResolvedAddress),
}

/// A network listener's port mapping on the runner host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunnerHostPort {
    pub(crate) transport: Transport,
    pub(crate) container_port: u16,
    pub(crate) host_port: u16,
}

/// An endpoint a participant consumes: a key it accepts in `connect`.
#[derive(Clone, Debug)]
pub struct ConsumedEndpoint {
    /// Endpoint kinds accepted as connection targets.
    pub accepts: Vec<EndpointKind>,
    /// Whether the case must connect this endpoint.
    pub required: bool,
    /// How a connection's resolved address reaches the consumer's processes.
    pub delivery: Delivery,
}

/// How a connection's resolved address reaches the consuming participant's processes.
#[derive(Clone, Debug)]
pub enum Delivery {
    /// Sets environment variables from the address seen by the consumer.
    Env {
        /// Keys the delivery writes; a case that connects this endpoint must not set them in `env`.
        keys: BTreeSet<String>,
        /// Computes exactly the declared keys from the consumer-visible address.
        value: fn(&ResolvedAddress) -> BTreeMap<String, String>,
    },
}

/// Assertions for participant declarations in downstream tests.
#[cfg(any(test, feature = "test-util"))]
pub mod test_support {
    use super::*;

    /// Builds an HTTP address as seen by a consumer.
    pub fn http_address(host: &str, port: u16) -> ResolvedAddress {
        ResolvedAddress {
            kind: EndpointKind::Http,
            url: format!("http://{host}:{port}"),
            host: Some(host.into()),
            port: Some(port),
            path: None,
        }
    }

    /// Builds a Unix-stream address as seen by a consumer.
    pub fn unix_stream_address(path: &str) -> ResolvedAddress {
        ResolvedAddress {
            kind: EndpointKind::UnixStream,
            url: format!("unix://{path}"),
            host: None,
            port: None,
            path: Some(path.into()),
        }
    }

    /// Checks whether a listener uses TLS.
    ///
    /// # Panics
    ///
    /// Panics if the TLS setting differs from `tls`.
    #[track_caller]
    pub fn assert_tls_endpoint(endpoint: &Endpoint, tls: bool) {
        assert_eq!(endpoint.tls, tls);
    }

    /// Checks a socket endpoint's kind, path and readiness evidence.
    ///
    /// # Panics
    ///
    /// Panics if any declaration differs from the expected value.
    #[track_caller]
    pub fn assert_socket_endpoint(endpoint: &Endpoint, kind: EndpointKind, path: &str, ready: Option<Evidence>) {
        assert_eq!(endpoint.kind, kind);
        assert!(matches!(&endpoint.listener, Listener::Path(actual) if actual == path));
        assert_eq!(endpoint.ready, ready);
    }

    /// Checks a named-pipe endpoint's name and readiness evidence.
    ///
    /// # Panics
    ///
    /// Panics if any declaration differs from the expected value.
    #[track_caller]
    pub fn assert_pipe_endpoint(endpoint: &Endpoint, name: &str, ready: Option<Evidence>) {
        assert_eq!(endpoint.kind, EndpointKind::NamedPipe);
        assert!(matches!(&endpoint.listener, Listener::Pipe(actual) if actual == name));
        assert_eq!(endpoint.ready, ready);
    }

    /// Checks an assigned endpoint's kind and returns its environment delivery.
    ///
    /// # Panics
    ///
    /// Panics if the endpoint is fixed or has a different kind.
    #[track_caller]
    pub fn assigned_delivery(endpoint: &Endpoint, kind: EndpointKind) -> fn(u16) -> BTreeMap<String, String> {
        assert_eq!(endpoint.kind, kind);
        let Listener::Port(Port::Assigned { deliver }) = endpoint.listener else {
            panic!("expected an assigned port")
        };
        deliver
    }

    /// Checks a network endpoint's kind, fixed port and readiness evidence.
    ///
    /// # Panics
    ///
    /// Panics if any declaration differs from the expected value.
    #[track_caller]
    pub fn assert_network_endpoint(endpoint: &Endpoint, kind: EndpointKind, port: u16, ready: Option<Evidence>) {
        assert_eq!(endpoint.kind, kind);
        assert!(matches!(endpoint.listener, Listener::Port(Port::Fixed(actual)) if actual == port));
        assert_eq!(endpoint.ready, ready);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_ports_exclude_unassigned_ports_and_socket_paths() {
        for constructor in [Endpoint::tcp, Endpoint::udp, Endpoint::http, Endpoint::grpc] {
            assert_eq!(constructor(Port::Fixed(4321)).fixed_port(), Some(4321));
            assert_eq!(constructor(Port::FromCase).fixed_port(), None);
            assert_eq!(
                constructor(Port::Assigned {
                    deliver: |_| BTreeMap::new()
                })
                .fixed_port(),
                None
            );
        }
        for endpoint in [
            Endpoint::unix_stream("/run/stream.sock"),
            Endpoint::unix_datagram("/run/dgram.sock"),
            Endpoint::named_pipe("pipe"),
        ] {
            assert_eq!(endpoint.fixed_port(), None);
        }
    }

    #[test]
    fn resolved_addresses_expose_network_hosts_and_ports() {
        let network = test_support::http_address("receiver", 4321);
        assert_eq!(network.host(), Some("receiver"));
        assert_eq!(network.port(), Some(4321));
        for (kind, url, path) in [
            (EndpointKind::UnixStream, "unix:///run/stream.sock", "/run/stream.sock"),
            (EndpointKind::NamedPipe, r"\\.\pipe\stream", r"\\.\pipe\stream"),
        ] {
            let address = ResolvedAddress {
                kind,
                url: url.into(),
                host: None,
                port: None,
                path: Some(path.into()),
            };
            assert_eq!(address.host(), None);
            assert_eq!(address.port(), None);
        }
    }

    #[test]
    fn readiness_evidence_has_a_default_bound_and_describes_its_check() {
        for (evidence, description) in [
            (Evidence::tcp_accepts("api"), "tcp-accepts on 'api'"),
            (Evidence::http_ok("api", "/ready"), "http-ok on 'api' at '/ready'"),
            (Evidence::socket_exists("log"), "socket-exists on 'log'"),
            (Evidence::file_exists("/ready"), "file-exists at '/ready'"),
        ] {
            assert_eq!(evidence.describe(), description);
            assert_eq!(evidence.within, Duration::from_secs(60));
            let check = evidence.check.clone();
            let bounded = evidence.within(Duration::from_secs(2));
            assert_eq!(
                bounded,
                Evidence {
                    check,
                    within: Duration::from_secs(2)
                }
            );
            assert_eq!(
                Endpoint::http(Port::Fixed(80)).with_ready(bounded.clone()).ready,
                Some(bounded)
            );
        }
        for endpoint in [
            Endpoint::tcp(Port::Fixed(80)),
            Endpoint::udp(Port::Fixed(80)),
            Endpoint::http(Port::Fixed(80)),
            Endpoint::grpc(Port::Fixed(80)),
            Endpoint::unix_datagram("/log.sock"),
            Endpoint::unix_stream("/log.sock"),
            Endpoint::named_pipe("log"),
        ] {
            assert_eq!(endpoint.ready, None);
            assert!(!endpoint.tls);
        }
    }

    #[test]
    fn endpoint_kinds_use_their_diagnostic_spellings() {
        for (kind, name) in [
            (EndpointKind::Tcp, "tcp"),
            (EndpointKind::Udp, "udp"),
            (EndpointKind::Http, "http"),
            (EndpointKind::Grpc, "grpc"),
            (EndpointKind::UnixDatagram, "unix-datagram"),
            (EndpointKind::UnixStream, "unix-stream"),
            (EndpointKind::NamedPipe, "named-pipe"),
        ] {
            assert_eq!(kind.name(), name);
        }
    }
}
