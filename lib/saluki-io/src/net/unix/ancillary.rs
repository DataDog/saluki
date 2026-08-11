use std::mem::{self, MaybeUninit};

const SOCKET_CREDENTIALS_LEN: usize = get_ucred_struct_size();

pub type SocketCredentialsAncillaryData = AncillaryData<SOCKET_CREDENTIALS_LEN>;

/// Stack allocated structure for ancillary (out-of-band) data.
pub struct AncillaryData<const N: usize> {
    buf: [MaybeUninit<u8>; N],
    len: usize,
}

impl<const N: usize> AncillaryData<N> {
    /// Creates a new `AncillaryData` structure of the given size.
    pub fn new() -> Self {
        Self {
            buf: [MaybeUninit::uninit(); N],
            len: 0,
        }
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

    /// Gets an iterator over any control messages in the buffer.
    pub unsafe fn messages(&self) -> ControlMessages<'_> {
        let buf = std::slice::from_raw_parts(self.buf.as_ptr() as *const _, self.len);
        ControlMessages::new(buf)
    }
}

/// An iterator over control messages in an ancillary data buffer.
pub struct ControlMessages<'a> {
    buf: &'a [u8],
    current: Option<&'a libc::cmsghdr>,
}

impl<'a> ControlMessages<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, current: None }
    }
}

impl<'a> Iterator for ControlMessages<'a> {
    type Item = ControlMessage<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            // Create a temporary message header that we can use to pull out control message headers from.
            let mut msg: libc::msghdr = mem::zeroed();
            msg.msg_control = self.buf.as_ptr() as *mut _;
            msg.msg_controllen = self.buf.len() as _;

            let cmsg = if let Some(current_cmsg) = self.current {
                // Get the next control message header after the current one.
                libc::CMSG_NXTHDR(&msg, current_cmsg)
            } else {
                // We haven't read a control message header yet, so take the first one.
                libc::CMSG_FIRSTHDR(&msg)
            };

            let cmsg = cmsg.as_ref()?;
            self.current = Some(cmsg);

            ControlMessage::try_from_cmsghdr(cmsg)
        }
    }
}

/// Control message.
pub enum ControlMessage<'a> {
    /// UNIX socket credentials.
    ///
    /// This captures the process ID, user ID, and group ID of the peer process on the other end of a Unix domain
    /// socket.
    Credentials(&'a libc::ucred),
}

impl<'a> ControlMessage<'a> {
    fn try_from_cmsghdr(cmsg: &'a libc::cmsghdr) -> Option<Self> {
        unsafe {
            // Calculate the size of the control message header, so we can figure out the byte offset to actually get at
            // the raw message data, and then create a slice to that data.

            // The type of `cmsg_len` varies between MUSL and glibc, so we need to handle both cases, hence the
            // unnecessary cast in some cases which is cleaner than target-specific code.
            #[allow(clippy::unnecessary_cast)]
            let cmsg_len = cmsg.cmsg_len as usize;
            let cmsg_len_offset = libc::CMSG_LEN(0) as usize;
            let data_len = cmsg_len.saturating_sub(cmsg_len_offset) as usize;
            let data_ptr = libc::CMSG_DATA(cmsg).cast();
            let data = std::slice::from_raw_parts(data_ptr, data_len);

            // Currently, all we handle is socket credentials.
            match cmsg.cmsg_level {
                libc::SOL_SOCKET => match cmsg.cmsg_type {
                    libc::SCM_CREDENTIALS => ControlMessage::as_credentials(data),
                    _ => None,
                },
                _ => None,
            }
        }
    }

    fn as_credentials(buf: &'a [u8]) -> Option<Self> {
        if buf.len() == mem::size_of::<libc::ucred>() {
            // SAFETY: We've already checked that the buffer is long enough to be mapped to `ucred`, and we're only here
            // if `cmsg_type` was SCM_CREDENTIALS, and our reference is safe to take because it's tied to the lifetime
            // of the buffer we're taking a pointer to.
            unsafe {
                let ucred_ptr: *const libc::ucred = buf.as_ptr().cast();
                ucred_ptr.as_ref().map(Self::Credentials)
            }
        } else {
            None
        }
    }
}

const fn get_ucred_struct_size() -> usize {
    let ucred_raw_size = mem::size_of::<libc::ucred>();
    let ucred_raw_size = if ucred_raw_size.wrapping_shr(u32::BITS) != 0 {
        // We do a const shift of the raw size to see if it has any additional bits past what we can fit in u32, and
        // this way we know that it's safe to directly cast the value to u32 without having truncated any bits.
        panic!("size of `ucred` struct greater than u32::MAX");
    } else {
        ucred_raw_size as u32
    };

    // SAFETY: This is part of a blanket "unsafe" wrapper around libc functions, but it's safe to call since it boils
    // down to a bunch of `size_of` calls and arithmetic for ensuring the values take alignment into consideration, etc.
    unsafe { libc::CMSG_SPACE(ucred_raw_size) as usize }
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
        // real `ucred`, so the reinterpretation in `as_credentials` reads correctly-aligned memory.
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
            None => panic!("a correctly-sized ucred payload should parse into credentials"),
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
}
