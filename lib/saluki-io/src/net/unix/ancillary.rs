use std::{
    mem::{self, MaybeUninit},
    os::fd::RawFd,
};

const SOCKET_CREDENTIALS_LEN: usize = get_ucred_struct_size();

// SAFETY: `CMSG_LEN` boils down to `size_of` calls and arithmetic for ensuring the values take alignment into
// consideration, so it's always safe to call.
const CONTROL_MESSAGE_HEADER_LEN: usize = unsafe { libc::CMSG_LEN(0) as usize };

/// Ancillary data buffer sized to hold exactly one `SCM_CREDENTIALS` control message.
///
/// Peers can send file descriptors (`SCM_RIGHTS`) alongside their payload, and the kernel installs as many of them as
/// fit in this buffer into our process, discarding the rest, so any that we receive must be closed. Sizing the buffer to
/// fit only the credentials limits how many can be installed, and in practice means that none are when `SO_PASSCRED` is
/// enabled, since Linux currently writes the credentials first. That ordering isn't documented, though, so it's only
/// an optimization: it's never relied upon to avoid leaking file descriptors.
pub type SocketCredentialsAncillaryData = AncillaryData<SOCKET_CREDENTIALS_LEN>;

/// Stack allocated structure for ancillary (out-of-band) data.
#[repr(C)]
pub struct AncillaryData<const N: usize> {
    // Aligns `buf` for `cmsghdr`, as we take references to control message headers that point directly into it.
    _align: [libc::cmsghdr; 0],
    buf: [MaybeUninit<u8>; N],
    len: usize,
}

impl<const N: usize> AncillaryData<N> {
    /// Creates a new `AncillaryData` structure of the given size.
    pub fn new() -> Self {
        Self {
            _align: [],
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
    /// # Safety
    ///
    /// The caller must ensure that the number of bytes filled is greater than or equal to the value of `new_len`.
    ///
    /// # Panics
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

    /// Returns `true` if the given control message, including its data, lies entirely within the buffer.
    fn contains(&self, cmsg: &libc::cmsghdr) -> bool {
        // The type of `cmsg_len` varies between MUSL and glibc, so we need to handle both cases, hence the unnecessary
        // cast in some cases which is cleaner than target-specific code.
        #[allow(clippy::unnecessary_cast)]
        let cmsg_len = cmsg.cmsg_len as usize;

        // `CMSG_FIRSTHDR` and `CMSG_NXTHDR` only return headers that fit within the buffer, so this can't underflow.
        let remaining = self.buf.len() - (cmsg as *const libc::cmsghdr as usize - self.buf.as_ptr() as usize);

        cmsg_len >= CONTROL_MESSAGE_HEADER_LEN && cmsg_len <= remaining
    }
}

impl<'a> Iterator for ControlMessages<'a> {
    type Item = ControlMessage<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
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

                // `CMSG_FIRSTHDR` and `CMSG_NXTHDR` only check that the header fits within the buffer, not the data
                // that follows it. If the control message claims to extend past the end of the buffer, or to be
                // shorter than its own header, the buffer is malformed and we can't trust anything after this point.
                if !self.contains(cmsg) {
                    return None;
                }

                self.current = Some(cmsg);

                // Skip over any control messages that we don't recognize, or that aren't valid, rather than ending
                // iteration, since a valid control message we do recognize may still follow them.
                if let Some(message) = ControlMessage::try_from_cmsghdr(cmsg) {
                    return Some(message);
                }
            }
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

    /// File descriptors passed by the peer.
    ///
    /// By the time these are visible, the kernel has already installed them in the receiving process, so the receiver
    /// is responsible for closing them if they aren't used.
    FileDescriptors(FileDescriptors<'a>),
}

impl<'a> ControlMessage<'a> {
    /// Attempts to parse a recognized control message from the given control message header.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the control message, including the data that follows its header as indicated by
    /// `cmsg_len`, lies entirely within an initialized buffer that lives at least as long as `'a`.
    unsafe fn try_from_cmsghdr(cmsg: &'a libc::cmsghdr) -> Option<Self> {
        unsafe {
            // Calculate the size of the control message header, so we can figure out the byte offset to actually get at
            // the raw message data, and then create a slice to that data.

            // The type of `cmsg_len` varies between MUSL and glibc, so we need to handle both cases, hence the
            // unnecessary cast in some cases which is cleaner than target-specific code.
            #[allow(clippy::unnecessary_cast)]
            let cmsg_len = cmsg.cmsg_len as usize;
            let data_len = cmsg_len.saturating_sub(CONTROL_MESSAGE_HEADER_LEN);
            let data_ptr = libc::CMSG_DATA(cmsg).cast();
            let data = std::slice::from_raw_parts(data_ptr, data_len);

            match cmsg.cmsg_level {
                libc::SOL_SOCKET => match cmsg.cmsg_type {
                    libc::SCM_CREDENTIALS => ControlMessage::as_credentials(data),
                    libc::SCM_RIGHTS => Some(ControlMessage::FileDescriptors(FileDescriptors::new(data))),
                    _ => None,
                },
                _ => None,
            }
        }
    }

    fn as_credentials(buf: &'a [u8]) -> Option<Self> {
        // When the kernel truncates a control message to fit the buffer, it sets `cmsg_len` to the number of bytes it
        // actually wrote, so a truncated `SCM_CREDENTIALS` message is caught here by its length not matching `ucred`.
        let ucred_ptr: *const libc::ucred = buf.as_ptr().cast();
        if buf.len() == mem::size_of::<libc::ucred>() && ucred_ptr.is_aligned() {
            // SAFETY: We've already checked that the buffer is long enough to be mapped to `ucred`, and that it's
            // suitably aligned, and we're only here if `cmsg_type` was SCM_CREDENTIALS, and our reference is safe to
            // take because it's tied to the lifetime of the buffer we're taking a pointer to.
            unsafe { ucred_ptr.as_ref().map(Self::Credentials) }
        } else {
            None
        }
    }
}

/// An iterator over the file descriptors in an `SCM_RIGHTS` control message.
pub struct FileDescriptors<'a> {
    fds: std::slice::Iter<'a, [u8; mem::size_of::<RawFd>()]>,
}

impl<'a> FileDescriptors<'a> {
    fn new(data: &'a [u8]) -> Self {
        // The kernel only ever writes whole file descriptors, so there's never a remainder unless the data is malformed,
        // in which case it couldn't be a file descriptor anyway.
        Self {
            fds: data.as_chunks().0.iter(),
        }
    }
}

impl Iterator for FileDescriptors<'_> {
    type Item = RawFd;

