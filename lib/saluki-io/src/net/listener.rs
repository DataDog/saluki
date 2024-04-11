use std::{future::pending, io};

use tokio::net::{TcpListener, UdpSocket};

use super::{addr::ListenAddress, stream::Stream};

enum ListenerInner {
    Tcp(TcpListener),
    Udp(Option<UdpSocket>),
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
            ListenAddress::Unix(addr) => {
                let addr = ensure_unix_socket_free(addr).await?;
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
            ListenerInner::Unix(unix) => unix.accept().await.map(Into::into),
        }
    }
}

#[cfg(unix)]
async fn ensure_unix_socket_free(addr: std::path::PathBuf) -> io::Result<std::path::PathBuf> {
    use std::os::unix::fs::FileTypeExt;

    use tracing::trace;

    // If the socket file already exists, we need to make sure it's already a UNIX socket, which means we're "clear" to
    // remove it before trying to claim it again when we create our listener.
    match tokio::fs::metadata(&addr).await {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "path already exists and is not a UNIX socket",
                ));
            }

            tokio::fs::remove_file(&addr).await?;

            trace!(
                socket_path = addr.to_string_lossy().as_ref(),
                "Cleared existing UNIX socket."
            );
        }
        Err(err) => {
            // If we can't find the file, that's good: nothing to clean up.
            //
            // Otherwise, forward the error.
            if err.kind() != io::ErrorKind::NotFound {
                return Err(err);
            }
        }
    }

    Ok(addr)
}
