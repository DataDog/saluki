use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::BufMut;
use pin_project::pin_project;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(unix)]
use tokio::net::{UnixDatagram, UnixStream};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, ReadBuf},
    net::{TcpStream, UdpSocket},
};
use tonic::transport::server::Connected;

use super::addr::ConnectionAddress;
#[cfg(unix)]
use super::unix::unixgram_recvmsg;
use crate::net::ProcessIdentity;

/// A connection-oriented socket.
///
/// This type wraps network sockets that operate in a connection-oriented manner, such as TCP or Unix domain sockets in
/// stream mode.
#[pin_project(project = ConnectionProjected)]
pub enum Connection {
    /// A TCP socket.
    Tcp(#[pin] TcpStream, SocketAddr),

    /// A Unix domain socket in stream mode (SOCK_STREAM).
    #[cfg(unix)]
    Unix(#[pin] tokio::net::UnixStream, ProcessIdentity),

    /// A Windows named pipe in byte stream mode.
    #[cfg(windows)]
    NamedPipe(#[pin] NamedPipeServer),
}

impl Connection {
    async fn receive<B: BufMut>(&mut self, buf: &mut B) -> io::Result<(usize, ConnectionAddress)> {
        match self {
            Self::Tcp(inner, addr) => inner.read_buf(buf).await.map(|n| (n, (*addr).into())),
            #[cfg(unix)]
            Self::Unix(inner, ident) => inner.read_buf(buf).await.map(|n| (n, (*ident).into())),
            #[cfg(windows)]
            Self::NamedPipe(inner) => inner
                .read_buf(buf)
                .await
                .map(|n| (n, ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable))),
        }
    }

    pub(super) fn remote_addr(&self) -> ConnectionAddress {
        match self {
            Self::Tcp(_, addr) => (*addr).into(),
            #[cfg(unix)]
            Self::Unix(_, ident) => (*ident).into(),
            #[cfg(windows)]
            Self::NamedPipe(_) => ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable),
        }
    }
}

impl AsyncRead for Connection {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProjected::Tcp(inner, _) => inner.poll_read(cx, buf),
            #[cfg(unix)]
            ConnectionProjected::Unix(inner, _) => inner.poll_read(cx, buf),
            #[cfg(windows)]
            ConnectionProjected::NamedPipe(inner) => inner.poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.project() {
            ConnectionProjected::Tcp(inner, _) => inner.poll_write(cx, buf),
            #[cfg(unix)]
            ConnectionProjected::Unix(inner, _) => inner.poll_write(cx, buf),
            #[cfg(windows)]
            ConnectionProjected::NamedPipe(inner) => inner.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProjected::Tcp(inner, _) => inner.poll_flush(cx),
            #[cfg(unix)]
            ConnectionProjected::Unix(inner, _) => inner.poll_flush(cx),
            #[cfg(windows)]
            ConnectionProjected::NamedPipe(inner) => inner.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProjected::Tcp(inner, _) => inner.poll_shutdown(cx),
            #[cfg(unix)]
            ConnectionProjected::Unix(inner, _) => inner.poll_shutdown(cx),
            #[cfg(windows)]
            ConnectionProjected::NamedPipe(inner) => inner.poll_shutdown(cx),
        }
    }
}

/// Exposes the remote peer's address to gRPC/HTTP services as a request extension.
///
/// For TCP, this is the peer's socket address. For Unix domain sockets, it's the peer's process identity, captured once
/// from `SO_PEERCRED` when the connection was accepted, and so fixed for the life of the connection.
impl Connected for Connection {
    type ConnectInfo = ConnectionAddress;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.remote_addr()
    }
}

/// A connectionless socket.
///
/// This type wraps network sockets that operate in a connectionless manner, such as UDP or Unix domain sockets in
/// datagram mode.
enum Connectionless {
    /// A UDP socket.
    Udp(UdpSocket),

    /// A Unix domain socket in datagram mode (SOCK_DGRAM).
    #[cfg(unix)]
    Unixgram(tokio::net::UnixDatagram),
}

impl Connectionless {
    async fn receive<B: BufMut>(&mut self, buf: &mut B) -> io::Result<(usize, ConnectionAddress)> {
        match self {
            Self::Udp(inner) => inner.recv_buf_from(buf).await.map(|(n, addr)| (n, addr.into())),
            #[cfg(unix)]
            Self::Unixgram(inner) => unixgram_recvmsg(inner, buf).await,
        }
    }
}

enum StreamInner {
    Connection { socket: Connection },
    Connectionless { socket: Connectionless },
}

