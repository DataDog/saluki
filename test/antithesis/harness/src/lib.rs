//! Shared helpers for Antithesis test commands.

use std::time::Duration;

pub mod config;
#[cfg(unix)]
pub mod driver;
pub mod payload;
pub mod rand;

/// How long a context may take to appear on both lanes before it counts as a
/// divergence.
pub const ACCEPTABLE_FLUSH_DELAY: Duration = Duration::from_secs(30);

/// The per-context point-count skew tolerated between the two lanes before a shared context counts
/// as a divergence. Absorbs steady-state flush phase and the non-atomic double-read of the lanes.
pub const ACCEPTABLE_COUNT_SKEW: i64 = 1;
