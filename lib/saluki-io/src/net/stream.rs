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

#[pin_project(project = ConnectionProjected)]
pub enum Connection {
    Tcp(#[pin] TcpStream, SocketAddr),
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

enum Connectionless {
    Udp(UdpSocket),
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

pub struct Stream {
    inner: StreamInner,
}

impl Stream {
    pub fn is_connectionless(&self) -> bool {
        matches!(self.inner, StreamInner::Connectionless { .. })
    }

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
