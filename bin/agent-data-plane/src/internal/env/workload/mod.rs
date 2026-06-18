//! A workload provider based on the Datadog Agent's remote tagger and workloadmeta APIs.

use std::{future::Future, num::NonZeroUsize, time::Duration};

use agent_data_plane_config_system::EnvConfig;
use datadog_agent_commons::ipc::client::RemoteAgentClient;
use resource_accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder};
use saluki_component_config::workload::WorkloadConfig;
use saluki_context::{
    origin::{OriginTagCardinality, RawOrigin},
    tags::SharedTagSet,
};
use saluki_core::{
    health::{Health, HealthRegistry},
    runtime::{RestartStrategy, Supervisor},
};
#[cfg(unix)]
use saluki_env::features::Feature;
#[cfg(target_os = "linux")]
use saluki_env::workload::collectors::CgroupsMetadataCollector;
#[cfg(unix)]
use saluki_env::workload::collectors::ContainerdMetadataCollector;
use saluki_env::{
    features::FeatureDetector,
    workload::{
        aggregator::MetadataAggregator,
        collectors::MetadataCollectorWorker,
        entity::EntityId,
        origin::{OriginResolver, ResolvedOrigin},
        stores::{ExternalDataStore, TagStore, TagStoreQuerier},
        OnDemandPIDResolver,
    },
    CaptureEntityResolver, WorkloadProvider,
};
use saluki_error::{generic_error, GenericError};
use stringtheory::interning::GenericMapInterner;

mod api;
use self::api::RemoteAgentWorkloadAPIWorker;

mod collectors;
use self::collectors::{RemoteAgentTaggerMetadataCollector, RemoteAgentWorkloadMetadataCollector};

// TODO: Make these configurable.

// SAFETY: The value is demonstrably not zero.
const DEFAULT_TAG_STORE_ENTITY_LIMIT: NonZeroUsize = NonZeroUsize::new(2000).unwrap();

// SAFETY: The value is demonstrably not zero.
const DEFAULT_EXTERNAL_DATA_STORE_ENTITY_LIMIT: NonZeroUsize = NonZeroUsize::new(2000).unwrap();

// SAFETY: We know the value is not zero.
const DEFAULT_STRING_INTERNER_SIZE_BYTES: NonZeroUsize = NonZeroUsize::new(512 * 1024).unwrap(); // 512KB.

/// Datadog Agent-based workload provider.
///
/// This provider is based primarily on the remote tagger API exposed by the Datadog Agent, which handles the bulk of
/// the work by collecting and aggregating tags for container entities. This remote tagger API operates in a streaming
/// fashion, which the provider uses to stream update operations to the tag store.
///
/// Additionally, two collectors are optionally used: a `containerd` collector and a `cgroups` collector. The
/// `containerd` collector will, if containerd is running, be used to collect metadata that allows mapping container
/// PIDs (UDS-based Origin Detection) to container IDs. The `cgroups` collector will collect metadata about the current
/// set of cgroups v1/v2 controllers, tracking any controllers which appear related to containers and storing a mapping
/// of controller inodes to container IDs.
///
/// These additional collectors are necessary to bridge the gap from container PID and cgroup controller inode, as the
/// remote tagger API doesn't stream us these mappings itself and only deals with resolved container IDs.
#[derive(Clone)]
pub struct RemoteAgentWorkloadProvider {
    tags_querier: TagStoreQuerier,
    origin_resolver: OriginResolver,
    on_demand_pid_resolver: OnDemandPIDResolver,
}

/// Common prefix for all health component names registered by the workload provider.
///
/// This is used both when registering the workload provider's components and when waiting for those components to
/// become ready (see `ADPEnvironmentProvider::wait_for_ready`), so the two stay in sync.
pub(crate) const WORKLOAD_HEALTH_PREFIX: &str = "env_provider.workload.";

