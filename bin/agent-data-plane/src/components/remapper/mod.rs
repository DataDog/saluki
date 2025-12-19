use std::num::NonZeroUsize;

use async_trait::async_trait;
use bytesize::ByteSize;
use memory_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_context::{Context, ContextResolver, ContextResolverBuilder};
use saluki_core::data_model::event::{metric::*, Event, EventType};
use saluki_core::{
    components::{transforms::*, ComponentContext},
    topology::OutputDefinition,
};
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;
use tokio::select;
use tracing::{debug, error};

mod rules;
use self::rules::get_datadog_agent_remappings;

/// Agent telemetry remapper transform.
///
/// Remaps internal telemetry metrics in their generic Saluki form to the corresponding for used by the Datadog Agent
/// itself, based on how ADP is configured to mirror the Agent. This emits a duplicated set of metrics with their
/// remapped names.
pub struct AgentTelemetryRemapperConfiguration {
    context_string_interner_bytes: ByteSize,
}

impl AgentTelemetryRemapperConfiguration {
    /// Creates a new `AgentTelemetryRemapperConfiguration` with a default configuration.
    ///
    /// Uses a context resolver with a string interner capacity of 512KiB.
    pub fn new() -> Self {
        Self {
            context_string_interner_bytes: ByteSize::kib(512),
        }
    }
}

#[async_trait]
impl TransformBuilder for AgentTelemetryRemapperConfiguration {
    fn input_event_type(&self) -> EventType {
        EventType::Metric
    }

    fn outputs(&self) -> &[OutputDefinition<EventType>] {
        static OUTPUTS: &[OutputDefinition<EventType>] = &[OutputDefinition::default_output(EventType::Metric)];
        OUTPUTS
    }

    async fn build(&self, context: ComponentContext) -> Result<Box<dyn Transform + Send>, GenericError> {
        let context_string_interner_size = NonZeroUsize::new(self.context_string_interner_bytes.as_u64() as usize)
            .ok_or_else(|| generic_error!("context_string_interner_size must be greater than 0"))?;

        let context_resolver =
            ContextResolverBuilder::from_name(format!("{}/remapper/primary", context.component_id()))
                .expect("resolver name is not empty")
                .with_interner_capacity_bytes(context_string_interner_size)
                .build();

        Ok(Box::new(AgentTelemetryRemapper {
            context_resolver,
            rules: get_datadog_agent_remappings(),
        }))
    }
}

impl MemoryBounds for AgentTelemetryRemapperConfiguration {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder
            .minimum()
            // Capture the size of the heap allocation when the component is built.
            .with_single_value::<AgentTelemetryRemapper>("component struct")
            // We also allocate the backing storage for the string interner up front, which is used by our context
            // resolver.
            .with_fixed_amount("string interner", self.context_string_interner_bytes.as_u64() as usize);
    }
}

