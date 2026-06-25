//! Shared state carried by each HTTP router.

use std::sync::Arc;

/// Shared application state for one target's HTTP router.
#[derive(Clone, Debug)]
pub(crate) struct AppState {
    /// Configured Agent hostname. Pyld17 checks each series host against it.
    pub(crate) hostname: Arc<str>,
}
