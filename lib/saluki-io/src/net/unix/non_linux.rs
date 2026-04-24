use std::io;

use bytes::BufMut;
use socket2::{MaybeUninitSlice, MsgHdrMut, SockAddr, SockAddrStorage, SockRef};

use crate::net::{ConnectionAddress, ProcessCredentials, ReceiveResult};

pub fn enable_uds_socket_credentials<'sock, S>(_socket: &'sock S) -> io::Result<()>
where
    SockRef<'sock>: From<&'sock S>,
{
    Ok(())
}

pub(super) fn uds_sendmsg<'sock, S>(
    _socket: &'sock S, _payload: &[u8], _process_credentials: ProcessCredentials,
) -> io::Result<usize>
where
    SockRef<'sock>: From<&'sock S>,
{
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix socket credential replay is not supported on this platform",
    ))
}

pub(super) fn uds_recvmsg<'sock, S, B: BufMut>(socket: &'sock S, buf: &mut B) -> io::Result<ReceiveResult>
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

    Ok(ReceiveResult {
        bytes_read: n,
        address: ConnectionAddress::ProcessLike(None),
        ancillary_data: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::{io::ErrorKind, os::unix::net::UnixDatagram};

    use super::uds_sendmsg;
    use crate::net::ProcessCredentials;

    #[test]
    fn uds_sendmsg_reports_unsupported() {
        let (sender, _receiver) = UnixDatagram::pair().expect("socket pair should create");
        let error = uds_sendmsg(&sender, b"metric:1|c", ProcessCredentials { pid: 1, uid: 2, gid: 3 })
            .expect_err("replay credentials should be unsupported on non-Linux platforms");

        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }
}
