//! Resolution: from a parsed policy to the artifact, by resolving ports, targets, and inline terminals.

use std::{collections::HashMap, io};

use saluki_metric_plan::{
    ir::{
        ArtifactBase, ArtifactEdge, ArtifactNode, ArtifactNodeId, ArtifactPortRef, Blobs, OperatorName, OutputId,
        ParamName, Params, PolicyArtifact, PortName, IR_VERSION,
    },
    limits::values::ARTIFACT_SIZE,
    operators::{names, IN},
};
use serde_json::Value;

use crate::{
    diagnostic::{Error, LineIndex, Span},
    graph::{canonical_order, terminal_id, NodeShape, INPUT_PARENT},
    parser::{Policy, Target},
    spans::SpanMap,
};

/// One node of the artifact being built, with where it came from.
struct Entry {
    node: ArtifactNode,
    shape: NodeShape,
    span: Span,
    param_spans: Vec<(String, Span)>,
}

/// An edge whose target may not be resolved yet.
struct Pending {
    from: ArtifactPortRef,
    /// For an edge written as a statement, the span of its source port, whose node is checked once every node is
    /// known. Edge lines in a block leave their own node, so they have none.
    from_span: Option<Span>,
    to: PendingTarget,
    span: Span,
}

enum PendingTarget {
    Resolved(ArtifactPortRef),
    Node {
        id: ArtifactNodeId,
        port: Option<PortName>,
        span: Span,
    },
}

/// Builds the artifact a parsed policy describes, with the spans of its parts.
///
/// # Errors
///
/// Returns every problem found: a missing `base`, a node defined twice, an edge from or to a node that is not
/// defined, a port the node's operator does not have, an edge line without a port on a node that has several, and an
/// inline terminal whose derived ID is already taken. If there are none, returns an error if the artifact's JSON is
/// longer than the artifact size limit.
pub(crate) fn build(source: &str, policy: Policy) -> Result<(PolicyArtifact, SpanMap), Vec<Error>> {
    let mut builder = Builder {
        lines: LineIndex::new(source),
        source,
        entries: Vec::new(),
        by_id: HashMap::new(),
        errors: Vec::new(),
    };

    let Some(base) = policy.base else {
        return Err(vec![Error::new(
            policy.close,
            "Expected `base \"<program hash>\"` in the policy: an artifact records the hash of the program it was \
             written against.",
        )]);
    };

    // Every defined node is known before any inline terminal is named, so a terminal whose derived ID a node defined
    // later already has is reported as the terminal's problem.
    let mut blocks = Vec::new();
    for def in policy.nodes {
        if let Some(&first) = builder.by_id.get(def.id.as_str()) {
            let message = format!(
                "The node `{}` is defined twice; it is first defined at {}.",
                def.id,
                builder.position(builder.entries[first].span)
            );
            builder.errors.push(Error::new(def.id_span, message));
            continue;
        }
        let shape = NodeShape::of(&def.op, &def.params);
        let index = builder.add(Entry {
            node: ArtifactNode {
                id: def.id.clone(),
                op: def.op,
                params: def.params,
            },
            shape,
            span: def.span,
            param_spans: def.param_spans,
        });
        blocks.push((index, def.id, def.edges));
    }

    let mut pending = Vec::new();
    for (index, id, block) in blocks {
        let mut previous_emit = None;
        for edge in block {
            let emit = match &edge.target {
                Target::Emit {
                    output, output_span, ..
                } => Some((output.clone(), *output_span)),
                _ => None,
            };
            let port = builder.source_port(index, edge.port, edge.span, previous_emit.take());
            previous_emit = emit;
            let Some(port) = port else {
                continue;
            };
            let Some(to) = builder.target(id.as_str(), &port, edge.target) else {
                continue;
            };
            pending.push(Pending {
                from: ArtifactPortRef::Node { node: id.clone(), port },
                from_span: None,
                to,
                span: edge.span,
            });
        }
    }
    for edge in policy.edges {
        let (parent, port) = match &edge.from {
            ArtifactPortRef::Input(name) => match PortName::new(name.as_str()) {
                Ok(port) => (INPUT_PARENT.to_string(), port),
                Err(error) => {
                    builder.errors.push(Error::new(edge.from_span, error.to_string()));
                    continue;
                }
            },
            ArtifactPortRef::Node { node, port } => (node.to_string(), port.clone()),
        };
        let Some(to) = builder.target(&parent, &port, edge.target) else {
            continue;
        };
        pending.push(Pending {
            from: edge.from,
            from_span: Some(edge.from_span),
            to,
            span: edge.span,
        });
    }

    let mut edges = Vec::with_capacity(pending.len());
    for edge in pending {
        if let (ArtifactPortRef::Node { node, port }, Some(span)) = (&edge.from, edge.from_span) {
            match builder.by_id.get(node.as_str()) {
                Some(&index) => builder.check_output(index, port, span),
                None => builder.errors.push(Error::new(
                    span,
                    format!("There is no node `{}`; define it with `node {} = ...`.", node, node),
                )),
            }
        }
        let to = match edge.to {
            PendingTarget::Resolved(to) => Some(to),
            PendingTarget::Node { id, port, span } => builder.target_port(id, port, span),
        };
        if let Some(to) = to {
            edges.push((ArtifactEdge { from: edge.from, to }, edge.span));
        }
    }
    if !builder.errors.is_empty() {
        return Err(builder.errors);
    }

    let blobs = match policy.blobs {
        None => Blobs::default(),
        Some(blobs) => serde_json::from_value(Value::Object(blobs))
            .map_err(|error| vec![Error::new(policy.close, format!("Invalid blob table: {}", error))])?,
    };

    let (nodes, shapes): (Vec<_>, Vec<_>) = builder
        .entries
        .iter()
        .map(|entry| (entry.node.clone(), entry.shape.clone()))
        .unzip();
    let (artifact_edges, edge_spans): (Vec<_>, Vec<_>) = edges.into_iter().unzip();
    let order = canonical_order(&nodes, &artifact_edges, &shapes);

    let mut spans = SpanMap::default();
    for &index in &order.nodes {
        let entry = &builder.entries[index];
        let id = entry.node.id.as_str();
        spans.insert_node(id, entry.span);
        for (path, span) in &entry.param_spans {
            spans.insert_param(id, path.clone(), *span);
        }
    }
    for &index in &order.edges {
        spans.push_edge(edge_spans[index]);
    }

    let artifact = PolicyArtifact {
        ir_version: policy.ir_version.unwrap_or(IR_VERSION),
        scope: policy.scope,
        base: ArtifactBase {
            program_hash: base.program_hash,
            outputs: base.outputs,
        },
        nodes: order.nodes.iter().map(|&index| nodes[index].clone()).collect(),
        edges: order.edges.iter().map(|&index| artifact_edges[index].clone()).collect(),
        blobs,
    };
    check_size(&artifact, policy.head)?;
    Ok((artifact, spans))
}