impl RemoteAgentWorkloadProvider {
    /// Create a new `RemoteAgentWorkloadProvider` based on the given configuration, along with a [`Supervisor`] that
    /// drives the aggregator and all collector workers.
    ///
    /// # Errors
    ///
    /// If there is an issue with any of the provider configuration, or creating the underlying metadata collectors, an
    /// error is returned.
    pub async fn from_typed(
        env_config: &EnvConfig, workload_config: &WorkloadConfig, client: RemoteAgentClient,
        component_registry: ComponentRegistry, health_registry: &HealthRegistry,
    ) -> Result<(Self, Supervisor), GenericError> {
        // The `saluki-env` collectors (containerd, cgroups, feature detection, on-demand PID
        // resolution) are out of scope for typed config; they consume the runtime source map via the
        // confined `EnvConfig` pass-through. The Remote Agent collectors receive their client from the
        // config-system's typed attachment instead of building one from raw config.
        let config = env_config.raw();
        let mut component_registry = component_registry.get_or_create("remote-agent");
        let mut provider_bounds = component_registry.bounds_builder();

        // Create our string interner which will get used primarily for tags, but also for any other long-ish lived strings.
        // The typed `WorkloadConfig` carries this as a plain `usize`; a value of 0 means "use the default".
        let string_interner_size_bytes = NonZeroUsize::new(workload_config.remote_agent_string_interner_size_bytes)
            .unwrap_or(DEFAULT_STRING_INTERNER_SIZE_BYTES);
        let string_interner = GenericMapInterner::new(string_interner_size_bytes);

        provider_bounds
            .subcomponent("string_interner")
            .firm()
            .with_fixed_amount("string interner", string_interner_size_bytes.get());

        // Construct our metadata aggregator and any relevant metadata collectors based on the detected features we've
        // been given.
        let aggregator_health = health_registry
            .register_component(format!("{WORKLOAD_HEALTH_PREFIX}remote_agent.aggregator"))
            .ok_or_else(|| {
                generic_error!(
                    "Component '{WORKLOAD_HEALTH_PREFIX}remote_agent.aggregator' already registered in health registry."
                )
            })?;
        let (mut aggregator, operations_tx) = MetadataAggregator::new(aggregator_health);

        let mut collector_bounds = provider_bounds.subcomponent("collectors");
        let mut collector_workers: Vec<MetadataCollectorWorker> = Vec::new();

        // Add the containerd collector if the feature is available.
        let feature_detector = FeatureDetector::automatic(config);
        #[cfg(unix)]
        if feature_detector.is_feature_available(Feature::Containerd) {
            let cri_collector = build_collector("containerd", health_registry, &mut collector_bounds, |health| {
                ContainerdMetadataCollector::from_configuration(config, health, string_interner.clone())
            })
            .await?;

            collector_workers.push(MetadataCollectorWorker::new(cri_collector, operations_tx.clone()));
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

            collector_workers.push(MetadataCollectorWorker::new(cgroups_collector, operations_tx.clone()));
        }

        // Finally, add the Remote Agent collectors: one for the tagger, and one for workloadmeta. These
        // receive their client from the typed attachment and are constructed infallibly.
        let ra_tags_collector =
            register_collector("remote-agent-tags", health_registry, &mut collector_bounds, |health| {
                RemoteAgentTaggerMetadataCollector::from_client(client.clone(), health, string_interner.clone())
            })?;

        collector_workers.push(MetadataCollectorWorker::new(ra_tags_collector, operations_tx.clone()));

        let ra_wmeta_collector =
            register_collector("remote-agent-wmeta", health_registry, &mut collector_bounds, |health| {
                RemoteAgentWorkloadMetadataCollector::from_client(client.clone(), health, string_interner.clone())
            })?;

        collector_workers.push(MetadataCollectorWorker::new(ra_wmeta_collector, operations_tx));

        // Create and attach the various metadata stores.
        let tag_store = TagStore::with_entity_limit(DEFAULT_TAG_STORE_ENTITY_LIMIT);
        let tags_querier = tag_store.querier();

        aggregator.add_store(tag_store);

        let external_data_store = ExternalDataStore::with_entity_limit(DEFAULT_EXTERNAL_DATA_STORE_ENTITY_LIMIT);
        let eds_resolver = external_data_store.resolver();

        aggregator.add_store(external_data_store);

        let on_demand_pid_resolver =
            OnDemandPIDResolver::from_configuration(config, feature_detector, string_interner)?;
        let origin_resolver = OriginResolver::new(eds_resolver.clone());

        // With the aggregator configured, update the memory bounds before handing it off to the supervisor.
        provider_bounds.with_subcomponent("aggregator", &aggregator);

        let api_worker = RemoteAgentWorkloadAPIWorker::from_state(tags_querier.clone(), eds_resolver);

        // Build the workload supervisor.
        let mut supervisor = Supervisor::new("workload")?
            .with_restart_strategy(RestartStrategy::one_to_one().with_intensity_and_period(5, Duration::from_secs(30)));
        supervisor.add_worker(aggregator);
        for worker in collector_workers {
            supervisor.add_worker(worker);
        }
        supervisor.add_worker(api_worker);

        let provider = Self {
            tags_querier,
            origin_resolver,
            on_demand_pid_resolver,
        };

        Ok((provider, supervisor))
    }
}

