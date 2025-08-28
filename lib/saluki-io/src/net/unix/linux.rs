use std::io;

use bytes::BufMut;
use socket2::{SockAddr, SockAddrStorage, SockRef};

use super::ancillary::{ControlMessage, SocketCredentialsAncillaryData};
use crate::net::addr::{ConnectionAddress, ProcessCredentials};

/// Enables the `SO_PASSCRED` option on the given socket.
///
/// ## Errors
///
/// If the underlying system call fails, an error is returned.
pub fn enable_uds_socket_credentials<'sock, S>(socket: &'sock S) -> io::Result<()>
where
    SockRef<'sock>: From<&'sock S>,
{
    let sock_ref = socket2::SockRef::from(socket);
    sock_ref.set_passcred(true)
}

pub(super) fn uds_recvmsg<'sock, S, B: BufMut>(socket: &'sock S, buf: &mut B) -> io::Result<(usize, ConnectionAddress)>
where
    SockRef<'sock>: From<&'sock S>,
{
    let sock_ref = socket2::SockRef::from(socket);

    // Create the message header struct that will be populated by the call to `recvmsg`, which includes the peer
    // address, message data, and any ancillary (out-of-band) data.
    //
    // SAFETY: We're allocating `sockaddr_storage`, which is always large enough to hold any address family's socket
    // address structure.
    let sock_storage = SockAddrStorage::zeroed();
    let sock_storage_len = sock_storage.size_of();
    let mut sock_addr = unsafe { SockAddr::new(sock_storage, sock_storage_len) };

    let mut ancillary_data = SocketCredentialsAncillaryData::new();

    let data_buf = unsafe { socket2::MaybeUninitSlice::new(buf.chunk_mut().as_uninit_slice_mut()) };
    let mut data_bufs = [data_buf];

    let mut msg_hdr = socket2::MsgHdrMut::new()
        .with_addr(&mut sock_addr)
        .with_buffers(&mut data_bufs)
        .with_control(ancillary_data.as_mut_uninit());

    let n = sock_ref.recvmsg(&mut msg_hdr, libc::MSG_CMSG_CLOEXEC)?;

    // If we got any socket credentials back, parse them.
    let control_len = msg_hdr.control_len();

    let maybe_process_creds = if control_len > 0 {
        unsafe {
            ancillary_data.set_len(control_len);
            let messages = ancillary_data.messages();
            messages
                .map(|m| match m {
                    ControlMessage::Credentials(creds) => ProcessCredentials {
                        pid: creds.pid,
                        uid: creds.uid,
                        gid: creds.gid,
                    },
                })
                .next()
        }
    } else {
        None
    };

    let conn_addr = ConnectionAddress::ProcessLike(maybe_process_creds);

    // Finally, update our buffer to reflect the bytes we've read.
    unsafe {
        buf.advance_mut(n);
    }

    Ok((n, conn_addr))
}
