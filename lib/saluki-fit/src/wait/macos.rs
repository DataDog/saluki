use std::io;
use std::sync::atomic::AtomicU32;
pub(crate) fn wait(word: &AtomicU32, expected: u32) -> io::Result<()> {
    let result = unsafe {
        libc::os_sync_wait_on_address(
            word as *const _ as *mut _,
            expected as u64,
            4,
            libc::OS_SYNC_WAIT_ON_ADDRESS_SHARED,
        )
    };
    if result >= 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::EINTR | libc::ENOMEM | libc::EFAULT)) {
        Ok(())
    } else {
        Err(error)
    }
}
pub(crate) fn wake(word: &AtomicU32) -> io::Result<()> {
    let result = unsafe {
        libc::os_sync_wake_by_address_any(word as *const _ as *mut _, 4, libc::OS_SYNC_WAKE_BY_ADDRESS_SHARED)
    };
    if result >= 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(())
    } else {
        Err(error)
    }
}
