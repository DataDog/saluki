use std::io;
use std::sync::atomic::AtomicU32;
pub(crate) fn wait(word: &AtomicU32, expected: u32) -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word as *const _ as *const u32,
            libc::FUTEX_WAIT,
            expected,
            std::ptr::null::<libc::timespec>(),
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) {
        Ok(())
    } else {
        Err(error)
    }
}
pub(crate) fn wake_all(word: &AtomicU32) -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_futex,
            word as *const _ as *const u32,
            libc::FUTEX_WAKE,
            libc::INT_MAX,
        )
    };
    if result >= 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
