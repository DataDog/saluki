//! Host tags: fetching the host's tags from the Core Agent, and the legacy transform that added them to metrics.
//!
//! Host tags are added temporarily, to compensate for backend delays when a new host comes online, preventing gaps in
//! queryability until the backend starts adding these tags automatically. The plan-driven topology fetches them once
//! at boot, as a host fact ([`fetch_host_tags`]), and adds them with the host tags kernel
//! (`saluki_metric_kernels::host_enrichment`). The legacy transform, `HostTagsConfiguration`, is compiled only for
//! tests, as a differential reference.

use std::sync::Arc;
#[cfg(test)]
use std::time::{Duration, Instant};

#[cfg(test)]
use async_trait::async_trait;
use datadog_agent_commons::ipc::{client::RemoteAgentClient, config::RemoteAgentClientConfiguration};
use saluki_core::data_model::tags::{SharedTagSet, Tag};
#[cfg(test)]
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{transforms::*, BuildContext},
    data_model::event::metric::Metric,
    topology::EventsBuffer,
};
use saluki_error::GenericError;
#[cfg(test)]
use saluki_metric_kernels::host_enrichment::apply_tags;
use stringtheory::MetaString;

/// Host Tags synchronous transform: the legacy component, a differential reference for tests.
///
/// Temporarily adds host tags to metrics to compensate for backend delays when a new host comes online,
/// preventing gaps in queryability until the backend starts adding these tags automatically.
///
/// Tags are added by the host tags kernel (`saluki_metric_kernels::host_enrichment`). The deadline stays the native
/// one: `expected_tags_duration` after the transform is built, on a monotonic clock, decided when each buffer is
/// processed.
#[cfg(test)]
pub struct HostTagsConfiguration {
    client_config: RemoteAgentClientConfiguration,
    expected_tags_duration: Duration,
}

#[cfg(test)]
impl HostTagsConfiguration {
    /// Creates a new `HostTagsConfiguration` that fetches host tags from the Core Agent over the given IPC client
    /// configuration. A non-zero `expected_tags_duration` turns enrichment on and bounds how long the tags are
    /// attached; the default of zero leaves the transform inert.
    pub fn new(client_config: RemoteAgentClientConfiguration, expected_tags_duration: Duration) -> Self {
        Self {
            client_config,
            expected_tags_duration,
        }
    }

    /// Returns `true` if host tags enrichment is enabled.
    ///
    /// Enrichment is enabled when `expected_tags_duration` is non-zero. At the default of zero, host tags are never
    /// fetched and the transform does nothing.
    pub fn enabled(&self) -> bool {
        !self.expected_tags_duration.is_zero()
    }
}

/// Fetches the host's tags from the Core Agent over the given IPC client configuration.
///
/// Only the `system` tags are kept: the `google_cloud_platform` tags are not relevant to metrics.
///
/// # Errors
///
/// Returns an error if the Core Agent cannot be reached or does not answer the request.
pub(crate) async fn fetch_host_tags(
    client_config: &RemoteAgentClientConfiguration,
) -> Result<SharedTagSet, GenericError> {
    let client = RemoteAgentClient::connect(client_config).await?;
    let host_tags_reply = client.get_host_tags().await?.into_inner();
    Ok(host_tags_reply
        .system
        .into_iter()
        .map(|s| Arc::from(s.as_str()))
        .map(MetaString::from)
        .map(Tag::from)
        .collect::<SharedTagSet>())
}

#[cfg(test)]
#[async_trait]
impl SynchronousTransformBuilder for HostTagsConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        // Only fetch host tags when enrichment is enabled (`expected_tags_duration > 0`), matching the Core
        // Agent; at the default of 0 the tags are discarded immediately. Fetching always would block `build()`
        // on the Core Agent's slow `GetHostTags` RPC, delaying DogStatsD draining and starving origin detection.
        let host_tags = if self.enabled() {
            Some(fetch_host_tags(&self.client_config).await?)
        } else {
            None
        };

        Ok(Box::new(HostTagsEnrichment {
            start: Instant::now(),
            expected_tags_duration: self.expected_tags_duration,
            host_tags,
        }))
    }
}

