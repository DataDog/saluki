use std::{
    fs::Permissions,
    io,
    os::unix::fs::{FileTypeExt as _, PermissionsExt as _},
    path::Path,
};

use bytes::BufMut;
use tokio::net::{UnixDatagram, UnixStream};
use tracing::trace;

#[cfg(target_os = "linux")]
mod ancillary;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use self::linux::enable_uds_socket_credentials;
#[cfg(target_os = "linux")]
use self::linux::uds_recvmsg;

#[cfg(not(target_os = "linux"))]
mod non_linux;
#[cfg(not(target_os = "linux"))]
pub use self::non_linux::enable_uds_socket_credentials;
#[cfg(not(target_os = "linux"))]
use self::non_linux::uds_recvmsg;

use super::addr::ConnectionAddress;

/// Ensures that the given path is read for use as a UNIX socket.
///
/// If the path already exists, and is a UNIX socket, it will be removed. If it is not a UNIX socket, an error will be
/// returned.
pub(super) async fn ensure_unix_socket_free<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = path.as_ref();

    // If the socket file already exists, we need to make sure it's already a UNIX socket, which means we're "clear" to
    // remove it before trying to claim it again when we create our listener.
    match tokio::fs::metadata(path).await {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "path already exists and is not a Unix domain socket",
                ));
            }

            tokio::fs::remove_file(path).await?;

            trace!(
                socket_path = path.to_string_lossy().as_ref(),
                "Cleared existing Unix domain socket."
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

    Ok(())
}

/// Sets the UNIX socket at the given path to be write-only by non-owners.
///
/// This ensures that normal clients can write to the socket but not read from it.
pub(super) async fn set_unix_socket_write_only<P: AsRef<Path>>(path: P) -> io::Result<()> {
    tokio::fs::set_permissions(path, Permissions::from_mode(0o722)).await
}

pub async fn unix_recvmsg<B: BufMut>(socket: &mut UnixStream, buf: &mut B) -> io::Result<(usize, ConnectionAddress)> {
    // TODO: We technically don't need to do this for SOCK_STREAM because we can do it once when the
    // connection is accepted, and then just do "normal" reads after that. We do still need to do it
    // for SOCK_DGRAM, though... so we might want to actually consider updating our `socket2` PR to
    // include support for even setting SO_PASSCRED (or the OS-specific equivalent) to also include
    // macOS and FreeBSD (*BSD, really) so that this also works correctly for all
    // `cfg(unix)`-capable platforms.

    // We manually call `recvmsg` on our domain socket as stdlib/`mio`/`tokio` don't yet expose a way to do out-of-band
    // reads to get ancillary data such as the socket credentials used to shuttle origin detection information.
    socket
        .async_io(tokio::io::Interest::READABLE, || uds_recvmsg(socket, buf))
        .await
}

pub async fn unixgram_recvmsg<B: BufMut>(
    socket: &mut UnixDatagram, buf: &mut B,
) -> io::Result<(usize, ConnectionAddress)> {
    // We manually call `recvmsg` on our domain socket as stdlib/`mio`/`tokio` don't yet expose a way to do out-of-band
    // reads to get ancillary data such as the socket credentials used to shuttle origin detection information.
    socket
        .async_io(tokio::io::Interest::READABLE, || uds_recvmsg(socket, buf))
        .await
}
