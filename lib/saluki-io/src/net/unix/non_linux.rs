use std::io;

use bytes::BufMut;
use socket2::{MaybeUninitSlice, MsgHdrMut, SockAddr, SockAddrStorage, SockRef};

use crate::net::addr::{ConnectionAddress, ProcessCredentialsError, ProcessIdentity};

pub fn enable_uds_socket_credentials<'sock, S>(_socket: &'sock S) -> io::Result<()>
where
    SockRef<'sock>: From<&'sock S>,
{
    Ok(())
}

pub(super) fn uds_recvmsg<'sock, S, B: BufMut>(socket: &'sock S, buf: &mut B) -> io::Result<(usize, ConnectionAddress)>
where
    SockRef<'sock>: From<&'sock S>,
{
    let sock_ref = SockRef::from(socket);

    // Create the message header struct that will be populated by the call to `recvmsg`, which includes the peer
    // address, message data, and any ancillary (out-of-band) data.
    //
    // SAFETY: We're allocating `sockaddr_storage`, which is always large enough to hold any address family's socket
    // address structure.
    let sock_storage = SockAddrStorage::zeroed();
    let sock_storage_len = sock_storage.size_of();
    let mut sock_addr = unsafe { SockAddr::new(sock_storage, sock_storage_len) };

    let data_buf = unsafe { MaybeUninitSlice::new(buf.chunk_mut().as_uninit_slice_mut()) };
    let mut data_bufs = [data_buf];

    let mut msg_hdr = MsgHdrMut::new().with_addr(&mut sock_addr).with_buffers(&mut data_bufs);

    let n = sock_ref.recvmsg(&mut msg_hdr, 0)?;

    // Finally, update our buffer to reflect the bytes we've read.
    unsafe {
        buf.advance_mut(n);
    }

    Ok((
        n,
        ConnectionAddress::ProcessLike(ProcessIdentity::Error(ProcessCredentialsError::UnsupportedPlatform)),
    ))
}

/// Returns `true` if `SO_REUSEPORT` is supported for UDP sockets on the current platform.
pub fn socket_reuseport_supported() -> bool {
    false
}

/// Stub — replay packet sending is Linux-only because it depends on `SCM_CREDENTIALS`.
pub async fn send_replay_packet(
    _socket: &tokio::net::UnixDatagram, _payload: &[u8], _captured_pid: i32, _replay_gid: u32,
) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "DogStatsD replay is only supported on Linux",
    ))
}
