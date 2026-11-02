use panoramic_courier::{Report, Target};

use crate::net::{bind_udp, connect_tcp};

pub(super) async fn probe(to: &Target) -> Report {
    let result = async {
        match to {
            Target::Tcp { host, port } => {
                connect_tcp(host, *port, to).await?;
                Ok(Report::Listening)
            }
            Target::Udp { host, port } => {
                let (socket, address) = bind_udp(host, *port, to).await?;
                socket.connect(address).await.map_err(|error| Report::Unreachable {
                    reason: format!("{to}: {error}"),
                })?;
                Ok(Report::Addressable)
            }
            #[cfg(unix)]
            Target::UnixStream { path } => {
                crate::net::connect_unix(path, to).await?;
                Ok(Report::Listening)
            }
            #[cfg(unix)]
            Target::UnixDatagram { path } => {
                crate::net::connect_unix_datagram(path, to)?;
                Ok(Report::Listening)
            }
            #[cfg(not(unix))]
            Target::UnixStream { .. } | Target::UnixDatagram { .. } => Err(Report::Failed {
                reason: crate::net::UNSUPPORTED_UNIX_REASON.into(),
            }),
        }
    }
    .await;
    result.unwrap_or_else(|report| report)
}
