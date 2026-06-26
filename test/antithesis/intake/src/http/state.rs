//! Shared state carried by each HTTP router.

use std::sync::{Arc, OnceLock};

/// Shared application state for one target's HTTP router.
#[derive(Clone, Debug, Default)]
pub(crate) struct AppState {
    /// First non-empty host resolved inbound. Pyld17 requires every series
    /// across all inbound traffic to resolve to this same host.
    pub(crate) established_host: Arc<OnceLock<String>>,
}
