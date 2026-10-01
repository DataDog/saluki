use std::mem::{self, MaybeUninit};

// Leave room for credentials alongside timestamps, security labels, or file descriptors. Larger ancillary
// payloads are reported via MSG_CTRUNC rather than silently treated as missing credentials.
const SOCKET_CREDENTIALS_LEN: usize = 512;

pub type SocketCredentialsAncillaryData = AncillaryData<SOCKET_CREDENTIALS_LEN>;

/// Stack allocated structure for ancillary (out-of-band) data.
#[repr(C)]
pub struct AncillaryData<const N: usize> {
    // Align the buffer as a cmsghdr without increasing its size.
    _alignment: [libc::cmsghdr; 0],
    buf: [MaybeUninit<u8>; N],
    len: usize,
}

impl<const N: usize> AncillaryData<N> {
    /// Creates a new `AncillaryData` structure of the given size.
    pub fn new() -> Self {
        Self {
            _alignment: [],
            buf: [MaybeUninit::uninit(); N],
            len: 0,
        }
    }

    /// Returns the initialized length of the control buffer.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Gets a mutable reference to the underlying buffer as a slice of uninitialized bytes.
    pub fn as_mut_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        &mut self.buf[..]
    }

    /// Sets the number of bytes that have been filled in the buffer.
    ///
    /// ## Safety
    ///
    /// The caller must ensure that the number of bytes filled is greater than or equal to the value of `new_len`.
    ///
    /// ## Panics
    ///
    /// If `new_len` is greater than the length of the buffer itself, this function will panic.
    pub unsafe fn set_len(&mut self, new_len: usize) {
        if new_len > self.buf.len() {
            panic!("new length exceeds buffer length");
        }

        self.len = new_len;
    }

    /// Gets an iterator over recognized control messages in the buffer.
    ///
    /// # Safety
    ///
    /// The first `len` bytes must be initialized, as required by `set_len`.
    pub unsafe fn messages(&self) -> ControlMessages<'_> {
        // SAFETY: set_len requires that the first len bytes are initialized.
        let buf = unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast(), self.len) };
        ControlMessages::new(buf)
    }
}

/// An iterator over recognized control messages in an ancillary data buffer.
pub struct ControlMessages<'a> {
    buf: &'a [u8],
}

impl<'a> ControlMessages<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
}

impl<'a> Iterator for ControlMessages<'a> {
    type Item = ControlMessage<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.buf.len() >= mem::size_of::<libc::cmsghdr>() {
            // SAFETY: the buffer contains a complete header. Read by value to support unaligned input.
            let cmsg = unsafe { self.buf.as_ptr().cast::<libc::cmsghdr>().read_unaligned() };
            // cmsg_len has different types on glibc and musl.
            #[allow(clippy::unnecessary_cast)]
            let cmsg_len = cmsg.cmsg_len as usize;
            let header_len = unsafe { libc::CMSG_LEN(0) as usize };
            if cmsg_len < header_len || cmsg_len > self.buf.len() {
                self.buf = &[];
                return None;
            }

            let data = &self.buf[header_len..cmsg_len];
            // CMSG_SPACE accounts for platform-specific padding between messages. Reject lengths that cannot be
            // represented by this libc interface; received control data is bounded by the stack buffer in practice.
            let Ok(data_len) = u32::try_from(data.len()) else {
                self.buf = &[];
                return None;
            };
            let space = unsafe { libc::CMSG_SPACE(data_len) as usize };
            self.buf = self.buf.get(space..).unwrap_or_default();

            match (cmsg.cmsg_level, cmsg.cmsg_type) {
                (libc::SOL_SOCKET, libc::SCM_CREDENTIALS) => {
                    if let Some(credentials) = ControlMessage::as_credentials(data) {
                        return Some(credentials);
                    }
                }
                (libc::SOL_SOCKET, libc::SCM_RIGHTS) => return Some(ControlMessage::FileDescriptors(data)),
                _ => {}
            }
        }
        None
    }
}

/// A recognized control message.
pub enum ControlMessage<'a> {
    /// Process ID, user ID, and group ID of the remote peer.
    Credentials(libc::ucred),

    /// Raw file descriptors received via SCM_RIGHTS, which the receiver must close if unused.
    FileDescriptors(&'a [u8]),
}

