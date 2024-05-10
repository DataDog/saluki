use std::{future::pending, io};

use snafu::{ResultExt as _, Snafu};
use tokio::net::{TcpListener, UdpSocket};

use super::{
    addr::ListenAddress,
    stream::Stream,
    unix::{enable_uds_socket_credentials, ensure_unix_socket_free, set_unix_socket_write_only},
};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum ListenerError {
    #[snafu(display("failed to bind to listen address {}: {}", address, source))]
    FailedToBind { address: ListenAddress, source: io::Error },

    #[snafu(display("failed to configure {} for listener on address {}: {}", setting, address, source))]
    FailedToConfigureListener {
        address: ListenAddress,
        setting: &'static str,
        source: io::Error,
    },

    #[snafu(display("failed to configure {} for {} stream: {}", setting, stream_type, source))]
    FailedToConfigureStream {
        setting: &'static str,
        stream_type: &'static str,
        source: io::Error,
    },

    #[snafu(display("failed to accept new stream for listener on address {}: {}", address, source))]
    FailedToAccept { address: ListenAddress, source: io::Error },
}

enum ListenerInner {
    Tcp(TcpListener),
    Udp(Option<UdpSocket>),
    #[cfg(unix)]
    Unixgram(Option<tokio::net::UnixDatagram>),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
}

pub struct Listener {
    listen_address: ListenAddress,
    inner: ListenerInner,
}

impl Listener {
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

    pub fn listen_address(&self) -> &ListenAddress {
        &self.listen_address
    }

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
