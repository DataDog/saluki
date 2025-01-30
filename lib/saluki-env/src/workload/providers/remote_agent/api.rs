use saluki_api::{
    extract::State,
    response::IntoResponse,
    routing::{get, Router},
    APIHandler,
};
use saluki_context::{
    origin::OriginTagCardinality,
    tags::{SharedTagSet, TagSet},
};
use serde::Serialize;

use crate::{
    prelude::*,
    workload::{entity::HighestPrecedenceEntityIdRef, stores::TagStoreQuerier, EntityId},
};

#[derive(Serialize)]
struct EntityInformation<'a> {
    entity_id: &'a EntityId,
    ancestors: Vec<&'a EntityId>,
    low_cardinality_tags: SharedTagSet,
    orchestrator_cardinality_tags: SharedTagSet,
    high_cardinality_tags: SharedTagSet,
}

/// State used for the Remote Agent workload API handler.
#[derive(Clone)]
pub struct RemoteAgentWorkloadState {
    tag_querier: TagStoreQuerier,
}

impl RemoteAgentWorkloadState {
    fn get_tags_dump_response(&self) -> String {
        let mut active_entities = FastHashSet::default();
        let mut entity_mappings = FastHashMap::default();
        let mut entity_info_map = FastHashMap::default();
        let empty_tagset = TagSet::default().into_shared();

        // First, collect a list of all entities presently in the tag store, and then also go through and collect the
        // entity mappings for each entity.
        self.tag_querier.visit_active_entities(|entity_id| {
            active_entities.insert(entity_id.clone());
        });

        self.tag_querier.visit_entity_mappings(|entity_id, parent_id| {
            active_entities.insert(entity_id.clone());
            entity_mappings.insert(entity_id.clone(), parent_id.clone());
        });

        // For each entity, build its ancestry chain and collect all of the relevant tags.
        for entity_id in active_entities.iter() {
            let mut ancestors = Vec::new();

            let mut current_entity = entity_id;
            while let Some(parent) = entity_mappings.get(current_entity) {
                ancestors.insert(0, parent);
                current_entity = parent;
            }

            let low_cardinality_tags = self
                .tag_querier
                .get_entity_tags(entity_id, OriginTagCardinality::Low)
                .unwrap_or_else(|| empty_tagset.clone());
            let orchestrator_cardinality_tags = self
                .tag_querier
                .get_entity_tags(entity_id, OriginTagCardinality::Orchestrator)
                .unwrap_or_else(|| empty_tagset.clone());
            let high_cardinality_tags = self
                .tag_querier
                .get_entity_tags(entity_id, OriginTagCardinality::High)
                .unwrap_or_else(|| empty_tagset.clone());

            entity_info_map.insert(
                entity_id,
                EntityInformation {
                    entity_id,
                    ancestors,
                    low_cardinality_tags,
                    orchestrator_cardinality_tags,
                    high_cardinality_tags,
                },
            );
        }

        // Collapse the entity information map into sorted vector of entity information, which is sorted in precedence
        // order of the entity ID.
        let mut entity_info = entity_info_map.into_values().collect::<Vec<_>>();
        entity_info.sort_by_cached_key(|entity_info| HighestPrecedenceEntityIdRef::from(entity_info.entity_id));

        serde_json::to_string(&entity_info).unwrap()
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
    pub(crate) fn from_state(tag_querier: TagStoreQuerier) -> Self {
        Self {
            state: RemoteAgentWorkloadState { tag_querier },
        }
    }

    async fn tags_dump_handler(State(state): State<RemoteAgentWorkloadState>) -> impl IntoResponse {
        state.get_tags_dump_response()
    }
}

impl APIHandler for RemoteAgentWorkloadAPIHandler {
    type State = RemoteAgentWorkloadState;

    fn generate_initial_state(&self) -> Self::State {
        self.state.clone()
    }

    fn generate_routes(&self) -> Router<Self::State> {
        Router::new().route("/workload/remote_agent/tags/dump", get(Self::tags_dump_handler))
    }
}
