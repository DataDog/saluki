use std::{
    sync::atomic::{
        AtomicBool, AtomicU64,
        Ordering::{AcqRel, Relaxed},
    },
    thread,
    time::{Duration, SystemTime},
};

static GLOBAL_COARSE_UNIX_TIMESTAMP: AtomicU64 = AtomicU64::new(0);
static GLOBAL_COARSE_UPDATER_STARTED: AtomicBool = AtomicBool::new(false);

/// Gets the current time as a Unix timestamp, in seconds.
///
/// Unix timestamps are the number of seconds since the Unix epoch (1970-01-01 00:00:00 UTC).
pub fn get_unix_timestamp() -> u64 {
    let since_unix_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    since_unix_epoch.as_secs()
}

/// Gets the coarse current time as a Unix timestamp, in seconds.
///
/// Unix timestamps are the number of seconds since the Unix epoch (1970-01-01 00:00:00 UTC).
///
/// This function uses a cached version of the current time which is updated multiple times per second, providing a
/// "coarse" value that is not as accurate as possible, but is much faster to obtain. This function should only be used
/// when absolute accuracy is not required. See [`initialize_coarse_time_updater`] for more information.
///
/// The coarse time updater thread must be initialized to update the coarse time, which is done by calling
/// [`initialize_coarse_time_updater`]. If the updater thread has not been started, this function will fall back to
/// using [`get_unix_timestamp`] to get the current time, which may be slower if called at a high rate.
///
pub fn get_unix_timestamp_coarse() -> u64 {
    match GLOBAL_COARSE_UNIX_TIMESTAMP.load(Relaxed) {
        0 => get_unix_timestamp(),
        ts => ts,
    }
}

/// Initializes the coarse time updater thread, which will update the coarse time every 100ms.
///
/// The accuracy of the timestamp return will fall between -100ms and +0ms, meaning the timestamp may be up to 100ms in
/// the past, but will not be in the future. This is on top of any accuracy guarantees provided by the system clock,
/// accessed through [`SystemTime`].
pub fn initialize_coarse_time_updater() {
    // Try and claim the right to spawn the updater thread.
    if !GLOBAL_COARSE_UPDATER_STARTED.swap(true, AcqRel) {
        return;
    }

    // Spawn the updater thread, which will update the coarse time every ~100ms.
    thread::spawn(|| loop {
        let current_time = get_unix_timestamp();
        GLOBAL_COARSE_UNIX_TIMESTAMP.store(current_time, Relaxed);

        thread::sleep(Duration::from_millis(100));
    });
}
