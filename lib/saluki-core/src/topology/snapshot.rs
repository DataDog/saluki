//! Serializable snapshots of topology structure.

use std::{collections::HashMap, fmt::Write as _};

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

    /// Renders this topology snapshot as a Mermaid flowchart.
    ///
    /// # Panics
    ///
    /// Panics if an edge references a missing output or component. Snapshots produced by topology blueprints satisfy
    /// this invariant.
    pub fn to_mermaid(&self) -> String {
        let mut component_node_ids = HashMap::new();
        for (index, component) in self.components.iter().enumerate() {
            component_node_ids.insert(component.id(), format!("component_{index}"));
        }

        let mut output_metadata = HashMap::new();
        for component in &self.components {
            for output in &component.outputs {
                output_metadata.insert(
                    output.id(),
                    MermaidOutput {
                        component_id: component.id(),
                        name: output.name(),
                        data_type: output.data_type(),
                    },
                );
            }
        }

        let mut output = String::from("flowchart LR\n");
        for component in &self.components {
            let node_id = component_node_ids
                .get(component.id())
                .expect("component should have generated node ID");
            let label = format!(
                "{} ({})",
                escape_mermaid_label(component.id()),
                escape_mermaid_label(component.kind())
            );
            writeln!(&mut output, "    {node_id}[\"{label}\"]").expect("writing to String should not fail");
        }

        for edge in &self.edges {
            let edge_output = output_metadata
                .get(edge.from())
                .expect("edge source output should exist");
            let from_node_id = component_node_ids
                .get(edge_output.component_id)
                .expect("edge source component should exist");
            let to_node_id = component_node_ids
                .get(edge.to())
                .expect("edge destination component should exist");

            let output_name = match edge_output.name {
                "_default" => "default",
                name => name,
            };
            let edge_label = escape_mermaid_label(&format!("{output_name} / {}", edge_output.data_type.label()));
            writeln!(&mut output, "    {from_node_id} -->|\"{edge_label}\"| {to_node_id}")
                .expect("writing to String should not fail");
        }

        output
    }
}

struct MermaidOutput<'a> {
    component_id: &'a str,
    name: &'a str,
    data_type: &'a TopologyDataTypeSnapshot,
}

fn escape_mermaid_label(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "&#124;")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mermaid_export_renders_components_and_edges_from_snapshot() {
        let snapshot = TopologySnapshot::new(
            vec![
                TopologyComponentSnapshot::new(
                    "source".to_string(),
                    "source".to_string(),
                    None,
                    vec![TopologyOutputSnapshot::new(
                        "source".to_string(),
                        "_default".to_string(),
                        TopologyDataTypeSnapshot::new("payload", vec!["raw"], "Raw".to_string()),
                    )],
                ),
                TopologyComponentSnapshot::new(
                    "forwarder".to_string(),
                    "forwarder".to_string(),
                    Some(TopologyDataTypeSnapshot::new("payload", vec!["raw"], "Raw".to_string())),
                    Vec::new(),
                ),
            ],
            vec![TopologyEdgeSnapshot::new(
                "source".to_string(),
                "forwarder".to_string(),
                TopologyDataTypeSnapshot::new("payload", vec!["raw"], "Raw".to_string()),
            )],
        );

        assert_eq!(
            snapshot.to_mermaid(),
            "flowchart LR\n    component_0[\"source (source)\"]\n    component_1[\"forwarder (forwarder)\"]\n    component_0 -->|\"default / Raw\"| component_1\n"
        );
    }

    #[test]
    fn mermaid_export_escapes_labels() {
        let snapshot = TopologySnapshot::new(
            vec![
                TopologyComponentSnapshot::new(
                    "transform".to_string(),
                    "transform".to_string(),
                    None,
                    vec![TopologyOutputSnapshot::new(
                        "transform.events".to_string(),
                        "events".to_string(),
                        TopologyDataTypeSnapshot::new(
                            "event",
                            vec!["metrics", "events"],
                            "Metric|DatadogEvent".to_string(),
                        ),
                    )],
                ),
                TopologyComponentSnapshot::new(
                    "destination".to_string(),
                    "destination".to_string(),
                    Some(TopologyDataTypeSnapshot::new(
                        "event",
                        vec!["metrics", "events"],
                        "Metric|DatadogEvent".to_string(),
                    )),
                    Vec::new(),
                ),
            ],
            vec![TopologyEdgeSnapshot::new(
                "transform.events".to_string(),
                "destination".to_string(),
                TopologyDataTypeSnapshot::new("event", vec!["metrics", "events"], "Metric|DatadogEvent".to_string()),
            )],
        );

        assert!(snapshot
            .to_mermaid()
            .contains("component_0 -->|\"events / Metric&#124;DatadogEvent\"| component_1"));
    }
}
