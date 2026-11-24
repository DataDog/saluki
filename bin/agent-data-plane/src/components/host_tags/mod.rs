use std::time::{Duration, Instant};

use async_trait::async_trait;
use datadog_agent_commons::ipc::{client::RemoteAgentClient, config::RemoteAgentClientConfiguration};
use saluki_core::{
    accounting::{MemoryBounds, MemoryBoundsBuilder},
    components::{transforms::*, BuildContext},
    data_model::event::metric::Metric,
    topology::EventsBuffer,
};
use saluki_error::GenericError;
use saluki_metric_kernels::host_tags::{filter_metric_host_tags, HostTagsKernel};

/// Host Tags synchronous transform.
///
/// Temporarily adds host tags to metrics to compensate for backend delays when a new host comes online,
/// preventing gaps in queryability until the backend starts adding these tags automatically.
pub struct HostTagsConfiguration {
    client_config: RemoteAgentClientConfiguration,
    expected_tags_duration: Duration,
}

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

#[async_trait]
impl SynchronousTransformBuilder for HostTagsConfiguration {
    async fn build(&self, _context: BuildContext) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        // Only fetch host tags when enrichment is enabled (`expected_tags_duration > 0`), matching the Core
        // Agent; at the default of 0 the tags are discarded immediately. Fetching always would block `build()`
        // on the Core Agent's slow `GetHostTags` RPC, delaying DogStatsD draining and starving origin detection.
        let host_tags = if self.enabled() {
            // We only pay attention to the "system" tags, as the "google_cloud_platform" tags are not relevant here.
            let client = RemoteAgentClient::connect(&self.client_config).await?;
            let host_tags_reply = client.get_host_tags().await?.into_inner();
            Some(filter_metric_host_tags(host_tags_reply.system))
        } else {
            None
        };

        Ok(Box::new(HostTagsEnrichment {
            start: Instant::now(),
            expected_tags_duration: self.expected_tags_duration,
            kernel: HostTagsKernel::new(host_tags),
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
    kernel: HostTagsKernel,
}

impl HostTagsEnrichment {
    fn enrich_metric(&mut self, metric: &mut Metric) {
        self.kernel.apply(metric);
    }
}

impl SynchronousTransform for HostTagsEnrichment {
    fn transform_buffer(&mut self, event_buffer: &mut EventsBuffer) {
        // Skip adding host tags if duration has elapsed.
        if self.start.elapsed() >= self.expected_tags_duration {
            self.kernel.clear();
            return;
        }

        for metric in event_buffer.events_mut::<Metric>() {
            self.enrich_metric(metric);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use saluki_core::data_model::{
        event::metric::{context::Context, Metric},
        tags::{SharedTagSet, Tag},
    };

    use super::*;

    #[test]
    fn basic() {
        let host_tags = SharedTagSet::from_iter(vec![Tag::from("hosttag1"), Tag::from("hosttag2")]);
        let mut host_tags_enrichment = HostTagsEnrichment {
            start: Instant::now(),
            expected_tags_duration: Duration::from_secs(30),
            kernel: HostTagsKernel::new(Some(host_tags.clone())),
        };

        // Enrich a metric, and ensure the host tags are added.
        let mut metric1 = Metric::gauge(Context::from_static_parts("test", &[]), 1.0);
        host_tags_enrichment.enrich_metric(&mut metric1);
        assert_eq!(metric1.context().tags().len(), host_tags.len());
        for tag in &host_tags {
            assert!(metric1.context().tags().has_tag(tag));
        }

        // Simulate exceeding our configured enrichment duration by clearing host tags.
        host_tags_enrichment.kernel.clear();

        // We should no longer enrich the metric with host tags.
        let mut metric2 = Metric::gauge(Context::from_static_parts("test", &[]), 1.0);
        host_tags_enrichment.enrich_metric(&mut metric2);
        assert_eq!(metric2.context().tags().len(), 0);
    }

    #[test]
    fn infra_mode_host_tags_are_not_attached_to_metrics() {
        // Mirrors the Agent's `WithoutInfraModeTags` (DataDog/datadog-agent#57905), which drops only
        // `infra_mode:*`.
        let host_tags = filter_metric_host_tags(vec![
            "env:prod".to_string(),
            "infra_mode:cloud_cost_only".to_string(),
            "infra_mode:".to_string(),
            "infra_mode".to_string(),
            "infra_mode_extra:x".to_string(),
        ]);

        let expected = SharedTagSet::from_iter(vec![
            Tag::from("env:prod"),
            Tag::from("infra_mode"),
            Tag::from("infra_mode_extra:x"),
        ]);
        assert_eq!(host_tags.len(), expected.len());
        for tag in &expected {
            assert!(host_tags.has_tag(tag), "{tag:?}");
        }
    }
}
