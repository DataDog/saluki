use crate::config::{validate_broadcast_layout, validate_capacity};
use crate::contract::{
    BROADCAST_HEADER_MAGIC, BROADCAST_LAYOUT_VERSION, BROADCAST_META_CAPACITY, BROADCAST_META_LAYOUT_VERSION,
    BROADCAST_META_MAGIC, BROADCAST_META_MAX_SUBSCRIBERS, BROADCAST_META_PROTOCOL_VERSION,
    BROADCAST_META_RECORD_HEADER, BROADCAST_META_REGION_SIZE, BROADCAST_META_RING_OFFSET, BROADCAST_META_SESSION,
    BROADCAST_META_SLOTS_OFFSET, BROADCAST_META_SLOT_STRIDE, BROADCAST_SLOTS_OFFSET, BROADCAST_SLOT_STRIDE,
    HEADER_MAGIC, HEADER_SIZE, LAYOUT_VERSION, RECORD_HEADER_SIZE,
};
use crate::invalid;
use crate::ring::RING_OFFSET;
use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::ptr::NonNull;

pub(crate) struct Shared {
    fd: RawFd,
    pub(crate) ptr: NonNull<u8>,
    name: Option<CString>,
    pub(crate) capacity: usize,
    /// Total mapped length. SPSC uses `RING_OFFSET + capacity`; broadcast adds
    /// the subscriber slot table, so this must never be derived from `RING_OFFSET`.
    pub(crate) region_len: usize,
    /// Physical byte offset of the payload ring inside the mapping.
    pub(crate) ring_offset: usize,
}

// SAFETY: the mapping is process-shared memory addressed by a stable raw
// pointer. Cross-thread access is synchronized by the transport's atomics and,
// for the publisher, by the registry mutex; no thread-local state is stored in
// the region. Broadcast setup moves the handle between the application thread
// and the control worker.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.region_len);
            libc::close(self.fd);
            if let Some(name) = &self.name {
                libc::shm_unlink(name.as_ptr());
            }
        }
    }
}

