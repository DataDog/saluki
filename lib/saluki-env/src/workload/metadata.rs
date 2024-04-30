use super::{entity::EntityId, helpers::OneOrMany};

#[derive(Clone, Copy, Debug)]
pub enum TagCardinality {
    /// Low cardinality.
    ///
    /// This generally covers tags which are static, or relatively slow to change, and generally results in a small
    /// number of unique values for the given tag key.
    Low,

    /// High cardinality.
    ///
    /// This generally covers tags which frequently change and generally results in a large number of unique values for
    /// the given tag key.
    High,
}

impl TagCardinality {
    /// Parses a `TagCardinality` from a string.
    ///
    /// If the value is not a valid cardinality identifier, `None` is returned.
    pub fn parse<S>(s: S) -> Option<Self>
    where
        S: AsRef<str>,
    {
        match s.as_ref() {
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

#[derive(Debug)]
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
    pub fn tag(entity_id: EntityId, cardinality: TagCardinality, key: String, value: String) -> Self {
        Self {
            entity_id,
            actions: OneOrMany::One(MetadataAction::AddTag {
                cardinality,
                key,
                value,
            }),
        }
    }

    /// Creates a new `MetadataOperation` that adds multiple tags to an entity.
    pub fn tags<T, K, V>(entity_id: EntityId, cardinality: TagCardinality, tags: T) -> Self
    where
        T: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            entity_id,
            actions: OneOrMany::One(MetadataAction::AddTags {
                cardinality,
                tags: tags.into_iter().map(|(k, v)| (k.into(), v.into())).collect(),
            }),
        }
    }
}

#[derive(Debug)]
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
    LinkAncestor { ancestor_entity_id: EntityId },

    /// Adds a key/value tag to the entity.
    AddTag {
        cardinality: TagCardinality,
        key: String,
        value: String,
    },

    /// Adds multiple key/value tags to the entity.
    AddTags {
        cardinality: TagCardinality,
        tags: Vec<(String, String)>,
    },
}
