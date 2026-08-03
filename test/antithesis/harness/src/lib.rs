//! Shared helpers for Antithesis test commands.

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub mod config;
pub mod contexts;
pub mod dogstatsd;
#[cfg(unix)]
pub mod driver;
pub mod payload;
pub mod rand;

/// How long a context may take to appear on both lanes before it counts as a
/// divergence.
pub const ACCEPTABLE_FLUSH_DELAY: Duration = Duration::from_secs(30);

/// Which differential check posted to an oracle. The intake picks its assertion name from this, so a
/// divergence under load reports apart from one that outlives the drain.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The check that runs while load is still arriving.
    Eventually,
    /// The check that runs once load has drained.
    Finally,
}
