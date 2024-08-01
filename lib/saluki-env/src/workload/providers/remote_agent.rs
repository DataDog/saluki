use std::{num::NonZeroUsize, sync::Arc};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use memory_accounting::{ComponentBounds, MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::TagSet;
use saluki_error::{generic_error, GenericError};
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
    bounds: ComponentBounds,
}

impl RemoteAgentWorkloadProvider {
    /// Create a new `RemoteAgentWorkloadProvider` based on the given configuration.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let tag_interner_size_bytes = config
            .try_get_typed::<usize>("remote_agent_tag_interner_size_bytes")
            .map_err(Into::into)
            .and_then(|v| match v {
                None => Ok(None),
                Some(v) => NonZeroUsize::new(v)
                    .ok_or_else(|| generic_error!("remote_agent_tag_interner_size_bytes cannot be zero"))
                    .map(Some),
            })?
            .unwrap_or(DEFAULT_TAG_INTERNER_SIZE_BYTES);
        let tag_interner = FixedSizeInterner::new(tag_interner_size_bytes);

        // Start capturing our bounds.
        //
        // We do this ahead of time because unlike some other codepaths calculating their bounds, we don't use an
        // intermediate configuration type that we can attach bounds calculation to, so we have to do it as we're
        // building the provider itself. We'll finalize these bounds at the end and then just refer to them later in the
        // bounds calculations for `RemoteAgentWorkloadProvider` itself.
        let mut bounds_builder = MemoryBoundsBuilder::new();
        let mut ra_bounds_builder = bounds_builder.component("remote-agent");

        ra_bounds_builder
            .component("tag_interner")
            .firm()
            .with_fixed_amount(tag_interner_size_bytes.get());

        // Construct our aggregator, and add any collectors based on the detected features we've been given.
        let mut aggregator = MetadataAggregator::new();

        ra_bounds_builder.bounded_component("aggregator", &aggregator);

        let mut collector_bounds_builder = ra_bounds_builder.component("collectors");

        let feature_detector = FeatureDetector::automatic(config);
        if feature_detector.is_feature_available(Feature::Containerd) {
            let cri_collector = ContainerdMetadataCollector::from_configuration(config, tag_interner.clone()).await?;
            collector_bounds_builder.bounded_component("containerd", &cri_collector);

            aggregator.add_collector(cri_collector);
        }

        let ra_collector = RemoteAgentMetadataCollector::from_configuration(config, tag_interner).await?;
        collector_bounds_builder.bounded_component("remote-agent", &ra_collector);

        aggregator.add_collector(ra_collector);

        // Attach the aggregator's tag store to the provider, and spawn the aggregator.
        let shared_tags = aggregator.tags();

        tokio::spawn(aggregator.run());

        let bounds = bounds_builder.finalize();

        Ok(Self { shared_tags, bounds })
    }
}

#[async_trait]
impl WorkloadProvider for RemoteAgentWorkloadProvider {
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: TagCardinality) -> Option<TagSet> {
        self.shared_tags.load().get_entity_tags(entity_id, cardinality)
    }
}

impl MemoryBounds for RemoteAgentWorkloadProvider {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.merge_existing(&self.bounds);
    }
}