    fn next(&mut self) -> Option<Self::Item> {
        self.fds.next().map(|fd| RawFd::from_ne_bytes(*fd))
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

    fn as_bytes<T>(value: &T) -> &[u8] {
        // SAFETY: we only call this with `cmsghdr` and `ucred`, which are made up entirely of integer fields and have no
        // implicit padding, so every byte is initialized.
        unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), mem::size_of::<T>()) }
    }

    fn ucred_with_pid(pid: libc::pid_t) -> libc::ucred {
        libc::ucred {
            pid,
            uid: 1000,
            gid: 2000,
        }
    }

    // Builds an ancillary-data buffer holding the given control messages (level, type, and data), laid out the same way
    // the kernel lays them out: each message's header and data are padded out to `CMSG_SPACE` bytes, except that the
    // buffer ends immediately after the data of the last message, as it does when the kernel truncates that message.
    fn ancillary_with_messages(messages: &[(libc::c_int, libc::c_int, &[u8])]) -> AncillaryData<256> {
        let mut ancillary = AncillaryData::<256>::new();
        let buf = ancillary.as_mut_uninit();
        for byte in buf.iter_mut() {
            byte.write(0);
        }

        let mut start = 0;
        let mut len = 0;
        for (level, ty, data) in messages {
            let mut header: libc::cmsghdr = unsafe { mem::zeroed() };
            header.cmsg_len = unsafe { libc::CMSG_LEN(data.len() as u32) } as _;
            header.cmsg_level = *level;
            header.cmsg_type = *ty;

            let data_start = start + CONTROL_MESSAGE_HEADER_LEN;
            for (dst, src) in buf[start..].iter_mut().zip(as_bytes(&header)) {
                dst.write(*src);
            }
            for (dst, src) in buf[data_start..].iter_mut().zip(data.iter()) {
                dst.write(*src);
            }
            len = data_start + data.len();
            start += unsafe { libc::CMSG_SPACE(data.len() as u32) } as usize;
        }

        // SAFETY: every byte of the buffer was initialized above, and `len` never exceeds its length.
        unsafe {
            ancillary.set_len(len);
        }
        ancillary
    }

    // Overwrites the `cmsg_len` field of the control message header at the given byte offset into the buffer.
    fn set_cmsg_len<const N: usize>(ancillary: &mut AncillaryData<N>, offset: usize, cmsg_len: usize) {
        let header = ancillary.as_mut_uninit()[offset..].as_mut_ptr().cast::<libc::cmsghdr>();

        // SAFETY: the buffer is aligned for `cmsghdr`, and callers only pass offsets of existing headers within it.
        unsafe {
            (*header).cmsg_len = cmsg_len as _;
        }
    }

    fn first_credentials_pid<const N: usize>(ancillary: &AncillaryData<N>) -> Option<libc::pid_t> {
        // SAFETY: test buffers are always fully initialized up to their length.
        unsafe { ancillary.messages() }.find_map(|message| match message {
            ControlMessage::Credentials(creds) => Some(creds.pid),
            ControlMessage::FileDescriptors(_) => None,
        })
    }

    #[test]
    fn ancillary_buffer_is_aligned_for_control_message_headers() {
        // We take `cmsghdr` (and `ucred`) references that point directly into the buffer, which requires it to be
        // aligned regardless of where the compiler places it within the struct.
        let mut ancillary = SocketCredentialsAncillaryData::new();
        assert!(ancillary.as_mut_uninit().as_ptr().cast::<libc::cmsghdr>().is_aligned());
    }

    #[test]
    fn credentials_found_after_unrecognized_control_message() {
        // An unrecognized control message, such as a timestamp, must be skipped over rather than ending iteration, so
        // that credentials which follow it are still found.
        let timestamp = [0u8; 16];
        let creds = ucred_with_pid(4242);
        let ancillary = ancillary_with_messages(&[
            (libc::SOL_SOCKET, libc::SCM_TIMESTAMP, &timestamp),
            (libc::SOL_SOCKET, libc::SCM_CREDENTIALS, as_bytes(&creds)),
        ]);

        assert_eq!(first_credentials_pid(&ancillary), Some(4242));
    }

    #[test]
    fn file_descriptors_parsed_alongside_credentials() {
        // Every control message is yielded, in order, so a receiver sees both the credentials and the file descriptors
        // that follow them, and can close the latter. These aren't real file descriptors, so we never close them here.
        let creds = ucred_with_pid(4242);
        let fds: [RawFd; 3] = [7, 8, 9];
        let fds_bytes: Vec<u8> = fds.iter().flat_map(|fd| fd.to_ne_bytes()).collect();
        let ancillary = ancillary_with_messages(&[
            (libc::SOL_SOCKET, libc::SCM_CREDENTIALS, as_bytes(&creds)),
            (libc::SOL_SOCKET, libc::SCM_RIGHTS, &fds_bytes),
        ]);

        // SAFETY: the buffer is fully initialized up to its length.
        let mut messages = unsafe { ancillary.messages() };
        assert!(matches!(messages.next(), Some(ControlMessage::Credentials(creds)) if creds.pid == 4242));
        match messages.next() {
            Some(ControlMessage::FileDescriptors(received)) => assert_eq!(received.collect::<Vec<_>>(), fds),
            _ => panic!("file descriptors should follow the credentials"),
        }
        assert!(messages.next().is_none());
    }

    #[test]
    fn credentials_found_after_invalid_credentials() {
        // A credentials control message with the wrong data length must be skipped over rather than ending iteration,
        // so that valid credentials which follow it are still found.
        let creds = ucred_with_pid(4242);
        let ancillary = ancillary_with_messages(&[
            (libc::SOL_SOCKET, libc::SCM_CREDENTIALS, &[0u8; 1]),
            (libc::SOL_SOCKET, libc::SCM_CREDENTIALS, as_bytes(&creds)),
        ]);

        assert_eq!(first_credentials_pid(&ancillary), Some(4242));
    }

    #[test]
    fn truncated_credentials_are_not_parsed() {
        // When the kernel truncates a control message, it sets `cmsg_len` to cover only the bytes it actually wrote, so
        // truncated credentials must be recognized by their short length and never parsed into a `ucred`.
        let creds = ucred_with_pid(4242);
        let ucred_len = mem::size_of::<libc::ucred>();
        for truncated_len in [0, 1, ucred_len - 1] {
            let ancillary = ancillary_with_messages(&[(
                libc::SOL_SOCKET,
                libc::SCM_CREDENTIALS,
                &as_bytes(&creds)[..truncated_len],
            )]);

            assert_eq!(
                first_credentials_pid(&ancillary),
                None,
                "credentials truncated to {truncated_len} bytes should not be parsed"
            );
        }
    }

    #[test]
    fn control_message_with_invalid_length_ends_iteration() {
        // A control message whose `cmsg_len` is shorter than its own header, or extends past the end of the buffer,
        // means the buffer is malformed: iteration must stop rather than reading out of bounds or looping forever, and
        // that applies to the first control message as well as to any that follow it.
        let timestamp = [0u8; 16];
        let creds = ucred_with_pid(4242);
        let creds_offset = unsafe { libc::CMSG_SPACE(timestamp.len() as u32) } as usize;

        for (offset, messages) in [
            (0, vec![(libc::SOL_SOCKET, libc::SCM_CREDENTIALS, as_bytes(&creds))]),
            (
                creds_offset,
                vec![
                    (libc::SOL_SOCKET, libc::SCM_TIMESTAMP, &timestamp[..]),
                    (libc::SOL_SOCKET, libc::SCM_CREDENTIALS, as_bytes(&creds)),
                ],
            ),
        ] {
            let ancillary_len = ancillary_with_messages(&messages).len;
            for bad_cmsg_len in [
                0,
                CONTROL_MESSAGE_HEADER_LEN - 1,
                ancillary_len - offset + 1,
                u32::MAX as usize,
            ] {
                let mut ancillary = ancillary_with_messages(&messages);
                set_cmsg_len(&mut ancillary, offset, bad_cmsg_len);

                assert_eq!(
                    first_credentials_pid(&ancillary),
                    None,
                    "control message at offset {offset} with `cmsg_len` of {bad_cmsg_len} should end iteration"
                );
            }

            // The same goes for when the buffer ends before the control message does, even if the control message
            // itself is otherwise valid credentials.
            let mut ancillary = ancillary_with_messages(&messages);
            unsafe {
                ancillary.set_len(ancillary_len - 1);
            }

            assert_eq!(
                first_credentials_pid(&ancillary),
                None,
                "control message at offset {offset} extending past the end of the buffer should end iteration"
            );
        }
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
}
