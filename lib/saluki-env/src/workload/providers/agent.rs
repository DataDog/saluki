use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use saluki_config::GenericConfiguration;
use saluki_event::metric::MetricTags;

use crate::{
    features::{Feature, FeatureDetector},
    workload::{
        aggregator::MetadataAggregator, collectors::ContainerdMetadataCollector, entity::EntityId,
        metadata::TagCardinality, store::TagSnapshot,
    },
    WorkloadProvider,
};

/// An Agent-like workload provider.
///
/// This emulates the workload metadata collection logic of the Agent, which utilizes a number of different "collectors"
/// to get metadata about the underlying system and the workloads running on top of it.
#[derive(Clone)]
pub struct AgentLikeWorkloadProvider {
    shared_tags: Arc<ArcSwap<TagSnapshot>>,
}

impl AgentLikeWorkloadProvider {
    /// Create a new `AgentLikeWorkloadProvider` based on the given detected features.
    pub async fn new(config: &GenericConfiguration, feature_detector: FeatureDetector) -> Result<Self, String> {
        // Construct our aggregator, and add any collectors based on the detected features we've been given.
        let mut aggregator = MetadataAggregator::new();

        if feature_detector.is_feature_available(Feature::Containerd) {
            let collector = ContainerdMetadataCollector::from_configuration(config).await?;
            aggregator.add_collector(collector);
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
impl WorkloadProvider for AgentLikeWorkloadProvider {
    type Error = std::convert::Infallible;

    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: TagCardinality) -> Option<MetricTags> {
        self.shared_tags.load().get_entity_tags(entity_id, cardinality)
    }
}
