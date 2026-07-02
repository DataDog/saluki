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
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, ReadBuf},
    net::{TcpStream, UdpSocket},
};

use super::addr::{ConnectionAddress, ProcessIdentity};
#[cfg(unix)]
use super::unix::{unix_recvmsg, unixgram_recvmsg};

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

    /// A Windows named pipe in byte stream mode.
    #[cfg(windows)]
    NamedPipe(#[pin] NamedPipeServer),
}

impl Connection {
    async fn receive<B: BufMut>(&mut self, buf: &mut B) -> io::Result<(usize, ConnectionAddress)> {
        match self {
            Self::Tcp(inner, addr) => inner.read_buf(buf).await.map(|n| (n, (*addr).into())),
            #[cfg(unix)]
            Self::Unix(inner) => unix_recvmsg(inner, buf).await,
            #[cfg(windows)]
            Self::NamedPipe(inner) => inner
                .read_buf(buf)
                .await
                .map(|n| (n, ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable))),
        }
    }

    pub(super) fn remote_addr(&self) -> ConnectionAddress {
        match self {
            Self::Tcp(_, addr) => ConnectionAddress::SocketLike(*addr),
            #[cfg(unix)]
            Self::Unix(_) => ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable),
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
            ConnectionProjected::Unix(inner) => inner.poll_read(cx, buf),
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
            ConnectionProjected::Unix(inner) => inner.poll_write(cx, buf),
            #[cfg(windows)]
            ConnectionProjected::NamedPipe(inner) => inner.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProjected::Tcp(inner, _) => inner.poll_flush(cx),
            #[cfg(unix)]
            ConnectionProjected::Unix(inner) => inner.poll_flush(cx),
            #[cfg(windows)]
            ConnectionProjected::NamedPipe(inner) => inner.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            ConnectionProjected::Tcp(inner, _) => inner.poll_shutdown(cx),
            #[cfg(unix)]
            ConnectionProjected::Unix(inner) => inner.poll_shutdown(cx),
            #[cfg(windows)]
            ConnectionProjected::NamedPipe(inner) => inner.poll_shutdown(cx),
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
/// `Stream` provides an abstraction over connectionless and connection-oriented network sockets. In many cases, it's
/// not required to know the exact socket family (for example, TCP, UDP, Unix domain socket) that's being used, and it can be
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
                Connection::Unix(inner) => socket2::SockRef::from(inner).recv_buffer_size(),
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
