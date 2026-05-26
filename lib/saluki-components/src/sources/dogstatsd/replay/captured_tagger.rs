//! In-memory tagger store backing replay traffic.
//!
//! When a replay session is active, this store holds the captured tagger state from the file's trailer. The DSD
//! packet handler routes lookups for replay-flagged packets here instead of the live `WorkloadProvider`, so replayed
//! metrics get the exact tags the entity had at capture time rather than whatever the live tagger holds now.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use datadog_protos::agent::TaggerState;
use saluki_common::collections::FastHashMap;
use saluki_context::{
    origin::OriginTagCardinality,
    tags::{SharedTagSet, Tag, TagSet},
};
use saluki_env::workload::EntityId;

use super::writer::parse_entity_id_str;

/// Per-entity captured tags, split by cardinality level.
///
/// Tags are stored as the writer recorded them: each level holds only the tags that belong to *that* level (not
/// cumulative). A lookup at cardinality `Orchestrator` returns `low + orchestrator`; a lookup at `High` returns
/// `low + orchestrator + high`. This matches how the live `WorkloadProvider` returns tag sets.
#[derive(Debug)]
struct CapturedEntity {
    low: SharedTagSet,
    orchestrator: SharedTagSet,
    high: SharedTagSet,
}

impl CapturedEntity {
    fn tags_for(&self, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        // Each cardinality is cumulative: Orchestrator includes Low; High includes both.
        let levels: &[&SharedTagSet] = match cardinality {
            OriginTagCardinality::None => return None,
            OriginTagCardinality::Low => &[&self.low],
            OriginTagCardinality::Orchestrator => &[&self.low, &self.orchestrator],
            OriginTagCardinality::High => &[&self.low, &self.orchestrator, &self.high],
        };

        let mut merged = SharedTagSet::default();
        for set in levels {
            merged.extend_from_shared(set);
        }
        (!merged.is_empty()).then_some(merged)
    }
}

/// Captured tagger snapshot consulted by replay-traffic origin resolution.
#[derive(Debug)]
pub(crate) struct CapturedTaggerStore {
    by_entity: FastHashMap<EntityId, CapturedEntity>,
    by_pid: FastHashMap<i32, EntityId>,
}

impl CapturedTaggerStore {
    /// Builds a `CapturedTaggerStore` from a decoded `TaggerState` trailer.
    ///
    /// Entity ID strings that don't match a known prefix (`container_id://`, `kubernetes_pod_uid://`, etc.) are
    /// skipped: there's no way to look them up downstream anyway.
    pub(crate) fn from_tagger_state(state: TaggerState) -> Self {
        let TaggerState {
            state: entities,
            pid_map,
            ..
        } = state;

        let mut by_entity = FastHashMap::with_capacity_and_hasher(entities.len(), Default::default());
        for (raw_entity_id, proto_entity) in entities {
            let Some(entity_id) = parse_entity_id_str(&raw_entity_id) else {
                continue;
            };
            by_entity.insert(
                entity_id,
                CapturedEntity {
                    low: tag_set_from_strings(&proto_entity.low_cardinality_tags),
                    orchestrator: tag_set_from_strings(&proto_entity.orchestrator_cardinality_tags),
                    high: tag_set_from_strings(&proto_entity.high_cardinality_tags),
                },
            );
        }

        let mut by_pid = FastHashMap::with_capacity_and_hasher(pid_map.len(), Default::default());
        for (pid, raw_entity_id) in pid_map {
            let Some(entity_id) = parse_entity_id_str(&raw_entity_id) else {
                continue;
            };
            by_pid.insert(pid, entity_id);
        }

        Self { by_entity, by_pid }
    }

    /// Returns tags for the given captured PID at the requested cardinality.
    ///
    /// Returns `None` if the PID wasn't captured, the entity has no tags at the requested cardinality, or the
    /// cardinality is `None`.
    pub(crate) fn lookup(&self, pid: i32, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        let entity_id = self.by_pid.get(&pid)?;
        self.lookup_by_entity(entity_id, cardinality)
    }

    /// Returns tags for an entity ID directly (bypassing the PID map). Used by paths that have already resolved the
    /// PID into an entity ID via some other route (e.g., External Data).
    pub(crate) fn lookup_by_entity(
        &self, entity_id: &EntityId, cardinality: OriginTagCardinality,
    ) -> Option<SharedTagSet> {
        self.by_entity.get(entity_id)?.tags_for(cardinality)
    }
}

