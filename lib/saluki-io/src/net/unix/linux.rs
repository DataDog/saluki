use std::{io, mem, os::fd::AsRawFd};

use bytes::BufMut;
use socket2::{Domain, MaybeUninitSlice, MsgHdrMut, Protocol, SockAddr, SockAddrStorage, SockRef, Socket, Type};
use tokio::net::UnixDatagram;

use super::ancillary::{ControlMessage, SocketCredentialsAncillaryData};
use crate::net::addr::{ConnectionAddress, ProcessCredentials, ProcessCredentialsError, ProcessIdentity};

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

    let process_identity = if control_len > 0 {
        unsafe {
            ancillary_data.set_len(control_len);

            match ancillary_data
                .messages()
                .map(|m| match m {
                    ControlMessage::Credentials(creds) => creds,
                })
                .next()
            {
                Some(creds) if creds.pid == 0 => ProcessIdentity::Error(ProcessCredentialsError::ZeroPid),
                Some(creds) => ProcessIdentity::Credentials(ProcessCredentials {
                    pid: creds.pid,
                    uid: creds.uid,
                    gid: creds.gid,
                }),
                None => ProcessIdentity::Error(ProcessCredentialsError::InvalidCredentials),
            }
        }
    } else {
        ProcessIdentity::Unavailable
    };

    let conn_addr = ConnectionAddress::ProcessLike(process_identity);

    // Finally, update our buffer to reflect the bytes we've read.
    unsafe {
        buf.advance_mut(n);
    }

    Ok((n, conn_addr))
}

/// Returns `true` if `SO_REUSEPORT` is supported for UDP sockets on the current platform.
pub fn socket_reuseport_supported() -> bool {
    let socket = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(socket) => socket,
        Err(_) => return false,
    };

    match socket.set_reuse_port(true) {
        Ok(()) => true,
        Err(_) => false,
    }
}

/// Sends data to the Unix domain socket.
///
/// This function is specifically for connected Unix domain sockets in datagram mode (that's, SOCK_DGRAM), which are
/// represented via `UnixDatagram` in `tokio`.
///
/// The payload is sent with an `SCM_CREDENTIALS` ancillary block using the provided `ProcessCredentials`.
///
/// Linux permits a sender to use its own PID, UID, and GID normally. Sending a forged PID requires `CAP_SYS_ADMIN`,
/// sending a forged UID requires `CAP_SETUID`, and sending a forged GID requires `CAP_SETGID`.
///
/// ## Errors
///
/// If socket readiness or the underlying system call fails, an error is returned.
pub async fn uds_sendmsg_with_creds(
    socket: &UnixDatagram, payload: &[u8], credentials: &ProcessCredentials,
) -> io::Result<usize> {
    let creds = libc::ucred {
        pid: credentials.pid as libc::pid_t,
        uid: credentials.uid,
        gid: credentials.gid,
    };

    socket
        .async_io(tokio::io::Interest::WRITABLE, || {
            sendmsg_with_ucred(socket.as_raw_fd(), payload, &creds)
        })
        .await
}

