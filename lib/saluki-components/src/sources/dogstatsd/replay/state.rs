use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use datadog_protos::agent::{Entity as RemoteEntity, EntityId as RemoteEntityId, TaggerState};
use saluki_context::{
    origin::OriginTagCardinality,
    tags::{SharedTagSet, Tag, TagSet},
};
use saluki_env::workload::EntityId;
use saluki_error::{generic_error, GenericError};
use stringtheory::MetaString;
use tracing::{debug, warn};

#[derive(Clone, Default)]
struct ReplayEntityTags {
    low: Option<SharedTagSet>,
    orchestrator: Option<SharedTagSet>,
    high: Option<SharedTagSet>,
}

impl ReplayEntityTags {
    fn from_remote_entity(entity_id: &EntityId, entity: RemoteEntity) -> Option<Self> {
        let RemoteEntity {
            high_cardinality_tags,
            orchestrator_cardinality_tags,
            low_cardinality_tags,
            standard_tags,
            ..
        } = entity;

        if !standard_tags.is_empty() {
            debug!(
                %entity_id,
                standard_tags = standard_tags.len(),
                "Ignoring replay standard tags because Saluki does not model them separately."
            );
        }

        let low = shared_tags_from_strings(low_cardinality_tags);
        let orchestrator = shared_tags_from_strings(orchestrator_cardinality_tags);
        let high = shared_tags_from_strings(high_cardinality_tags);

        if low.is_none() && orchestrator.is_none() && high.is_none() {
            None
        } else {
            Some(Self {
                low,
                orchestrator,
                high,
            })
        }
    }

    fn merge_for_cardinality(&self, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        match cardinality {
            OriginTagCardinality::None => None,
            OriginTagCardinality::Low => self.low.clone(),
            OriginTagCardinality::Orchestrator => {
                let mut merged = SharedTagSet::default();
                let mut saw_tags = false;

                if let Some(tags) = &self.low {
                    merged.extend_from_shared(tags);
                    saw_tags = true;
                }
                if let Some(tags) = &self.orchestrator {
                    merged.extend_from_shared(tags);
                    saw_tags = true;
                }

                saw_tags.then_some(merged)
            }
            OriginTagCardinality::High => {
                let mut merged = SharedTagSet::default();
                let mut saw_tags = false;

                if let Some(tags) = &self.low {
                    merged.extend_from_shared(tags);
                    saw_tags = true;
                }
                if let Some(tags) = &self.orchestrator {
                    merged.extend_from_shared(tags);
                    saw_tags = true;
                }
                if let Some(tags) = &self.high {
                    merged.extend_from_shared(tags);
                    saw_tags = true;
                }

                saw_tags.then_some(merged)
            }
        }
    }
}

#[derive(Clone, Default)]
struct LoadedReplayState {
    entity_tags: HashMap<EntityId, ReplayEntityTags>,
    pid_map: HashMap<u32, EntityId>,
}

/// In-memory replay state for DogStatsD capture replays.
///
/// This handle stores the saved PID-to-entity mappings and saved entity tags extracted from a capture file so ADP can
/// later replay traffic against the same logical workload state that existed when the capture was recorded.
#[derive(Clone, Default)]
pub struct DogStatsDReplayState {
    inner: Arc<RwLock<Option<LoadedReplayState>>>,
}

impl DogStatsDReplayState {
    /// Creates a new, empty replay-state store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns whether replay state is currently loaded.
    pub fn is_loaded(&self) -> bool {
        self.inner
            .read()
            .expect("dogstatsd replay state lock poisoned")
            .is_some()
    }

