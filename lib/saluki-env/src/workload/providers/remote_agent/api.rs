use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler,
};
use saluki_common::collections::{FastHashMap, FastHashSet};
use saluki_context::{
    origin::OriginTagCardinality,
    tags::{SharedTagSet, TagSet},
};
use serde::Serialize;

use crate::workload::{
    entity::HighestPrecedenceEntityIdRef,
    stores::{ExternalDataStoreResolver, TagStoreQuerier},
    EntityId,
};

#[derive(Serialize)]
struct EntityTags {
    low_cardinality: SharedTagSet,
    orchestrator_cardinality: SharedTagSet,
    high_cardinality: SharedTagSet,
}

#[derive(Serialize)]
struct EntityInformation<'a> {
    entity_id: &'a EntityId,

    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<&'a EntityId>,

    tags: EntityTags,
}

/// State used for the Remote Agent workload API handler.
#[derive(Clone)]
pub struct RemoteAgentWorkloadState {
    tag_querier: TagStoreQuerier,
    eds_resolver: ExternalDataStoreResolver,
}

impl RemoteAgentWorkloadState {
    fn get_tags_dump_response(&self) -> String {
        let mut active_entities = FastHashSet::default();
        let mut entity_aliases = FastHashMap::default();
        let mut entity_info_map = FastHashMap::default();
        let empty_tagset = TagSet::default().into_shared();

        // First, collect a list of all entities presently in the tag store, and then also go through and collect the
        // entity mappings for each entity.
        self.tag_querier.visit_active_entities(|entity_id| {
            active_entities.insert(entity_id.clone());
        });

        self.tag_querier.visit_entity_aliases(|entity_id, target_entity_id| {
            active_entities.insert(entity_id.clone());
            entity_aliases.insert(entity_id.clone(), target_entity_id.clone());
        });

        // For each entity, build its information.
        for entity_id in active_entities.iter() {
            let alias = entity_aliases.get(entity_id);
            let low_cardinality_tags = self
                .tag_querier
                .get_exact_entity_tags(entity_id, OriginTagCardinality::Low)
                .unwrap_or_else(|| empty_tagset.clone());
            let orchestrator_cardinality_tags = self
                .tag_querier
                .get_exact_entity_tags(entity_id, OriginTagCardinality::Orchestrator)
                .unwrap_or_else(|| empty_tagset.clone());
            let high_cardinality_tags = self
                .tag_querier
                .get_exact_entity_tags(entity_id, OriginTagCardinality::High)
                .unwrap_or_else(|| empty_tagset.clone());

            let tags = EntityTags {
                low_cardinality: low_cardinality_tags,
                orchestrator_cardinality: orchestrator_cardinality_tags,
                high_cardinality: high_cardinality_tags,
            };

            entity_info_map.insert(entity_id, EntityInformation { entity_id, alias, tags });
        }

        // Collapse the entity information map into sorted vector of entity information, which is sorted in precedence
        // order of the entity ID.
        let mut entity_info = entity_info_map.into_values().collect::<Vec<_>>();
        entity_info.sort_by_cached_key(|entity_info| HighestPrecedenceEntityIdRef::from(entity_info.entity_id));

        serde_json::to_string(&entity_info).unwrap()
    }

    fn get_eds_dump_response(&self) -> String {
        let mut mappings = Vec::new();
        self.eds_resolver.with_latest_snapshot(|ed| {
            mappings.push(ed.clone());
        });
        serde_json::to_string(&mappings).unwrap()
    }
}

/// An API handler for interacting with the underlying data stores that comprise the Remote Agent workload provider.
///
/// This handler registers a number of routes that allow for introspecting the state of the underlying data stores to
/// understand exactly what entities are being tracked and what tags are associated with them.
///
/// # Routes
///
/// ## GET `/workload/remote_agent/tags/dump`
///
/// This route will dump the contents of the associated tag store in a human-readable form.
///
/// All entities present in the tag store will be listed, along with their ancestry chain and the tags associated with
/// the entity at each tag cardinality level. Entities are sorted in the output from highest to lowest precedence.
pub struct RemoteAgentWorkloadAPIHandler {
    state: RemoteAgentWorkloadState,
}

impl RemoteAgentWorkloadAPIHandler {
    pub(crate) fn from_state(tag_querier: TagStoreQuerier, eds_resolver: ExternalDataStoreResolver) -> Self {
        Self {
            state: RemoteAgentWorkloadState {
                tag_querier,
                eds_resolver,
            },
        }
    }

    async fn tags_dump_handler(State(state): State<RemoteAgentWorkloadState>) -> impl IntoResponse {
        state.get_tags_dump_response()
    }

    async fn eds_dump_handler(State(state): State<RemoteAgentWorkloadState>) -> impl IntoResponse {
        state.get_eds_dump_response()
    }
}

impl APIHandler for RemoteAgentWorkloadAPIHandler {
    type State = RemoteAgentWorkloadState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new()
            .route("/workload/remote_agent/tags/dump", get(Self::tags_dump_handler))
            .route("/workload/remote_agent/external_data/dump", get(Self::eds_dump_handler))
    }
}
