//! Network listeners.
use std::{future::pending, io, net::SocketAddr};

use snafu::{ResultExt as _, Snafu};
use socket2::SockRef;
use tokio::net::{TcpListener, UdpSocket as TokioUdpSocket};

use super::{
    addr::ListenAddress,
    stream::{Connection, Stream},
    unix::{enable_uds_socket_credentials, ensure_unix_socket_free, set_unix_socket_write_only},
};

const SOCKET_RECV_BUFFER_SIZE_SETTING: &str = "SO_RCVBUF";

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
    Udp(Option<TokioUdpSocket>),
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
/// For listeners on connection-oriented address families (for example, TCP, Unix domain sockets in stream mode), the listener
/// will listen for an accept new connections in the typical fashion. However, for connectionless address families
/// (for example, UDP, Unix domain sockets in datagram mode), there is no concept of a "connection" and so nothing to be
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
    socket_receive_buffer_size: Option<usize>,
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
            ListenAddress::Udp(addr) => TokioUdpSocket::bind(addr)
                .await
                .map(Some)
                .map(ListenerInner::Udp)
                .context(FailedToBind {
                    address: listen_address.clone(),
                })?,
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

        Ok(Self {
            listen_address,
            inner,
            socket_receive_buffer_size: None,
        })
    }

    /// Sets the socket receive buffer size for this listener.
    ///
    /// The receive buffer size applies to accepted streams. `None` keeps the OS default.
    pub fn with_receive_buffer_size(mut self, socket_receive_buffer_size: Option<usize>) -> Self {
        self.socket_receive_buffer_size = socket_receive_buffer_size;
        self
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
        let stream_type = self.listen_address.listener_type();
        match &mut self.inner {
            ListenerInner::Tcp(tcp) => {
                let (socket, addr) = tcp.accept().await.context(FailedToAccept {
                    address: self.listen_address.clone(),
                })?;
                configure_stream_socket_receive_buffer_size(&socket, self.socket_receive_buffer_size, stream_type)?;
                Ok((socket, addr).into())
            }
            ListenerInner::Udp(udp) => {
                // TODO: We only emit a single stream here, but we _could_ do something like an internal configuration
                // to allow for multiple streams to be emitted, where the socket is bound via SO_REUSEPORT and then we
                // get load balancing between the sockets.... basically make it possible to parallelize UDP handling if
                // that's a thing we want to do.
                if let Some(socket) = udp.take() {
                    configure_stream_socket_receive_buffer_size(&socket, self.socket_receive_buffer_size, stream_type)?;
                    Ok(socket.into())
                } else {
                    pending().await
                }
            }
            #[cfg(unix)]
            ListenerInner::Unixgram(unix) => {
                if let Some(socket) = unix.take() {
                    configure_stream_socket_receive_buffer_size(&socket, self.socket_receive_buffer_size, stream_type)?;
                    enable_uds_socket_credentials(&socket).context(FailedToConfigureStream {
                        setting: "SO_PASSCRED",
                        stream_type,
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
                    configure_stream_socket_receive_buffer_size(&socket, self.socket_receive_buffer_size, stream_type)?;
                    enable_uds_socket_credentials(&socket).context(FailedToConfigureStream {
                        setting: "SO_PASSCRED",
                        stream_type,
                    })?;
                    Ok(socket.into())
                }),
        }
    }
}

fn configure_stream_socket_receive_buffer_size<'sock, S>(
    socket: &'sock S, recv_buffer_size: Option<usize>, stream_type: &'static str,
) -> Result<(), ListenerError>
where
    SockRef<'sock>: From<&'sock S>,
{
    if let Some(size) = recv_buffer_size {
        SockRef::from(socket)
            .set_recv_buffer_size(size)
            .context(FailedToConfigureStream {
                setting: SOCKET_RECV_BUFFER_SIZE_SETTING,
                stream_type,
            })?;
    }

    Ok(())
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

    /// Returns the actual local socket address this listener is bound to.
    ///
    /// This is useful when binding to port `0` to discover the ephemeral port assigned by the OS.
    pub fn local_addr(&self) -> Result<SocketAddr, ListenerError> {
        match &self.inner {
            ConnectionOrientedListenerInner::Tcp(tcp) => tcp.local_addr().context(FailedToConfigureListener {
                address: self.listen_address.clone(),
                setting: "local_addr",
            }),
            #[cfg(unix)]
            ConnectionOrientedListenerInner::Unix(_) => Err(ListenerError::InvalidConfiguration {
                reason: "local_addr is not supported for Unix listeners",
            }),
        }
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
                    let stream_type = self.listen_address.listener_type();
                    enable_uds_socket_credentials(&socket).context(FailedToConfigureStream {
                        setting: "SO_PASSCRED",
                        stream_type,
                    })?;
                    Ok(Connection::Unix(socket))
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::BytesMut;
    use tokio::{net::TcpStream, time::timeout};

    use super::*;

    const REQUESTED_RECV_BUFFER_SIZE: usize = 131_072;
    const TEST_PACKET: &[u8] = b"hello";

    #[tokio::test]
    async fn zero_receive_buffer_size_preserves_udp_default() {
        let default_address = ListenAddress::Udp(([127, 0, 0, 1], 0).into());
        let mut default_listener = Listener::from_listen_address(default_address)
            .await
            .expect("default listener should bind");
        let default_stream = default_listener
            .accept()
            .await
            .expect("default stream should be accepted");
        let default_recv_buffer_size = default_stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");

        let zero_address = ListenAddress::Udp(([127, 0, 0, 1], 0).into());
        let mut zero_listener = Listener::from_listen_address(zero_address)
            .await
            .expect("zero-sized listener should bind")
            .with_receive_buffer_size(None);
        let zero_stream = zero_listener
            .accept()
            .await
            .expect("zero-sized stream should be accepted");
        let zero_recv_buffer_size = zero_stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");

        assert_eq!(zero_recv_buffer_size, default_recv_buffer_size);
    }

    #[tokio::test]
    async fn udp_listener_sets_receive_buffer_size() {
        let address = ListenAddress::Udp(([127, 0, 0, 1], 0).into());
        let mut listener = Listener::from_listen_address(address)
            .await
            .expect("listener should bind")
            .with_receive_buffer_size(Some(REQUESTED_RECV_BUFFER_SIZE));
        let local_addr = udp_local_addr(&listener).expect("local addr should be available");

        let sender = TokioUdpSocket::bind("127.0.0.1:0").await.expect("sender should bind");
        sender
            .send_to(TEST_PACKET, local_addr)
            .await
            .expect("packet should send");

        let mut stream = listener.accept().await.expect("listener should accept UDP stream");
        let actual_recv_buffer_size = stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");
        assert!(
            actual_recv_buffer_size >= REQUESTED_RECV_BUFFER_SIZE,
            "expected receive buffer size >= {REQUESTED_RECV_BUFFER_SIZE}, got {actual_recv_buffer_size}"
        );

        let mut buffer = BytesMut::with_capacity(TEST_PACKET.len());
        let (received, _) = timeout(Duration::from_secs(1), stream.receive(&mut buffer))
            .await
            .expect("receive should not time out")
            .expect("packet should receive");

        assert_eq!(received, TEST_PACKET.len());
        assert_eq!(&buffer[..], TEST_PACKET);
    }

    #[tokio::test]
    async fn tcp_listener_sets_receive_buffer_size() {
        let address = ListenAddress::Tcp(([127, 0, 0, 1], 0).into());
        let mut listener = Listener::from_listen_address(address)
            .await
            .expect("listener should bind")
            .with_receive_buffer_size(Some(REQUESTED_RECV_BUFFER_SIZE));
        let local_addr = tcp_local_addr(&listener).expect("local addr should be available");

        let client = tokio::spawn(async move { TcpStream::connect(local_addr).await });
        let stream = timeout(Duration::from_secs(1), listener.accept())
            .await
            .expect("accept should not time out")
            .expect("listener should accept TCP stream");
        client
            .await
            .expect("client task should complete")
            .expect("client should connect");

        let actual_recv_buffer_size = stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");
        assert!(
            actual_recv_buffer_size >= REQUESTED_RECV_BUFFER_SIZE,
            "expected receive buffer size >= {REQUESTED_RECV_BUFFER_SIZE}, got {actual_recv_buffer_size}"
        );
        assert!(!stream.is_connectionless());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unixgram_listener_sets_receive_buffer_size() {
        let temp_dir = tempfile::tempdir().expect("temp dir should be created");
        let socket_path = temp_dir.path().join("dogstatsd.sock");
        let address = ListenAddress::Unixgram(socket_path.clone());
        let mut listener = Listener::from_listen_address(address)
            .await
            .expect("listener should bind")
            .with_receive_buffer_size(Some(REQUESTED_RECV_BUFFER_SIZE));

        let sender = tokio::net::UnixDatagram::unbound().expect("sender should be created");
        sender
            .send_to(TEST_PACKET, &socket_path)
            .await
            .expect("packet should send");

        let mut stream = listener
            .accept()
            .await
            .expect("listener should accept UDS datagram stream");
        let actual_recv_buffer_size = stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");
        assert!(
            actual_recv_buffer_size >= REQUESTED_RECV_BUFFER_SIZE,
            "expected receive buffer size >= {REQUESTED_RECV_BUFFER_SIZE}, got {actual_recv_buffer_size}"
        );

        let mut buffer = BytesMut::with_capacity(TEST_PACKET.len());
        let (received, _) = timeout(Duration::from_secs(1), stream.receive(&mut buffer))
            .await
            .expect("receive should not time out")
            .expect("packet should receive");

        assert_eq!(received, TEST_PACKET.len());
        assert_eq!(&buffer[..], TEST_PACKET);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_stream_listener_accepts_with_receive_buffer_size() {
        let temp_dir = tempfile::tempdir().expect("temp dir should be created");
        let socket_path = temp_dir.path().join("dogstatsd-stream.sock");
        let address = ListenAddress::Unix(socket_path.clone());
        let mut listener = Listener::from_listen_address(address)
            .await
            .expect("listener should bind")
            .with_receive_buffer_size(Some(REQUESTED_RECV_BUFFER_SIZE));

        let client = tokio::spawn(async move { tokio::net::UnixStream::connect(socket_path).await });
        let stream = timeout(Duration::from_secs(1), listener.accept())
            .await
            .expect("accept should not time out")
            .expect("listener should accept UDS stream");
        client
            .await
            .expect("client task should complete")
            .expect("client should connect");

        let actual_recv_buffer_size = stream
            .recv_buffer_size()
            .expect("receive buffer size should be available");
        assert!(
            actual_recv_buffer_size >= REQUESTED_RECV_BUFFER_SIZE,
            "expected receive buffer size >= {REQUESTED_RECV_BUFFER_SIZE}, got {actual_recv_buffer_size}"
        );
        assert!(!stream.is_connectionless());
    }

    fn tcp_local_addr(listener: &Listener) -> io::Result<SocketAddr> {
        match &listener.inner {
            ListenerInner::Tcp(tcp) => tcp.local_addr(),
            _ => panic!("expected TCP listener"),
        }
    }

    fn udp_local_addr(listener: &Listener) -> io::Result<SocketAddr> {
        match &listener.inner {
            ListenerInner::Udp(Some(socket)) => socket.local_addr(),
            _ => panic!("expected UDP listener"),
        }
    }
}
