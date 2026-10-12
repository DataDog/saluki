//! Correspondence: which plan node each artifact node became when the artifact was composed.
//!
//! Composition places an artifact's nodes into the plan under IDs that make the plan diff meaningful. An artifact node
//! whose local ID equals a reserved ID inside its base export's scope keeps that reserved ID, so it corresponds to the
//! default program's node across generations. Its provenance switches to the artifact if its operator or parameters
//! differ, or, for a node that can lose records or runs in a host, if it is fed other records than the default program
//! feeds it (see [`compose`](crate::compose::compose)), so installing an unmodified export diffs as no change. Every
//! other artifact node enters the `policy` namespace, as `policy/<local>`.
//!
//! The diff itself compares nodes by ID, as for any two plans; the correspondence adds the way back from a plan node to
//! the artifact node it came from, so that a diff can be shown in the artifact's own terms, such as in its source.

use std::collections::BTreeMap;

use super::diff::{diff, PlanDiff};
use crate::{
    ir::{ArtifactNodeId, NodeId, Plan},
    operators::OperatorRegistry,
};

/// How an artifact node relates to the default program it was composed into.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CorrespondenceKind {
    /// The node keeps a reserved ID of the scope, with the operator and parameters the default program gives it, and,
    /// if it can lose records or runs in a host, fed what the default program feeds it: it keeps its native
    /// provenance.
    Unmodified,

    /// The node keeps a reserved ID of the scope, with another operator or other parameters, or, if it can lose records
    /// or runs in a host, fed other records: its provenance is the artifact.
    Modified,

    /// The node is the artifact's own, in the `policy` namespace, with the artifact's provenance.
    Added,
}

/// The plan node one artifact node became.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeCorrespondence {
    /// The node's ID in the composed plan.
    pub node: NodeId,

    /// How it relates to the default program.
    pub kind: CorrespondenceKind,
}

/// Which plan node each node of an artifact became, and which reserved nodes of the scope the artifact left out.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Correspondence {
    nodes: BTreeMap<ArtifactNodeId, NodeCorrespondence>,
    by_node: BTreeMap<NodeId, ArtifactNodeId>,
    removed: Vec<NodeId>,
}

impl Correspondence {
    /// Creates an empty correspondence.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the plan node an artifact node became, replacing any earlier record for either.
    pub fn insert(&mut self, artifact_node: ArtifactNodeId, correspondence: NodeCorrespondence) {
        if let Some(previous) = self.nodes.remove(&artifact_node) {
            self.by_node.remove(&previous.node);
        }
        if let Some(previous) = self.by_node.remove(&correspondence.node) {
            self.nodes.remove(&previous);
        }
        self.by_node.insert(correspondence.node.clone(), artifact_node.clone());
        self.nodes.insert(artifact_node, correspondence);
    }

    /// Records a reserved node of the scope that the artifact does not keep.
    pub fn insert_removed(&mut self, node: NodeId) {
        if !self.removed.contains(&node) {
            self.removed.push(node);
        }
    }

    /// Returns the plan node an artifact node became.
    pub fn get(&self, artifact_node: &ArtifactNodeId) -> Option<&NodeCorrespondence> {
        self.nodes.get(artifact_node)
    }

    /// Returns the artifact node a plan node came from, if it came from the artifact.
    pub fn artifact_node(&self, node: &NodeId) -> Option<&ArtifactNodeId> {
        self.by_node.get(node)
    }

    /// Returns every artifact node with the plan node it became, in artifact node ID order.
    pub fn iter(&self) -> impl Iterator<Item = (&ArtifactNodeId, &NodeCorrespondence)> {
        self.nodes.iter()
    }

    /// Returns the reserved nodes of the scope that the artifact does not keep, in the order they were recorded.
    pub fn removed(&self) -> &[NodeId] {
        &self.removed
    }
}

/// A plan diff with the correspondence of the candidate's artifact nodes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorrespondingDiff {
    /// The diff, node by node, by plan node ID.
    pub diff: PlanDiff,

    /// For each node of the candidate that came from the artifact, the artifact node it came from.
    pub artifact_nodes: BTreeMap<NodeId, ArtifactNodeId>,
}