impl Shared {
    pub(crate) fn unlink_name(&mut self) -> io::Result<()> {
        if let Some(name) = &self.name {
            if unsafe { libc::shm_unlink(name.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.name = None;
        }
        Ok(())
    }

    fn map(
        fd: RawFd, name: Option<CString>, capacity: usize, region_len: usize, ring_offset: usize,
    ) -> io::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                region_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
                if let Some(name) = &name {
                    libc::shm_unlink(name.as_ptr());
                }
            }
            return Err(error);
        }
        Ok(Self {
            fd,
            ptr: NonNull::new(ptr.cast()).unwrap(),
            name,
            capacity,
            region_len,
            ring_offset,
        })
    }

    fn bytes(&self) -> &[u8] {
        // This is used only while validating the fresh mapping before Ready.
        // Live queue traffic must use atomic indexes and bounded record reads.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.region_len) }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        // Called only by the creator before Offer, while no other process can open
        // the fresh object's undisclosed name. Shared owns the mapping lifetime.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.region_len) }
    }

    /// Opens and validates a POSIX shared-memory object created by `create`,
    /// checking the backing size, owner, mode, header, and fresh zero region.
    fn open_object(
        name: &str, region_len: usize, ring_offset: usize, capacity: usize, expected_header: [u8; HEADER_SIZE],
    ) -> io::Result<Self> {
        if !name.starts_with("/mc-") || name.len() > 30 || name[1..].contains('/') {
            return Err(invalid("invalid shared-memory name"));
        }
        let c_name = CString::new(name).map_err(|_| invalid("shared-memory name contains NUL"))?;
        let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDWR, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }
        // macOS reports a page-rounded backing size and zero permission bits
        // for POSIX shm. Linux reports the requested logical size and mode.
        #[cfg(target_os = "macos")]
        let expected_size = {
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if page <= 0 {
                unsafe {
                    libc::close(fd);
                }
                return Err(invalid("cannot determine shared-memory page size"));
            }
            region_len.div_ceil(page as usize) * page as usize
        };
        #[cfg(target_os = "linux")]
        let expected_size = region_len;
        #[cfg(target_os = "macos")]
        let mode_valid = stat.st_mode == 0 || stat.st_mode & 0o077 == 0;
        #[cfg(target_os = "linux")]
        let mode_valid = stat.st_mode & 0o077 == 0;
        if stat.st_size != expected_size as libc::off_t || stat.st_uid != unsafe { libc::geteuid() } || !mode_valid {
            unsafe {
                libc::close(fd);
            }
            return Err(invalid(format!(
                "shared-memory metadata mismatch: backing size={} (expected {}), owner={} (expected {}), mode={:o}",
                stat.st_size,
                expected_size,
                stat.st_uid,
                unsafe { libc::geteuid() },
                stat.st_mode & 0o777
            )));
        }
        let shared = Self::map(fd, None, capacity, region_len, ring_offset)?;
        let bytes = shared.bytes();
        if bytes[..HEADER_SIZE] != expected_header {
            return Err(invalid("shared-memory header mismatch"));
        }
        Ok(shared)
    }

    pub(crate) fn create(id: u64, capacity: usize, protocol_version: u32) -> io::Result<(Self, String)> {
        validate_capacity(capacity)?;
        let region_size = RING_OFFSET + capacity;
        // Keep the name within macOS's short POSIX shared-memory name limit.
        let name = format!("/mc-{:x}-{id:016x}", std::process::id());
        let c_name = CString::new(name.clone()).unwrap();
        let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::ftruncate(fd, region_size as libc::off_t) } != 0 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
                libc::shm_unlink(c_name.as_ptr());
            }
            return Err(error);
        }
        let mut shared = Self::map(fd, Some(c_name), capacity, region_size, RING_OFFSET)?;
        let bytes = shared.bytes_mut();
        bytes.fill(0);
        bytes[..8].copy_from_slice(HEADER_MAGIC);
        bytes[8..12].copy_from_slice(&LAYOUT_VERSION.to_be_bytes());
        bytes[12..16].copy_from_slice(&protocol_version.to_be_bytes());
        bytes[16..24].copy_from_slice(&id.to_be_bytes());
        bytes[24..28].copy_from_slice(&(region_size as u32).to_be_bytes());
        bytes[28..32].copy_from_slice(&(RING_OFFSET as u32).to_be_bytes());
        bytes[32..36].copy_from_slice(&(capacity as u32).to_be_bytes());
        bytes[36..40].copy_from_slice(&RECORD_HEADER_SIZE.to_be_bytes());
        Ok((shared, name))
    }

    pub(crate) fn open(name: &str, id: u64, capacity: usize, protocol_version: u32) -> io::Result<Self> {
        validate_capacity(capacity)?;
        let region_size = RING_OFFSET + capacity;
        let mut expected = [0; HEADER_SIZE];
        expected[..8].copy_from_slice(HEADER_MAGIC);
        expected[8..12].copy_from_slice(&LAYOUT_VERSION.to_be_bytes());
        expected[12..16].copy_from_slice(&protocol_version.to_be_bytes());
        expected[16..24].copy_from_slice(&id.to_be_bytes());
        expected[24..28].copy_from_slice(&(region_size as u32).to_be_bytes());
        expected[28..32].copy_from_slice(&(RING_OFFSET as u32).to_be_bytes());
        expected[32..36].copy_from_slice(&(capacity as u32).to_be_bytes());
        expected[36..40].copy_from_slice(&RECORD_HEADER_SIZE.to_be_bytes());
        let shared = Self::open_object(name, region_size, RING_OFFSET, capacity, expected)?;
        // A fresh SPSC object must have a completely zero live region: the
        // consumer has not started and no other process has the name yet.
        if shared.bytes()[HEADER_SIZE..].iter().any(|&b| b != 0) {
            return Err(invalid("shared-memory header or fresh queue region is invalid"));
        }
        Ok(shared)
    }

    /// Creates a broadcast mapping: immutable metadata, a write cursor, the
    /// subscriber slot table, and the payload ring. The whole region is zeroed
    /// before the name is disclosed, so a late subscriber never observes a
    /// partially initialized layout.
    pub(crate) fn create_broadcast(
        id: u64, capacity: usize, protocol_version: u32, max_subscribers: usize,
    ) -> io::Result<(Self, String)> {
        validate_capacity(capacity)?;
        validate_broadcast_layout(capacity, max_subscribers)?;
        let slots_bytes = BROADCAST_SLOT_STRIDE
            .checked_mul(max_subscribers)
            .ok_or_else(|| invalid("broadcast slot table size overflow"))?;
        let ring_offset = BROADCAST_SLOTS_OFFSET
            .checked_add(slots_bytes)
            .ok_or_else(|| invalid("broadcast ring offset overflow"))?;
        let region_size = ring_offset
            .checked_add(capacity)
            .ok_or_else(|| invalid("broadcast mapping size overflow"))?;
        if region_size > u32::MAX as usize {
            return Err(invalid("broadcast mapping exceeds the 32-bit wire size"));
        }
        let name = format!("/mc-{:x}-{id:016x}", std::process::id());
        let c_name = CString::new(name.clone()).unwrap();
        let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::ftruncate(fd, region_size as libc::off_t) } != 0 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
                libc::shm_unlink(c_name.as_ptr());
            }
            return Err(error);
        }
        let mut shared = Self::map(fd, Some(c_name), capacity, region_size, ring_offset)?;
        let bytes = shared.bytes_mut();
        bytes.fill(0);
        bytes[BROADCAST_META_MAGIC..BROADCAST_META_MAGIC + 8].copy_from_slice(BROADCAST_HEADER_MAGIC);
        bytes[BROADCAST_META_LAYOUT_VERSION..BROADCAST_META_LAYOUT_VERSION + 4]
            .copy_from_slice(&BROADCAST_LAYOUT_VERSION.to_be_bytes());
        bytes[BROADCAST_META_PROTOCOL_VERSION..BROADCAST_META_PROTOCOL_VERSION + 4]
            .copy_from_slice(&protocol_version.to_be_bytes());
        bytes[BROADCAST_META_SESSION..BROADCAST_META_SESSION + 8].copy_from_slice(&id.to_be_bytes());
        bytes[BROADCAST_META_REGION_SIZE..BROADCAST_META_REGION_SIZE + 4]
            .copy_from_slice(&(region_size as u32).to_be_bytes());
        bytes[BROADCAST_META_RING_OFFSET..BROADCAST_META_RING_OFFSET + 4]
            .copy_from_slice(&(ring_offset as u32).to_be_bytes());
        bytes[BROADCAST_META_CAPACITY..BROADCAST_META_CAPACITY + 4].copy_from_slice(&(capacity as u32).to_be_bytes());
        bytes[BROADCAST_META_RECORD_HEADER..BROADCAST_META_RECORD_HEADER + 4]
            .copy_from_slice(&RECORD_HEADER_SIZE.to_be_bytes());
        bytes[BROADCAST_META_SLOT_STRIDE..BROADCAST_META_SLOT_STRIDE + 4]
            .copy_from_slice(&(BROADCAST_SLOT_STRIDE as u32).to_be_bytes());
        bytes[BROADCAST_META_MAX_SUBSCRIBERS..BROADCAST_META_MAX_SUBSCRIBERS + 4]
            .copy_from_slice(&(max_subscribers as u32).to_be_bytes());
        bytes[BROADCAST_META_SLOTS_OFFSET..BROADCAST_META_SLOTS_OFFSET + 4]
            .copy_from_slice(&(BROADCAST_SLOTS_OFFSET as u32).to_be_bytes());
        Ok((shared, name))
    }

    /// Maps a live broadcast object for a late subscriber. Only the immutable
    /// metadata is read as ordinary bytes; live controls and slot state are
    /// accessed atomically by the caller. The fresh-zero validation used for
    /// SPSC must not be applied here: subscribers join a changing mapping.
    pub(crate) fn open_broadcast(
        name: &str, id: u64, capacity: usize, protocol_version: u32, max_subscribers: usize,
        expected_region_size: usize, expected_ring_offset: usize,
    ) -> io::Result<Self> {
        validate_capacity(capacity)?;
        validate_broadcast_layout(capacity, max_subscribers)?;
        let slots_bytes = BROADCAST_SLOT_STRIDE
            .checked_mul(max_subscribers)
            .ok_or_else(|| invalid("broadcast slot table size overflow"))?;
        let ring_offset = BROADCAST_SLOTS_OFFSET
            .checked_add(slots_bytes)
            .ok_or_else(|| invalid("broadcast ring offset overflow"))?;
        let region_size = ring_offset
            .checked_add(capacity)
            .ok_or_else(|| invalid("broadcast mapping size overflow"))?;
        if region_size != expected_region_size || ring_offset != expected_ring_offset {
            return Err(invalid("broadcast offered bounds are inconsistent"));
        }
        let mut expected = [0; HEADER_SIZE];
        expected[BROADCAST_META_MAGIC..BROADCAST_META_MAGIC + 8].copy_from_slice(BROADCAST_HEADER_MAGIC);
        expected[BROADCAST_META_LAYOUT_VERSION..BROADCAST_META_LAYOUT_VERSION + 4]
            .copy_from_slice(&BROADCAST_LAYOUT_VERSION.to_be_bytes());
        expected[BROADCAST_META_PROTOCOL_VERSION..BROADCAST_META_PROTOCOL_VERSION + 4]
            .copy_from_slice(&protocol_version.to_be_bytes());
        expected[BROADCAST_META_SESSION..BROADCAST_META_SESSION + 8].copy_from_slice(&id.to_be_bytes());
        expected[BROADCAST_META_REGION_SIZE..BROADCAST_META_REGION_SIZE + 4]
            .copy_from_slice(&(region_size as u32).to_be_bytes());
        expected[BROADCAST_META_RING_OFFSET..BROADCAST_META_RING_OFFSET + 4]
            .copy_from_slice(&(ring_offset as u32).to_be_bytes());
        expected[BROADCAST_META_CAPACITY..BROADCAST_META_CAPACITY + 4]
            .copy_from_slice(&(capacity as u32).to_be_bytes());
        expected[BROADCAST_META_RECORD_HEADER..BROADCAST_META_RECORD_HEADER + 4]
            .copy_from_slice(&RECORD_HEADER_SIZE.to_be_bytes());
        expected[BROADCAST_META_SLOT_STRIDE..BROADCAST_META_SLOT_STRIDE + 4]
            .copy_from_slice(&(BROADCAST_SLOT_STRIDE as u32).to_be_bytes());
        expected[BROADCAST_META_MAX_SUBSCRIBERS..BROADCAST_META_MAX_SUBSCRIBERS + 4]
            .copy_from_slice(&(max_subscribers as u32).to_be_bytes());
        expected[BROADCAST_META_SLOTS_OFFSET..BROADCAST_META_SLOTS_OFFSET + 4]
            .copy_from_slice(&(BROADCAST_SLOTS_OFFSET as u32).to_be_bytes());
        Self::open_object(name, region_size, ring_offset, capacity, expected)
    }

    pub(crate) fn ring(&self) -> *mut u8 {
        unsafe { self.ptr.as_ptr().add(self.ring_offset) }
    }
}
