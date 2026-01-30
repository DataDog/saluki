//! Simulated request tracking.

use rand::Rng;

use crate::model::TraceContext;

/// A simulated request being processed through the service graph.
pub struct SimulatedRequest {
    /// The root trace context for this request.
    pub root_context: TraceContext,
}

impl SimulatedRequest {
    /// Creates a new simulated request with a fresh trace context.
    pub fn new(rng: &mut impl Rng) -> Self {
        Self {
            root_context: TraceContext::new_root(rng),
        }
    }
}