/// Agent telemetry remapper transform.
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
                maybe_events = context.events().next() => match maybe_events {
                    Some(events) => {
                        let mut buffered_dispatcher = context.dispatcher().buffered().expect("default output must always exist");
                        for event in &events {
                            if let Some(new_event) = event.try_as_metric().and_then(|metric| self.try_remap_metric(metric).map(Event::Metric)) {
                                if let Err(e) = buffered_dispatcher.push(new_event).await {
                                    error!(error = %e, "Failed to dispatch event.");
                                }
                            }
                        }

                        if let Err(e) = buffered_dispatcher.flush().await {
                            error!(error = %e, "Failed to dispatch events.");
                        }

                        if let Err(e) = context.dispatcher().dispatch(events).await {
                            error!(error = %e, "Failed to dispatch events.");
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

/// A metric remapping rule.
///
/// Rules define the basic matching behavior -- metric name, and optionally tags -- as well as how to remap the new copy
/// of the metric. This can include copying tags as-is from the source metric, copying specific tags over with a new
/// name, and adding an additional fixed set of tags to the new metric.
pub struct RemapperRule {
    existing_name: &'static str,
    existing_tags: &'static [&'static str],
    new_name: &'static str,
    remapped_tags: Vec<(&'static str, &'static str)>,
    additional_tags: Vec<MetaString>,
}

impl RemapperRule {
    /// Creates a new `RemapperRule` that matches a source metric by name only.
    pub fn by_name(existing_name: &'static str, new_name: &'static str) -> Self {
        Self {
            existing_name,
            existing_tags: &[],
            new_name,
            remapped_tags: Vec::new(),
            additional_tags: Vec::new(),
        }
    }

    /// Creates a new `RemapperRule` that matches a source metric by name and tags.
    pub fn by_name_and_tags(
        existing_name: &'static str, existing_tags: &'static [&'static str], new_name: &'static str,
    ) -> Self {
        Self {
            existing_name,
            existing_tags,
            new_name,
            remapped_tags: Vec::new(),
            additional_tags: Vec::new(),
        }
    }

    /// Adds a set of tags to remap from the source metric by changing their name.
    ///
    /// Remapped tags must be given in the form of `(source_tag, destination_tag)`. If a tag by the name `source_tag` is
    /// found in the source metric, it is copied to the remapped metric with a name of `destination_tag`.
    ///
    /// This method is additive, so it can be called multiple times to add more remapped tags. Tag remapping is
    /// order-dependent, so if a tag is configured to be remapped, or copied, multiple times, then the first match will
    /// take precedence.
    pub fn with_remapped_tags<I>(mut self, remapped_tags: I) -> Self
    where
        I: IntoIterator<Item = (&'static str, &'static str)>,
    {
        self.remapped_tags.extend(remapped_tags);
        self
    }

    /// Adds a set of tags to remap from the source metric without changing their name.
    ///
    /// Remapped tags must be given in the form of `source_tag`. If a tag by the name `source_tag` is found in the
    /// source metric, it is copied to the remapped metric with the same name.
    ///
    /// This method is additive, so it can be called multiple times to add more original tags. Tag remapping is
    /// order-dependent, so if a tag is configured to be copied, or remapped, multiple times, then the first match will
    /// take precedence.
    fn with_original_tags<I>(mut self, original_tags: I) -> Self
    where
        I: IntoIterator<Item = &'static str>,
    {
        self.remapped_tags
            .extend(original_tags.into_iter().map(|tag| (tag, tag)));
        self
    }

    /// Adds a fixed set of tags to add to the remapped metric.
    ///
    /// Additional tags are given in the form of `tag`, which can be any valid tag value: bare or key/value.
    ///
    /// This method is additive, so it can be called multiple times to add more additional tags.
    pub fn with_additional_tags<I>(mut self, additional_tags: I) -> Self
    where
        I: IntoIterator<Item = &'static str>,
    {
        self.additional_tags
            .extend(additional_tags.into_iter().map(MetaString::from_static));
        self
    }

    /// Attempts to match the given metric against this rule.
    ///
    /// If the rule is a match, `Some` is returned with the remapped metric. Otherwise, `None` is returned.
    pub fn try_match(&self, metric: &Metric, context_resolver: &mut ContextResolver) -> Option<Context> {
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

        // Add any additional tags that we need to include.
        for additional_tag in &self.additional_tags {
            new_tags.push(additional_tag.clone());
        }

        context_resolver.resolve(self.new_name, new_tags.as_slice(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remap_object_pool_metrics() {
        let mut remapper = AgentTelemetryRemapper {
            context_resolver: ContextResolverBuilder::for_tests().build(),
            rules: get_datadog_agent_remappings(),
        };

        let context = Context::from_static_parts("adp.object_pool_acquired", &["pool_name:dsd_packet_bufs"]);
        let metric = Metric::counter(context, 1.0);
        let new_metric = remapper.try_remap_metric(&metric).expect("should have remapped");
        assert_eq!(new_metric.context().name(), "dogstatsd.packet_pool_get");

        let context = Context::from_static_parts("adp.object_pool_released", &["pool_name:dsd_packet_bufs"]);
        let metric = Metric::counter(context, 1.0);
        let new_metric = remapper.try_remap_metric(&metric).expect("should have remapped");
        assert_eq!(new_metric.context().name(), "dogstatsd.packet_pool_put");

        let context = Context::from_static_parts("adp.object_pool_in_use", &["pool_name:dsd_packet_bufs"]);
        let metric = Metric::gauge(context, 1.0);
        let new_metric = remapper.try_remap_metric(&metric).expect("should have remapped");
        assert_eq!(new_metric.context().name(), "dogstatsd.packet_pool");
    }
}
