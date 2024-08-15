use std::{num::NonZeroUsize, sync::Arc};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use memory_accounting::ComponentRegistry;
use saluki_config::GenericConfiguration;
use saluki_context::TagSet;
use saluki_error::GenericError;
use stringtheory::interning::FixedSizeInterner;

use crate::{
    features::{Feature, FeatureDetector},
    workload::{
        aggregator::MetadataAggregator,
        collectors::{ContainerdMetadataCollector, RemoteAgentMetadataCollector},
        entity::EntityId,
        metadata::TagCardinality,
        store::TagSnapshot,
    },
    WorkloadProvider,
};

// SAFETY: We know the value is not zero.
const DEFAULT_TAG_INTERNER_SIZE_BYTES: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(512 * 1024) }; // 512KB.

/// Datadog Agent-based workload provider.
///
/// This provider is based on two major components: `containerd` events and the Tagger Entity API exposed by the Datadog
/// Agent. The Tagger Entity API exposes computed tags for a given entity -- specific containers, namely -- and is used
/// here to collect those tags for querying. We also listen for `containerd` events, which get used to map container
/// task PIDs to their container ID, which allows us to provide end-to-end linking of metrics sent by a specific
/// container task ID to their container ID, and in turn the tags associated with that container.
#[derive(Clone)]
pub struct RemoteAgentWorkloadProvider {
    shared_tags: Arc<ArcSwap<TagSnapshot>>,
}

impl RemoteAgentWorkloadProvider {
    /// Create a new `RemoteAgentWorkloadProvider` based on the given configuration.
    pub async fn from_configuration(
        config: &GenericConfiguration, component_registry: ComponentRegistry,
    ) -> Result<Self, GenericError> {
        let mut component_registry = component_registry.get_or_create("remote-agent");
        let mut provider_bounds = component_registry.bounds_builder();

        // Create our tag interner.
        let tag_interner_size_bytes = config
            .try_get_typed::<NonZeroUsize>("remote_agent_tag_interner_size_bytes")?
            .unwrap_or(DEFAULT_TAG_INTERNER_SIZE_BYTES);
        let tag_interner = FixedSizeInterner::new(tag_interner_size_bytes);

        provider_bounds
            .subcomponent("tag_interner")
            .firm()
            .with_fixed_amount(tag_interner_size_bytes.get());

        // Construct our aggregator, and add any collectors based on the detected features we've been given.
        let mut aggregator = MetadataAggregator::new();
        provider_bounds.with_subcomponent("aggregator", &aggregator);

        let mut collector_bounds = provider_bounds.subcomponent("collectors");

        let feature_detector = FeatureDetector::automatic(config);
        if feature_detector.is_feature_available(Feature::Containerd) {
            let cri_collector = ContainerdMetadataCollector::from_configuration(config, tag_interner.clone()).await?;
            collector_bounds.with_subcomponent("containerd", &cri_collector);

            aggregator.add_collector(cri_collector);
        }

        let ra_collector = RemoteAgentMetadataCollector::from_configuration(config, tag_interner).await?;
        collector_bounds.with_subcomponent("remote-agent", &ra_collector);

        aggregator.add_collector(ra_collector);

        // Attach the aggregator's tag store to the provider, and spawn the aggregator.
        let shared_tags = aggregator.tags();

        tokio::spawn(aggregator.run());

        Ok(Self { shared_tags })
    }
}

#[async_trait]
impl WorkloadProvider for RemoteAgentWorkloadProvider {
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: TagCardinality) -> Option<TagSet> {
        self.shared_tags.load().get_entity_tags(entity_id, cardinality)
    }
}
