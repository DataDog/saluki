use std::num::NonZeroUsize;

use async_trait::async_trait;
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_context::{Context, ContextResolver, ContextResolverBuilder};
use saluki_core::{components::transforms::*, topology::OutputDefinition};
use saluki_error::{generic_error, GenericError};
use saluki_event::{metric::*, DataType, Event};
use stringtheory::MetaString;
use tokio::select;
use tracing::{debug, error};

/// Agent telemetry remapper transform.
///
/// Remaps internal telemetry metrics in their generic Saluki form to the corresponding for used by the Datadog Agent
/// itself, based on how ADP is configured to mirror the Agent. This emits a duplicated set of metrics with their
/// remapped names.
pub struct AgentTelemetryRemapperConfiguration {
    context_string_interner_bytes: ByteSize,
}

impl AgentTelemetryRemapperConfiguration {
    pub fn new() -> Self {
        Self {
            context_string_interner_bytes: ByteSize::kib(512),
        }
    }
}

#[async_trait]
impl TransformBuilder for AgentTelemetryRemapperConfiguration {
    fn input_data_type(&self) -> DataType {
        DataType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];
        OUTPUTS
    }

    async fn build(&self) -> Result<Box<dyn Transform + Send>, GenericError> {
        let context_string_interner_size = NonZeroUsize::new(self.context_string_interner_bytes.as_u64() as usize)
            .ok_or_else(|| generic_error!("context_string_interner_size must be greater than 0"))?;
        let context_resolver = ContextResolverBuilder::from_name("agent_telemetry_remapper")
            .expect("resolver name is not empty")
            .with_interner_capacity_bytes(context_string_interner_size)
            .build();

        Ok(Box::new(AgentTelemetryRemapper {
            context_resolver,
            rules: generate_remapper_rules(),
        }))
    }
}

impl MemoryBounds for AgentTelemetryRemapperConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<AgentTelemetryRemapper>()
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_fixed_amount(self.context_string_interner_bytes.as_u64() as usize);
    }
}

pub struct AgentTelemetryRemapper {
    context_resolver: ContextResolver,
    rules: Vec<RemapperRule>,
}

impl AgentTelemetryRemapper {
    fn try_remap_metric(&mut self, metric: &Metric) -> Option<Metric> {
        for rule in &self.rules {
            if let Some(new_context) = rule.try_match(metric, &mut self.context_resolver) {
                return Some(Metric::from_parts(
                    new_context,
                    metric.values().clone(),
                    metric.metadata().clone(),
                ));
            }
        }

        None
    }
}

#[async_trait]
impl Transform for AgentTelemetryRemapper {
    async fn run(mut self: Box<Self>, mut context: TransformContext) -> Result<(), GenericError> {
        let mut health = context.take_health_handle();
        health.mark_ready();

        debug!("Agent telemetry remapper transform started.");

        loop {
            select! {
                _ = health.live() => continue,
                maybe_events = context.event_stream().next() => match maybe_events {
                    Some(events) => {
                        let mut buffered_forwarder = context.forwarder().buffered().expect("default output must always exist");
                        for event in &events {
                            if let Some(new_event) = event.try_as_metric().and_then(|metric| self.try_remap_metric(metric).map(Event::Metric)) {
                                if let Err(e) = buffered_forwarder.push(new_event).await {
                                    error!(error = %e, "Failed to forward event.");
                                }
                            }
                        }

                        if let Err(e) = buffered_forwarder.flush().await {
                            error!(error = %e, "Failed to forward events.");
                        }

                        if let Err(e) = context.forwarder().forward_buffer(events).await {
                            error!(error = %e, "Failed to forward events.");
                        }
                    },
                    None => break,
                },
            }
        }

        debug!("Agent telemetry remapper transform stopped.");

        Ok(())
    }
}

fn generate_remapper_rules() -> Vec<RemapperRule> {
    vec![
        // Object pool.
        RemapperRule::by_name_and_tags(
            "datadog.saluki.object_pool_acquired",
            &["pool_name:dsd_packet_bufs"],
            "datadog.agent.dogstatsd.packet_pool_get",
        ),
        RemapperRule::by_name_and_tags(
            "datadog.saluki.object_pool_released",
            &["pool_name:dsd_packet_bufs"],
            "datadog.agent.dogstatsd.packet_pool_put",
        ),
        RemapperRule::by_name_and_tags(
            "datadog.saluki.object_pool_in_use",
            &["pool_name:dsd_packet_bufs"],
            "datadog.agent.dogstatsd.packet_pool",
        ),
        // DogStatsD source.
        RemapperRule::by_name_and_tags(
            "datadog.saluki.component_packets_received_total",
            &["component_id:dsd_in", "listener_type:udp"],
            "datadog.agent.dogstatsd.udp_packets",
        )
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "datadog.saluki.component_bytes_received_total",
            &["component_id:dsd_in", "listener_type:udp"],
            "datadog.agent.dogstatsd.udp_packet_bytes",
        ),
        RemapperRule::by_name_and_tags(
            "datadog.saluki.component_packets_received_total",
            &["component_id:dsd_in", "listener_type:unixgram"],
            "datadog.agent.dogstatsd.uds_packets",
        )
        .with_remapped_tags([("listener_type", "transport")])
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "datadog.saluki.component_bytes_received_total",
            &["component_id:dsd_in", "listener_type:unixgram"],
            "datadog.agent.dogstatsd.uds_packet_bytes",
        )
        .with_remapped_tags([("listener_type", "transport")])
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "datadog.saluki.component_packets_received_total",
            &["component_id:dsd_in", "listener_type:unix"],
            "datadog.agent.dogstatsd.uds_packets",
        )
        .with_remapped_tags([("listener_type", "transport")])
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "datadog.saluki.component_bytes_received_total",
            &["component_id:dsd_in", "listener_type:unix"],
            "datadog.agent.dogstatsd.uds_packet_bytes",
        )
        .with_remapped_tags([("listener_type", "transport")])
        .with_original_tags(["state"]),
        RemapperRule::by_name_and_tags(
            "datadog.saluki.component_connections_active",
            &["component_id:dsd_in", "listener_type:unix"],
            "datadog.agent.dogstatsd.uds_connections",
        )
        .with_remapped_tags([("listener_type", "transport")]),
    ]
}

