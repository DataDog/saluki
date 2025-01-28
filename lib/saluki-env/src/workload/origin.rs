//! Origin detection and resolution.

use std::sync::Arc;

use papaya::HashMap;
use saluki_context::origin::{OriginKey, OriginRef, OriginTagCardinality};
use tracing::trace;

use super::stores::ExternalDataStoreResolver;
use crate::workload::EntityId;

/// A resolved External Data entry.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ResolvedExternalData {
    pod_entity_id: EntityId,
    container_entity_id: EntityId,
}

impl ResolvedExternalData {
    /// Creates a new `ResolvedExternalData` from the given pod and container entity IDs.
    pub fn new(pod_entity_id: EntityId, container_entity_id: EntityId) -> Self {
        Self {
            pod_entity_id,
            container_entity_id,
        }
    }

    /// Returns a reference to the pod entity ID.
    pub fn pod_entity_id(&self) -> &EntityId {
        &self.pod_entity_id
    }

    /// Returns a reference to the container entity ID.
    pub fn container_entity_id(&self) -> &EntityId {
        &self.container_entity_id
    }
}

#[derive(Hash)]
struct ResolvedOriginInner {
    cardinality: Option<OriginTagCardinality>,
    process_id: Option<EntityId>,
    container_id: Option<EntityId>,
    pod_uid: Option<EntityId>,
    resolved_external_data: Option<ResolvedExternalData>,
}

/// An resolved representation of `OriginRef<'a>`
///
/// This representation is used to store the pre-calculated entity IDs derived from a borrowed `OriginRef<'a>` in order
/// to speed the lookup of origin tags attached to each individual entity ID that comprises an origin.
///
/// This type can be cheaply cloned and shared across threads.
#[derive(Clone, Hash)]
pub struct ResolvedOrigin {
    inner: Arc<ResolvedOriginInner>,
}

impl ResolvedOrigin {
    pub(crate) fn from_ref(origin: OriginRef<'_>, ed_resolver: &ExternalDataStoreResolver) -> Self {
        Self {
            inner: Arc::new(ResolvedOriginInner {
                cardinality: origin.cardinality(),
                process_id: origin.process_id().map(EntityId::ContainerPid),
                container_id: origin.container_id().and_then(EntityId::from_raw_container_id),
                pod_uid: origin.pod_uid().and_then(EntityId::from_pod_uid),
                resolved_external_data: origin.external_data().and_then(|raw_ed| ed_resolver.resolve(raw_ed)),
            }),
        }
    }

    /// Returns the cardinality of the origin.
    pub fn cardinality(&self) -> Option<OriginTagCardinality> {
        self.inner.cardinality
    }

    /// Returns the process ID of the origin.
    pub fn process_id(&self) -> Option<&EntityId> {
        self.inner.process_id.as_ref()
    }

    /// Returns the container ID of the origin.
    pub fn container_id(&self) -> Option<&EntityId> {
        self.inner.container_id.as_ref()
    }

    /// Returns the pod UID of the origin.
    pub fn pod_uid(&self) -> Option<&EntityId> {
        self.inner.pod_uid.as_ref()
    }

    /// Returns the resolved external data of the origin.
    pub fn resolved_external_data(&self) -> Option<&ResolvedExternalData> {
        self.inner.resolved_external_data.as_ref()
    }
}

/// Resolves and tracks origins.
#[derive(Clone)]
pub struct OriginResolver {
    ed_resolver: ExternalDataStoreResolver,
    key_mappings: Arc<HashMap<OriginKey, ResolvedOrigin>>,
}

impl OriginResolver {
    /// Creates a new `OriginResolver`.
    pub fn new(ed_resolver: ExternalDataStoreResolver) -> Self {
        Self {
            ed_resolver,
            key_mappings: Arc::new(HashMap::default()),
        }
    }

    pub(crate) fn resolve_origin(&self, origin: OriginRef<'_>) -> Option<OriginKey> {
        // If there's no origin information at all, then there's nothing to key off of.
        if origin.is_empty() {
            return None;
        }

        // We quickly hash the origin to its key form, and see if we're already tracking it.
        //
        // If we haven't seen this origin yet, we generate an owned representation of it, which includes fully resolving
        // any External Data to retrieve the resulting container ID, and store it in the state, mapped to the resulting
        // key. We'll utilize these mappings later on when actually resolving the necessary origin tags.
        let origin_key = OriginKey::from_opaque(&origin);

        // TODO: This is a slow leak because we never remove entries and have no signal to know when to do so.
        //
        // We should likely just use `quick-cache` here, but it's not a huge deal for now.
        let _ = self
            .key_mappings
            .pin()
            .get_or_insert_with(origin_key, || ResolvedOrigin::from_ref(origin, &self.ed_resolver));

        Some(origin_key)
    }

    pub(crate) fn get_resolved_origin_by_key(&self, origin_key: &OriginKey) -> Option<ResolvedOrigin> {
        match self.key_mappings.pin().get(origin_key) {
            Some(origin) => Some(origin.clone()),
            None => {
                trace!(?origin_key, "No origin found for key.");
                None
            }
        }
    }
}
