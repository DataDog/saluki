use std::{io, io::IoSlice, mem, ptr};

use bytes::BufMut;
use socket2::{MaybeUninitSlice, MsgHdr, MsgHdrMut, SockAddr, SockAddrStorage, SockRef};

use super::ancillary::{ControlMessage, SocketCredentialsAncillaryData};
use crate::net::{ConnectionAddress, ProcessCredentials, ReceiveResult};

/// Enables the `SO_PASSCRED` option on the given socket.
///
/// ## Errors
///
/// If the underlying system call fails, an error is returned.
pub fn enable_uds_socket_credentials<'sock, S>(socket: &'sock S) -> io::Result<()>
where
    SockRef<'sock>: From<&'sock S>,
{
    let sock_ref = SockRef::from(socket);
    sock_ref.set_passcred(true)
}

pub(super) fn uds_sendmsg<'sock, S>(
    socket: &'sock S, payload: &[u8], process_credentials: ProcessCredentials,
) -> io::Result<usize>
where
    SockRef<'sock>: From<&'sock S>,
{
    let sock_ref = SockRef::from(socket);

    let ancillary_data = socket_credentials_control_message(process_credentials);
    let data_bufs = [IoSlice::new(payload)];
    let msg_hdr = MsgHdr::new()
        .with_buffers(&data_bufs)
        .with_control(ancillary_data.bytes());

    sock_ref.sendmsg(&msg_hdr, 0)
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

    let mut ancillary_data = SocketCredentialsAncillaryData::new();

    let data_buf = unsafe { MaybeUninitSlice::new(buf.chunk_mut().as_uninit_slice_mut()) };
    let mut data_bufs = [data_buf];

    let mut msg_hdr = MsgHdrMut::new()
        .with_addr(&mut sock_addr)
        .with_buffers(&mut data_bufs)
        .with_control(ancillary_data.as_mut_uninit());

    let n = sock_ref.recvmsg(&mut msg_hdr, libc::MSG_CMSG_CLOEXEC)?;

    // If we got any socket credentials back, parse them.
    let control_len = msg_hdr.control_len();

    let (maybe_process_creds, ancillary) = if control_len > 0 {
        unsafe {
            ancillary_data.set_len(control_len);

            let ancillary = ancillary_data.bytes().to_vec();

            let maybe_process_creds = ancillary_data.messages().find_map(|m| match m {
                ControlMessage::Credentials(creds) => Some(ProcessCredentials {
                    pid: creds.pid,
                    uid: creds.uid,
                    gid: creds.gid,
                }),
            });

            (maybe_process_creds, ancillary)
        }
    } else {
        (None, Vec::new())
    };

    // Finally, update our buffer to reflect the bytes we've read.
    unsafe {
        buf.advance_mut(n);
    }

    Ok(ReceiveResult {
        bytes_read: n,
        address: ConnectionAddress::ProcessLike(maybe_process_creds),
        ancillary_data: ancillary,
    })
}

fn socket_credentials_control_message(process_credentials: ProcessCredentials) -> SocketCredentialsAncillaryData {
    let mut ancillary_data = SocketCredentialsAncillaryData::new();
    let control_len = {
        let control_buf = ancillary_data.as_mut_uninit();

        // SAFETY: The ancillary buffer is exactly sized for a single `SCM_CREDENTIALS` control message, and we write a
        // single `cmsghdr` plus one `ucred` payload into it before exposing the initialized bytes to `sendmsg`.
        unsafe {
            // Zero the full ancillary buffer so the safe byte slice we expose afterward is fully initialized.
            ptr::write_bytes(control_buf.as_mut_ptr().cast::<u8>(), 0, control_buf.len());

            let mut msg_hdr: libc::msghdr = mem::zeroed();
            msg_hdr.msg_control = control_buf.as_mut_ptr().cast();
            msg_hdr.msg_controllen = control_buf.len();

            let cmsg = libc::CMSG_FIRSTHDR(&msg_hdr);
            assert!(!cmsg.is_null(), "ancillary buffer should fit one credentials message");

            #[allow(clippy::unnecessary_cast)]
            {
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_CREDENTIALS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<libc::ucred>() as u32) as _;
            }

            let ucred_ptr = libc::CMSG_DATA(cmsg).cast::<libc::ucred>();
            ucred_ptr.write(libc::ucred {
                pid: process_credentials.pid,
                uid: process_credentials.uid,
                gid: process_credentials.gid,
            });
        }

        control_buf.len()
    };

    unsafe {
        ancillary_data.set_len(control_len);
    }

    ancillary_data
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixDatagram;

    use bytes::BytesMut;

    use super::{enable_uds_socket_credentials, uds_recvmsg, uds_sendmsg};
    use crate::net::{ConnectionAddress, ProcessCredentials};

    #[test]
    fn uds_sendmsg_attaches_process_credentials() {
        let (sender, receiver) = UnixDatagram::pair().expect("socket pair should create");
        enable_uds_socket_credentials(&receiver).expect("receiver should enable credentials");

        let expected = ProcessCredentials {
            pid: std::process::id() as i32,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        };

        uds_sendmsg(&sender, b"metric:1|c", expected.clone()).expect("sendmsg should succeed");

        let mut buf = BytesMut::with_capacity(64);
        let received = uds_recvmsg(&receiver, &mut buf).expect("recvmsg should succeed");

        assert_eq!(&buf[..], b"metric:1|c");
        match received.address {
            ConnectionAddress::ProcessLike(Some(actual)) => {
                assert_eq!(actual.pid, expected.pid);
                assert_eq!(actual.uid, expected.uid);
                assert_eq!(actual.gid, expected.gid);
            }
            _ => panic!("expected process credentials from ancillary data"),
        }
    }
}
