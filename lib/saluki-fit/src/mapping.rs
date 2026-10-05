use crate::config::validate_capacity;
use crate::contract::{HEADER_MAGIC, HEADER_SIZE, LAYOUT_VERSION, RECORD_HEADER_SIZE};
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
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), RING_OFFSET + self.capacity);
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

    fn map(fd: RawFd, name: Option<CString>, capacity: usize) -> io::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                RING_OFFSET + capacity,
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
        })
    }

    fn bytes(&self) -> &[u8] {
        // This is used only while validating the fresh mapping before Ready.
        // Live queue traffic must use atomic indexes and bounded record reads.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), RING_OFFSET + self.capacity) }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        // Called only by the creator before Offer, while no other process can open
        // the fresh object's undisclosed name. Shared owns the mapping lifetime.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), RING_OFFSET + self.capacity) }
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
        let mut shared = Self::map(fd, Some(c_name), capacity)?;
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
            region_size.div_ceil(page as usize) * page as usize
        };
        #[cfg(target_os = "linux")]
        let expected_size = region_size;
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
        let shared = Self::map(fd, None, capacity)?;
        let bytes = shared.bytes();
        let mut expected = [0; HEADER_SIZE];
        expected[..8].copy_from_slice(HEADER_MAGIC);
        expected[8..12].copy_from_slice(&LAYOUT_VERSION.to_be_bytes());
        expected[12..16].copy_from_slice(&protocol_version.to_be_bytes());
        expected[16..24].copy_from_slice(&id.to_be_bytes());
        expected[24..28].copy_from_slice(&(region_size as u32).to_be_bytes());
        expected[28..32].copy_from_slice(&(RING_OFFSET as u32).to_be_bytes());
        expected[32..36].copy_from_slice(&(capacity as u32).to_be_bytes());
        expected[36..40].copy_from_slice(&RECORD_HEADER_SIZE.to_be_bytes());
        if bytes[..HEADER_SIZE] != expected || bytes[HEADER_SIZE..].iter().any(|&b| b != 0) {
            return Err(invalid("shared-memory header or fresh queue region is invalid"));
        }
        Ok(shared)
    }
}
