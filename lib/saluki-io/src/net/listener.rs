use std::{future::pending, io};

use tokio::net::{TcpListener, UdpSocket};

use super::{
    addr::ListenAddress,
    stream::Stream,
    unix::{configure_unix_socket, ensure_unix_socket_free},
};

enum ListenerInner {
    Tcp(TcpListener),
    Udp(Option<UdpSocket>),
    #[cfg(unix)]
    Unixgram(Option<tokio::net::UnixDatagram>),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

pub struct Listener {
    inner: ListenerInner,
}

impl Listener {
    pub async fn from_listen_address(listen_address: ListenAddress) -> Result<Self, io::Error> {
        let inner = match listen_address {
            ListenAddress::Tcp(addr) => TcpListener::bind(addr).await.map(ListenerInner::Tcp),
            ListenAddress::Udp(addr) => UdpSocket::bind(addr).await.map(Some).map(ListenerInner::Udp),
            #[cfg(unix)]
            ListenAddress::Unixgram(addr) => {
                ensure_unix_socket_free(&addr).await?;
                tokio::net::UnixDatagram::bind(addr)
                    .map(Some)
                    .map(ListenerInner::Unixgram)
            }
            #[cfg(unix)]
            ListenAddress::Unix(addr) => {
                ensure_unix_socket_free(&addr).await?;
                tokio::net::UnixListener::bind(addr).map(ListenerInner::Unix)
            }
        };

        Ok(Self { inner: inner? })
    }

    pub async fn accept(&mut self) -> io::Result<Stream> {
        match &mut self.inner {
            ListenerInner::Tcp(tcp) => tcp.accept().await.map(Into::into),
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
                    configure_unix_socket(&socket)?;
                    Ok(socket.into())
                } else {
                    pending().await
                }
            }
            #[cfg(unix)]
            ListenerInner::Unix(unix) => unix.accept().await.and_then(|(socket, _)| {
                configure_unix_socket(&socket)?;
                Ok(socket.into())
            }),
        }
    }
}
