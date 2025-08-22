//! Time-related functions.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Once,
    },
    thread,
    time::{Duration, SystemTime},
};

static COARSE_TIME_INITIALIZED: Once = Once::new();
static COARSE_TIME: AtomicU64 = AtomicU64::new(0);
const COARSE_TIME_UPDATE_INTERVAL: Duration = Duration::from_millis(250);

/// Get the current Unix timestamp, in seconds.
///
/// This function is accurate, as it always retrieves the current time for each call. In scenarios where this function
/// is being called frequently, it may pose an unacceptable performance overhead. In such cases, consider using
/// `get_coarse_unix_timestamp`, which provides a cached value that is updated periodically.
pub fn get_unix_timestamp() -> u64 {
    let since_unix_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    since_unix_epoch.as_secs()
}

/// Get the current Unix timestamp, in nanoseconds.
///
/// This function is accurate, as it always retrieves the current time for each call. In scenarios where this function
/// is being called frequently, it may pose an unacceptable performance overhead. In such cases, consider using
/// `get_coarse_unix_timestamp`, which provides a cached value that is updated periodically.
pub fn get_unix_timestamp_nanos() -> u128 {
    let since_unix_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    since_unix_epoch.as_nanos()
}

/// Get the current coarse Unix timestamp, in seconds.
///
/// In scenarios where the current Unix timestamp is needed frequently, this function provides a cached value that is
/// updated periodically. As the precision of the timestamp is one second, this function allows trading off accuracy
/// (see below) for reduced overhead. The resulting value is considered "coarse", because it might be off by significant
/// percentage of the overall precision.
///
/// # Accuracy
///
/// The underlying coarse time is updated roughly every 250 milliseconds, so the value returned by this function may be
/// behind by up to 250 milliseconds. This means that if calling the function at true time `t` (where `t` is in
/// seconds), the value returned may be `t-1` _or_ `t`, but will never be behind by more than 250 milliseconds, and
/// never _ahead_ of `t`.
pub fn get_coarse_unix_timestamp() -> u64 {
    // Initialize a background thread to update the coarse time if it hasn't been initialized yet.
    COARSE_TIME_INITIALIZED.call_once(|| {
        // Initialize the coarse time with the current Unix timestamp.
        COARSE_TIME.store(get_unix_timestamp(), Relaxed);

        thread::spawn(|| {
            loop {
                // Sleep for 250 milliseconds.
                thread::sleep(COARSE_TIME_UPDATE_INTERVAL);

                // Update the coarse time with the current Unix timestamp.
                COARSE_TIME.store(get_unix_timestamp(), Relaxed);
            }
        });
    });

    COARSE_TIME.load(Relaxed)
}