/// Checks that the artifact's JSON, written compactly, is no longer than the artifact size limit, reporting it at
/// `head`, the start of the policy, if it is longer.
fn check_size(artifact: &PolicyArtifact, head: Span) -> Result<(), Vec<Error>> {
    /// Counts the bytes written to it.
    struct Counter(u64);

    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len() as u64;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, artifact).map_err(|error| {
        vec![Error::new(
            head,
            format!("The artifact cannot be written as JSON: {}", error),
        )]
    })?;
    if counter.0 > ARTIFACT_SIZE {
        let message = format!(
            "The artifact is {} bytes as JSON, more than the artifact size limit of {} bytes.",
            counter.0, ARTIFACT_SIZE
        );
        return Err(vec![Error::new(head, message)]);
    }
    Ok(())
}

struct Builder<'s> {
    source: &'s str,
    lines: LineIndex,
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    errors: Vec<Error>,
}

impl Builder<'_> {
    fn position(&self, span: Span) -> String {
        let (line, column) = self.lines.line_column(self.source, span.start);
        format!("line {}, column {}", line, column)
    }

    fn add(&mut self, entry: Entry) -> usize {
        let index = self.entries.len();
        self.by_id.insert(entry.node.id.to_string(), index);
        self.entries.push(entry);
        index
    }

    fn describe(&self, index: usize) -> String {
        let node = &self.entries[index].node;
        format!("Node `{}` (`{}`)", node.id, node.op)
    }

    /// Resolves the port an edge line of node `index` leaves: the one it names, or the node's only output port.
    ///
    /// `previous_emit` is the output, and its span, of the line before this one, if it is an inline `emit`. When it
    /// names one of this node's ports, the author left out the output, so `emit` took the port of this line as its
    /// output, and the error is reported there.
    fn source_port(
        &mut self, index: usize, port: Option<(PortName, Span)>, span: Span, previous_emit: Option<(OutputId, Span)>,
    ) -> Option<PortName> {
        match port {
            Some((port, port_span)) => {
                self.check_output(index, &port, port_span);
                Some(port)
            }
            None => {
                let shape = &self.entries[index].shape;
                if let Some(port) = shape.default_output() {
                    return Some(port);
                }
                let outputs = shape.outputs().unwrap_or_default();
                let swallowed = previous_emit.filter(|(output, _)| {
                    PortName::new(output.as_str()).is_ok_and(|port| shape.has_output(&port) == Some(true))
                });
                let (span, message) = if let Some((output, output_span)) = swallowed {
                    let node = &self.entries[index].node;
                    let message = format!(
                        "Expected an output ID after `emit`, found `{}`, which is a port of node `{}` (`{}`) and \
                         starts the next edge. Write the output after `emit`, as in `emit <output>`.",
                        output, node.id, node.op
                    );
                    (output_span, message)
                } else if outputs.is_empty() {
                    let message = format!("{} has no output ports, so it cannot have edges.", self.describe(index));
                    (span, message)
                } else {
                    let message = format!(
                        "{} has {} output ports ({}); name the port this edge leaves, as in `{} -> ...`.",
                        self.describe(index),
                        outputs.len(),
                        list_ports(outputs),
                        outputs[0]
                    );
                    (span, message)
                };
                self.errors.push(Error::new(span, message));
                None
            }
        }
    }

    /// Checks that node `index` has the output port `port`, when its ports are known.
    fn check_output(&mut self, index: usize, port: &PortName, span: Span) {
        let shape = &self.entries[index].shape;
        let (Some(false), Some(outputs)) = (shape.has_output(port), shape.outputs()) else {
            return;
        };
        let message = if outputs.is_empty() {
            format!(
                "{} has no output ports, so it has no port `{}`.",
                self.describe(index),
                port
            )
        } else {
            format!(
                "{} has no output port `{}`; its output ports are {}.",
                self.describe(index),
                port,
                list_ports(outputs)
            )
        };
        self.errors.push(Error::new(span, message));
    }

    /// Resolves an edge's target, adding the node of an inline terminal. `parent` and `port` are where the edge
    /// leaves, which name the inline terminal.
    ///
    /// A target that names a node is resolved once every node is known. Returns `None`, after recording why, if an
    /// inline terminal's derived ID is already taken.
    fn target(&mut self, parent: &str, port: &PortName, target: Target) -> Option<PendingTarget> {
        let (op, params, output_span, span, what) = match target {
            Target::Node { id, port, span } => return Some(PendingTarget::Node { id, port, span }),
            Target::Input { name, .. } => return Some(PendingTarget::Resolved(ArtifactPortRef::Input(name))),
            Target::Emit {
                output,
                output_span,
                span,
            } => (names::EMIT, emit_params(&output), Some(output_span), span, "emit"),
            Target::Drop { span } => (names::DROP, Params::new(), None, span, "drop"),
        };
        let in_port = PortName::new(IN).ok();
        let (Some(id), Ok(op), Some(in_port)) = (terminal_id(parent, port), OperatorName::new(op), in_port) else {
            // Derived IDs of valid parents and ports are always valid.
            self.errors
                .push(Error::new(span, "This inline terminal has no valid node ID."));
            return None;
        };
        if let Some(&existing) = self.by_id.get(id.as_str()) {
            let message = format!(
                "This inline `{}` is the node `{}` (named after port `{}` of `{}`), but `{}` is already defined at {}. \
                 Rename that node, or define this terminal as a node of its own.",
                what,
                id,
                port,
                parent,
                id,
                self.position(self.entries[existing].span)
            );
            self.errors.push(Error::new(span, message));
            return None;
        }
        let shape = NodeShape::of(&op, &params);
        self.add(Entry {
            node: ArtifactNode {
                id: id.clone(),
                op,
                params,
            },
            shape,
            span,
            param_spans: output_span
                .map(|span| ("output".to_string(), span))
                .into_iter()
                .collect(),
        });
        Some(PendingTarget::Resolved(ArtifactPortRef::Node {
            node: id,
            port: in_port,
        }))
    }

    /// Resolves the input port of an edge to node `id`: the port it names, or the node's only input port.
    fn target_port(&mut self, id: ArtifactNodeId, port: Option<PortName>, span: Span) -> Option<ArtifactPortRef> {
        let Some(&index) = self.by_id.get(id.as_str()) else {
            self.errors.push(Error::new(
                span,
                format!("There is no node `{}`; define it with `node {} = ...`.", id, id),
            ));
            return None;
        };
        let shape = &self.entries[index].shape;
        let port = match port {
            Some(port) => {
                if let (Some(false), Some(inputs)) = (shape.has_input(&port), shape.inputs()) {
                    let message = if inputs.is_empty() {
                        format!("{} has no input ports.", self.describe(index))
                    } else {
                        format!(
                            "{} has no input port `{}`; its input ports are {}.",
                            self.describe(index),
                            port,
                            list_ports(inputs)
                        )
                    };
                    self.errors.push(Error::new(span, message));
                    return None;
                }
                port
            }
            None => match shape.default_input() {
                Some(port) => port,
                None => {
                    let inputs = shape.inputs().unwrap_or_default();
                    let message = if inputs.is_empty() {
                        format!("{} has no input ports, so no edge can enter it.", self.describe(index))
                    } else {
                        format!(
                            "{} has {} input ports ({}); name the port this edge enters, as in `{}.{}`.",
                            self.describe(index),
                            inputs.len(),
                            list_ports(inputs),
                            id,
                            inputs[0]
                        )
                    };
                    self.errors.push(Error::new(span, message));
                    return None;
                }
            },
        };
        Some(ArtifactPortRef::Node { node: id, port })
    }
}

/// Returns the parameters of an inline `emit`.
fn emit_params(output: &OutputId) -> Params {
    ParamName::new("output")
        .map(|name| std::iter::once((name, Value::String(output.to_string()))).collect())
        .unwrap_or_default()
}

fn list_ports(ports: &[PortName]) -> String {
    ports
        .iter()
        .map(|port| format!("`{}`", port))
        .collect::<Vec<_>>()
        .join(", ")
}