/// Shared atomic slot holding the currently-active captured tagger store, if any.
///
/// Cloned freely between the replay control surface (which sets the current store as sessions start and finish) and
/// the DSD origin tag resolver (which loads it on every replay-flagged packet). Reads are lock-free; writes are
/// infrequent (per replay start/end), so `ArcSwapOption` is the right primitive: it gives readers a cheap `.load()`
/// while writers swap atomically.
#[derive(Clone, Default)]
pub(crate) struct CapturedTaggerHandle {
    inner: Arc<ArcSwapOption<CapturedTaggerStore>>,
}

impl CapturedTaggerHandle {
    /// Creates a new, empty handle.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Sets the current captured store for replay-origin resolution.
    ///
    /// `None` means the active replay session has no captured tagger snapshot, so replay-origin lookups return no
    /// tags.
    pub(crate) fn set_current(&self, store: Option<CapturedTaggerStore>) {
        self.inner.store(store.map(Arc::new));
    }

    /// Loads the current captured store, if a replay is active.
    pub(crate) fn current(&self) -> Option<Arc<CapturedTaggerStore>> {
        self.inner.load_full()
    }
}

fn tag_set_from_strings(raw: &[String]) -> SharedTagSet {
    TagSet::from_iter(raw.iter().map(|s| Tag::from(s.as_str()))).into_shared()
}

#[cfg(test)]
mod tests {
    use datadog_protos::agent::Entity as ProtoEntity;

    use super::*;

    fn make_state() -> TaggerState {
        let entity = ProtoEntity {
            low_cardinality_tags: vec!["env:prod".into(), "service:api".into()],
            orchestrator_cardinality_tags: vec!["pod_name:api-123".into()],
            high_cardinality_tags: vec!["container_name:api".into()],
            ..Default::default()
        };

        let mut entities = std::collections::HashMap::new();
        entities.insert("container_id://container-xyz".to_string(), entity);

        let mut pid_map = std::collections::HashMap::new();
        pid_map.insert(99, "container_id://container-xyz".to_string());
        // An unparseable entity id should be silently skipped.
        pid_map.insert(123, "unknown-prefix://something".to_string());

        TaggerState {
            state: entities,
            pid_map,
            duration: 0,
        }
    }

    #[test]
    fn lookup_returns_cardinality_appropriate_tag_set() {
        let store = CapturedTaggerStore::from_tagger_state(make_state());

        let low = store.lookup(99, OriginTagCardinality::Low).expect("low cardinality");
        assert!(low.into_iter().any(|t| t.as_str() == "env:prod"));

        let orch = store
            .lookup(99, OriginTagCardinality::Orchestrator)
            .expect("orchestrator cardinality");
        assert!(orch.into_iter().any(|t| t.as_str() == "pod_name:api-123"));
        assert!(store
            .lookup(99, OriginTagCardinality::Orchestrator)
            .expect("orchestrator")
            .into_iter()
            .any(|t| t.as_str() == "env:prod"));

        let high = store.lookup(99, OriginTagCardinality::High).expect("high cardinality");
        let high_tags: Vec<String> = high.into_iter().map(|t| t.as_str().to_string()).collect();
        assert!(high_tags.contains(&"container_name:api".to_string()));
        assert!(high_tags.contains(&"pod_name:api-123".to_string()));
        assert!(high_tags.contains(&"env:prod".to_string()));
    }

    #[test]
    fn lookup_at_none_cardinality_returns_none() {
        let store = CapturedTaggerStore::from_tagger_state(make_state());
        assert!(store.lookup(99, OriginTagCardinality::None).is_none());
    }

    #[test]
    fn unknown_pid_returns_none() {
        let store = CapturedTaggerStore::from_tagger_state(make_state());
        assert!(store.lookup(42, OriginTagCardinality::Low).is_none());
    }

    #[test]
    fn unparseable_entity_id_strings_are_skipped() {
        let store = CapturedTaggerStore::from_tagger_state(make_state());
        // PID 123 was mapped to a malformed entity id; lookup should miss cleanly.
        assert!(store.lookup(123, OriginTagCardinality::Low).is_none());
    }
}
