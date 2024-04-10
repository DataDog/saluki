use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::BufMut;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, ReadBuf},
    net::{unix::SocketAddr as UnixSocketAddr, TcpStream, UdpSocket, UnixStream},
};

use super::addr::ConnectionAddress;

enum Connection {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl AsyncRead for Connection {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Unix(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

enum Connectionless {
    Udp(UdpSocket),
}

impl Connectionless {
    async fn receive<B: BufMut>(&mut self, buf: &mut B) -> io::Result<(usize, SocketAddr)> {
        match self {
            Self::Udp(inner) => inner.recv_buf_from(buf).await,
        }
    }
}

enum StreamInner {
    Connection {
        conn: Connection,
        remote_addr: ConnectionAddress,
    },
    Connectionless {
        socket: Connectionless,
    },
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
            StreamInner::Connection { conn, remote_addr } => {
                let bytes_read = conn.read_buf(buf).await?;
                let remote_addr = remote_addr.clone();
                Ok((bytes_read, remote_addr))
            }
            StreamInner::Connectionless { socket } => socket.receive(buf).await.map(|(n, addr)| (n, addr.into())),
        }
    }
}

impl From<(TcpStream, SocketAddr)> for Stream {
    fn from((stream, remote_addr): (TcpStream, SocketAddr)) -> Self {
        Self {
            inner: StreamInner::Connection {
                conn: Connection::Tcp(stream),
                remote_addr: remote_addr.into(),
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

impl From<(UnixStream, UnixSocketAddr)> for Stream {
    fn from((stream, remote_addr): (UnixStream, UnixSocketAddr)) -> Self {
        Self {
            inner: StreamInner::Connection {
                conn: Connection::Unix(stream),
                remote_addr: remote_addr.into(),
            },
        }
    }
}