impl ControlMessage<'_> {
    fn as_credentials(buf: &[u8]) -> Option<Self> {
        if buf.len() != mem::size_of::<libc::ucred>() {
            return None;
        }
        // SAFETY: the payload has exactly the size of ucred. All its integer fields accept any bit pattern.
        // Reading by value avoids requiring an aligned input buffer or creating a reference into it.
        Some(Self::Credentials(unsafe {
            buf.as_ptr().cast::<libc::ucred>().read_unaligned()
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::mem;

    use super::*;

    // Builds an ancillary-data buffer of `len` initialized (zeroed) bytes. Zeroing first keeps the later
    // `messages()`/CMSG walk from ever reading uninitialized memory when the buffer is deliberately undersized.
    fn zeroed_ancillary(len: usize) -> SocketCredentialsAncillaryData {
        let mut ancillary = SocketCredentialsAncillaryData::new();
        for byte in ancillary.as_mut_uninit() {
            byte.write(0);
        }

        // SAFETY: every byte of the buffer was just initialized above, and `len` never exceeds its length.
        unsafe {
            ancillary.set_len(len);
        }
        ancillary
    }

    #[test]
    fn credentials_parsed_from_exact_ucred_payload() {
        // A control-message payload that's exactly `ucred`-sized is decoded field-for-field. The bytes point at a
        // real `ucred`; parsing copies its integer fields into the returned value.
        let creds = libc::ucred {
            pid: 4242,
            uid: 1000,
            gid: 2000,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&creds as *const libc::ucred).cast::<u8>(),
                mem::size_of::<libc::ucred>(),
            )
        };

        match ControlMessage::as_credentials(bytes) {
            Some(ControlMessage::Credentials(parsed)) => {
                assert_eq!(parsed.pid, 4242);
                assert_eq!(parsed.uid, 1000);
                assert_eq!(parsed.gid, 2000);
            }
            _ => panic!("a correctly-sized ucred payload should parse into credentials"),
        }
    }

    #[test]
    fn credentials_rejected_when_payload_length_is_wrong() {
        // Untrusted, adversarial control data: a payload whose length doesn't exactly match `ucred` must be rejected
        // (`None`), never reinterpreted as a `ucred` (which would be an out-of-bounds read or a garbage identity).
        let ucred_len = mem::size_of::<libc::ucred>();
        for bad_len in [0, 1, ucred_len - 1, ucred_len + 1] {
            let buf = vec![0u8; bad_len];
            assert!(
                ControlMessage::as_credentials(&buf).is_none(),
                "payload of {bad_len} bytes should be rejected (ucred is {ucred_len} bytes)"
            );
        }
    }

    #[test]
    fn truncated_ancillary_buffer_yields_no_messages() {
        // A control buffer too small to hold even a single `cmsghdr` (as can happen with truncated/garbage ancillary
        // data) must produce zero control messages — `CMSG_FIRSTHDR` returns null — rather than panicking or
        // fabricating a credential.
        for len in [0usize, 1, mem::size_of::<libc::cmsghdr>() - 1] {
            let ancillary = zeroed_ancillary(len);

            // SAFETY: the buffer is fully initialized and its length was set to `len` above.
            let mut messages = unsafe { ancillary.messages() };
            assert!(
                messages.next().is_none(),
                "a {len}-byte control buffer should yield no control messages"
            );
        }
    }
    fn append_control_message(buf: &mut Vec<u8>, level: i32, kind: i32, payload: &[u8]) {
        let start = buf.len();
        let space = unsafe { libc::CMSG_SPACE(payload.len() as u32) as usize };
        let header_len = unsafe { libc::CMSG_LEN(0) as usize };
        buf.resize(start + space, 0);
        let mut header: libc::cmsghdr = unsafe { mem::zeroed() };
        header.cmsg_level = level;
        header.cmsg_type = kind;
        header.cmsg_len = unsafe { libc::CMSG_LEN(payload.len() as u32) as _ };
        // SAFETY: the resized buffer contains the complete header and payload, even when unaligned.
        unsafe {
            buf.as_mut_ptr()
                .add(start)
                .cast::<libc::cmsghdr>()
                .write_unaligned(header)
        };
        buf[start + header_len..start + header_len + payload.len()].copy_from_slice(payload);
    }

    fn credential_payload() -> Vec<u8> {
        let creds = libc::ucred {
            pid: 4242,
            uid: 1000,
            gid: 2000,
        };
        // SAFETY: ucred contains three initialized integer fields and no padding on Linux.
        unsafe {
            std::slice::from_raw_parts(
                (&creds as *const libc::ucred).cast::<u8>(),
                mem::size_of::<libc::ucred>(),
            )
            .to_vec()
        }
    }

    #[test]
    fn credentials_after_unrecognized_control_messages_are_parsed() {
        let mut buf = Vec::new();
        append_control_message(&mut buf, libc::SOL_SOCKET, libc::SCM_TIMESTAMP, &[0; 16]);
        append_control_message(&mut buf, libc::SOL_IP, 0, &[0; 4]);
        append_control_message(&mut buf, libc::SOL_SOCKET, libc::SCM_CREDENTIALS, &credential_payload());
        let mut messages = ControlMessages::new(&buf);
        assert!(matches!(messages.next(), Some(ControlMessage::Credentials(creds)) if creds.pid == 4242));
        assert!(messages.next().is_none());
        assert!(messages.next().is_none());
    }

    #[test]
    fn malformed_credential_payload_does_not_hide_later_credentials() {
        let mut buf = Vec::new();
        append_control_message(&mut buf, libc::SOL_SOCKET, libc::SCM_CREDENTIALS, &[0; 1]);
        append_control_message(&mut buf, libc::SOL_SOCKET, libc::SCM_CREDENTIALS, &credential_payload());
        assert!(
            matches!(ControlMessages::new(&buf).next(), Some(ControlMessage::Credentials(creds)) if creds.pid == 4242)
        );
    }

    #[test]
    fn invalid_control_message_lengths_are_rejected() {
        for len in [0, mem::size_of::<libc::cmsghdr>() - 1, 512, usize::MAX] {
            let mut buf = Vec::new();
            append_control_message(&mut buf, libc::SOL_SOCKET, libc::SCM_CREDENTIALS, &credential_payload());
            // SAFETY: the buffer has room for a cmsghdr; only its advertised length is invalid.
            unsafe {
                let ptr = buf.as_mut_ptr().cast::<libc::cmsghdr>();
                let mut header = ptr.read_unaligned();
                header.cmsg_len = len as _;
                ptr.write_unaligned(header);
            };
            let mut messages = ControlMessages::new(&buf);
            assert!(messages.next().is_none());
            assert!(messages.next().is_none());
        }
    }

    #[test]
    fn unaligned_control_buffer_is_parsed() {
        let mut buf = Vec::new();
        append_control_message(&mut buf, libc::SOL_SOCKET, libc::SCM_CREDENTIALS, &credential_payload());
        let mut unaligned = vec![0];
        unaligned.extend_from_slice(&buf);
        assert!(
            matches!(ControlMessages::new(&unaligned[1..]).next(), Some(ControlMessage::Credentials(creds)) if creds.pid == 4242)
        );
    }
}
