use std::{num::NonZeroUsize, sync::Arc};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use memory_accounting::ComponentRegistry;
use saluki_config::GenericConfiguration;
use saluki_context::TagSet;
use saluki_error::{generic_error, GenericError};
use saluki_event::metric::OriginTagCardinality;
use saluki_health::HealthRegistry;
use stringtheory::interning::GenericMapInterner;

#[cfg(target_os = "linux")]
use crate::workload::collectors::CgroupsMetadataCollector;
use crate::{
    features::{Feature, FeatureDetector},
    workload::{
        aggregator::MetadataAggregator,
        collectors::{ContainerdMetadataCollector, RemoteAgentMetadataCollector},
        entity::EntityId,
        store::TagSnapshot,
    },
    WorkloadProvider,
};

// SAFETY: We know the value is not zero.
const DEFAULT_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(512 * 1024) }; // 512KB.

/// Datadog Agent-based workload provider.
///
/// This provider is based primarily on the remote tagger API exposed by the Datadog Agent, which handles the bulk of
/// the work by collecting and aggregating tags for container entities. This remote tagger API operates in a streaming
/// fashion, which the provider uses to stream update operations to the tag store.
///
/// Additionally, two collectors are optionally used: a `containerd` collector and a `cgroups-v2` collector. The
/// `containerd` collector will, if containerd is running, be used to collect metadata that allows mapping container
/// PIDs (UDS-based Origin Detection) to container IDs. The `cgroups-v2` collector will collect metadata about the
/// current set of cgroups v2 controllers, tracking any controllers which appear related to containers and storing a
/// mapping of controller inodes to container IDs.
///
/// These additional collectors are necessary to bridge the gap from container PID and cgroup controller inode, as the
/// remote tagger API does not stream us these mappings itself and only deals with resolved container IDs.
#[derive(Clone)]
pub struct RemoteAgentWorkloadProvider {
    shared_tags: Arc<ArcSwap<TagSnapshot>>,
}

impl RemoteAgentWorkloadProvider {
    /// Create a new `RemoteAgentWorkloadProvider` based on the given configuration.
    pub async fn from_configuration(
        config: &GenericConfiguration, component_registry: ComponentRegistry, health_registry: &HealthRegistry,
    ) -> Result<Self, GenericError> {
        let mut component_registry = component_registry.get_or_create("remote-agent");
        let mut provider_bounds = component_registry.bounds_builder();

        // Create our string interner which will get used primarily for tags, but also for any other long-ish lived strings.
        let string_interner_size_bytes = config
            .try_get_typed::<NonZeroUsize>("remote_agent_string_interner_size_bytes")?
            .unwrap_or(DEFAULT_STRING_INTERNER_SIZE_BYTES);
        let string_interner = GenericMapInterner::new(string_interner_size_bytes);

        provider_bounds
            .subcomponent("string_interner")
            .firm()
            .with_fixed_amount(string_interner_size_bytes.get());

        // Construct our aggregator, and add any collectors based on the detected features we've been given.
        let aggregator_health = health_registry
            .register_component("env_provider.workload.remote_agent.aggregator")
            .ok_or_else(|| {
                generic_error!(
                    "Component 'env_provider.workload.remote_agent.aggregator' already registered in health registry."
                )
            })?;
        let mut aggregator = MetadataAggregator::new(aggregator_health);
        provider_bounds.with_subcomponent("aggregator", &aggregator);

        let mut collector_bounds = provider_bounds.subcomponent("collectors");

        // Add the containerd collector if the feature is available.
        let feature_detector = FeatureDetector::automatic(config);
        if feature_detector.is_feature_available(Feature::Containerd) {
            let collector_health = health_registry.register_component("env_provider.workload.remote_agent.collector.containerd")
                .ok_or_else(|| generic_error!("Component 'env_provider.workload.remote_agent.collector.containerd' already registered in health registry."))?;
            let cri_collector =
                ContainerdMetadataCollector::from_configuration(config, collector_health, string_interner.clone())
                    .await?;
            collector_bounds.with_subcomponent("containerd", &cri_collector);

            aggregator.add_collector(cri_collector);
        }

        // Add the cgroups collector if the feature if we're on Linux.
        #[cfg(target_os = "linux")]
        {
            let collector_health = health_registry.register_component("env_provider.workload.remote_agent.collector.cgroups")
                .ok_or_else(|| generic_error!("Component 'env_provider.workload.remote_agent.collector.cgroups' already registered in health registry."))?;
            let cgroups_collector = CgroupsMetadataCollector::from_configuration(
                config,
                feature_detector,
                collector_health,
                string_interner.clone(),
            )
            .await?;
            collector_bounds.with_subcomponent("cgroups", &cgroups_collector);

            aggregator.add_collector(cgroups_collector);
        }

        // Finally, add the Remote Agent collector.
        let collector_health = health_registry.register_component("env_provider.workload.remote_agent.collector.remote-agent")
                .ok_or_else(|| generic_error!("Component 'env_provider.workload.remote_agent.collector.remote-agent' already registered in health registry."))?;
        let ra_collector =
            RemoteAgentMetadataCollector::from_configuration(config, collector_health, string_interner).await?;
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
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<TagSet> {
        self.shared_tags.load().get_entity_tags(entity_id, cardinality)
    }
}
