//! Network listeners.
use std::{future::pending, io};

use snafu::{ResultExt as _, Snafu};
use tokio::net::{TcpListener, UdpSocket};

use super::{
    addr::ListenAddress,
    stream::{Connection, Stream},
    unix::{enable_uds_socket_credentials, ensure_unix_socket_free, set_unix_socket_write_only},
};

/// A listener error.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ListenerError {
    /// An invalid configuration was given when creating the listener.
    #[snafu(display("invalid configuration: {}", reason))]
    InvalidConfiguration {
        /// Cause of the invalid configuration.
        reason: &'static str,
    },

    /// Failed to bind to the listen address.
    #[snafu(display("failed to bind to listen address {}: {}", address, source))]
    FailedToBind {
        /// Listen address.
        address: ListenAddress,

        /// Source of the error.
        source: io::Error,
    },

    /// Failed to configure a setting on the listening socket.
    #[snafu(display("failed to configure {} for listener on address {}: {}", setting, address, source))]
    FailedToConfigureListener {
        /// Listen address.
        address: ListenAddress,

        /// Name of the setting.
        setting: &'static str,

        /// Source of the error.
        source: io::Error,
    },

    /// Failed to configure a setting on an accepted stream.
    #[snafu(display("failed to configure {} for {} stream: {}", setting, stream_type, source))]
    FailedToConfigureStream {
        /// Name of the setting.
        setting: &'static str,

        /// Type of stream.
        stream_type: &'static str,

        /// Source of the error.
        source: io::Error,
    },

    /// Failed to accept a new stream from the listener.
    #[snafu(display("failed to accept new stream for listener on address {}: {}", address, source))]
    FailedToAccept {
        /// Listen address.
        address: ListenAddress,

        /// Source of the error.
        source: io::Error,
    },
}

enum ListenerInner {
    Tcp(TcpListener),
    Udp(Option<UdpSocket>),
    #[cfg(unix)]
    Unixgram(Option<tokio::net::UnixDatagram>),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

/// A network listener.
///
/// `Listener` is a abstract listener that works in conjunction with `Stream`, providing the ability to listen on
/// arbitrary addresses and accept new streams of that address family.
///
/// ## Connection-oriented vs connectionless listeners
///
/// For listeners on connection-oriented address families (e.g. TCP, Unix domain sockets in stream mode), the listener
/// will listen for an accept new connections in the typical fashion. However, for connectionless address families
/// (e.g. UDP, Unix domain sockets in datagram mode), there is no concept of a "connection" and so nothing to be
/// continually "accepted". Instead, `Listener` will emit a single `Stream` that can be used to send and receive data
/// from multiple remote peers.
///
/// ## Missing
///
/// - Ability to configure `Listener` to emit multiple streams for connectionless address families, allowing for load
///   balancing. (Only possible for UDP via SO_REUSEPORT, as UDS does not support SO_REUSEPORT.)
pub struct Listener {
    listen_address: ListenAddress,
    inner: ListenerInner,
}

impl Listener {
    /// Creates a new `Listener` from the given listen address.
    ///
    /// ## Errors
    ///
    /// If the listen address cannot be bound, or if the listener cannot be configured correctly, an error is returned.
    pub async fn from_listen_address(listen_address: ListenAddress) -> Result<Self, ListenerError> {
        let inner = match &listen_address {
            ListenAddress::Tcp(addr) => {
                TcpListener::bind(addr)
                    .await
                    .map(ListenerInner::Tcp)
                    .context(FailedToBind {
                        address: listen_address.clone(),
                    })?
            }
            ListenAddress::Udp(addr) => {
                UdpSocket::bind(addr)
                    .await
                    .map(Some)
                    .map(ListenerInner::Udp)
                    .context(FailedToBind {
                        address: listen_address.clone(),
                    })?
            }
            #[cfg(unix)]
            ListenAddress::Unixgram(addr) => {
                ensure_unix_socket_free(addr).await.context(FailedToBind {
                    address: listen_address.clone(),
                })?;

                let listener = tokio::net::UnixDatagram::bind(addr)
                    .map(Some)
                    .map(ListenerInner::Unixgram)
                    .context(FailedToBind {
                        address: listen_address.clone(),
                    })?;

                set_unix_socket_write_only(addr)
                    .await
                    .context(FailedToConfigureListener {
                        address: listen_address.clone(),
                        setting: "read/write permissions",
                    })?;

                listener
            }
            #[cfg(unix)]
            ListenAddress::Unix(addr) => {
                ensure_unix_socket_free(addr).await.context(FailedToBind {
                    address: listen_address.clone(),
                })?;

                let listener = tokio::net::UnixListener::bind(addr)
                    .map(ListenerInner::Unix)
                    .context(FailedToBind {
                        address: listen_address.clone(),
                    })?;
                set_unix_socket_write_only(addr)
                    .await
                    .context(FailedToConfigureListener {
                        address: listen_address.clone(),
                        setting: "read/write permissions",
                    })?;

                listener
            }
        };

        Ok(Self { listen_address, inner })
    }

    /// Gets a reference to the listen address.
    pub fn listen_address(&self) -> &ListenAddress {
        &self.listen_address
    }

