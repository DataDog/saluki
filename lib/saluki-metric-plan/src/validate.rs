//! Whole-program validator for [`MetricPlan`].
//!
//! Validates graph structure (acyclicity, port schemas, arity, reachability), uniqueness of source streams and emit
//! targets, and static operator parameters before a plan can be interpreted or compiled.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use saluki_metric_kernels::mapper::MapperKernel;
use snafu::Snafu;

use crate::{
    effects::{operator_spec, PortArity},
    ir::{Edge, MetricPlan, Node, NodeId, Op},
};

/// Errors detected while validating a [`MetricPlan`].
#[derive(Clone, Debug, Eq, PartialEq, Snafu)]
pub enum ValidationError {
    /// The plan schema version is unsupported.
    #[snafu(display("unsupported plan version {version}; expected {}", MetricPlan::CURRENT_VERSION))]
    UnsupportedVersion {
        /// Version supplied in the plan.
        version: u32,
    },

    /// Two nodes share the same identifier.
    #[snafu(display("duplicate node id `{id}`"))]
    DuplicateNodeId {
        /// Repeated node ID.
        id: NodeId,
    },

    /// Two `Source` nodes declare the same logical stream name.
    #[snafu(display("duplicate source stream `{stream}` on node `{id}`"))]
    DuplicateSourceStream {
        /// Repeated stream name.
        stream: String,
        /// Node ID carrying the duplicate stream.
        id: NodeId,
    },

    /// Two `Emit` nodes declare the same logical output target name.
    #[snafu(display("duplicate emit target `{target}` on node `{id}`"))]
    DuplicateEmitTarget {
        /// Repeated target name.
        target: String,
        /// Node ID carrying the duplicate target.
        id: NodeId,
    },

    /// The plan has no `Source` nodes.
    #[snafu(display("plan must contain at least one source node"))]
    NoSources,

    /// An edge references a node ID that does not exist.
    #[snafu(display("edge `{from}.{from_port} -> {to}` references unknown node `{missing}`"))]
    UnknownNode {
        /// Edge source node ID.
        from: NodeId,
        /// Edge source port name.
        from_port: String,
        /// Edge target node ID.
        to: NodeId,
        /// Missing node ID.
        missing: NodeId,
    },

    /// An edge references an output port not declared by the upstream operator.
    #[snafu(display("node `{from}` ({op}) does not declare output port `{port}`"))]
    UnknownOutputPort {
        /// Upstream node ID.
        from: NodeId,
        /// Operator name.
        op: &'static str,
        /// Invalid port name.
        port: String,
    },

    /// A non-fanout output port has multiple outgoing edges.
    #[snafu(display("node `{from}` ({op}) port `{port}` has {count} outgoing edges; insert a `tee` for fan-out"))]
    DisallowedFanout {
        /// Upstream node ID.
        from: NodeId,
        /// Operator name.
        op: &'static str,
        /// Port name.
        port: String,
        /// Outgoing edge count.
        count: usize,
    },

    /// A node received the wrong number of incoming edges for its operator.
    #[snafu(display("node `{id}` ({op}) has {actual} incoming edge(s), expected {expected}"))]
    InvalidInputArity {
        /// Node ID.
        id: NodeId,
        /// Operator name.
        op: &'static str,
        /// Actual incoming edge count.
        actual: usize,
        /// Expected arity description.
        expected: &'static str,
    },

    /// The plan graph contains a cycle.
    #[snafu(display("plan graph contains a cycle involving node(s): {nodes:?}"))]
    CycleDetected {
        /// Node IDs remaining with non-zero in-degree after topological sort.
        nodes: Vec<NodeId>,
    },

    /// Operator parameters failed static validation.
    #[snafu(display("node `{id}` ({op}) has invalid configuration: {reason}"))]
    InvalidOperatorConfig {
        /// Node ID.
        id: NodeId,
        /// Operator name.
        op: &'static str,
        /// Plain-language failure reason.
        reason: String,
    },
}

/// A validated [`MetricPlan`] bundled with its deterministic topological execution order.
#[derive(Clone, Debug)]
pub struct ValidatedPlan {
    plan: MetricPlan,
    topo_order: Vec<NodeId>,
}

impl ValidatedPlan {
    /// Returns the underlying [`MetricPlan`].
    pub fn plan(&self) -> &MetricPlan {
        &self.plan
    }

    /// Consumes `self` and returns the underlying [`MetricPlan`].
    pub fn into_plan(self) -> MetricPlan {
        self.plan
    }

    /// Returns node IDs in deterministic topological order (upstream before downstream).
    pub fn topo_order(&self) -> &[NodeId] {
        &self.topo_order
    }

