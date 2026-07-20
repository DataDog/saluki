//! Shared test-support helpers for the DogStatsD capture/replay tests.

use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Polls `is_ongoing` until it reports inactive (up to a fixed deadline), then asserts it actually stopped.
///
/// The capture writer and the capture control both expose an "is ongoing" predicate, but with different receiver
/// types, so this takes the predicate as a closure rather than a concrete handle.
pub(super) fn wait_until_inactive(is_ongoing: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while is_ongoing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    assert!(!is_ongoing(), "capture did not stop in time");
}

/// Creates, and returns the path to, a unique, freshly created temporary directory for a test.
pub(super) fn unique_dir(label: &str) -> PathBuf {
    let path = unique_path(label);
    fs::create_dir_all(&path).expect("test directory should be created");
    path
}

/// Returns a unique temporary path scoped to the current process and a nanosecond timestamp.
pub(super) fn unique_path(label: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("saluki-{}-{}-{}", label, std::process::id(), timestamp))
}
