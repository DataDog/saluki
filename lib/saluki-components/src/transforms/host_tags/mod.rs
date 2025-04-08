use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::{ContextResolver, ContextResolverBuilder};
use saluki_core::{components::transforms::*, topology::interconnect::FixedSizeEventBuffer};
use saluki_env::helpers::remote_agent::RemoteAgentClient;
use saluki_error::{generic_error, GenericError};
use saluki_event::metric::Metric;

/// Host Tags synchronous transform.
///
/// Temporarily adds host tags to metrics to compensate for backend delays when a new host comes online,
/// preventing gaps in queryability until the backend starts adding these tags automatically.
pub struct HostTagsConfiguration {
    client: RemoteAgentClient,
    host_tags_context_string_interner_bytes: ByteSize,
    expected_tags_duration: u64,
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
        })
    }
}

#[async_trait]
impl SynchronousTransformBuilder for HostTagsConfiguration {
    async fn build(&self) -> Result<Box<dyn SynchronousTransform + Send>, GenericError> {
        // Request the host tags from the Datadog Agent only once.
        let host_tags_reply = self.client.get_host_tags().await?.into_inner();
        // `HostTagReply` consists of `system` and `google_cloud_platform` tags but only `system` tags are attached.
        let host_tags = host_tags_reply.system.to_owned();

        let context_string_interner_size =
            NonZeroUsize::new(self.host_tags_context_string_interner_bytes.as_u64() as usize)
                .ok_or_else(|| generic_error!("context_string_interner_size must be greater than 0"))
                .unwrap();
        let context_resolver = ContextResolverBuilder::from_name("host_tags")
            .expect("resolver name is not empty")
            .with_interner_capacity_bytes(context_string_interner_size)
            .with_idle_context_expiration(Duration::from_secs(self.expected_tags_duration))
            .with_expiration_interval(Duration::from_secs(1))
            .build();
        Ok(Box::new(HostTagsEnrichment {
            start: Instant::now(),
            context_resolver: Some(context_resolver),
            expected_tags_duration: Duration::from_secs(self.expected_tags_duration),
            host_tags: Some(host_tags),
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
    host_tags: Option<Vec<String>>,
}

impl HostTagsEnrichment {
    fn enrich_metric(&mut self, metric: &mut Metric) {
        let resolver = self.context_resolver.as_mut().unwrap();
        let host_tags = self.host_tags.as_ref().unwrap();

        let mut tags = Vec::with_capacity(metric.context().tags().len() + host_tags.len());
        tags.extend(metric.context().tags().into_iter().map(|t| t.as_str()));
        tags.extend(host_tags.iter().map(|t| t.as_str()));

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
        if self.start.elapsed() >= self.expected_tags_duration {
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
