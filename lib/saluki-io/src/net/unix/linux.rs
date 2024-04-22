use std::{io, mem};

use bytes::BufMut;
use socket2::{SockAddr, SockRef};

use crate::net::addr::{ConnectionAddress, ProcessCredentials};

use super::ancillary::{ControlMessage, SocketCredentialsAncillaryData};

pub(super) fn set_uds_passcred<'sock, S>(socket: &'sock S) -> io::Result<()>
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
    //
    // TODO: We could probably scope this down to `sockaddr_un`, to do a smaller stack allocation... but I need to
    // research more if that's the right choice in all cases for a Unix domain socket.
    let sock_storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let sock_storage_len = mem::size_of_val(&sock_storage) as libc::socklen_t;
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
