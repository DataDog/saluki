//! Shared state carried by each HTTP router.

use std::sync::{Arc, OnceLock};

use crate::capture;

/// Per-router state: the shared recorder handle plus the lane this router writes to.
#[derive(Clone, Debug)]
pub struct AppState {
    pub(crate) recorder: capture::State,
    pub(crate) target: capture::Target,
    /// First non-empty host resolved on this lane, set once. Pyld17 requires every series across all
    /// inbound traffic on the lane to resolve to this same host.
    pub(crate) established_host: Arc<OnceLock<String>>,
}

impl AppState {
    /// Creates router state for Datadog Agent intake.
    #[must_use]
    pub fn agent(recorder: &capture::State) -> Self {
        Self {
            recorder: recorder.clone(),
            target: capture::Target::Agent,
            established_host: Arc::default(),
        }
    }

    /// Creates router state for ADP intake.
    #[must_use]
    pub fn adp(recorder: &capture::State) -> Self {
        Self {
            recorder: recorder.clone(),
            target: capture::Target::Adp,
            established_host: Arc::default(),
        }
    }
}
