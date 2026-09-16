//! A workload provider based on the Datadog Agent's remote tagger and workloadmeta APIs.

use std::{future::Future, num::NonZeroUsize, time::Duration};

use agent_data_plane_config::shared::Environment;
use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use saluki_context::{
    origin::{OriginTagCardinality, RawOrigin},
    tags::SharedTagSet,
};
use saluki_core::accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder};
use saluki_core::{
    health::{Health, HealthRegistry},
    runtime::{RestartStrategy, Supervisor},
    support::SubsystemIdentifier,
};
#[cfg(unix)]
use saluki_env::features::Feature;
#[cfg(target_os = "linux")]
use saluki_env::workload::{collectors::CgroupsMetadataCollector, CgroupsConfiguration};
#[cfg(unix)]
use saluki_env::workload::{collectors::ContainerdMetadataCollector, ContainerdConfiguration};
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
use crate::internal::env::root_provider_id;

mod collectors;
use self::collectors::{RemoteAgentTaggerMetadataCollector, RemoteAgentWorkloadMetadataCollector};

// TODO: Make these configurable.

// SAFETY: The value is demonstrably not zero.
const DEFAULT_TAG_STORE_ENTITY_LIMIT: NonZeroUsize = NonZeroUsize::new(2000).unwrap();

// SAFETY: The value is demonstrably not zero.
const DEFAULT_EXTERNAL_DATA_STORE_ENTITY_LIMIT: NonZeroUsize = NonZeroUsize::new(2000).unwrap();

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

