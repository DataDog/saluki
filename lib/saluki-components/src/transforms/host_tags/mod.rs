use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::{DurationString, GenericConfiguration};
use saluki_context::tags::{SharedTagSet, Tag};
use saluki_core::{components::transforms::*, topology::EventsBuffer};
use saluki_core::{components::ComponentContext, data_model::event::metric::Metric};
use saluki_env::helpers::remote_agent::RemoteAgentClient;
use saluki_error::GenericError;
use stringtheory::MetaString;

/// Host Tags synchronous transform.
///
/// Temporarily adds host tags to metrics to compensate for backend delays when a new host comes online,
/// preventing gaps in queryability until the backend starts adding these tags automatically.
pub struct HostTagsConfiguration {
    client: RemoteAgentClient,
    expected_tags_duration: Duration,
}

const DEFAULT_EXPECTED_TAGS_DURATION: Duration = Duration::ZERO;

impl HostTagsConfiguration {
    /// Creates a new `HostTagsConfiguration` from the given configuration.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let client = RemoteAgentClient::from_configuration(config).await?;
        let expected_tags_duration = config
            .try_get_typed::<DurationString>("expected_tags_duration")?
            .map(|ds| ds.as_duration())
            .unwrap_or(DEFAULT_EXPECTED_TAGS_DURATION);

        Ok(Self {
            client,
            expected_tags_duration,
        })
    }
}

#[async_trait]
impl SynchronousTransformBuilder for HostTagsConfiguration {
    async fn build(&self, _context: ComponentContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        // Make an initial request of the host tags from the Datadog Agent.
        //
        // We only pay attention to the "system" tags, as the "google_cloud_platform" tags are not relevant here.
        let host_tags_reply = self.client.get_host_tags().await?.into_inner();
        let host_tags = host_tags_reply
            .system
            .into_iter()
            .map(|s| Arc::from(s.as_str()))
            .map(MetaString::from)
            .map(Tag::from)
            .collect::<SharedTagSet>();

        Ok(Box::new(HostTagsEnrichment {
            start: Instant::now(),
            expected_tags_duration: self.expected_tags_duration,
            host_tags: Some(host_tags),
        }))
    }
}

impl MemoryBounds for HostTagsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            .with_single_value::<HostTagsEnrichment>("component struct");
    }
}

pub struct HostTagsEnrichment {
    start: Instant,
    expected_tags_duration: Duration,
    host_tags: Option<SharedTagSet>,
}

impl HostTagsEnrichment {
    fn enrich_metric(&mut self, metric: &mut Metric) {
        let host_tags = match self.host_tags.as_ref() {
            Some(host_tags) => host_tags,
            None => return,
        };

        metric.context_mut().mutate_tags(|tags| tags.merge_shared(host_tags));
    }
}

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

    use saluki_context::Context;
    use saluki_core::data_model::event::metric::Metric;

    use super::*;

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