#[cfg(test)]
impl MemoryBounds for HostTagsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<HostTagsEnrichment>("component struct");
    }
}

#[cfg(test)]
pub struct HostTagsEnrichment {
    start: Instant,
    expected_tags_duration: Duration,
    host_tags: Option<SharedTagSet>,
}

#[cfg(test)]
impl HostTagsEnrichment {
    fn enrich_metric(&mut self, metric: &mut Metric) {
        let host_tags = match self.host_tags.as_ref() {
            Some(host_tags) => host_tags,
            None => return,
        };

        apply_tags(host_tags, metric);
    }
}

#[cfg(test)]
impl SynchronousTransform for HostTagsEnrichment {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        // Skip adding host tags if duration has elapsed.
        if self.start.elapsed() >= self.expected_tags_duration {
            self.host_tags = None;
            return;
        }

        for event in event_buffer {
            if let Some(metric) = event.try_as_metric_mut() {
                self.enrich_metric(metric);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use saluki_core::data_model::event::{
        metric::{context::Context, Metric},
        Event,
    };

    use super::*;

    #[test]
    fn enrichment_stops_once_the_duration_has_elapsed() {
        let host_tags = SharedTagSet::from_iter(vec![Tag::from("hosttag1")]);
        let mut enrichment = HostTagsEnrichment {
            start: Instant::now(),
            expected_tags_duration: Duration::from_secs(3600),
            host_tags: Some(host_tags.clone()),
        };
        let buffer = || {
            let mut buffer = EventsBuffer::default();
            let metric = Metric::gauge(Context::from_static_parts("test", &[]), 1.0);
            assert!(buffer.try_push(Event::Metric(metric)).is_none());
            buffer
        };
        let tag_count = |buffer: EventsBuffer| {
            buffer
                .into_iter()
                .filter_map(|event| event.try_into_metric())
                .map(|metric| metric.context().tags().len())
                .collect::<Vec<_>>()
        };

        let mut before = buffer();
        enrichment.transform_buffer(&mut before);
        assert_eq!(tag_count(before), [1]);

        // Once the duration has elapsed, nothing is added, and the tags are let go for good.
        enrichment.expected_tags_duration = Duration::ZERO;
        let mut after = buffer();
        enrichment.transform_buffer(&mut after);
        assert_eq!(tag_count(after), [0]);
        assert!(enrichment.host_tags.is_none());
        enrichment.expected_tags_duration = Duration::from_secs(3600);
        let mut later = buffer();
        enrichment.transform_buffer(&mut later);
        assert_eq!(tag_count(later), [0]);
    }

    #[test]
    fn basic() {
        let host_tags = SharedTagSet::from_iter(vec![Tag::from("hosttag1"), Tag::from("hosttag2")]);
        let mut host_tags_enrichment = HostTagsEnrichment {
            start: Instant::now(),
            expected_tags_duration: Duration::from_secs(30),
            host_tags: Some(host_tags.clone()),
        };

        // Enrich a metric, and ensure the host tags are added.
        let mut metric1 = Metric::gauge(Context::from_static_parts("test", &[]), 1.0);
        host_tags_enrichment.enrich_metric(&mut metric1);
        assert_eq!(metric1.context().tags().len(), host_tags.len());
        for tag in &host_tags {
            assert!(metric1.context().tags().has_tag(tag));
        }

        // Simulate exceeding our configured enrichment duration by clearing host tags.
        host_tags_enrichment.host_tags = None;

        // We should no longer enrich the metric with host tags.
        let mut metric2 = Metric::gauge(Context::from_static_parts("test", &[]), 1.0);
        host_tags_enrichment.enrich_metric(&mut metric2);
        assert_eq!(metric2.context().tags().len(), 0);
    }
}
