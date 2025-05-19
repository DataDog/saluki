use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::{
    tags::{Tag, TagSet},
    ContextResolver, ContextResolverBuilder,
};
use saluki_core::data_model::event::metric::Metric;
use saluki_core::{components::transforms::*, topology::interconnect::FixedSizeEventBuffer};
use saluki_env::helpers::remote_agent::RemoteAgentClient;
use saluki_error::{generic_error, GenericError};

/// Host Tags synchronous transform.
///
/// Temporarily adds host tags to metrics to compensate for backend delays when a new host comes online,
/// preventing gaps in queryability until the backend starts adding these tags automatically.
pub struct HostTagsConfiguration {
    client: RemoteAgentClient,
    host_tags_context_string_interner_bytes: ByteSize,
    expected_tags_duration: u64,
    ignore_duration: bool,
}

const DEFAULT_EXPECTED_TAGS_DURATION: u64 = 0;
const DEFAULT_HOST_TAGS_CONTEXT_STRING_INTERNER_BYTES: ByteSize = ByteSize::kib(64);

impl HostTagsConfiguration {
    /// Creates a new `HostTagsConfiguration` from the given configuration.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let client = RemoteAgentClient::from_configuration(config).await?;
        let expected_tags_duration = config
            .try_get_typed::<u64>("expected_tags_duration")?
            .unwrap_or(DEFAULT_EXPECTED_TAGS_DURATION);
        let host_tags_context_string_interner_bytes = config
            .try_get_typed::<ByteSize>("host_tags_context_string_interner_bytes")?
            .unwrap_or(DEFAULT_HOST_TAGS_CONTEXT_STRING_INTERNER_BYTES);

        Ok(Self {
            client,
            host_tags_context_string_interner_bytes,
            expected_tags_duration,
            ignore_duration: false,
        })
    }

    /// Ignore `expected_tags_duration` and always add host tags.
    pub fn ignore_duration(&mut self) {
        self.ignore_duration = true;
    }
}

#[async_trait]
impl SynchronousTransformBuilder for HostTagsConfiguration {
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        // Make an initial request of the  host tags from the Datadog Agent.
        //
        // We only pay attention to the "system" tags, as the "google_cloud_platform" tags are not relevant here.
        let host_tags_reply = self.client.get_host_tags().await?.into_inner();
        let host_tags = host_tags_reply.system.into_iter().map(Tag::from).collect::<TagSet>();

        let context_string_interner_size =
            NonZeroUsize::new(self.host_tags_context_string_interner_bytes.as_u64() as usize)
                .ok_or_else(|| generic_error!("context_string_interner_size must be greater than 0"))
                .unwrap();
        let context_resolver = ContextResolverBuilder::from_name("host_tags")
            .expect("resolver name is not empty")
            .with_interner_capacity_bytes(context_string_interner_size)
            .with_idle_context_expiration(Duration::from_secs(30))
            .with_expiration_interval(Duration::from_secs(1))
            .build();
        Ok(Box::new(HostTagsEnrichment {
            start: Instant::now(),
            context_resolver: Some(context_resolver),
            expected_tags_duration: Duration::from_secs(self.expected_tags_duration),
            host_tags: Some(host_tags),
            ignore_duration: self.ignore_duration,
        }))
    }
}

impl MemoryBounds for HostTagsConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            // Capture the size of the heap allocation when the component is built.
            .minimum()
            .with_single_value::<HostTagsEnrichment>("component struct")
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_fixed_amount(
                "string interner",
                self.host_tags_context_string_interner_bytes.as_u64() as usize,
            );
    }
}

pub struct HostTagsEnrichment {
    start: Instant,
    context_resolver: Option<ContextResolver>,
    expected_tags_duration: Duration,
    host_tags: Option<TagSet>,
    ignore_duration: bool,
}

impl HostTagsEnrichment {
    fn enrich_metric(&mut self, metric: &mut Metric) {
        if self.context_resolver.is_none() || self.host_tags.is_none() {
            return;
        }
        let resolver = self.context_resolver.as_mut().unwrap();
        let host_tags = self.host_tags.as_ref().unwrap();

        let tags = metric.context().tags().into_iter().chain(host_tags);

        if let Some(context) =
            resolver.resolve_with_origin_tags(metric.context().name(), tags, metric.context().origin_tags().clone())
        {
            *metric.context_mut() = context;
        }
    }
}

impl SynchronousTransform for HostTagsEnrichment {
    fn transform_buffer(&mut self, event_buffer: &mut FixedSizeEventBuffer) {
        // Skip adding host tags if duration has elapsed
        if !self.ignore_duration && self.start.elapsed() >= self.expected_tags_duration {
            self.context_resolver = None;
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

    use saluki_context::{Context, ContextResolverBuilder};
    use saluki_core::data_model::event::metric::Metric;

    use super::*;

    #[tokio::test]
    async fn basic() {
        let context_resolver = ContextResolverBuilder::for_tests().build();
        let host_tags = TagSet::from_iter(vec![Tag::from("hosttag1"), Tag::from("hosttag2")]);
        let mut host_tags_enrichment = HostTagsEnrichment {
            start: Instant::now(),
            context_resolver: Some(context_resolver),
            expected_tags_duration: Duration::from_secs(30),
            host_tags: Some(host_tags.clone()),
            ignore_duration: false,
        };

        let mut metric1 = Metric::gauge(Context::from_static_parts("test", &[]), 1.0);
        host_tags_enrichment.enrich_metric(&mut metric1);
        assert_eq!(metric1.context().tags().len(), host_tags.len());
        for tag in host_tags {
            assert!(metric1.context().tags().has_tag(tag));
        }

        // Simulate expected_tags_duration elapsing with context_resolver and host_tags being None
        host_tags_enrichment.context_resolver = None;
        host_tags_enrichment.host_tags = None;
        let mut metric2 = Metric::gauge(Context::from_static_parts("test", &[]), 1.0);
        host_tags_enrichment.enrich_metric(&mut metric2);
        assert_eq!(metric2.context().tags().len(), 0);
    }
}
