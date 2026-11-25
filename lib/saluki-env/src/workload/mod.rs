//! Workload provider.
//!
//! This modules provides the `WorkloadProvider` trait, which deals with providing information about workloads running
//! on the process host.
//!
//! A number of building blocks are included: generic entity identifiers, tag storage, metadata collection and
//! aggregation.

use saluki_core::data_model::{
    origin::{OriginTagCardinality, RawExternalData, RawOrigin},
    tags::SharedTagSet,
};

pub mod aggregator;
pub mod collectors;

pub mod entity;
pub use self::entity::EntityId;

mod helpers;
#[cfg(target_os = "linux")]
pub use self::helpers::cgroups::CgroupsConfiguration;
#[cfg(unix)]
pub use self::helpers::containerd::ContainerdConfiguration;

mod metadata;
pub use self::metadata::{MetadataAction, MetadataOperation};

mod on_demand_pid;
pub use self::on_demand_pid::OnDemandPIDResolver;

pub mod origin;
use self::origin::ResolvedOrigin;

pub mod providers;

pub mod stores;

/// Resolves the process IDs of live processes to workload entities.
///
/// This trait is narrower than [`WorkloadProvider`]. Use it only for a process ID that is current when you resolve it:
/// a process ID from local socket credentials, or one that a sender reports, such as the OTLP `process.pid` attribute,
/// when the data arrives. A caller that processes the data later keeps the returned entity ID, and doesn't resolve the
/// process ID again. This isn't a lookup for historical process IDs.
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

    /// Gets low-cardinality tags for the container that runs this process.
    ///
    /// Providers that cannot determine the process container return `None`.
    fn get_self_container_tags(&self) -> Option<SharedTagSet> {
        None
    }

    /// Resolves a raw origin.
    ///
    ///  If the origin is empty, `None` is returned. Otherwise, `Some(ResolvedOrigin)` will be returned, which contains
    ///  fully resolved versions of the raw origin components.
    fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin>;

    /// Gets the container entity that an entity is an alias of, such as a container process ID or a cgroup inode.
    ///
    /// The container doesn't need tags. Returns `None` if the entity is no alias of a container, or if the provider
    /// doesn't track aliases.
    fn get_aliased_container_entity(&self, _entity_id: &EntityId) -> Option<EntityId> {
        None
    }

    /// Gets the container entity that External Data identifies.
    ///
    /// The lookup reads the current workload metadata, and doesn't cache its result. The container doesn't need tags.
    /// Returns `None` if no known container matches, or if the provider doesn't track External Data.
    fn get_container_entity_for_external_data(&self, _external_data: &RawExternalData<'_>) -> Option<EntityId> {
        None
    }
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

    fn get_self_container_tags(&self) -> Option<SharedTagSet> {
        match self.as_ref() {
            Some(provider) => provider.get_self_container_tags(),
            None => None,
        }
    }

    fn get_resolved_origin(&self, origin: RawOrigin<'_>) -> Option<ResolvedOrigin> {
        match self.as_ref() {
            Some(provider) => provider.get_resolved_origin(origin),
            None => None,
        }
    }

    fn get_aliased_container_entity(&self, entity_id: &EntityId) -> Option<EntityId> {
        match self.as_ref() {
            Some(provider) => provider.get_aliased_container_entity(entity_id),
            None => None,
        }
    }

    fn get_container_entity_for_external_data(&self, external_data: &RawExternalData<'_>) -> Option<EntityId> {
        match self.as_ref() {
            Some(provider) => provider.get_container_entity_for_external_data(external_data),
            None => None,
        }
    }
}
