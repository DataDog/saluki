use std::fmt;

use saluki_context::{
    origin::{ExternalData, OriginTagCardinality},
    tags::TagSet,
};

use super::{entity::EntityId, helpers::OneOrMany};

/// A metadata operation.
///
/// Operations involve a number of actions to perform in the context of the given entity. Such actions typically include
/// setting tags or establishing ancestry links.
#[derive(Clone, Debug)]
pub struct MetadataOperation {
    /// The entity ID this operation is associated with.
    pub entity_id: EntityId,

    /// The action(s) to perform.
    pub actions: OneOrMany<MetadataAction>,
}

impl MetadataOperation {
    /// Creates a new `MetadataOperation` that aliases an entity to another.
    pub fn add_alias(entity_id: EntityId, target_entity_id: EntityId) -> Self {
        Self {
            entity_id,
            actions: OneOrMany::One(MetadataAction::AddAlias { target_entity_id }),
        }
    }

    /// Creates a new `MetadataOperation` that deletes all metadata for an entity.
    pub fn delete(entity_id: EntityId) -> Self {
        Self {
            entity_id,
            actions: OneOrMany::One(MetadataAction::Delete),
        }
    }

    /// Creates a new `MetadataOperation` that attaches External Data to an entity.
    pub fn attach_external_data(entity_id: EntityId, external_data: ExternalData) -> Self {
        Self {
            entity_id,
            actions: OneOrMany::One(MetadataAction::AttachExternalData { external_data }),
        }
    }
}

/// A metadata action.
#[derive(Clone)]
pub enum MetadataAction {
    /// Delete all metadata for the entity.
    Delete,

    /// Adds an alias to the entity.
    ///
    /// This can be used to allow for querying for information about an entity, where the information is not directly
    /// associated with the entity itself. For example, process ID entities usually are aliased to an underlying
    /// container ID entity, where the container ID entity itself is the one with associated tags and so on. When
    /// querying for the tags of the process ID entity, an alias to the container ID entity can be established to
    /// instead allow for getting the container ID entity's tags.
    AddAlias {
        /// Entity ID of the target to alias to.
        target_entity_id: EntityId,
    },

    /// Sets the tags for the entity.
    ///
    /// This overwrites any existing tags for the entity.
    SetTags {
        /// Cardinality to set the tags at.
        cardinality: OriginTagCardinality,

        /// Tags to set.
        tags: TagSet,
    },

    /// Attaches External Data to the entity.
    ///
    /// External Data is free-form string data that is attached to entities from external systems to allow resolving the
    /// entity's ID when it cannot be passed directly to the entity such that the entity can provide it in telemetry
    /// payloads itself.
    AttachExternalData {
        /// External Data to attach.
        external_data: ExternalData,
    },
}

impl fmt::Debug for MetadataAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Delete => write!(f, "Delete"),
            Self::AddAlias { target_entity_id } => write!(f, "AddAlias({:?})", target_entity_id),
            Self::SetTags { cardinality, tags } => write!(f, "SetTags(cardinality={:?}, tags={:?})", cardinality, tags),
            Self::AttachExternalData { external_data } => write!(f, "AttachExternalData({:?})", external_data),
        }
    }
}