/// Synchronously writes one payload with the given credentials to the raw file descriptor.
///
/// Constructs a `cmsghdr` header followed by the `ucred` body in a single control buffer, then invokes `sendmsg`.
fn sendmsg_with_ucred(fd: libc::c_int, payload: &[u8], creds: &libc::ucred) -> io::Result<usize> {
    // SAFETY: `CMSG_SPACE` is a const expression on `size_of::<ucred>()`; the call is safe and returns the byte count
    // needed to hold one aligned cmsghdr plus a ucred payload.
    let control_len = unsafe { libc::CMSG_SPACE(mem::size_of::<libc::ucred>() as u32) as usize };

    // `CMSG_SPACE` gives us the padded byte length, but the backing storage also has to be aligned for `cmsghdr` and
    // `ucred` because the CMSG_* macros return typed pointers into it.
    let control_words = control_len.div_ceil(mem::size_of::<usize>());
    let mut control_buf = vec![0usize; control_words];

    // SAFETY: we construct a `msghdr` pointing at the payload and the control buffer, then walk the control buffer
    // with the libc CMSG_FIRSTHDR / CMSG_DATA macros to write the cmsghdr header and ucred body. Pointers all
    // reference live local memory; lifetimes don't escape the call.
    let n = unsafe {
        // We use `IoSlice`-style iovec entries pointing at the payload.
        let mut iov = libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        };

        let mut msg: libc::msghdr = mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control_buf.as_mut_ptr().cast::<libc::c_void>();
        msg.msg_controllen = control_len as _;

        // Populate the cmsghdr at the start of the control buffer.
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("failed to obtain cmsghdr from control buffer"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_CREDENTIALS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<libc::ucred>() as u32) as _;

        // Copy the ucred body into the cmsg data region.
        let data_ptr = libc::CMSG_DATA(cmsg) as *mut libc::ucred;
        std::ptr::write(data_ptr, *creds);

        // Send.
        libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL)
    };

    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use bytes::BytesMut;

    use super::*;

    // Creates a connected AF_UNIX/SOCK_DGRAM socket pair, returning `(sender, receiver)`.
    fn unix_dgram_socketpair() -> (Socket, Socket) {
        let mut fds: [libc::c_int; 2] = [-1, -1];
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair failed: {}", io::Error::last_os_error());

        // SAFETY: `socketpair` succeeded, so both descriptors are valid and owned by us.
        unsafe { (Socket::from_raw_fd(fds[0]), Socket::from_raw_fd(fds[1])) }
    }

    #[test]
    fn uds_recvmsg_reads_peer_credentials() {
        // With SO_PASSCRED enabled on the receiver, the kernel attaches the sender's real PID/UID/GID as an
        // SCM_CREDENTIALS control message, which `uds_recvmsg` must parse into `ProcessIdentity::Credentials`.
        let (sender, receiver) = unix_dgram_socketpair();
        enable_uds_socket_credentials(&receiver).expect("enabling SO_PASSCRED should succeed");

        let payload = b"origin-detection-payload";
        let sent = sender.send(payload).expect("send should succeed");
        assert_eq!(sent, payload.len());

        let mut buf = BytesMut::with_capacity(128);
        let (n, addr) = uds_recvmsg(&receiver, &mut buf).expect("recvmsg should succeed");
        assert_eq!(n, payload.len());
        assert_eq!(&buf[..], payload);

        let creds = addr
            .process_credentials()
            .expect("peer credentials should be present after SO_PASSCRED");
        assert_eq!(creds.pid, std::process::id() as libc::pid_t);
        assert_eq!(creds.uid, unsafe { libc::getuid() });
        assert_eq!(creds.gid, unsafe { libc::getgid() });
    }

    #[test]
    fn uds_recvmsg_without_passcred_reports_unavailable() {
        // Without SO_PASSCRED, no ancillary credentials are delivered: control length is zero, so the identity is
        // `Unavailable` (a "no origin info" state), not an error and not fabricated credentials.
        let (sender, receiver) = unix_dgram_socketpair();

        let payload = b"no-creds-payload";
        sender.send(payload).expect("send should succeed");

        let mut buf = BytesMut::with_capacity(128);
        let (n, addr) = uds_recvmsg(&receiver, &mut buf).expect("recvmsg should succeed");
        assert_eq!(n, payload.len());
        assert_eq!(&buf[..], payload);

        assert!(matches!(
            addr,
            ConnectionAddress::ProcessLike(ProcessIdentity::Unavailable)
        ));
        assert!(addr.process_credentials().is_none());
    }

    #[test]
    fn sendmsg_with_current_credentials_round_trips_payload() {
        // Construct a socketpair, send a payload with our own creds, read it back from the receiver, assert payload
        // bytes match. This exercises the sendmsg construction path without requiring elevated capabilities.
        let (sender, receiver) = unsafe {
            let mut fds: [libc::c_int; 2] = [-1, -1];
            let rc = libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr());
            assert_eq!(rc, 0, "socketpair failed: {}", io::Error::last_os_error());
            (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
        };

        let creds = libc::ucred {
            pid: std::process::id() as libc::pid_t,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        };
        let payload = b"uds-sendmsg-test-payload";
        let written = sendmsg_with_ucred(sender.as_raw_fd(), payload, &creds).expect("send should succeed");
        assert_eq!(written, payload.len());

        // Read back to confirm the receiver got the bytes.
        let mut buf = [0u8; 64];
        let read = unsafe {
            libc::recv(
                receiver.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        assert!(read > 0, "recv failed: {}", io::Error::last_os_error());
        assert_eq!(&buf[..read as usize], payload);
    }
}
