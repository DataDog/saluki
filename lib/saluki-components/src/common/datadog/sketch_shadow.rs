//! Destination eligibility and correlation for sampled sketch payloads.

use std::sync::Arc;

use http::HeaderValue;

use super::{
    endpoints::{extract_datadog_site, ResolvedEndpoint},
    protocol::V3ApiSettings,
};

/// Returns whether this V2 destination can receive sampled sketch copies.
pub(crate) fn sketch_shadow_eligible(endpoint: &ResolvedEndpoint, settings: &V3ApiSettings) -> bool {
    settings.shadow_sample_rate > 0.0
        && !settings
            .endpoints
            .iter()
            .any(|url| url == endpoint.configured_endpoint())
        && extract_datadog_site(endpoint.endpoint()).is_some_and(|site| settings.shadow_sites.contains(&site))
}

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