/// Diffs a composed candidate plan against the active one, keeping the way back to the artifact's nodes.
///
/// Nodes correspond by ID, as in [`diff`]: composition already gave every artifact node the ID that corresponds, so
/// an unmodified export of the active default program diffs as no change.
pub fn diff_with_correspondence(
    active: &Plan, candidate: &Plan, correspondence: &Correspondence, registry: &OperatorRegistry,
) -> CorrespondingDiff {
    let diff = diff(active, candidate, registry);
    let artifact_nodes = candidate
        .nodes
        .iter()
        .filter_map(|node| {
            correspondence
                .artifact_node(&node.id)
                .map(|artifact_node| (node.id.clone(), artifact_node.clone()))
        })
        .collect();
    CorrespondingDiff { diff, artifact_nodes }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{json, Value};

    use super::*;
    use crate::{
        compose::{compose, receive_artifact, Approval},
        export::export,
        install::diff::NodeChange,
        testing::{default_program_catalog, with_opaque_gateways, DEFAULT_PROGRAM_JSON},
    };

    fn artifact_node(id: &str) -> ArtifactNodeId {
        id.parse().unwrap()
    }

    fn node(id: &str) -> NodeId {
        id.parse().unwrap()
    }

    /// Exports the `post_enrich` scope of `plan`, edits the artifact document, composes it back, and diffs the
    /// composed plan against `plan`.
    fn compose_edit(plan: &Plan, edit: impl FnOnce(&mut Value)) -> (CorrespondingDiff, Correspondence) {
        let registry = OperatorRegistry::native();
        // The scope's nodes compose unmodified or bind no configuration in these edits, so the catalog needs no
        // values.
        let catalog = default_program_catalog();
        let scope = "post_enrich".parse().unwrap();
        let mut document =
            serde_json::to_value(export(plan, &scope, &catalog, &BTreeMap::new()).unwrap().artifact).unwrap();
        edit(&mut document);
        let approval = Approval {
            revision: 1,
            approved_against: None,
            active_plan_hash: None,
            in_effect: false,
            upgraded: Default::default(),
        };
        let composed = compose(
            plan,
            &receive_artifact(document).unwrap(),
            &approval,
            &catalog,
            &registry,
        )
        .unwrap();
        let diff = diff_with_correspondence(plan, &composed.plan, &composed.correspondence, &registry);
        (diff, composed.correspondence)
    }

    #[test]
    fn installing_an_unmodified_export_diffs_as_no_change() {
        // (plan, the number of nodes in its scope)
        let plans = [
            (Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap(), 14),
            (
                Plan::from_value(with_opaque_gateways(DEFAULT_PROGRAM_JSON)).unwrap(),
                10,
            ),
        ];
        for (plan, scope_nodes) in &plans {
            let (diff, correspondence) = compose_edit(plan, |_| {});
            assert!(diff.diff.is_empty(), "{:?}", diff.diff);
            assert_eq!(diff.diff.changes().count(), 0);
            // Every node of the scope comes from the artifact node of the same name.
            assert_eq!(correspondence.iter().count(), *scope_nodes);
            assert_eq!(diff.artifact_nodes.len(), *scope_nodes);
            for (plan_node, artifact) in &diff.artifact_nodes {
                assert_eq!(plan_node.to_string(), artifact.to_string());
            }
            assert_eq!(
                diff.artifact_nodes.get(&node("metrics_policy")),
                Some(&artifact_node("metrics_policy"))
            );
            assert_eq!(diff.artifact_nodes.get(&node("dsd_agg")), None);
        }
    }

    #[test]
    fn a_diff_leads_back_from_renamed_nodes_to_the_artifact() {
        let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
        let (diff, _) = compose_edit(&plan, |document| {
            for node in document["nodes"].as_array_mut().unwrap() {
                if node["id"] == "metrics_policy_0" {
                    node["id"] = json!("primary");
                }
            }
            for edge in document["edges"].as_array_mut().unwrap() {
                if edge["to"] == "metrics_policy_0.in" {
                    edge["to"] = json!("primary.in");
                }
            }
        });
        let changes = diff
            .diff
            .changes()
            .map(|change| (change.id.to_string(), change.change.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            changes,
            [
                (
                    "metrics_policy".to_string(),
                    NodeChange::Changed {
                        operator: false,
                        params: false,
                        provenance: false,
                        edges: true
                    }
                ),
                ("policy/primary".to_string(), NodeChange::Added),
                ("metrics_policy_0".to_string(), NodeChange::Removed),
            ]
        );
        assert_eq!(
            diff.artifact_nodes.get(&node("policy/primary")),
            Some(&artifact_node("primary"))
        );
        assert_eq!(diff.artifact_nodes.get(&node("metrics_policy_0")), None);
    }

    #[test]
    fn correspondence_maps_both_ways_and_keeps_one_record_per_node() {
        let mut correspondence = Correspondence::new();
        correspondence.insert(
            artifact_node("metrics_policy"),
            NodeCorrespondence {
                node: node("metrics_policy"),
                kind: CorrespondenceKind::Unmodified,
            },
        );
        correspondence.insert(
            artifact_node("debug_drop"),
            NodeCorrespondence {
                node: node("policy/debug_drop"),
                kind: CorrespondenceKind::Added,
            },
        );
        correspondence.insert_removed(node("mrf_metrics_gateway"));
        correspondence.insert_removed(node("mrf_metrics_gateway"));

        assert_eq!(
            correspondence.artifact_node(&node("policy/debug_drop")),
            Some(&artifact_node("debug_drop"))
        );
        assert_eq!(correspondence.artifact_node(&node("debug_drop")), None);
        assert_eq!(
            correspondence.get(&artifact_node("metrics_policy")).map(|c| c.kind),
            Some(CorrespondenceKind::Unmodified)
        );
        assert_eq!(correspondence.removed(), [node("mrf_metrics_gateway")]);

        // Re-recording an artifact node replaces its earlier plan node in both directions.
        correspondence.insert(
            artifact_node("metrics_policy"),
            NodeCorrespondence {
                node: node("policy/metrics_policy"),
                kind: CorrespondenceKind::Added,
            },
        );
        assert_eq!(correspondence.artifact_node(&node("metrics_policy")), None);
        assert_eq!(correspondence.iter().count(), 2);
    }
}
