//! Configuration-dependent served and consumed endpoint declarations.

use std::collections::BTreeSet;

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
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    /// How a consumer reaches this endpoint.
    pub kind: EndpointKind,
}

/// An endpoint a participant consumes: a key it accepts in `connect`.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsumedEndpoint {
    /// Endpoint kinds accepted as connection targets.
    pub accepts: Vec<EndpointKind>,
    /// Whether the case must connect this endpoint.
    pub required: bool,
    /// How a connection's resolved address reaches the consumer's processes.
    pub delivery: Delivery,
}

/// How a connection's resolved address reaches the consuming participant's processes.
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    /// Sets these environment variables. A case that connects the endpoint must not set them in `env`.
    Env(BTreeSet<String>),
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
