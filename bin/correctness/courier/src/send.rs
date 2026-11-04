use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::net::{bind_udp, connect_tcp};
use crate::{Report, Target};

pub(crate) async fn send(to: &Target, payload: &[u8]) -> Report {
    let result = async {
        match to {
            Target::Tcp { host, port } => {
                let stream = connect_tcp(host, *port, to).await?;
                write_stream(stream, to, payload).await?;
            }
            Target::Udp { host, port } => {
                let (socket, address) = bind_udp(host, *port, to).await?;
                let written = socket
                    .send_to(payload, address)
                    .await
                    .map_err(|error| Report::SendFailed {
                        reason: format!("{to}: {error}"),
                    })?;
                check_datagram_length(to, written, payload.len())?;
            }
            #[cfg(windows)]
            Target::NamedPipe { name } => {
                let mut pipe = crate::net::connect_named_pipe(name, to).await?;
                let result = async {
                    pipe.write_all(payload).await?;
                    // Named-pipe writes can return while Mio still owns buffered bytes; Tokio's flush is a no-op.
                    // A further write waits for that operation and reports its error before the courier exits.
                    pipe.write(&[]).await.map(|_| ())
                }
                .await;
                result.map_err(|error| Report::SendFailed {
                    reason: format!("{to}: {error}"),
                })?;
            }
            #[cfg(not(windows))]
            Target::NamedPipe { .. } => {
                return Err(Report::Failed {
                    reason: crate::net::UNSUPPORTED_PIPE_REASON.into(),
                })
            }
            #[cfg(unix)]
            Target::UnixStream { path } => {
                let stream = crate::net::connect_unix(path, to).await?;
                write_stream(stream, to, payload).await?;
            }
            #[cfg(unix)]
            Target::UnixDatagram { path } => {
                let socket = crate::net::connect_unix_datagram(path, to)?;
                let written = socket.send(payload).await.map_err(|error| Report::SendFailed {
                    reason: format!("{to}: {error}"),
                })?;
                check_datagram_length(to, written, payload.len())?;
            }
            #[cfg(not(unix))]
            Target::UnixStream { .. } | Target::UnixDatagram { .. } => {
                return Err(Report::Failed {
                    reason: crate::net::UNSUPPORTED_UNIX_REASON.into(),
                });
            }
        }
        Ok(Report::Sent {
            bytes: payload.len() as u64,
        })
    }
    .await;
    result.unwrap_or_else(|report| report)
}

async fn write_stream(mut stream: impl AsyncWrite + Unpin, to: &Target, payload: &[u8]) -> Result<(), Report> {
    let result = async {
        stream.write_all(payload).await?;
        stream.shutdown().await
    }
    .await;
    result.map_err(|error| Report::SendFailed {
        reason: format!("{to}: {error}"),
    })
}

fn check_datagram_length(to: &Target, written: usize, expected: usize) -> Result<(), Report> {
    if written == expected {
        Ok(())
    } else {
        Err(Report::SendFailed {
            reason: format!("{to}: datagram write accepted {written} of {expected} bytes"),
        })
    }
}