/// A network stream.
///
/// `Stream` provides an abstraction over connectionless and connection-oriented network sockets. In many cases, it's
/// not required to know the exact socket family (for example, TCP, UDP, Unix domain socket) that's being used, and it can be
/// beneficial to allow abstracting over the differences to facilitate simpler code.
///
/// # Connection-oriented mode
///
/// In connection-oriented mode, the stream is backed by a socket that operates in a connection-oriented manner, which
/// ensures a reliable, ordered stream of messages to and from the remote peer.
///
/// The connection address returned when receiving data _should_ be stable for the life of the `Stream`.
///
/// # Connectionless mode
///
/// In connectionless mode, the stream is backed by a socket that operates in a connectionless manner, which doesn't
/// provide any assurances around reliability and ordering of messages to and from the remote peer. While a stream might
/// be backed by a Unix domain socket in datagram mode, which _does_ provide reliability of messages, this can't and
/// shouldn't be relied upon when using `Stream`.
pub struct Stream {
    inner: StreamInner,
}

impl Stream {
    /// Returns `true` if the stream is connectionless.
    pub fn is_connectionless(&self) -> bool {
        matches!(self.inner, StreamInner::Connectionless { .. })
    }

    /// Receives data from the stream.
    ///
    /// On success, returns the number of bytes read and the address from whence the data came.
    ///
    /// ## Errors
    ///
    /// If the underlying system call fails, an error is returned.
    pub async fn receive<B: BufMut>(&mut self, buf: &mut B) -> io::Result<(usize, ConnectionAddress)> {
        match &mut self.inner {
            StreamInner::Connection { socket } => socket.receive(buf).await,
            StreamInner::Connectionless { socket } => socket.receive(buf).await,
        }
    }

    #[cfg(test)]
    pub(crate) fn recv_buffer_size(&self) -> io::Result<usize> {
        match &self.inner {
            StreamInner::Connection { socket } => match socket {
                Connection::Tcp(inner, _) => socket2::SockRef::from(inner).recv_buffer_size(),
                #[cfg(unix)]
                Connection::Unix(inner, _) => socket2::SockRef::from(inner).recv_buffer_size(),
                #[cfg(windows)]
                Connection::NamedPipe(_) => Ok(0),
            },
            StreamInner::Connectionless { socket } => match socket {
                Connectionless::Udp(inner) => socket2::SockRef::from(inner).recv_buffer_size(),
                #[cfg(unix)]
                Connectionless::Unixgram(inner) => socket2::SockRef::from(inner).recv_buffer_size(),
            },
        }
    }
}

impl From<(TcpStream, SocketAddr)> for Stream {
    fn from((stream, remote_addr): (TcpStream, SocketAddr)) -> Self {
        Self {
            inner: StreamInner::Connection {
                socket: Connection::Tcp(stream, remote_addr),
            },
        }
    }
}

impl From<UdpSocket> for Stream {
    fn from(socket: UdpSocket) -> Self {
        Self {
            inner: StreamInner::Connectionless {
                socket: Connectionless::Udp(socket),
            },
        }
    }
}

#[cfg(unix)]
impl From<UnixDatagram> for Stream {
    fn from(socket: UnixDatagram) -> Self {
        Self {
            inner: StreamInner::Connectionless {
                socket: Connectionless::Unixgram(socket),
            },
        }
    }
}

#[cfg(unix)]
impl From<(UnixStream, ProcessIdentity)> for Stream {
    fn from((stream, ident): (UnixStream, ProcessIdentity)) -> Self {
        Self {
            inner: StreamInner::Connection {
                socket: Connection::Unix(stream, ident),
            },
        }
    }
}

#[cfg(windows)]
impl From<NamedPipeServer> for Stream {
    fn from(stream: NamedPipeServer) -> Self {
        Self {
            inner: StreamInner::Connection {
                socket: Connection::NamedPipe(stream),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn connection_connect_info_tcp_peer_address() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener should bind");
        let local_addr = listener.local_addr().expect("listener should have a local address");

        let client = TcpStream::connect(local_addr).await.expect("client should connect");
        let client_addr = client.local_addr().expect("client should have a local address");

        let (socket, peer_addr) = listener.accept().await.expect("listener should accept");
        let connection = Connection::Tcp(socket, peer_addr);

        match connection.connect_info() {
            ConnectionAddress::SocketLike(addr) => assert_eq!(addr, client_addr),
            other => panic!("expected a socket-like address, got {other}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connection_connect_info_uds_peer_address() {
        let temp_dir = tempfile::tempdir().expect("temp dir should be created");
        let socket_path = temp_dir.path().join("connect-info.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("listener should bind");

        let _client = tokio::net::UnixStream::connect(&socket_path)
            .await
            .expect("client should connect");
        let (socket, _) = listener.accept().await.expect("listener should accept");
        let ident = socket
            .peer_cred()
            .map(ProcessIdentity::from)
            .expect("process credentials should be present");
        let connection = Connection::Unix(socket, ident);

        let connect_info = connection.connect_info();
        assert!(connect_info.process_credentials().is_some());

        // Make sure the matched process ID of the "peer" is actually us. Our cast is theoretically lossy
        // but in reality, systems will have a max PID of 4 million or so, so there's practically _zero_
        // risk of somehow over/underflowing when casting from signed to unsigned.
        let peer_creds = connect_info.process_credentials().unwrap();
        assert_eq!(std::process::id(), peer_creds.pid as u32)
    }
}
