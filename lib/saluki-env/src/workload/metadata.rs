use std::fmt;

use saluki_context::tags::{Tag, TagSet};
use saluki_event::metric::OriginTagCardinality;

use super::{entity::EntityId, external_data::ExternalData, helpers::OneOrMany};

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
    /// Creates a new `MetadataOperation` that links an entity to its ancestor.
    pub fn link_ancestor(entity_id: EntityId, ancestor_entity_id: EntityId) -> Self {
        Self {
            entity_id,
            actions: OneOrMany::One(MetadataAction::LinkAncestor { ancestor_entity_id }),
        }
    }

    /// Creates a new `MetadataOperation` that deletes all metadata for an entity.
    pub fn delete(entity_id: EntityId) -> Self {
        Self {
            entity_id,
            actions: OneOrMany::One(MetadataAction::Delete),
        }
    }

    /// Creates a new `MetadataOperation` that adds a tag to an entity.
    pub fn tag<T>(entity_id: EntityId, cardinality: OriginTagCardinality, tag: T) -> Self
    where
        T: Into<Tag>,
    {
        Self {
            entity_id,
            actions: OneOrMany::One(MetadataAction::AddTag {
                cardinality,
                tag: tag.into(),
            }),
        }
    }

    /// Creates a new `MetadataOperation` that adds multiple tags to an entity.
    pub fn tags<I, T>(entity_id: EntityId, cardinality: OriginTagCardinality, tags: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<Tag>,
    {
        Self {
            entity_id,
            actions: OneOrMany::One(MetadataAction::AddTags {
                cardinality,
                tags: tags.into_iter().map(Into::into).collect(),
            }),
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

    /// Establishes a link between the entity and its ancestor.
    ///
    /// This creates an entity hierarchy, which allows for aggregation of entity metadata, such as including
    /// higher-level metadata, from the cluster or pod level, when getting the metadata for a specific container.
    ///
    /// Additionally, it can be used for canonicalizing an entity ID, such as mapping a container PID to the container
    /// ID which owns that process. This is useful because it can potentially help avoid the need to clone (allocate) the
    /// canonicalized version.
    LinkAncestor {
        /// Entity ID of the ancestor to link with.
        ancestor_entity_id: EntityId,
    },

    /// Establishes a link between the entity and its descendant.
    ///
    /// This creates an entity hierarchy, which allows for aggregation of entity metadata, such as including
    /// higher-level metadata, from the cluster or pod level, when getting the metadata for a specific container.
    LinkDescendant {
        /// Entity ID of the descendant to link with.
        descendant_entity_id: EntityId,
    },

    /// Adds a key/value tag to the entity.
    AddTag {
        /// Cardinality to add the tag at.
        cardinality: OriginTagCardinality,

        /// Tag to add.
        tag: Tag,
    },

    /// Adds multiple key/value tags to the entity.
    AddTags {
        /// Cardinality to add the tags at.
        cardinality: OriginTagCardinality,

        /// Tags to add.
        tags: TagSet,
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
            Self::LinkAncestor { ancestor_entity_id } => write!(f, "LinkAncestor({:?})", ancestor_entity_id),
            Self::LinkDescendant { descendant_entity_id } => write!(f, "LinkDescendant({:?})", descendant_entity_id),
            Self::AddTag { cardinality, tag } => write!(f, "AddTag(cardinality={:?}, tag={:?})", cardinality, tag),
            Self::AddTags { cardinality, tags } => write!(f, "AddTags(cardinality={:?}, tags={:?})", cardinality, tags),
            Self::SetTags { cardinality, tags } => write!(f, "SetTags(cardinality={:?}, tags={:?})", cardinality, tags),
            Self::AttachExternalData { external_data } => write!(f, "AttachExternalData({:?})", external_data),
        }
    }
}
