//! The node IDs the syntax derives for nodes written without an `as` clause.
//!
//! Lowering names nodes with these functions, and resugaring compares an artifact's IDs with them to decide where a
//! `let` or an `as` clause must give a node its ID, so the two passes cannot disagree. The rules (see the [design
//! document](../../../docs/design/programmable-metrics-pipeline-syntax.md#node-ids)):
//!
//! - In `let x = s1 | ... | sn`, the last stage's node is `x`, and each earlier stage is [`stage`]`(x, op, n)`; a tuple
//!   head is [`merge`]`(x)`.
//! - In a statement without `let`, the anchor is the port the pipeline starts from ([`port_anchor`],
//!   [`input_anchor`], [`tee_port`], or for a tuple head, the merge [`merge`]`(<anchor of its first element>)`), and
//!   every stage is [`stage`]`(anchor, op, n)`.
//! - A stream used more than once is followed by the tee [`fanout`].
//! - A sink is the inline terminal [`terminal`]`(<parent>, <port>)`, as [`derived_terminal_id`] names it.
//!
//! Every ID is a [`join`], which shortens an ID longer than an identifier may be as [`derived_terminal_id`] does.

use std::collections::HashMap;

use saluki_metric_plan::{
    ir::{derived_terminal_id, ArtifactNodeId, LocalId, NodeId, PortName, SCOPE_INPUT_NODE},
    operators::names,
};

/// The operator a `where` stage runs, whose name its derived ID uses.
pub(crate) const WHERE_OPERATOR: &str = names::SPLIT;

/// Returns `<parent>_<suffix>`, shortened as [`derived_terminal_id`] shortens it when it is longer than an identifier
/// may be.
///
/// `parent` is an identifier and `suffix` an identifier or a decimal index, as every caller in this module passes;
/// anything else is a bug.
pub(crate) fn join(parent: &str, suffix: &str) -> ArtifactNodeId {
    let parent = NodeId::reserved(LocalId::new(parent).expect("derived IDs extend identifiers"));
    let suffix = PortName::new(suffix).expect("derived ID suffixes are identifiers or indices");
    ArtifactNodeId::new(derived_terminal_id(&parent, &suffix).local().as_str())
        .expect("a derived ID is never the reserved `input` node's name, since it contains an underscore")
}

/// The ID of the `ordinal`th stage running `operator` under `anchor`, counting from 1: `<anchor>_<operator>`, then
/// `<anchor>_<operator>_2`, and so on.
///
/// The ordinal is joined to the stage's first ID rather than to the operator's name, so that a long operator name, which
/// would not be a valid suffix with an ordinal appended, is shortened like any other long ID.
pub(crate) fn stage(anchor: &str, operator: &str, ordinal: usize) -> ArtifactNodeId {
    let first = join(anchor, operator);
    if ordinal <= 1 {
        first
    } else {
        join(first.as_str(), &ordinal.to_string())
    }
}

/// The ID of the merge of a tuple under `anchor`: `<anchor>_in`.
pub(crate) fn merge(anchor: &str) -> ArtifactNodeId {
    join(anchor, "in")
}

/// The ID of the tee after a stream used more than once: `<node>_fanout` for a node's only output port,
/// `<node>_<port>_fanout` for one of several.
pub(crate) fn fanout(node: &str, port: Option<&PortName>) -> ArtifactNodeId {
    join(port_anchor(node, port).as_str(), "fanout")
}

/// The anchor of a pipeline that starts from a node's output port: the node's ID for its only output port, or
/// `<node>_<port>` for one of several.
pub(crate) fn port_anchor(node: &str, port: Option<&PortName>) -> ArtifactNodeId {
    match port {
        None => ArtifactNodeId::new(node).expect("node IDs are identifiers"),
        Some(port) => join(node, port.as_str()),
    }
}

/// The anchor of a pipeline that starts from the source `input`: `input_<input>`.
pub(crate) fn input_anchor(input: &str) -> ArtifactNodeId {
    join(SCOPE_INPUT_NODE, input)
}

/// The anchor of a pipeline that starts from port `index` of an implicit tee: `<tee>_<index>`.
pub(crate) fn tee_port(tee: &str, index: usize) -> ArtifactNodeId {
    join(tee, &index.to_string())
}

/// The ID of the inline terminal at port `port` of the node `parent`: `<parent>_<port>`.
pub(crate) fn terminal(parent: &str, port: &PortName) -> ArtifactNodeId {
    join(parent, port.as_str())
}

/// Counts stages by operator under one anchor, for [`stage`]'s ordinals.
#[derive(Debug, Default)]
pub(crate) struct Ordinals(HashMap<String, usize>);

impl Ordinals {
    /// Returns the ordinal of the next stage running `operator`, counting from 1.
    pub(crate) fn next(&mut self, operator: &str) -> usize {
        let count = self.0.entry(operator.to_string()).or_insert(0);
        *count += 1;
        *count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_ids_follow_the_documented_scheme() {
        // (derived, expected)
        let cases = [
            (stage("dsd", "aggregate", 1), "dsd_aggregate"),
            (stage("dsd", "split", 2), "dsd_split_2"),
            (merge("dsd_post_agg_filter"), "dsd_post_agg_filter_in"),
            (fanout("dsd_post_agg_filter", None), "dsd_post_agg_filter_fanout"),
            (
                fanout("ts", Some(&PortName::new("passthrough").unwrap())),
                "ts_passthrough_fanout",
            ),
            (port_anchor("ts", None), "ts"),
            (
                port_anchor("ts", Some(&PortName::new("default").unwrap())),
                "ts_default",
            ),
            (input_anchor("dogstatsd"), "input_dogstatsd"),
            (tee_port("metrics_fanout", 0), "metrics_fanout_0"),
            (
                terminal("metrics_routing_filter_0", &PortName::new("false").unwrap()),
                "metrics_routing_filter_0_false",
            ),
        ];
        for (derived, expected) in cases {
            assert_eq!(derived.as_str(), expected);
        }

        // A long ID is shortened as terminal IDs are, so distinct long names stay distinct.
        let long = "a".repeat(60);
        assert_eq!(stage(&long, "aggregate", 1).as_str().len(), 63);
        assert_ne!(stage(&long, "aggregate", 1), stage(&long, "aggregate", 2));
        let long_operator = format!("op{}", "x".repeat(60));
        assert_eq!(stage("a", &long_operator, 2).as_str().len(), 63);
    }

    #[test]
    fn ordinals_count_each_operator_separately() {
        let mut ordinals = Ordinals::default();
        assert_eq!(ordinals.next("split"), 1);
        assert_eq!(ordinals.next("tee"), 1);
        assert_eq!(ordinals.next("split"), 2);
    }
}
