use std::{io, os::unix::fs::FileTypeExt as _, path::Path};

use socket2::SockRef;
use tracing::trace;

mod ancillary;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use self::linux::set_uds_passcred;

mod recvmsg;
pub(super) use self::recvmsg::{unix_recvmsg, unixgram_recvmsg};

#[cfg(not(target_os = "linux"))]
mod non_linux;
#[cfg(not(target_os = "linux"))]
use self::non_linux::set_uds_passcred;

/// Configures a Unix domain socket for use.
///
/// This sets any OS-specific configuration/options on the socket to ensure it's ready for use.
pub(super) fn configure_unix_socket<'sock, S>(socket: &'sock S) -> io::Result<()>
where
    SockRef<'sock>: From<&'sock S>,
{
    set_uds_passcred(socket)
}

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
                    "path already exists and is not a UNIX socket",
                ));
            }

            tokio::fs::remove_file(path).await?;

            trace!(
                socket_path = path.to_string_lossy().as_ref(),
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

    Ok(())
}
