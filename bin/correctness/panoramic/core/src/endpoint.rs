//! Served listeners, consumer-visible addresses and connection delivery declarations.

use std::collections::{BTreeMap, BTreeSet};

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
}

/// An endpoint a participant serves.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub(crate) kind: EndpointKind,
    pub(crate) listener: Listener,
}

#[derive(Clone, Debug)]
pub(crate) enum Listener {
    Port(Port),
    Path(String),
    Pipe(#[expect(dead_code, reason = "Named-pipe listener names are retained for address rendering.")] String),
}

/// Where a network listener's port comes from.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "Only core's test participant types declare listener ports.")
)]
pub(crate) enum Port {
    /// A port the participant type derives from its configuration.
    Fixed(u16),
    /// A port only the case knows, supplied as `config.endpoints.<name>.port`.
    FromCase,
    /// A host-process-group port core assigns when configuration sets none.
    /// `deliver` maps it to the environment variables from which the type derives it.
    Assigned {
        deliver: fn(u16) -> BTreeMap<String, String>,
    },
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "Endpoints are constructed by core's test participant types.")
)]
impl Endpoint {
    /// Declares a TCP listener at the given port.
    pub(crate) fn tcp(port: Port) -> Self {
        Self {
            kind: EndpointKind::Tcp,
            listener: Listener::Port(port),
        }
    }

    /// Declares a UDP listener at the given port.
    pub(crate) fn udp(port: Port) -> Self {
        Self {
            kind: EndpointKind::Udp,
            listener: Listener::Port(port),
        }
    }

    /// Declares an HTTP listener at the given port.
    pub(crate) fn http(port: Port) -> Self {
        Self {
            kind: EndpointKind::Http,
            listener: Listener::Port(port),
        }
    }

    /// Declares a gRPC listener at the given port.
    pub(crate) fn grpc(port: Port) -> Self {
        Self {
            kind: EndpointKind::Grpc,
            listener: Listener::Port(port),
        }
    }

    /// Declares a Unix datagram socket at the given path.
    pub(crate) fn unix_datagram(path: impl Into<String>) -> Self {
        Self {
            kind: EndpointKind::UnixDatagram,
            listener: Listener::Path(path.into()),
        }
    }

    /// Declares a Unix stream socket at the given path.
    pub(crate) fn unix_stream(path: impl Into<String>) -> Self {
        Self {
            kind: EndpointKind::UnixStream,
            listener: Listener::Path(path.into()),
        }
    }

    /// Declares a named pipe with the given name.
    pub(crate) fn named_pipe(name: impl Into<String>) -> Self {
        Self {
            kind: EndpointKind::NamedPipe,
            listener: Listener::Pipe(name.into()),
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

#[cfg(test)]
mod tests {
    use super::*;

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
