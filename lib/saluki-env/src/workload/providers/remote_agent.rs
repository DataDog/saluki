use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use saluki_config::GenericConfiguration;
use saluki_error::GenericError;
use saluki_event::metric::MetricTags;

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
    /// Create a new `RemoteAgentWorkloadProvider` based on the given detected features.
    pub async fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let feature_detector = FeatureDetector::automatic(config);

        // Construct our aggregator, and add any collectors based on the detected features we've been given.
        let mut aggregator = MetadataAggregator::new();

        let ra_collector = RemoteAgentMetadataCollector::from_configuration(config).await?;
        aggregator.add_collector(ra_collector);

        if feature_detector.is_feature_available(Feature::Containerd) {
            let cri_collector = ContainerdMetadataCollector::from_configuration(config).await?;
            aggregator.add_collector(cri_collector);
        }

        // Attach the aggregator's tag store to the provider, and spawn the aggregator.
        let shared_tags = aggregator.tags();

        tokio::spawn(aggregator.run());

        Ok(Self { shared_tags })
    }

    /// Gets a shared reference to the latest tag store snapshot.
    ///
    /// This can be used to query for the tags of a specific entity, and is updated as workload changes are observed and
    /// processed.
    pub fn tags(&self) -> ArcSwap<TagSnapshot> {
        ArcSwap::new(self.shared_tags.load_full())
    }
}

#[async_trait]
impl WorkloadProvider for RemoteAgentWorkloadProvider {
    type Error = std::convert::Infallible;

    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: TagCardinality) -> Option<MetricTags> {
        self.shared_tags.load().get_entity_tags(entity_id, cardinality)
    }
}