    /// Loads replay state into memory.
    ///
    /// If both the saved entity map and PID map are empty, the existing replay state is cleared and `false` is
    /// returned. This preserves the Go "empty request clears replay state" behavior while still allowing pid-map-only
    /// captures to load successfully.
    pub fn load(&self, replay_state: TaggerState) -> Result<bool, GenericError> {
        if replay_state.state.is_empty() && replay_state.pid_map.is_empty() {
            self.clear();
            return Ok(false);
        }

        let mut loaded_state = LoadedReplayState::default();

        for (raw_entity_key, entity) in replay_state.state {
            let Some(entity_id) = entity
                .id
                .clone()
                .and_then(remote_entity_id_to_entity_id)
                .or_else(|| parse_entity_id_str(&raw_entity_key))
            else {
                warn!(
                    raw_entity_key,
                    "Skipping replay entity with an unsupported or missing entity ID."
                );
                continue;
            };

            if let Some(entity_tags) = ReplayEntityTags::from_remote_entity(&entity_id, entity) {
                loaded_state.entity_tags.insert(entity_id, entity_tags);
            }
        }

        for (pid, raw_entity_id) in replay_state.pid_map {
            let process_id = u32::try_from(pid).map_err(|_| {
                generic_error!("Replay state contained a negative PID mapping for '{}'.", raw_entity_id)
            })?;

            let Some(entity_id) = parse_entity_id_str(&raw_entity_id) else {
                warn!(
                    pid = process_id,
                    raw_entity_id, "Skipping replay PID mapping with an unsupported entity ID."
                );
                continue;
            };

            loaded_state.pid_map.insert(process_id, entity_id);
        }

        let mut state = self.inner.write().expect("dogstatsd replay state lock poisoned");
        *state = Some(loaded_state);

        Ok(true)
    }

    /// Clears any loaded replay state from memory.
    pub fn clear(&self) {
        let mut state = self.inner.write().expect("dogstatsd replay state lock poisoned");
        *state = None;
    }

    pub(crate) fn get_tags_for_entity(
        &self, entity_id: &EntityId, cardinality: OriginTagCardinality,
    ) -> Option<SharedTagSet> {
        if cardinality == OriginTagCardinality::None {
            return None;
        }

        let state = self.inner.read().expect("dogstatsd replay state lock poisoned");
        let loaded_state = state.as_ref()?;

        let resolved_entity = match entity_id {
            EntityId::ContainerPid(pid) => loaded_state.pid_map.get(pid).unwrap_or(entity_id),
            _ => entity_id,
        };

        loaded_state
            .entity_tags
            .get(resolved_entity)
            .and_then(|entity_tags| entity_tags.merge_for_cardinality(cardinality))
    }

    pub(crate) fn resolve_container_entity_for_pid(&self, process_id: u32) -> Option<EntityId> {
        self.inner
            .read()
            .expect("dogstatsd replay state lock poisoned")
            .as_ref()
            .and_then(|loaded_state| loaded_state.pid_map.get(&process_id).cloned())
    }
}

fn shared_tags_from_strings(tags: Vec<String>) -> Option<SharedTagSet> {
    if tags.is_empty() {
        return None;
    }

    Some(TagSet::from_iter(tags.into_iter().map(Tag::from)).into_shared())
}

fn remote_entity_id_to_entity_id(remote_entity_id: RemoteEntityId) -> Option<EntityId> {
    match remote_entity_id.prefix.as_str() {
        "container_id" => Some(EntityId::Container(remote_entity_id.uid.into())),
        "kubernetes_pod_uid" => Some(EntityId::PodUid(remote_entity_id.uid.into())),
        "internal" if remote_entity_id.uid == "global-entity-id" => Some(EntityId::Global),
        "container_pid" => remote_entity_id.uid.parse().ok().map(EntityId::ContainerPid),
        "container_inode" => remote_entity_id.uid.parse().ok().map(EntityId::ContainerInode),
        _ => None,
    }
}