impl WorkloadProvider for RemoteAgentWorkloadProvider {
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        // Query the tag store for the tags associated with the given entity ID.
        match self.tags_querier.get_entity_tags(entity_id, cardinality) {
            Some(tags) => Some(tags),
            None => {
                // If no tags came back, check if the entity ID is a PID. If it is, we can try to resolve it to a
                // container ID first before trying again.
                if let EntityId::ContainerPid(pid) = entity_id {
                    if let Some(container_id) = self.on_demand_pid_resolver.resolve(*pid) {
                        // If we successfully resolved the PID to a container ID, try again.
                        return self.tags_querier.get_entity_tags(&container_id, cardinality);
                    }
                }

                None
            }
        }
    }

    fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        self.origin_resolver.get_resolved_origin(origin)
    }
}

impl CaptureEntityResolver for RemoteAgentWorkloadProvider {
    fn resolve_container_entity_for_live_pid(&self, process_id: u32) -> Option<EntityId> {
        self.on_demand_pid_resolver.resolve(process_id)
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
            "{WORKLOAD_HEALTH_PREFIX}remote_agent.collector.{collector_name}"
        ))
        .ok_or_else(|| {
            generic_error!(
                "Component '{WORKLOAD_HEALTH_PREFIX}remote_agent.collector.{collector_name}' already registered in \
                 health registry."
            )
        })?;
    let collector = build(health).await?;
    bounds_builder.with_subcomponent(collector_name, &collector);

    Ok(collector)
}

/// Registers a synchronously constructed collector: reserves its health component, builds it, and
/// records its memory bounds. The Remote Agent collectors no longer build a client from raw config,
/// so they are infallible to construct and do not need the async `build_collector`.
fn register_collector<F, O>(
    collector_name: &str, health_registry: &HealthRegistry, bounds_builder: &mut MemoryBoundsBuilder<'_>, build: F,
) -> Result<O, GenericError>
where
    F: FnOnce(Health) -> O,
    O: MemoryBounds,
{
    let health = health_registry
        .register_component(format!(
            "{WORKLOAD_HEALTH_PREFIX}remote_agent.collector.{collector_name}"
        ))
        .ok_or_else(|| {
            generic_error!(
                "Component '{WORKLOAD_HEALTH_PREFIX}remote_agent.collector.{collector_name}' already registered in \
                 health registry."
            )
        })?;
    let collector = build(health);
    bounds_builder.with_subcomponent(collector_name, &collector);

    Ok(collector)
}
