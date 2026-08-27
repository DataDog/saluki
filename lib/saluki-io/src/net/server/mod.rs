use std::net::SocketAddr;

pub mod grpc;
pub mod http;
pub mod multiplex_service;

#[cfg(test)]
pub(crate) mod test_util;

/// The socket address bound by a running server.
///
/// Servers assert this value only when configured with an identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundServerAddress(pub SocketAddr);
