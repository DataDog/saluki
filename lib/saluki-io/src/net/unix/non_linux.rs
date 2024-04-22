use std::{io, mem};

use bytes::BufMut;
use socket2::{SockAddr, SockRef};

use crate::net::addr::ConnectionAddress;

pub(super) fn set_uds_passcred<'sock, S>(_socket: &'sock S) -> io::Result<()>
where
    SockRef<'sock>: From<&'sock S>,
{
    Ok(())
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

    let data_buf = unsafe { socket2::MaybeUninitSlice::new(buf.chunk_mut().as_uninit_slice_mut()) };
    let mut data_bufs = [data_buf];

    let mut msg_hdr = socket2::MsgHdrMut::new()
        .with_addr(&mut sock_addr)
        .with_buffers(&mut data_bufs);

    let n = sock_ref.recvmsg(&mut msg_hdr, 0)?;

    // Finally, update our buffer to reflect the bytes we've read.
    unsafe {
        buf.advance_mut(n);
    }

    Ok((n, ConnectionAddress::ProcessLike(None)))
}
