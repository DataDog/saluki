use std::time::SystemTime;

use super::TimeProvider;

/// A time provider based on the operating system's time facilities.
///
/// This is the equivalent of calling [`SystemTime`] or [`Instant`] from the standard library, which defers to the
/// operating system, such as by calling `clock_gettime(...)` on Linux.
pub struct OperatingSystemPassthroughTimeProvider;

impl TimeProvider for OperatingSystemPassthroughTimeProvider {
	fn get_unix_timestamp(&self) -> u64 {
		let since_unix_epoch = SystemTime::now()
			.duration_since(SystemTime::UNIX_EPOCH)
			.unwrap_or_default();
		since_unix_epoch.as_secs()
	}
}
