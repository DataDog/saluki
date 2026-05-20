//! Linux-only helpers for sending DogStatsD replay traffic.
//!
//! Replay packets carry a synthetic `SCM_CREDENTIALS` ancillary block that lets the receive-side packet handler
//! distinguish replay from live and recover the captured PID. Stamping a GID the process doesn't actually own requires
//! `CAP_SETGID` (or root) — replay assumes the operator has run ADP with the appropriate privileges (typically `sudo`,
//! matching the Go agent's replay subcommand). If the capability is missing, the first `sendmsg` will fail with
//! `EPERM` and the replay task aborts.

use std::{
    io, mem,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

use tokio::net::UnixDatagram;

/// Sends a payload over a connected `UnixDatagram` socket with a spoofed `SCM_CREDENTIALS` ancillary block.
///
/// The captured PID is packed into `uid` and the replay marker GID into `gid`, matching the protocol the receive-side
/// packet handler watches for.
pub async fn send_replay_packet(
    socket: &UnixDatagram, payload: &[u8], captured_pid: i32, replay_gid: u32,
) -> io::Result<usize> {
    let our_pid = std::process::id() as libc::pid_t;
    let creds = libc::ucred {
        pid: our_pid,
        uid: captured_pid as u32,
        gid: replay_gid,
    };

    socket
        .async_io(tokio::io::Interest::WRITABLE, || {
            send_with_ucred(socket.as_raw_fd(), payload, &creds)
        })
        .await
}

/// Synchronously writes one payload with the given credentials to the raw fd.
///
/// Constructs a `cmsghdr` header followed by the `ucred` body in a single control buffer, then invokes `sendmsg`. The
/// buffer is stack-allocated and sized via `CMSG_SPACE` to satisfy the kernel's alignment requirements.
fn send_with_ucred(fd: libc::c_int, payload: &[u8], creds: &libc::ucred) -> io::Result<usize> {
    // SAFETY: `CMSG_SPACE` is a const expression on `size_of::<ucred>()`; the call is safe and returns the byte count
    // needed to hold one aligned cmsghdr plus a ucred payload.
    let control_len = unsafe { libc::CMSG_SPACE(mem::size_of::<libc::ucred>() as u32) as usize };

    // Stack-allocated control buffer. `CMSG_SPACE` rounds up for alignment, so the resulting buffer is correctly
    // aligned when accessed via the libc CMSG_* macros below.
    let mut control_buf = vec![0u8; control_len];

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
        msg.msg_control = control_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = control_len as _;

        // Populate the cmsghdr at the start of the control buffer.
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to obtain cmsghdr from control buffer",
            ));
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
    use std::os::fd::AsRawFd;

    use super::*;

    #[test]
    fn send_with_own_credentials_round_trips_payload() {
        // Construct a socketpair, send a payload with our own creds, read it back from the receiver, assert payload
        // bytes match. This exercises the same construction code path as send_replay_packet without requiring
        // CAP_SETGID.
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
        let payload = b"replay-test-payload";
        let written = send_with_ucred(sender.as_raw_fd(), payload, &creds).expect("send should succeed");
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
