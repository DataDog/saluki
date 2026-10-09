//! Port type propagation and checks.

use super::{
    graph::{emit_output, split_predicate, Graph},
    Finding, Rule, ValidationReport,
};
use crate::{
    ir::Plan,
    model::{KindSet, PortType, TimestampForm},
    operators::union_of,
};

/// The port types computed for a plan.
pub(super) struct Types {
    /// For each node, the union of the types of its inputs, when every input is known.
    pub inputs: Vec<Option<PortType>>,

    /// For each node and output port, the type of the port, when it is known.
    pub outputs: Vec<Vec<Option<PortType>>>,
}

impl Types {
    /// Returns the kinds a node can receive, or every kind when that is not known.
    pub fn input_kinds(&self, node: usize) -> KindSet {
        self.inputs[node].map_or(KindSet::ALL, |input| input.kinds)
    }
}

/// Describes a port type for messages, such as `counter, gauge (mixed timestamps)`.
pub(crate) fn describe(port_type: &PortType) -> String {
    let kinds = if port_type.kinds.is_empty() {
        "nothing".to_string()
    } else if port_type.kinds.is_all() {
        "every kind".to_string()
    } else {
        port_type
            .kinds
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let timestamps = match port_type.timestamps {
        TimestampForm::Timestamped => "timestamped",
        TimestampForm::Untimestamped => "untimestamped",
        TimestampForm::Mixed => "mixed timestamps",
    };
    format!("{} ({})", kinds, timestamps)
}

/// Propagates port types from the sources, in topological order, and reports every type finding.
///
/// A node whose inputs are not all known, because of an earlier structural problem, has unknown outputs, and is not
/// checked.
pub(super) fn check(graph: &Graph<'_>, plan: &Plan, report: &mut ValidationReport) -> Types {
    let mut types = Types {
        inputs: vec![None; graph.nodes.len()],
        outputs: graph
            .nodes
            .iter()
            .map(|node| vec![None; node.op.as_ref().map_or(0, |op| op.ports().outputs.len())])
            .collect(),
    };

    for &node in &graph.order {
        let Some(op) = graph.op(node) else { continue };
        let inputs = (0..op.ports().inputs.len())
            .map(|port| {
                graph
                    .source(node, port)
                    .and_then(|(from, from_port)| types.outputs[from][from_port])
            })
            .collect::<Option<Vec<_>>>();
        let Some(inputs) = inputs else { continue };

        for (port, input) in op.ports().inputs.iter().zip(&inputs) {
            if !port.accepts.accepts(input) {
                report.push(Finding::new(
                    Rule::PortTypes,
                    graph.port_location(node, &port.name),
                    format!(
                        "`{}` accepts {} on this port, but can receive {}",
                        op.name(),
                        describe(&port.accepts),
                        describe(input)
                    ),
                ));
            }
        }

        let input = union_of(&inputs);
        if !inputs.is_empty() {
            types.inputs[node] = Some(input);
        }

        if let Some(output) = emit_output(op).and_then(|id| plan.output(id)) {
            if !output.accepts.accepts(&input) {
                report.push(Finding::new(
                    Rule::OutputAccepts,
                    graph.location(node),
                    format!(
                        "output `{}` accepts {}, but this `emit` can receive {}; narrow what reaches it, for example \
                         with a `split` on `kind_in`",
                        output.id,
                        describe(&output.accepts),
                        describe(&input)
                    ),
                ));
            }
        }

        let outputs = op.output_types(&inputs);
        let receives = !inputs.is_empty() && !input.kinds.is_empty();
        if receives {
            if let Some(predicate) = split_predicate(op) {
                if outputs.first().is_some_and(|matching| matching.kinds.is_empty()) {
                    report.push(Finding::new(
                        Rule::PredicateCanMatch,
                        graph.location(node),
                        format!(
                            "the predicate can never match: this `split` can receive only {}, and the predicate is \
                             false for every one of those kinds ({})",
                            describe(&input),
                            serde_json::to_string(predicate).unwrap_or_default()
                        ),
                    ));
                }
            } else {
                for (port, output) in op.ports().outputs.iter().zip(&outputs) {
                    if output.kinds.is_empty() {
                        report.push(Finding::new(
                            Rule::PortCanCarry,
                            graph.port_location(node, &port.name),
                            format!(
                                "this port can never carry a record: `{}` can receive only {}",
                                op.name(),
                                describe(&input)
                            ),
                        ));
                    }
                }
            }
        }

        for (slot, output) in types.outputs[node].iter_mut().zip(outputs) {
            *slot = Some(output);
        }
    }

    types
}