    /// Returns nodes in topological order.
    pub fn nodes_in_topo_order(&self) -> Vec<&Node> {
        let by_id: BTreeMap<&NodeId, &Node> = self.plan.nodes.iter().map(|n| (&n.id, n)).collect();
        self.topo_order.iter().filter_map(|id| by_id.get(id).copied()).collect()
    }

    /// Returns the number of stateful operators in this plan.
    pub fn stateful_node_count(&self) -> usize {
        self.plan
            .nodes
            .iter()
            .filter(|node| operator_spec(&node.op).stateful)
            .count()
    }
}

/// Validates `plan` against all structural, arity, port, cycle, and operator rules.
pub fn validate_plan(plan: &MetricPlan) -> Result<ValidatedPlan, Vec<ValidationError>> {
    let mut errors = Vec::new();

    if plan.version != MetricPlan::CURRENT_VERSION {
        errors.push(ValidationError::UnsupportedVersion { version: plan.version });
    }

    let mut nodes_by_id: BTreeMap<&NodeId, &Node> = BTreeMap::new();
    let mut source_streams: BTreeSet<&str> = BTreeSet::new();
    let mut emit_targets: BTreeSet<&str> = BTreeSet::new();

    for node in &plan.nodes {
        if nodes_by_id.insert(&node.id, node).is_some() {
            errors.push(ValidationError::DuplicateNodeId { id: node.id.clone() });
            continue;
        }

        match &node.op {
            Op::Source { stream } => {
                if stream.is_empty() {
                    errors.push(ValidationError::InvalidOperatorConfig {
                        id: node.id.clone(),
                        op: "source",
                        reason: "stream name must not be empty".to_string(),
                    });
                } else if !source_streams.insert(stream.as_str()) {
                    errors.push(ValidationError::DuplicateSourceStream {
                        stream: stream.clone(),
                        id: node.id.clone(),
                    });
                }
            }
            Op::Emit { target } => {
                if target.is_empty() {
                    errors.push(ValidationError::InvalidOperatorConfig {
                        id: node.id.clone(),
                        op: "emit",
                        reason: "target name must not be empty".to_string(),
                    });
                } else if !emit_targets.insert(target.as_str()) {
                    errors.push(ValidationError::DuplicateEmitTarget {
                        target: target.clone(),
                        id: node.id.clone(),
                    });
                }
            }
            Op::Map { profiles } => {
                if let Err(err) = MapperKernel::compile(profiles) {
                    errors.push(ValidationError::InvalidOperatorConfig {
                        id: node.id.clone(),
                        op: "map",
                        reason: err.to_string(),
                    });
                }
            }
            Op::Aggregate { context_limit, .. } => {
                if *context_limit == 0 {
                    errors.push(ValidationError::InvalidOperatorConfig {
                        id: node.id.clone(),
                        op: "aggregate",
                        reason: "context_limit must be greater than zero".to_string(),
                    });
                }
            }
            Op::EnrichHostTags { binding } if binding.is_empty() => {
                errors.push(ValidationError::InvalidOperatorConfig {
                    id: node.id.clone(),
                    op: "enrich_host_tags",
                    reason: "binding name must not be empty".to_string(),
                });
            }
            _ => {}
        }
    }

    if source_streams.is_empty() {
        errors.push(ValidationError::NoSources);
    }

    // Track incoming/outgoing edges for port, fanout, arity, and cycle checks.
    let mut in_degree: BTreeMap<&NodeId, usize> = nodes_by_id.keys().map(|&id| (id, 0)).collect();
    let mut outgoing_per_port: BTreeMap<(&NodeId, &str), usize> = BTreeMap::new();
    let mut adjacency: BTreeMap<&NodeId, Vec<&NodeId>> = BTreeMap::new();

    for Edge { from, from_port, to } in &plan.edges {
        let from_node = nodes_by_id.get(from);
        let to_node = nodes_by_id.get(to);

        if from_node.is_none() {
            errors.push(ValidationError::UnknownNode {
                from: from.clone(),
                from_port: from_port.clone(),
                to: to.clone(),
                missing: from.clone(),
            });
        }
        if to_node.is_none() {
            errors.push(ValidationError::UnknownNode {
                from: from.clone(),
                from_port: from_port.clone(),
                to: to.clone(),
                missing: to.clone(),
            });
        }

        let (Some(from_node), Some(to_node)) = (from_node, to_node) else {
            continue;
        };

        let spec = operator_spec(&from_node.op);
        if !spec.output_ports.contains(&from_port.as_str()) {
            errors.push(ValidationError::UnknownOutputPort {
                from: from.clone(),
                op: spec.name,
                port: from_port.clone(),
            });
        }

        *outgoing_per_port
            .entry((&from_node.id, from_port.as_str()))
            .or_insert(0) += 1;
        *in_degree.entry(&to_node.id).or_insert(0) += 1;
        adjacency.entry(&from_node.id).or_default().push(&to_node.id);
    }

    // Check fan-out rules per (node, port).
    for ((node_id, port), count) in outgoing_per_port {
        if count > 1 {
            let spec = operator_spec(&nodes_by_id[node_id].op);
            if !spec.allows_fanout {
                errors.push(ValidationError::DisallowedFanout {
                    from: node_id.clone(),
                    op: spec.name,
                    port: port.to_string(),
                    count,
                });
            }
        }
    }

    // Check input arity for every node.
    for (node_id, node) in &nodes_by_id {
        let spec = operator_spec(&node.op);
        let actual = in_degree[node_id];
        match spec.input_arity {
            PortArity::Zero if actual != 0 => errors.push(ValidationError::InvalidInputArity {
                id: (*node_id).clone(),
                op: spec.name,
                actual,
                expected: "0",
            }),
            PortArity::One if actual != 1 => errors.push(ValidationError::InvalidInputArity {
                id: (*node_id).clone(),
                op: spec.name,
                actual,
                expected: "1",
            }),
            PortArity::OneOrMore if actual == 0 => errors.push(ValidationError::InvalidInputArity {
                id: (*node_id).clone(),
                op: spec.name,
                actual,
                expected: "at least 1",
            }),
            _ => {}
        }
    }

    // Kahn's topological sort using a deterministic min-heap / sorted queue.
    let mut remaining_in_degree = in_degree;
    let mut ready: VecDeque<&NodeId> = remaining_in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();
    let mut topo_order = Vec::with_capacity(nodes_by_id.len());

    while let Some(node_id) = ready.pop_front() {
        topo_order.push(node_id.clone());
        if let Some(neighbors) = adjacency.get(node_id) {
            let mut newly_ready = Vec::new();
            for &next in neighbors {
                let deg = remaining_in_degree
                    .get_mut(next)
                    .expect("edge target must exist in in_degree map");
                *deg -= 1;
                if *deg == 0 {
                    newly_ready.push(next);
                }
            }
            newly_ready.sort();
            ready.extend(newly_ready);
        }
    }

    if topo_order.len() != nodes_by_id.len() {
        let cycle_nodes: Vec<NodeId> = remaining_in_degree
            .into_iter()
            .filter(|(_, deg)| *deg > 0)
            .map(|(id, _)| id.clone())
            .collect();
        errors.push(ValidationError::CycleDetected { nodes: cycle_nodes });
    }

    if errors.is_empty() {
        Ok(ValidatedPlan {
            plan: plan.clone(),
            topo_order,
        })
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use saluki_metric_kernels::histogram::HistogramConfiguration;

    use super::*;

    #[test]
    fn valid_linear_plan_passes_and_orders_topologically() {
        let plan = MetricPlan::builder()
            .node(
                "src",
                Op::Source {
                    stream: "dogstatsd".to_string(),
                },
            )
            .node(
                "agg",
                Op::Aggregate {
                    window_duration_secs: NonZeroU64::new(10).unwrap(),
                    context_limit: 1000,
                    counter_expiry_secs: 300,
                    flush_open_windows: true,
                    histogram: HistogramConfiguration::default(),
                },
            )
            .node(
                "out",
                Op::Emit {
                    target: "primary".to_string(),
                },
            )
            .edge("src", "agg")
            .edge("agg", "out")
            .build();

        let validated = validate_plan(&plan).expect("plan should be valid");
        assert_eq!(
            validated.topo_order(),
            &[NodeId::from("src"), NodeId::from("agg"), NodeId::from("out")]
        );
        assert_eq!(validated.stateful_node_count(), 1);
    }

    #[test]
    fn rejects_cycle_and_disallowed_fanout() {
        let cyclic = MetricPlan::builder()
            .node(
                "src",
                Op::Source {
                    stream: "dsd".to_string(),
                },
            )
            .node("m", Op::Merge)
            .node("t", Op::Tee)
            .node(
                "out",
                Op::Emit {
                    target: "primary".to_string(),
                },
            )
            .edge("src", "m")
            .edge("m", "t")
            .edge("t", "m")
            .edge("t", "out")
            .build();

        let errs = validate_plan(&cyclic).expect_err("cyclic plan must fail");
        assert!(errs.iter().any(|e| matches!(e, ValidationError::CycleDetected { .. })));

        let fanout = MetricPlan::builder()
            .node(
                "src",
                Op::Source {
                    stream: "dsd".to_string(),
                },
            )
            .node(
                "a",
                Op::Emit {
                    target: "a".to_string(),
                },
            )
            .node(
                "b",
                Op::Emit {
                    target: "b".to_string(),
                },
            )
            .edge("src", "a")
            .edge("src", "b")
            .build();

        let errs = validate_plan(&fanout).expect_err("direct fanout without tee must fail");
        assert!(errs
            .iter()
            .any(|e| matches!(e, ValidationError::DisallowedFanout { .. })));
    }
}
