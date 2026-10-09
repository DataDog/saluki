//! Wall-clock timestamps for records the UI shows.

use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current time as milliseconds since the Unix epoch.
///
/// A clock set before the epoch reads as zero.
pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
