//! Destination eligibility and correlation for sampled sketch payloads.

use std::sync::{Arc, LazyLock};

use http::HeaderValue;
use regex::Regex;
use url::Url;

use super::{endpoints::ResolvedEndpoint, protocol::V3ApiSettings};

static SITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|\.)([a-z]{2,}\d{1,2}\.)?(datad(?:oghq|0g)\.(?:com|eu)|ddog-gov\.com)$").unwrap()
});

pub(crate) fn site_allowed(endpoint: &Url, sites: &[String]) -> bool {
    let Some(host) = endpoint.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let Some(captures) = SITE.captures(&host) else {
        return false;
    };
    let start = captures.get(1).or_else(|| captures.get(2)).unwrap().start();
    sites.iter().any(|site| site == &host[start..])
}

/// Returns whether this V2 destination can receive sampled sketch copies.
pub(crate) fn sketch_shadow_eligible(endpoint: &ResolvedEndpoint, settings: &V3ApiSettings) -> bool {
    settings.shadow_sample_rate > 0.0
        && !settings
            .endpoints
            .iter()
            .any(|url| url == endpoint.configured_endpoint())
        && site_allowed(endpoint.endpoint(), &settings.shadow_sites)
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
