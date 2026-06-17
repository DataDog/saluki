use std::num::NonZeroUsize;

use datadog_agent_commons::ipc::client::RemoteAgentClient;
use datadog_protos::agent::TagCardinality;
use resource_accounting::{MemoryBounds, MemoryBoundsBuilder};
use saluki_context::{
    origin::{OriginTagCardinality, RawOrigin},
    tags::SharedTagSet,
};
use saluki_core::{health::HealthRegistry, runtime::Supervisor};
use saluki_env::workload::{
    aggregator::MetadataAggregator,
    collectors::MetadataCollectorWorker,
    origin::{OriginResolver, ResolvedOrigin},
    stores::{ExternalDataStore, TagStore, TagStoreQuerier},
    CaptureEntityResolver, EntityId, WorkloadProvider,
};
use saluki_error::{generic_error, GenericError};

mod collectors;
use self::collectors::{tagger::TaggerMetadataCollector, workloadmeta::WorkloadmetaMetadataCollector};

/// Health-check name prefix used by workload provider workers.
pub const WORKLOAD_HEALTH_PREFIX: &str = "workload/";

const DEFAULT_ENTITY_LIMIT: usize = 500_000;

/// Remote-Agent-backed workload provider.
#[derive(Clone)]
pub struct RemoteAgentWorkloadProvider {
    tag_store: TagStoreQuerier,
    origin_resolver: OriginResolver,
}

impl RemoteAgentWorkloadProvider {
    /// Creates the workload provider and its collector supervisor.
    ///
    /// # Errors
    ///
    /// If the supervisor or health registration cannot be created, an error is returned.
    pub fn from_client(
        client: RemoteAgentClient, health_registry: &HealthRegistry,
    ) -> Result<(Self, Supervisor), GenericError> {
        let entity_limit = NonZeroUsize::new(DEFAULT_ENTITY_LIMIT).expect("default entity limit is non-zero");

        let tag_store = TagStore::with_entity_limit(entity_limit);
        let tag_store_querier = tag_store.querier();
        let external_data_store = ExternalDataStore::with_entity_limit(entity_limit);
        let origin_resolver = OriginResolver::new(external_data_store.resolver());

        let health = health_registry
            .register_component(format!("{WORKLOAD_HEALTH_PREFIX}aggregator"))
            .ok_or_else(|| generic_error!("workload aggregator health component is already registered"))?;
        let (mut aggregator, operations_tx) = MetadataAggregator::new(health);
        aggregator.add_store(tag_store);
        aggregator.add_store(external_data_store);

        let mut supervisor = Supervisor::new("workload")?;
        supervisor.add_worker(aggregator);
        supervisor.add_worker(MetadataCollectorWorker::new(
            TaggerMetadataCollector::new(client.clone(), TagCardinality::Low),
            operations_tx.clone(),
        ));
        supervisor.add_worker(MetadataCollectorWorker::new(
            TaggerMetadataCollector::new(client.clone(), TagCardinality::Orchestrator),
            operations_tx.clone(),
        ));
        supervisor.add_worker(MetadataCollectorWorker::new(
            TaggerMetadataCollector::new(client.clone(), TagCardinality::High),
            operations_tx.clone(),
        ));
        supervisor.add_worker(MetadataCollectorWorker::new(
            WorkloadmetaMetadataCollector::new(client),
            operations_tx,
        ));

        Ok((
            Self {
                tag_store: tag_store_querier,
                origin_resolver,
            },
            supervisor,
        ))
    }
}

impl WorkloadProvider for RemoteAgentWorkloadProvider {
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        self.tag_store.get_entity_tags(entity_id, cardinality)
    }

    fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        self.origin_resolver.get_resolved_origin(origin)
    }
}

impl CaptureEntityResolver for RemoteAgentWorkloadProvider {
    fn resolve_container_entity_for_live_pid(&self, process_id: u32) -> Option<EntityId> {
        let pid_entity = EntityId::ContainerPid(process_id);
        match self.tag_store.get_entity_alias(&pid_entity) {
            Some(entity @ EntityId::Container(_)) => Some(entity),
            _ => None,
        }
    }
}

impl MemoryBounds for RemoteAgentWorkloadProvider {
    fn specify_bounds(&self, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_single_value::<Self>("component struct");
    }
}