    /// Accepts a new stream from the listener.
    ///
    /// For connection-oriented address families, this will accept a new connection and return a `Stream` that is bound
    /// to that remote peer. For connectionless address families, this will return a single `Stream` that will receive
    /// from multiple remote peers, and no further streams will be emitted.
    ///
    /// ## Errors
    ///
    /// If the listener fails to accept a new stream, or if the accepted stream cannot be configured correctly, an error
    /// is returned.
    pub async fn accept(&mut self) -> Result<Stream, ListenerError> {
        match &mut self.inner {
            ListenerInner::Tcp(tcp) => tcp.accept().await.map(Into::into).context(FailedToAccept {
                address: self.listen_address.clone(),
            }),
            ListenerInner::Udp(udp) => {
                // TODO: We only emit a single stream here, but we _could_ do something like an internal configuration
                // to allow for multiple streams to be emitted, where the socket is bound via SO_REUSEPORT and then we
                // get load balancing between the sockets.... basically make it possible to parallelize UDP handling if
                // that's a thing we want to do.
                if let Some(socket) = udp.take() {
                    Ok(socket.into())
                } else {
                    pending().await
                }
            }
            #[cfg(unix)]
            ListenerInner::Unixgram(unix) => {
                if let Some(socket) = unix.take() {
                    enable_uds_socket_credentials(&socket).context(FailedToConfigureStream {
                        setting: "SO_PASSCRED",
                        stream_type: "UDS (datagram)",
                    })?;
                    Ok(socket.into())
                } else {
                    pending().await
                }
            }
            #[cfg(unix)]
            ListenerInner::Unix(unix) => unix
                .accept()
                .await
                .context(FailedToAccept {
                    address: self.listen_address.clone(),
                })
                .and_then(|(socket, _)| {
                    enable_uds_socket_credentials(&socket).context(FailedToConfigureStream {
                        setting: "SO_PASSCRED",
                        stream_type: "UDS (stream)",
                    })?;
                    Ok(socket.into())
                }),
        }
    }
}

enum ConnectionOrientedListenerInner {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

/// A connection-oriented network listener.
///
/// `ConnectionOrientedListener` is conceptually the same as `Listener`, but specifically works with connection-oriented
/// protocols. This variant is provided to facilitate usages where only a connection-oriented stream makes sense, such
/// as an HTTP server.
pub struct ConnectionOrientedListener {
    listen_address: ListenAddress,
    inner: ConnectionOrientedListenerInner,
}

impl ConnectionOrientedListener {
    /// Creates a new `ConnectionOrientedListener` from the given listen address.
    ///
    /// ## Errors
    ///
    /// If the listen address is not a connection-oriented address family, or if the listen address cannot be bound, or
    /// if the listener cannot be configured correctly, an error is returned.
    pub async fn from_listen_address(listen_address: ListenAddress) -> Result<Self, ListenerError> {
        let inner = match &listen_address {
            ListenAddress::Tcp(addr) => TcpListener::bind(addr)
                .await
                .map(ConnectionOrientedListenerInner::Tcp)
                .context(FailedToBind {
                    address: listen_address.clone(),
                })?,
            #[cfg(unix)]
            ListenAddress::Unix(addr) => {
                ensure_unix_socket_free(addr).await.context(FailedToBind {
                    address: listen_address.clone(),
                })?;

                let listener = tokio::net::UnixListener::bind(addr)
                    .map(ConnectionOrientedListenerInner::Unix)
                    .context(FailedToBind {
                        address: listen_address.clone(),
                    })?;
                set_unix_socket_write_only(addr)
                    .await
                    .context(FailedToConfigureListener {
                        address: listen_address.clone(),
                        setting: "read/write permissions",
                    })?;

                listener
            }
            _ => {
                return Err(ListenerError::InvalidConfiguration {
                    reason: "only TCP and Unix listen addresses are supported",
                })
            }
        };

        Ok(Self { listen_address, inner })
    }

    /// Gets a reference to the listen address.
    pub fn listen_address(&self) -> &ListenAddress {
        &self.listen_address
    }

    /// Accepts a new connection from the listener.
    ///
    /// ## Errors
    ///
    /// If the listener fails to accept a new connection, or if the accepted connection cannot be configured correctly,
    /// an error is returned.
    pub async fn accept(&mut self) -> Result<Connection, ListenerError> {
        match &mut self.inner {
            ConnectionOrientedListenerInner::Tcp(tcp) => tcp
                .accept()
                .await
                .map(|(stream, addr)| Connection::Tcp(stream, addr))
                .context(FailedToAccept {
                    address: self.listen_address.clone(),
                }),
            #[cfg(unix)]
            ConnectionOrientedListenerInner::Unix(unix) => unix
                .accept()
                .await
                .context(FailedToAccept {
                    address: self.listen_address.clone(),
                })
                .and_then(|(socket, _)| {
                    enable_uds_socket_credentials(&socket).context(FailedToConfigureStream {
                        setting: "SO_PASSCRED",
                        stream_type: "UDS (stream)",
                    })?;
                    Ok(Connection::Unix(socket))
                }),
        }
    }
}
