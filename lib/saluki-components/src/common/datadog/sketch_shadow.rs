//! Destination-specific correlation for sampled sketch payloads.

use std::sync::Arc;

use http::HeaderValue;

/// A sampled destination and the ID shared by its V2 and V3 payload sets.
#[derive(Clone, Debug)]
pub(crate) struct SketchShadowDestination {
    pub configured_endpoint: Arc<str>,
    pub batch_id: HeaderValue,
}

/// Correlation metadata consumed before a transaction enters an endpoint's retry queue.
#[derive(Clone, Debug)]
pub(crate) struct SketchShadowMetadata {
    pub sampled_destinations: Arc<[SketchShadowDestination]>,
    pub request_index: usize,
    pub request_count: usize,
    pub is_shadow: bool,
}

impl SketchShadowMetadata {
    pub fn batch_id(&self, configured_endpoint: &str) -> Option<&HeaderValue> {
        self.sampled_destinations
            .iter()
            .find(|destination| destination.configured_endpoint.as_ref() == configured_endpoint)
            .map(|destination| &destination.batch_id)
    }
}
