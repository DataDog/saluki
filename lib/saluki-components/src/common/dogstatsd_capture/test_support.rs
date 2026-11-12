//! Shared test-support helpers for the DogStatsD capture/replay tests.

use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

use super::writer::{CaptureRecord, CaptureTargetDir, TrafficCaptureWriter};

/// Polls `is_ongoing` until it reports inactive (up to a fixed deadline), then asserts it actually stopped.
///
/// The capture writer and the capture control both expose an "is ongoing" predicate, but with different receiver
/// types, so this takes the predicate as a closure rather than a concrete handle.
pub(crate) fn wait_until_inactive(is_ongoing: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while is_ongoing() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    assert!(!is_ongoing(), "capture did not stop in time");
}

/// Creates a fresh temporary directory, removed when the returned guard drops.
pub(super) fn temp_dir() -> TempDir {
    tempfile::tempdir().expect("temporary directory should be created")
}

/// Captures `records` with `writer` into a fresh temporary directory, waiting for the capture file to be finished.
///
/// Returns the directory guard, which must outlive any use of the returned capture path.
pub(super) fn run_capture(
    writer: &TrafficCaptureWriter, compressed: bool, records: impl IntoIterator<Item = CaptureRecord>,
) -> (TempDir, PathBuf) {
    let dir = temp_dir();
    let path = writer
        .start_capture(
            CaptureTargetDir::Explicit(dir.path().to_path_buf()),
            Duration::from_secs(5),
            compressed,
        )
        .expect("capture should start");

    for record in records {
        assert!(writer.enqueue(record), "capture should accept the record");
    }
    writer.stop_capture();
    wait_until_inactive(|| writer.is_ongoing());

    (dir, path)
}