struct RemapperRule {
    existing_name: &'static str,
    existing_tags: &'static [&'static str],
    new_name: &'static str,
    remapped_tags: Vec<(&'static str, &'static str)>,
}

impl RemapperRule {
    fn by_name_and_tags(
        existing_name: &'static str, existing_tags: &'static [&'static str], new_name: &'static str,
    ) -> Self {
        Self {
            existing_name,
            existing_tags,
            new_name,
            remapped_tags: Vec::new(),
        }
    }

    fn with_remapped_tags<I>(mut self, remapped_tags: I) -> Self
    where
        I: IntoIterator<Item = (&'static str, &'static str)>,
    {
        self.remapped_tags.extend(remapped_tags);
        self
    }

    fn with_original_tags<I>(mut self, original_tags: I) -> Self
    where
        I: IntoIterator<Item = &'static str>,
    {
        self.remapped_tags
            .extend(original_tags.into_iter().map(|tag| (tag, tag)));
        self
    }

    fn try_match(&self, metric: &Metric, context_resolver: &mut ContextResolver) -> Option<Context> {
        // See if the metric matches the name and, potentially, tags that we're looking for.
        if metric.context().name() != self.existing_name {
            return None;
        }

        let metric_tags = metric.context().tags();
        for existing_tag in self.existing_tags {
            if !metric_tags.has_tag(existing_tag) {
                return None;
            }
        }

        // Build the new tags to use.
        //
        // We always add `emitted_by:adp` to the new context to avoid overwriting metrics by the same name that are
        // still emitted by the Datadog Agent, and then after that, we handle any tag remapping. Remapped tags are
        // either a straight copy (take the tag as-is) or a rename (different tag name).
        let mut new_tags = vec![MetaString::from_static("emitted_by:adp")];
        for (original_tag_name, new_tag_name) in &self.remapped_tags {
            if original_tag_name == new_tag_name {
                // All we need to do is find the tag and clone it since the name isn't changing.
                if let Some(tag) = metric_tags.get_single_tag(original_tag_name) {
                    if original_tag_name == new_tag_name {
                        // Just clone the tag since the name isn't changing.
                        new_tags.push(tag.clone().into_inner());
                    } else {
                        // Build our new tag if this one has a value.
                        match tag.value() {
                            Some(value) => {
                                new_tags.push(MetaString::from(format!("{}:{}", new_tag_name, value)));
                            }
                            None => {
                                new_tags.push(MetaString::from(*new_tag_name));
                            }
                        }
                    }
                }
            }
        }

        let context_ref = context_resolver.create_context_ref(self.new_name, new_tags.as_slice().iter());
        context_resolver.resolve(context_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remap_object_pool_metrics() {
        let mut remapper = AgentTelemetryRemapper {
            context_resolver: ContextResolverBuilder::for_tests(),
            rules: generate_remapper_rules(),
        };

        let context = Context::from_static_parts("datadog.saluki.object_pool_acquired", &["pool_name:dsd_packet_bufs"]);
        let metric = Metric::counter(context, 1.0);
        let new_metric = remapper.try_remap_metric(&metric).expect("should have remapped");
        assert_eq!(new_metric.context().name(), "datadog.agent.dogstatsd.packet_pool_get");

        let context = Context::from_static_parts("datadog.saluki.object_pool_released", &["pool_name:dsd_packet_bufs"]);
        let metric = Metric::counter(context, 1.0);
        let new_metric = remapper.try_remap_metric(&metric).expect("should have remapped");
        assert_eq!(new_metric.context().name(), "datadog.agent.dogstatsd.packet_pool_put");

        let context = Context::from_static_parts("datadog.saluki.object_pool_in_use", &["pool_name:dsd_packet_bufs"]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = remapper.try_remap_metric(&metric).expect("should have remapped");
        assert_eq!(new_metric.context().name(), "datadog.agent.dogstatsd.packet_pool");
    }
}
