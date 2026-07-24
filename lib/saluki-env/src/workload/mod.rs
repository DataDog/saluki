//! Workload provider.
//!
//! This modules provides the `WorkloadProvider` trait, which deals with providing information about workloads running
//! on the process host.
//!
//! A number of building blocks are included: generic entity identifiers, tag storage, metadata collection and
//! aggregation.

use saluki_context::{
    origin::{OriginTagCardinality, RawOrigin},
    tags::SharedTagSet,
};

pub mod aggregator;
pub mod collectors;

pub mod entity;
pub use self::entity::EntityId;

mod helpers;
mod metadata;
pub use self::metadata::{MetadataAction, MetadataOperation};

mod on_demand_pid;
pub use self::on_demand_pid::OnDemandPIDResolver;

pub mod origin;
use self::origin::ResolvedOrigin;

pub mod providers;

pub mod stores;

/// Resolves live process IDs observed from local socket credentials to workload entities.
///
/// This is intentionally narrower than [`WorkloadProvider`]: callers should only use it for current PIDs obtained
/// directly from the local operating system. Callers that defer processing should retain the returned entity ID rather
/// than resolve the PID again later. This isn't a general-purpose historical PID lookup API.
pub trait CaptureEntityResolver {
    /// Resolves a live process ID to the container entity that owns it, if known.
    fn resolve_container_entity_for_live_pid(&self, process_id: u32) -> Option<EntityId>;
}

impl<T> CaptureEntityResolver for Option<T>
where
    T: CaptureEntityResolver,
{
    fn resolve_container_entity_for_live_pid(&self, process_id: u32) -> Option<EntityId> {
        match self.as_ref() {
            Some(resolver) => resolver.resolve_container_entity_for_live_pid(process_id),
            None => None,
        }
    }
}

/// Provides information about workloads running on the process host.
pub trait WorkloadProvider {
    /// Gets the tags for an entity.
    ///
    /// Entities are workload resources running on the process host, such as containers or pods. The cardinality of the
    /// tags to get can be controlled via `cardinality`.
    ///
    /// Returns `Some(SharedTagSet)` if the entity has tags, or `None` if the entity doesn't have any tags or if the
    /// entity wasn't found.
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet>;

    /// Resolves a raw origin.
    ///
    ///  If the origin is empty, `None` is returned. Otherwise, `Some(ResolvedOrigin)` will be returned, which contains
    ///  fully resolved versions of the raw origin components.
    fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin>;
}

impl<T> WorkloadProvider for Option<T>
where
    T: WorkloadProvider,
{
    fn get_tags_for_entity(&self, entity_id: &EntityId, cardinality: OriginTagCardinality) -> Option<SharedTagSet> {
        match self.as_ref() {
            Some(provider) => provider.get_tags_for_entity(entity_id, cardinality),
            None => None,
        }
    }

    fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        match self.as_ref() {
            Some(provider) => provider.get_resolved_origin(origin),
            None => None,
        }
    }
}