impl RemoteAgentWorkloadProvider {
    /// Creates a provider and the [`Supervisor`] that drives its collectors.
    ///
    /// # Errors
    ///
    /// If there is an issue creating the underlying metadata collectors, an error is returned.
    pub async fn new(
        string_interner_size_bytes: NonZeroUsize, environment: &Environment,
        client_config: &RemoteAgentClientConfiguration, component_registry: &ComponentRegistry,
        health_registry: &HealthRegistry,
    ) -> Result<(Self, Supervisor), GenericError> {
        let workload_provider_id = root_provider_id().child("workload").child("remote_agent");
        let mut provider_bounds = component_registry.bounds_builder(&workload_provider_id);

        let string_interner = GenericMapInterner::new(string_interner_size_bytes);

        provider_bounds
            .subcomponent("string_interner")
            .firm()
            .with_fixed_amount("string interner", string_interner_size_bytes.get());

        // Construct our metadata aggregator and any relevant metadata collectors based on the detected features we've
        // been given.
        let aggregator_id = workload_provider_id.clone().child("aggregator");
        let aggregator_health = health_registry
            .register_component(&aggregator_id)
            .ok_or_else(|| generic_error!("Component '{aggregator_id}' already registered in health registry."))?;
        let (mut aggregator, operations_tx) = MetadataAggregator::new(aggregator_health);

        let collectors_root = workload_provider_id.child("collectors");
        let mut collector_bounds = provider_bounds.subcomponent("collectors");
        let mut collector_workers: Vec<MetadataCollectorWorker> = Vec::new();

        let containerd_socket_path = environment
            .containerd
            .socket_path
            .is_explicit()
            .then(|| environment.containerd.socket_path.value.clone());
        let container_proc_root = environment
            .container_roots
            .proc_root
            .is_explicit()
            .then(|| environment.container_roots.proc_root.value.clone());
        let container_cgroup_root = environment
            .container_roots
            .cgroup_root
            .is_explicit()
            .then(|| environment.container_roots.cgroup_root.value.clone());

        let feature_detector = FeatureDetector::automatic(containerd_socket_path.clone());

        // Add the containerd collector if the feature is available.
        #[cfg(unix)]
        if feature_detector.is_feature_available(Feature::Containerd) {
            let containerd_config = ContainerdConfiguration {
                connection_timeout: environment.containerd.connection_timeout,
                query_timeout: environment.containerd.query_timeout,
            };
            let cri_collector = build_collector(
                &collectors_root,
                "containerd",
                health_registry,
                &mut collector_bounds,
                |health| {
                    ContainerdMetadataCollector::new(
                        containerd_socket_path.clone(),
                        &containerd_config,
                        health,
                        string_interner.clone(),
                    )
                },
            )
            .await?;

            collector_workers.push(MetadataCollectorWorker::new(cri_collector, operations_tx.clone()));
        }

        // Add the cgroups collector if the feature if we're on Linux.
        #[cfg(target_os = "linux")]
        {
            let cgroups_config = CgroupsConfiguration::new(
                container_proc_root.clone(),
                container_cgroup_root.clone(),
                &feature_detector,
            );
            let cgroups_collector = build_collector(
                &collectors_root,
                "cgroups",
                health_registry,
                &mut collector_bounds,
                |health| CgroupsMetadataCollector::new(&cgroups_config, health, string_interner.clone()),
            )
            .await?;

            collector_workers.push(MetadataCollectorWorker::new(cgroups_collector, operations_tx.clone()));
        }

        // Finally, add the Remote Agent collectors: one for the tagger, and one for workloadmeta.
        let ra_tags_collector = build_collector(
            &collectors_root,
            "remote_agent_tags",
            health_registry,
            &mut collector_bounds,
            |health| RemoteAgentTaggerMetadataCollector::new(client_config, health, string_interner.clone()),
        )
        .await?;

        collector_workers.push(MetadataCollectorWorker::new(ra_tags_collector, operations_tx.clone()));

        let ra_wmeta_collector = build_collector(
            &collectors_root,
            "remote_agent_wmeta",
            health_registry,
            &mut collector_bounds,
            |health| RemoteAgentWorkloadMetadataCollector::new(client_config, health, string_interner.clone()),
        )
        .await?;

        collector_workers.push(MetadataCollectorWorker::new(ra_wmeta_collector, operations_tx));

        // Create and attach the various metadata stores.
        let tag_store = TagStore::with_entity_limit(DEFAULT_TAG_STORE_ENTITY_LIMIT);
        let tags_querier = tag_store.querier();

        aggregator.add_store(tag_store);

        let external_data_store = ExternalDataStore::with_entity_limit(DEFAULT_EXTERNAL_DATA_STORE_ENTITY_LIMIT);
        let eds_resolver = external_data_store.resolver();

        aggregator.add_store(external_data_store);

        let on_demand_pid_resolver = OnDemandPIDResolver::new(
            container_proc_root,
            container_cgroup_root,
            &feature_detector,
            string_interner,
        )?;
        let origin_resolver = OriginResolver::new(eds_resolver.clone());

        // With the aggregator configured, update the memory bounds before handing it off to the supervisor.
        provider_bounds.with_subcomponent("aggregator", &aggregator);

        let api_worker = RemoteAgentWorkloadAPIWorker::from_state(tags_querier.clone(), eds_resolver.clone());

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

    fn get_self_container_tags(&self) -> Option<SharedTagSet> {
        let self_container_entity = self.on_demand_pid_resolver.resolve_self_container()?;
        self.tags_querier
            .get_entity_tags(&self_container_entity, OriginTagCardinality::Low)
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
    collectors_root: &SubsystemIdentifier, collector_name: &str, health_registry: &HealthRegistry,
    bounds_builder: &mut MemoryBoundsBuilder<'_>, build: F,
) -> Result<O, GenericError>
where
    F: FnOnce(Health) -> Fut,
    Fut: Future<Output = Result<O, GenericError>>,
    O: MemoryBounds,
{
    let collector_id = collectors_root.clone().child(collector_name);
    let health = health_registry
        .register_component(&collector_id)
        .ok_or_else(|| generic_error!("Component '{collector_id}' already registered in health registry."))?;
    let collector = build(health).await?;
    bounds_builder.with_subcomponent(collector_name, &collector);

    Ok(collector)
}
