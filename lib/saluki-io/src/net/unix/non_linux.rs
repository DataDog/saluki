use std::{io, mem};

use bytes::BufMut;
use socket2::{SockAddr, SockRef};

use crate::net::addr::ConnectionAddress;

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
    let sock_ref = socket2::SockRef::from(socket);

    // Create the message header struct that will be populated by the call to `recvmsg`, which includes the peer
    // address, message data, and any ancillary (out-of-band) data.
    //
    // SAFETY: We're allocating `sockaddr_storage`, which is always large enough to hold any address family's socket
    // address structure.
    let sock_storage = SockAddrStorage::zeroed();
    let sock_storage_len = sock_storage.size_of();
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
