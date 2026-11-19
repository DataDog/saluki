//! The limits of a step that polls the software it checks.

use std::time::Duration;

/// Maximum duration of one polling attempt.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause between polling attempts.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(500);
