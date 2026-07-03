//! Topology snapshot comparison.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

use serde::Serialize;

use super::{
    TopologyComponentSnapshot, TopologyDataTypeSnapshot, TopologyEdgeSnapshot, TopologyOutputSnapshot, TopologySnapshot,
};

/// A JSON diff between two topology snapshots.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TopologyDiffSnapshot {
    components: ComponentDiffs,
    edges: EdgeDiffs,
}

/// Compares two topology snapshots and returns a minimal diff snapshot.
pub fn diff_topology_snapshots(base: &TopologySnapshot, head: &TopologySnapshot) -> TopologyDiffSnapshot {
    let base_index = SnapshotIndex::new(base);
    let head_index = SnapshotIndex::new(head);

    TopologyDiffSnapshot {
        components: diff_components(&base_index, &head_index),
        edges: diff_edges(&base_index, &head_index),
    }
}

/// Renders a unified Mermaid graph for the diff between two topology snapshots.
pub fn topology_diff_to_mermaid(base: &TopologySnapshot, head: &TopologySnapshot) -> String {
    let base_index = SnapshotIndex::new(base);
    let head_index = SnapshotIndex::new(head);
    let component_statuses = component_statuses(&base_index, &head_index);
    let edge_statuses = edge_statuses(&base_index, &head_index);
    let components = unified_components(&base_index, &head_index);
    let edges = unified_edges(&base_index, &head_index);

    let mut component_node_ids = BTreeMap::new();
    for (index, component) in components.iter().enumerate() {
        component_node_ids.insert(component.id(), format!("component_{index}"));
    }

    let output_metadata = unified_output_metadata(&base_index, &head_index);
    let mut output = String::from("flowchart LR\n");
    for component in &components {
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

    let mut link_index = 0;
    let mut link_styles = Vec::new();
    for edge in &edges {
        let Some(edge_output) = output_metadata.get(edge.from()) else {
            continue;
        };
        let Some(from_node_id) = component_node_ids.get(edge_output.component_id.as_str()) else {
            continue;
        };
        let Some(to_node_id) = component_node_ids.get(edge.to()) else {
            continue;
        };

        let output_name = match edge_output.name.as_str() {
            "_default" => "default",
            name => name,
        };
        let edge_label = escape_mermaid_label(&format!("{output_name} / {}", edge_output.data_type.label()));
        writeln!(&mut output, "    {from_node_id} -->|\"{edge_label}\"| {to_node_id}")
            .expect("writing to String should not fail");

        match edge_statuses.get(edge_key(edge).as_str()).copied() {
            Some(DiffStatus::Added) => link_styles.push((link_index, "stroke:#16a34a,stroke-width:3px")),
            Some(DiffStatus::Removed) => {
                link_styles.push((link_index, "stroke:#dc2626,stroke-width:3px,stroke-dasharray:5 5"));
            }
            Some(DiffStatus::Modified) => link_styles.push((link_index, "stroke:#d97706,stroke-width:3px")),
            Some(DiffStatus::Unchanged) | None => {}
        }
        link_index += 1;
    }

    writeln!(
        &mut output,
        "    classDef added fill:#dcfce7,stroke:#16a34a,color:#166534,stroke-width:2px"
    )
    .expect("writing to String should not fail");
    writeln!(
        &mut output,
        "    classDef removed fill:#fee2e2,stroke:#dc2626,color:#991b1b,stroke-width:2px"
    )
    .expect("writing to String should not fail");
    for component in &components {
        match component_statuses.get(component.id()).copied() {
            Some(DiffStatus::Added) => {
                let node_id = component_node_ids
                    .get(component.id())
                    .expect("component should have generated node ID");
                writeln!(&mut output, "    class {node_id} added").expect("writing to String should not fail");
            }
            Some(DiffStatus::Removed) => {
                let node_id = component_node_ids
                    .get(component.id())
                    .expect("component should have generated node ID");
                writeln!(&mut output, "    class {node_id} removed").expect("writing to String should not fail");
            }
            Some(DiffStatus::Modified | DiffStatus::Unchanged) | None => {}
        }
    }
    for (index, style) in link_styles {
        writeln!(&mut output, "    linkStyle {index} {style}").expect("writing to String should not fail");
    }

    output
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct ComponentDiffs {
    added: Vec<TopologyComponentSnapshot>,
    removed: Vec<TopologyComponentSnapshot>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct EdgeDiffs {
    added: Vec<TopologyEdgeSnapshot>,
    removed: Vec<TopologyEdgeSnapshot>,
    modified: Vec<ModifiedEdge>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ModifiedEdge {
    key: String,
    base: TopologyEdgeSnapshot,
    head: TopologyEdgeSnapshot,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    signals_added: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    signals_removed: Vec<String>,
}

#[derive(Clone, Copy)]
enum DiffStatus {
    Added,
    Removed,
    Modified,
    Unchanged,
}

struct SnapshotIndex<'a> {
    components_by_id: BTreeMap<&'a str, &'a TopologyComponentSnapshot>,
    outputs_by_id: BTreeMap<&'a str, &'a TopologyOutputSnapshot>,
    edges_by_key: BTreeMap<String, &'a TopologyEdgeSnapshot>,
}

impl<'a> SnapshotIndex<'a> {
    fn new(snapshot: &'a TopologySnapshot) -> Self {
        let mut components_by_id = BTreeMap::new();
        let mut outputs_by_id = BTreeMap::new();
        for component in snapshot.components() {
            components_by_id.insert(component.id(), component);
            for output in component.outputs() {
                outputs_by_id.insert(output.id(), output);
            }
        }

        let mut edges_by_key = BTreeMap::new();
        for edge in snapshot.edges() {
            edges_by_key.insert(edge_key(edge), edge);
        }

        Self {
            components_by_id,
            outputs_by_id,
            edges_by_key,
        }
    }
}

struct OutputMetadata {
    component_id: String,
    name: String,
    data_type: TopologyDataTypeSnapshot,
}

fn component_statuses(base: &SnapshotIndex<'_>, head: &SnapshotIndex<'_>) -> BTreeMap<String, DiffStatus> {
    let mut statuses = BTreeMap::new();
    for component_id in base
        .components_by_id
        .keys()
        .chain(head.components_by_id.keys())
        .copied()
        .collect::<BTreeSet<_>>()
    {
        let status = match (
            base.components_by_id.contains_key(component_id),
            head.components_by_id.contains_key(component_id),
        ) {
            (true, false) => DiffStatus::Removed,
            (false, true) => DiffStatus::Added,
            (true, true) => DiffStatus::Unchanged,
            (false, false) => unreachable!("component ID came from one of the indexes"),
        };
        statuses.insert(component_id.to_string(), status);
    }

    statuses
}

fn edge_statuses(base: &SnapshotIndex<'_>, head: &SnapshotIndex<'_>) -> BTreeMap<String, DiffStatus> {
    let mut statuses = BTreeMap::new();
    for edge_key in base
        .edges_by_key
        .keys()
        .chain(head.edges_by_key.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
    {
        let status = match (base.edges_by_key.get(&edge_key), head.edges_by_key.get(&edge_key)) {
            (Some(_), None) => DiffStatus::Removed,
            (None, Some(_)) => DiffStatus::Added,
            (Some(base_edge), Some(head_edge)) if data_type_changed(base, base_edge, head, head_edge) => {
                DiffStatus::Modified
            }
            (Some(_), Some(_)) => DiffStatus::Unchanged,
            (None, None) => unreachable!("edge key came from one of the indexes"),
        };
        statuses.insert(edge_key, status);
    }

    statuses
}

fn unified_components(base: &SnapshotIndex<'_>, head: &SnapshotIndex<'_>) -> Vec<TopologyComponentSnapshot> {
    base.components_by_id
        .keys()
        .chain(head.components_by_id.keys())
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|component_id| {
            match (
                base.components_by_id.get(component_id),
                head.components_by_id.get(component_id),
            ) {
                (Some(base_component), Some(head_component)) => Some(merge_component(base_component, head_component)),
                (Some(base_component), None) => Some((*base_component).clone()),
                (None, Some(head_component)) => Some((*head_component).clone()),
                (None, None) => None,
            }
        })
        .collect()
}

fn merge_component(base: &TopologyComponentSnapshot, head: &TopologyComponentSnapshot) -> TopologyComponentSnapshot {
    let mut outputs = BTreeMap::<&str, TopologyOutputSnapshot>::new();
    for output in base.outputs() {
        outputs.insert(output.id(), output.clone());
    }
    for output in head.outputs() {
        outputs.insert(output.id(), output.clone());
    }

    TopologyComponentSnapshot::new(
        head.id().to_string(),
        head.kind().to_string(),
        head.rust_type().to_string(),
        head.input().cloned(),
        outputs.into_values().collect(),
    )
}

fn unified_edges(base: &SnapshotIndex<'_>, head: &SnapshotIndex<'_>) -> Vec<TopologyEdgeSnapshot> {
    let mut edges = Vec::new();
    for (key, edge) in &base.edges_by_key {
        if !head.edges_by_key.contains_key(key) {
            edges.push((*edge).clone());
        }
    }
    edges.extend(head.edges_by_key.values().map(|edge| (*edge).clone()));
    edges
}

fn unified_output_metadata(base: &SnapshotIndex<'_>, head: &SnapshotIndex<'_>) -> BTreeMap<String, OutputMetadata> {
    let mut output_metadata = BTreeMap::new();
    for component in base.components_by_id.values().chain(head.components_by_id.values()) {
        for output in component.outputs() {
            output_metadata.insert(
                output.id().to_string(),
                OutputMetadata {
                    component_id: component.id().to_string(),
                    name: output.name().to_string(),
                    data_type: output.data_type().clone(),
                },
            );
        }
    }

    output_metadata
}

fn diff_components(base: &SnapshotIndex<'_>, head: &SnapshotIndex<'_>) -> ComponentDiffs {
    ComponentDiffs {
        added: head
            .components_by_id
            .iter()
            .filter(|(id, _)| !base.components_by_id.contains_key(*id))
            .map(|(_, component)| (*component).clone())
            .collect(),
        removed: base
            .components_by_id
            .iter()
            .filter(|(id, _)| !head.components_by_id.contains_key(*id))
            .map(|(_, component)| (*component).clone())
            .collect(),
    }
}

fn diff_edges(base: &SnapshotIndex<'_>, head: &SnapshotIndex<'_>) -> EdgeDiffs {
    let mut modified = Vec::new();
    for (key, base_edge) in &base.edges_by_key {
        let Some(head_edge) = head.edges_by_key.get(key) else {
            continue;
        };

        if data_type_changed(base, base_edge, head, head_edge) {
            let (signals_added, signals_removed) = signal_changes(base, base_edge, head, head_edge);
            modified.push(ModifiedEdge {
                key: key.clone(),
                base: (*base_edge).clone(),
                head: (*head_edge).clone(),
                signals_added,
                signals_removed,
            });
        }
    }

    EdgeDiffs {
        added: head
            .edges_by_key
            .iter()
            .filter(|(key, _)| !base.edges_by_key.contains_key(*key))
            .map(|(_, edge)| (*edge).clone())
            .collect(),
        removed: base
            .edges_by_key
            .iter()
            .filter(|(key, _)| !head.edges_by_key.contains_key(*key))
            .map(|(_, edge)| (*edge).clone())
            .collect(),
        modified,
    }
}

fn data_type_changed(
    base: &SnapshotIndex<'_>, base_edge: &TopologyEdgeSnapshot, head: &SnapshotIndex<'_>,
    head_edge: &TopologyEdgeSnapshot,
) -> bool {
    let base_data_type = edge_data_type(base, base_edge);
    let head_data_type = edge_data_type(head, head_edge);
    base_data_type.category() != head_data_type.category() || signal_set(base_data_type) != signal_set(head_data_type)
}

fn signal_changes(
    base: &SnapshotIndex<'_>, base_edge: &TopologyEdgeSnapshot, head: &SnapshotIndex<'_>,
    head_edge: &TopologyEdgeSnapshot,
) -> (Vec<String>, Vec<String>) {
    let base_signals = signal_set(edge_data_type(base, base_edge));
    let head_signals = signal_set(edge_data_type(head, head_edge));

    (
        head_signals.difference(&base_signals).cloned().collect(),
        base_signals.difference(&head_signals).cloned().collect(),
    )
}

fn edge_data_type<'a>(index: &'a SnapshotIndex<'a>, edge: &TopologyEdgeSnapshot) -> &'a TopologyDataTypeSnapshot {
    index
        .outputs_by_id
        .get(edge.from())
        .expect("edge source output should exist")
        .data_type()
}

fn signal_set(data_type: &TopologyDataTypeSnapshot) -> BTreeSet<String> {
    data_type.signals().iter().cloned().collect()
}

fn edge_key(edge: &TopologyEdgeSnapshot) -> String {
    format!("{}->{}", edge.from(), edge.to())
}

fn escape_mermaid_label(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "&#124;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{TopologyDataTypeSnapshot, TopologyOutputSnapshot};

    #[test]
    fn topology_diff_reports_only_added_removed_and_modified_items() {
        let base = TopologySnapshot::new(
            vec![
                component(
                    "source",
                    "source",
                    None,
                    vec![output("source", "_default", event(&["metrics"], "Metric"))],
                ),
                component(
                    "encoder",
                    "encoder",
                    Some(event(&["metrics"], "Metric")),
                    vec![output(
                        "encoder",
                        "_default",
                        payload(&["http", "trace_stats"], "HTTP | TraceStats"),
                    )],
                ),
                component("mapper", "transform", Some(event(&["metrics"], "Metric")), Vec::new()),
                component("dd_out", "forwarder", Some(payload(&["http"], "HTTP")), Vec::new()),
                component(
                    "deleted",
                    "destination",
                    Some(event(&["metrics"], "Metric")),
                    Vec::new(),
                ),
            ],
            vec![
                edge("source", "mapper"),
                edge("mapper", "dd_out"),
                edge("encoder", "dd_out"),
                edge("source", "deleted"),
            ],
        );
        let head = TopologySnapshot::new(
            vec![
                component(
                    "source",
                    "source",
                    None,
                    vec![output("source", "_default", event(&["metrics"], "Metric"))],
                ),
                component(
                    "encoder",
                    "encoder",
                    Some(event(&["metrics"], "Metric")),
                    vec![output("encoder", "_default", payload(&["http"], "HTTP"))],
                ),
                component(
                    "mapper",
                    "transform",
                    Some(event(&["metrics", "trace_stats"], "Metric | TraceStats")),
                    Vec::new(),
                ),
                component(
                    "aggregate",
                    "transform",
                    Some(event(&["metrics"], "Metric")),
                    Vec::new(),
                ),
                component("dd_out", "forwarder", Some(payload(&["http"], "HTTP")), Vec::new()),
            ],
            vec![
                edge("source", "mapper"),
                edge("aggregate", "dd_out"),
                edge("encoder", "dd_out"),
                edge("source", "aggregate"),
            ],
        );

        let diff = diff_topology_snapshots(&base, &head);

        assert_eq!(diff.components.added.len(), 1);
        assert_eq!(diff.components.added[0].id(), "aggregate");
        assert_eq!(diff.components.removed.len(), 1);
        assert_eq!(diff.components.removed[0].id(), "deleted");

        assert_eq!(diff.edges.added.len(), 2);
        assert!(diff
            .edges
            .added
            .iter()
            .any(|edge| edge.from() == "aggregate" && edge.to() == "dd_out"));
        assert!(diff
            .edges
            .added
            .iter()
            .any(|edge| edge.from() == "source" && edge.to() == "aggregate"));
        assert_eq!(diff.edges.removed.len(), 2);
        assert!(diff
            .edges
            .removed
            .iter()
            .any(|edge| edge.from() == "mapper" && edge.to() == "dd_out"));
        assert!(diff
            .edges
            .removed
            .iter()
            .any(|edge| edge.from() == "source" && edge.to() == "deleted"));
        assert_eq!(diff.edges.modified.len(), 1);

        let modified_edge = &diff.edges.modified[0];
        assert_eq!(modified_edge.key, "encoder->dd_out");
        assert_eq!(modified_edge.signals_added, Vec::<String>::new());
        assert_eq!(modified_edge.signals_removed, vec!["trace_stats"]);
    }

    #[test]
    fn signal_order_does_not_modify_an_edge() {
        let base = TopologySnapshot::new(
            vec![
                component(
                    "source",
                    "source",
                    None,
                    vec![output(
                        "source",
                        "_default",
                        event(&["metrics", "events"], "Metric | DatadogEvent"),
                    )],
                ),
                component(
                    "destination",
                    "destination",
                    Some(event(&["metrics", "events"], "Metric | DatadogEvent")),
                    Vec::new(),
                ),
            ],
            vec![edge("source", "destination")],
        );
        let head = TopologySnapshot::new(
            vec![
                component(
                    "source",
                    "source",
                    None,
                    vec![output(
                        "source",
                        "_default",
                        event(&["events", "metrics"], "Metric | DatadogEvent"),
                    )],
                ),
                component(
                    "destination",
                    "destination",
                    Some(event(&["events", "metrics"], "Metric | DatadogEvent")),
                    Vec::new(),
                ),
            ],
            vec![edge("source", "destination")],
        );

        let diff = diff_topology_snapshots(&base, &head);

        assert!(diff.components.added.is_empty());
        assert!(diff.components.removed.is_empty());
        assert!(diff.edges.added.is_empty());
        assert!(diff.edges.removed.is_empty());
        assert!(diff.edges.modified.is_empty());
    }

    #[test]
    fn mermaid_diff_renders_unified_graph_with_change_styles() {
        let base = TopologySnapshot::new(
            vec![
                component(
                    "source",
                    "source",
                    None,
                    vec![output(
                        "source",
                        "_default",
                        event(&["metrics", "trace_stats"], "Metric | TraceStats"),
                    )],
                ),
                component(
                    "destination",
                    "destination",
                    Some(event(&["metrics"], "Metric")),
                    Vec::new(),
                ),
            ],
            vec![edge("source", "destination")],
        );
        let head = TopologySnapshot::new(
            vec![
                component(
                    "source",
                    "source",
                    None,
                    vec![output("source", "_default", event(&["metrics"], "Metric"))],
                ),
                component("middle", "transform", Some(event(&["metrics"], "Metric")), Vec::new()),
                component(
                    "destination",
                    "destination",
                    Some(event(&["metrics"], "Metric")),
                    Vec::new(),
                ),
            ],
            vec![edge("source", "middle"), edge("source", "destination")],
        );

        let mermaid = topology_diff_to_mermaid(&base, &head);

        assert!(mermaid.contains("middle (transform)"));
        assert!(mermaid.contains("class component_1 added"));
        assert!(mermaid.contains("linkStyle"));
        assert!(mermaid.contains("stroke:#16a34a"));
        assert!(mermaid.contains("stroke:#d97706"));
    }

    #[test]
    fn serialized_diff_omits_redundant_rollups() {
        let base = TopologySnapshot::new(
            vec![
                component(
                    "source",
                    "source",
                    None,
                    vec![output("source", "_default", event(&["metrics"], "Metric"))],
                ),
                component(
                    "destination",
                    "destination",
                    Some(event(&["metrics"], "Metric")),
                    Vec::new(),
                ),
            ],
            vec![edge("source", "destination")],
        );
        let head = TopologySnapshot::new(
            vec![
                component(
                    "source",
                    "source",
                    None,
                    vec![output("source", "_default", event(&["metrics"], "Metric"))],
                ),
                component(
                    "destination",
                    "destination",
                    Some(event(&["metrics"], "Metric")),
                    Vec::new(),
                ),
            ],
            vec![edge("source", "destination")],
        );

        let diff = diff_topology_snapshots(&base, &head);
        let serialized = serde_json::to_string(&diff).expect("diff should serialize");

        assert!(!serialized.contains("schema_version"));
        assert!(!serialized.contains("summary"));
        assert!(!serialized.contains("rewired"));
        assert!(!serialized.contains("data_flow_changed"));
        assert!(!serialized.contains("unchanged"));
    }

    fn component(
        id: &str, kind: &str, input: Option<TopologyDataTypeSnapshot>, outputs: Vec<TopologyOutputSnapshot>,
    ) -> TopologyComponentSnapshot {
        TopologyComponentSnapshot::new(
            id.to_string(),
            kind.to_string(),
            format!("test::{kind}::{id}"),
            input,
            outputs,
        )
    }

    fn output(id: &str, name: &str, data_type: TopologyDataTypeSnapshot) -> TopologyOutputSnapshot {
        TopologyOutputSnapshot::new(id.to_string(), name.to_string(), data_type)
    }

    fn edge(from: &str, to: &str) -> TopologyEdgeSnapshot {
        TopologyEdgeSnapshot::new(from.to_string(), to.to_string())
    }

    fn event(signals: &[&'static str], label: &str) -> TopologyDataTypeSnapshot {
        TopologyDataTypeSnapshot::new("event", signals.to_vec(), label.to_string())
    }

    fn payload(signals: &[&'static str], label: &str) -> TopologyDataTypeSnapshot {
        TopologyDataTypeSnapshot::new("payload", signals.to_vec(), label.to_string())
    }
}
