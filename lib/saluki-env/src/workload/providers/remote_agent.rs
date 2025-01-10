use std::{future::Future, num::NonZeroUsize};

use async_trait::async_trait;
use memory_accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder};
use saluki_config::GenericConfiguration;
use saluki_context::tags::SharedTagSet;
use saluki_error::{generic_error, GenericError};
use saluki_event::metric::OriginTagCardinality;
use saluki_health::{Health, HealthRegistry};
use stringtheory::interning::GenericMapInterner;

#[cfg(target_os = "linux")]
use crate::workload::collectors::CgroupsMetadataCollector;
use crate::{
    features::{Feature, FeatureDetector},
    workload::{
        aggregator::MetadataAggregator,
        collectors::{
            ContainerdMetadataCollector, RemoteAgentTaggerMetadataCollector, RemoteAgentWorkloadMetadataCollector,
        },
        entity::EntityId,
        stores::{ExternalDataStore, ExternalDataStoreResolver, TagStore, TagStoreQuerier},
    },
    WorkloadProvider,
};

// TODO: Make these configurable.

// SAFETY: The value is demonstrably not zero.
const DEFAULT_TAG_STORE_ENTITY_LIMIT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(2000) };

// SAFETY: The value is demonstrably not zero.
const DEFAULT_EXTERNAL_DATA_STORE_ENTITY_LIMIT: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(2000) };

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
    tag_querier: TagStoreQuerier,
    external_data_resolver: ExternalDataStoreResolver,
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

        let mut collector_bounds = provider_bounds.subcomponent("collectors");

        // Add the containerd collector if the feature is available.
        let feature_detector = FeatureDetector::automatic(config);
        if feature_detector.is_feature_available(Feature::Containerd) {
            let cri_collector = build_collector("containerd", health_registry, &mut collector_bounds, |health| {
                ContainerdMetadataCollector::from_configuration(config, health, string_interner.clone())
            })
            .await?;

            aggregator.add_collector(cri_collector);
        }

        // Add the cgroups collector if the feature if we're on Linux.
        #[cfg(target_os = "linux")]
        {
            let cgroups_collector = build_collector("cgroups", health_registry, &mut collector_bounds, |health| {
                CgroupsMetadataCollector::from_configuration(
                    config,
                    feature_detector.clone(),
                    health,
                    string_interner.clone(),
                )
            })
            .await?;

            aggregator.add_collector(cgroups_collector);
        }

        // Finally, add the Remote Agent collectors: one for the tagger, and one for workloadmeta.
        let ra_tags_collector =
            build_collector("remote-agent-tags", health_registry, &mut collector_bounds, |health| {
                RemoteAgentTaggerMetadataCollector::from_configuration(config, health, string_interner.clone())
            })
            .await?;

        aggregator.add_collector(ra_tags_collector);

        let ra_wmeta_collector =
            build_collector("remote-agent-wmeta", health_registry, &mut collector_bounds, |health| {
                RemoteAgentWorkloadMetadataCollector::from_configuration(config, health, string_interner.clone())
            })
            .await?;

        aggregator.add_collector(ra_wmeta_collector);

        // Create and attach the various metadata stores.
        let tag_store = TagStore::with_entity_limit(DEFAULT_TAG_STORE_ENTITY_LIMIT);
        let tag_querier = tag_store.querier();

        aggregator.add_store(tag_store);

        let external_data_store = ExternalDataStore::with_entity_limit(DEFAULT_EXTERNAL_DATA_STORE_ENTITY_LIMIT);
        let external_data_resolver = external_data_store.resolver();

        aggregator.add_store(external_data_store);

        // With the aggregator configured, update the memory bounds and spawn the aggregator.
        provider_bounds.with_subcomponent("aggregator", &aggregator);

        tokio::spawn(aggregator.run());

        Ok(Self {
            tag_querier,
            external_data_resolver,
        })
    }
}

#[async_trait]
impl WorkloadProvider for RemoteAgentWorkloadProvider {
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        self.tag_querier.get_entity_tags(entity_id, cardinality)
    }

    fn resolve_entity_id_from_external_data(&self, external_data: &str) -> Option<EntityId> {
        self.external_data_resolver.resolve_entity_id(external_data)
    }
}

async fn build_collector<F, Fut, O>(
    collector_name: &str, health_registry: &HealthRegistry, bounds_builder: &mut MemoryBoundsBuilder<'_>, build: F,
) -> Result<O, GenericError>
where
    F: FnOnce(Health) -> Fut,
    Fut: Future<Output = Result<O, GenericError>>,
    O: MemoryBounds,
{
    let health = health_registry
        .register_component(format!(
            "env_provider.workload.remote_agent.collector.{}",
            collector_name
        ))
        .ok_or_else(|| {
            generic_error!(
                "Component 'env_provider.workload.remote_agent.collector.{}' already registered in health registry.",
                collector_name
            )
        })?;
    let collector = build(health).await?;
    bounds_builder.with_subcomponent(collector_name, &collector);

    Ok(collector)
}
