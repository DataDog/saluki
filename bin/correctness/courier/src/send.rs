use std::{io, net::SocketAddr, time::Duration};

use snafu::ResultExt as _;
use tokio::{io::AsyncWriteExt as _, net::UdpSocket};

use crate::{net, Address, Error, HostPort, TargetSnafu, UnsupportedSnafu};

/// Writes `payload` to `address` as given: one connection, or one datagram.
///
/// A `tcp`, `unix` or `pipe` send connects, writes the whole payload and closes the connection. A `udp` or `unixgram`
/// send sends the payload as one datagram.
///
/// `limit` bounds the call, but a name lookup it cuts short keeps running on its blocking thread until the resolver
/// answers.
///
/// # Errors
///
/// Fails with [`Error::Unsupported`] for an `http` or `grpc` address, which takes requests and not raw bytes, and for
/// a socket or pipe this OS does not have. Fails with [`Error::Target`] when the connection or the write fails,
/// including a datagram sent short, and [`Error::TimedOut`] when `limit` passes.
pub async fn send(address: &Address, payload: &[u8], limit: Duration) -> Result<(), Error> {
    let unsupported = |reason| {
        UnsupportedSnafu {
            operation: "send",
            address: address.clone(),
            reason,
        }
        .fail()
    };
    let context = || TargetSnafu {
        action: "send to",
        target: address.to_string(),
    };
    net::within("send", address, limit, async {
        match address {
            Address::Tcp(host_port) => {
                let mut stream = net::connect_tcp(host_port).await?;
                stream.write_all(payload).await.with_context(|_| context())?;
                stream.shutdown().await.with_context(|_| context())
            }
            Address::Udp(host_port) => {
                let sent = send_udp(host_port, payload).await?;
                check_whole(sent, payload).with_context(|_| context())
            }
            Address::Http(_) | Address::Grpc(_) => unsupported("send writes raw bytes; use http for requests"),
            #[cfg(unix)]
            Address::UnixStream(path) => {
                let mut stream = tokio::net::UnixStream::connect(path)
                    .await
                    .with_context(|_| context())?;
                stream.write_all(payload).await.with_context(|_| context())?;
                stream.shutdown().await.with_context(|_| context())
            }
            #[cfg(unix)]
            Address::UnixDatagram(path) => {
                let socket = tokio::net::UnixDatagram::unbound().with_context(|_| context())?;
                let sent = socket.send_to(payload, path).await.with_context(|_| context())?;
                check_whole(sent, payload).with_context(|_| context())
            }
            #[cfg(not(unix))]
            Address::UnixStream(_) | Address::UnixDatagram(_) => unsupported("this OS has no Unix sockets"),
            #[cfg(windows)]
            Address::NamedPipe(name) => {
                let mut pipe = net::open_pipe(name).with_context(|_| context())?;
                pipe.write_all(payload).await.with_context(|_| context())?;
                pipe.flush().await.with_context(|_| context())
            }
            #[cfg(not(windows))]
            Address::NamedPipe(_) => unsupported("named pipes exist only on Windows"),
        }
    })
    .await
}

/// Sends one datagram to the first resolved address, from a socket of the same address family.
async fn send_udp(host_port: &HostPort, payload: &[u8]) -> Result<usize, Error> {
    let addr = net::resolve(host_port).await?[0];
    let local: SocketAddr = match addr {
        SocketAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        SocketAddr::V6(_) => ([0u16; 8], 0).into(),
    };
    let context = || TargetSnafu {
        action: "send to",
        target: host_port.to_string(),
    };
    let socket = UdpSocket::bind(local).await.with_context(|_| context())?;
    socket.send_to(payload, addr).await.with_context(|_| context())
}

fn check_whole(sent: usize, payload: &[u8]) -> io::Result<()> {
    if sent == payload.len() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("sent {sent} of {} bytes in one datagram", payload.len()),
        ))
    }
}
