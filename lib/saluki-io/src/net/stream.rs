use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::BufMut;
use pin_project::pin_project;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, ReadBuf},
    net::{TcpStream, UdpSocket},
};

use super::{
    addr::ConnectionAddress,
    unix::{unix_recvmsg, unixgram_recvmsg},
};

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
    Unix(#[pin] tokio::net::UnixStream),
}

impl Connection {
    async fn receive<B: BufMut>(&mut self, buf: &mut B) -> io::Result<(usize, ConnectionAddress)> {
        match self {
            Self::Tcp(inner, addr) => inner.read_buf(buf).await.map(|n| (n, (*addr).into())),
            #[cfg(unix)]
            Self::Unix(inner) => unix_recvmsg(inner, buf).await,
        }
    }

    pub(super) fn remote_addr(&self) -> ConnectionAddress {
        match self {
            Self::Tcp(_, addr) => ConnectionAddress::SocketLike(*addr),
            #[cfg(unix)]
            Self::Unix(_) => ConnectionAddress::ProcessLike(None),
        }
    }
}

impl AsyncRead for Connection {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProjected::Tcp(inner, _) => inner.poll_read(cx, buf),
            #[cfg(unix)]
            ConnectionProjected::Unix(inner) => inner.poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.project() {
            ConnectionProjected::Tcp(inner, _) => inner.poll_write(cx, buf),
            #[cfg(unix)]
            ConnectionProjected::Unix(inner) => inner.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProjected::Tcp(inner, _) => inner.poll_flush(cx),
            #[cfg(unix)]
            ConnectionProjected::Unix(inner) => inner.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProjected::Tcp(inner, _) => inner.poll_shutdown(cx),
            #[cfg(unix)]
            ConnectionProjected::Unix(inner) => inner.poll_shutdown(cx),
        }
    }
}

impl tonic::transport::server::Connected for Connection {
    type ConnectInfo = ConnectionAddress;

    fn connect_info(&self) -> Self::ConnectInfo {
        match self {
            Self::Tcp(_, addr) => ConnectionAddress::SocketLike(*addr),
            #[cfg(unix)]
            Self::Unix(_) => ConnectionAddress::ProcessLike(None),
        }
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
/// `Stream` provides an abstraction over connectionless and connection-oriented network sockets. In many cases, it is
/// not required to know the exact socket family (e.g. TCP, UDP, Unix domain socket) that is being used, and it can be
/// beneficial to allow abstracting over the differences to facilitate simpler code.
///
/// ## Connection-oriented mode
///
/// In connection-oriented mode, the stream is backed by a socket that operates in a connection-oriented manner, which
/// ensures a reliable, ordered stream of messages to and from the remote peer.
///
/// The connection address returned when receiving data _should_ be stable for the life of the `Stream`.
///
/// ## Connectionless mode
///
/// In connectionless mode, the stream is backed by a socket that operates in a connectionless manner, which does not
/// provide any assurances around reliability and ordering of messages to and from the remote peer. While a stream might
/// be backed by a Unix domain socket in datagram mode, which _does_ provide reliability of messages, this cannot and
/// should not be relied upon when using `Stream`.
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
impl From<tokio::net::UnixDatagram> for Stream {
    fn from(socket: tokio::net::UnixDatagram) -> Self {
        Self {
            inner: StreamInner::Connectionless {
                socket: Connectionless::Unixgram(socket),
            },
        }
    }
}

#[cfg(unix)]
impl From<tokio::net::UnixStream> for Stream {
    fn from(stream: tokio::net::UnixStream) -> Self {
        Self {
            inner: StreamInner::Connection {
                socket: Connection::Unix(stream),
            },
        }
    }
}