fn parse_entity_id_str(value: &str) -> Option<EntityId> {
    const CONTAINER_ID_PREFIX: &str = "container_id://";
    const POD_UID_PREFIX: &str = "kubernetes_pod_uid://";
    const CONTAINER_PID_PREFIX: &str = "container_pid://";
    const CONTAINER_INODE_PREFIX: &str = "container_inode://";

    if let Some(container_id) = value.strip_prefix(CONTAINER_ID_PREFIX) {
        Some(EntityId::Container(MetaString::from(container_id)))
    } else if let Some(pod_uid) = value.strip_prefix(POD_UID_PREFIX) {
        Some(EntityId::PodUid(MetaString::from(pod_uid)))
    } else if let Some(pid) = value.strip_prefix(CONTAINER_PID_PREFIX) {
        pid.parse().ok().map(EntityId::ContainerPid)
    } else if let Some(inode) = value.strip_prefix(CONTAINER_INODE_PREFIX) {
        inode.parse().ok().map(EntityId::ContainerInode)
    } else if value == "system://global" {
        Some(EntityId::Global)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use datadog_protos::agent::{Entity, EntityId as RemoteEntityId, TaggerState};
    use saluki_context::{
        origin::OriginTagCardinality,
        tags::{Tag, TagSet},
    };
    use saluki_env::workload::EntityId;

    use super::DogStatsDReplayState;

    #[test]
    fn loads_entity_tags_and_pid_aliases() {
        let replay_state = DogStatsDReplayState::new();
        let loaded = replay_state
            .load(sample_replay_state())
            .expect("replay state should load");

        assert!(loaded);
        assert!(replay_state.is_loaded());
        assert_eq!(
            replay_state.resolve_container_entity_for_pid(42),
            Some(EntityId::Container("cid-123".into()))
        );

        let low_tags = replay_state
            .get_tags_for_entity(&EntityId::ContainerPid(42), OriginTagCardinality::Low)
            .expect("low-cardinality tags should resolve");
        assert_eq!(
            TagSet::from_iter((&low_tags).into_iter().cloned()),
            TagSet::from_iter([Tag::from("env:prod")])
        );

        let high_tags = replay_state
            .get_tags_for_entity(&EntityId::ContainerPid(42), OriginTagCardinality::High)
            .expect("high-cardinality tags should resolve");
        assert_eq!(
            TagSet::from_iter((&high_tags).into_iter().cloned()),
            TagSet::from_iter([
                Tag::from("container_name:api"),
                Tag::from("env:prod"),
                Tag::from("pod_name:api-123"),
            ])
        );
    }

    #[test]
    fn empty_state_clears_existing_state() {
        let replay_state = DogStatsDReplayState::new();
        replay_state
            .load(sample_replay_state())
            .expect("replay state should load");

        let loaded = replay_state
            .load(TaggerState::default())
            .expect("empty replay state should clear");

        assert!(!loaded);
        assert!(!replay_state.is_loaded());
        assert!(replay_state
            .get_tags_for_entity(&EntityId::Container("cid-123".into()), OriginTagCardinality::High)
            .is_none());
    }

    #[test]
    fn pid_map_only_state_still_loads() {
        let replay_state = DogStatsDReplayState::new();
        let loaded = replay_state
            .load(TaggerState {
                pid_map: [(7, "container_id://cid-456".to_string())].into_iter().collect(),
                ..Default::default()
            })
            .expect("pid-map-only replay state should load");

        assert!(loaded);
        assert_eq!(
            replay_state.resolve_container_entity_for_pid(7),
            Some(EntityId::Container("cid-456".into()))
        );
    }

    fn sample_replay_state() -> TaggerState {
        TaggerState {
            state: [(
                "container_id://cid-123".to_string(),
                Entity {
                    id: Some(RemoteEntityId {
                        prefix: "container_id".to_string(),
                        uid: "cid-123".to_string(),
                    }),
                    high_cardinality_tags: vec!["container_name:api".to_string()],
                    orchestrator_cardinality_tags: vec!["pod_name:api-123".to_string()],
                    low_cardinality_tags: vec!["env:prod".to_string()],
                    standard_tags: vec!["service:api".to_string()],
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            pid_map: [(42, "container_id://cid-123".to_string())].into_iter().collect(),
            duration: 5_000,
        }
    }
}
