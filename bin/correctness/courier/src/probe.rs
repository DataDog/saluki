use std::time::Duration;

use crate::{net, Address, Error, UnsupportedSnafu};

/// Checks that something accepts a connection at `address`.
///
/// A `tcp`, `http` or `grpc` probe opens a TCP connection and closes it. A `unix` probe connects to the stream
/// socket, and a `unixgram` probe connects a datagram socket to it, which fails unless a socket is bound at the path.
/// A `pipe` probe opens the named pipe, and counts a pipe that is busy with other clients as present.
///
/// `limit` bounds the call, but a name lookup it cuts short keeps running on its blocking thread until the resolver
/// answers.
///
/// # Errors
///
/// Fails with [`Error::Unsupported`] for a `udp` address, which a probe cannot confirm, and for a socket or pipe this
/// OS does not have. Fails with [`Error::Target`] when nothing accepts, and [`Error::TimedOut`] when `limit` passes.
pub async fn probe(address: &Address, limit: Duration) -> Result<(), Error> {
    let unsupported = |reason| {
        UnsupportedSnafu {
            operation: "probe",
            address: address.clone(),
            reason,
        }
        .fail()
    };
    net::within("probe", address, limit, async {
        match address {
            Address::Tcp(host_port) | Address::Http(host_port) | Address::Grpc(host_port) => {
                net::connect_tcp(host_port).await.map(drop)
            }
            Address::Udp(_) => unsupported("a probe cannot confirm a UDP listener"),
            #[cfg(unix)]
            Address::UnixStream(path) => unix::connect_stream(path).await,
            #[cfg(unix)]
            Address::UnixDatagram(path) => unix::connect_datagram(path),
            #[cfg(not(unix))]
            Address::UnixStream(_) | Address::UnixDatagram(_) => unsupported("this OS has no Unix sockets"),
            #[cfg(windows)]
            Address::NamedPipe(name) => pipe(address, name),
            #[cfg(not(windows))]
            Address::NamedPipe(_) => unsupported("named pipes exist only on Windows"),
        }
    })
    .await
}

#[cfg(windows)]
fn pipe(address: &Address, name: &str) -> Result<(), Error> {
    use snafu::ResultExt as _;

    // Win32 ERROR_PIPE_BUSY: the pipe exists and every instance has a client.
    const ERROR_PIPE_BUSY: i32 = 231;

    match net::open_pipe(name) {
        Ok(_) => Ok(()),
        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => Ok(()),
        Err(e) => Err(e).context(crate::TargetSnafu {
            action: "open",
            target: address.to_string(),
        }),
    }
}

#[cfg(unix)]
mod unix {
    use std::path::Path;

    use snafu::ResultExt as _;
    use tokio::net::{UnixDatagram, UnixStream};

    use crate::{Error, TargetSnafu};

    pub(super) async fn connect_stream(path: &Path) -> Result<(), Error> {
        UnixStream::connect(path).await.map(drop).context(TargetSnafu {
            action: "connect to",
            target: path.display().to_string(),
        })
    }

    pub(super) fn connect_datagram(path: &Path) -> Result<(), Error> {
        let context = || TargetSnafu {
            action: "connect to",
            target: path.display().to_string(),
        };
        let socket = UnixDatagram::unbound().with_context(|_| context())?;
        socket.connect(path).with_context(|_| context())
    }
}
