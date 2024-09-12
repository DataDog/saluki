//! Time provider.
//!
//! This module provides the `TimeProvider` trait, which deals with querying the current time in different forms.

use async_trait::async_trait;

mod coarse;
pub use self::coarse::CachedCoarseTimeProvider;

mod os;
pub use self::os::OperatingSystemPassthroughTimeProvider;

/// Allows querying the current time in different forms.
#[async_trait]
pub trait TimeProvider {
    /// Gets the current time as a Unix timestamp, in seconds.
    fn get_unix_timestamp(&self) -> u64;
}
