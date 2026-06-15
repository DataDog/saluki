//! Serializable snapshots of topology structure.

use serde::Serialize;

/// Schema version for topology snapshots.
pub const TOPOLOGY_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// A serializable snapshot of a topology graph.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TopologySnapshot {
    schema_version: u32,
    components: Vec<TopologyComponentSnapshot>,
    edges: Vec<TopologyEdgeSnapshot>,
}

impl TopologySnapshot {
    pub(super) fn new(components: Vec<TopologyComponentSnapshot>, edges: Vec<TopologyEdgeSnapshot>) -> Self {
        Self {
            schema_version: TOPOLOGY_SNAPSHOT_SCHEMA_VERSION,
            components,
            edges,
        }
    }

    /// Returns the schema version for this topology snapshot.
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Returns the component snapshots.
    pub fn components(&self) -> &[TopologyComponentSnapshot] {
        &self.components
    }

    /// Returns the edge snapshots.
    pub fn edges(&self) -> &[TopologyEdgeSnapshot] {
        &self.edges
    }
}

/// A serializable snapshot of one topology component.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TopologyComponentSnapshot {
    id: String,
    kind: String,
    input: Option<TopologyDataTypeSnapshot>,
    outputs: Vec<TopologyOutputSnapshot>,
}

impl TopologyComponentSnapshot {
    pub(super) fn new(
        id: String, kind: String, input: Option<TopologyDataTypeSnapshot>, outputs: Vec<TopologyOutputSnapshot>,
    ) -> Self {
        Self {
            id,
            kind,
            input,
            outputs,
        }
    }

    /// Returns the component ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the component kind.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns the component input data type, if it has one.
    pub const fn input(&self) -> Option<&TopologyDataTypeSnapshot> {
        self.input.as_ref()
    }

    /// Returns the configured component outputs.
    pub fn outputs(&self) -> &[TopologyOutputSnapshot] {
        &self.outputs
    }
}

/// A serializable snapshot of one topology component output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TopologyOutputSnapshot {
    id: String,
    name: String,
    data_type: TopologyDataTypeSnapshot,
}

impl TopologyOutputSnapshot {
    pub(super) fn new(id: String, name: String, data_type: TopologyDataTypeSnapshot) -> Self {
        Self { id, name, data_type }
    }

    /// Returns the component output ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the output name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the output data type.
    pub const fn data_type(&self) -> &TopologyDataTypeSnapshot {
        &self.data_type
    }
}

/// A serializable snapshot of one topology edge.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TopologyEdgeSnapshot {
    from: String,
    to: String,
    data_type: TopologyDataTypeSnapshot,
}

impl TopologyEdgeSnapshot {
    pub(super) fn new(from: String, to: String, data_type: TopologyDataTypeSnapshot) -> Self {
        Self { from, to, data_type }
    }

    /// Returns the upstream component output ID.
    pub fn from(&self) -> &str {
        &self.from
    }

    /// Returns the downstream component ID.
    pub fn to(&self) -> &str {
        &self.to
    }

    /// Returns the data type carried by this edge.
    pub const fn data_type(&self) -> &TopologyDataTypeSnapshot {
        &self.data_type
    }
}

/// A serializable event or payload data type.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TopologyDataTypeSnapshot {
    category: String,
    signals: Vec<String>,
    label: String,
}

impl TopologyDataTypeSnapshot {
    pub(super) fn new(category: &'static str, signals: Vec<&'static str>, label: String) -> Self {
        Self {
            category: category.to_string(),
            signals: signals.into_iter().map(String::from).collect(),
            label,
        }
    }

    /// Returns the top-level data category.
    pub fn category(&self) -> &str {
        &self.category
    }

    /// Returns the signal names contained in this data type.
    pub fn signals(&self) -> &[String] {
        &self.signals
    }

    /// Returns the human-readable data type label.
    pub fn label(&self) -> &str {
        &self.label
    }
}
