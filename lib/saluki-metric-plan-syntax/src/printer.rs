//! The printer: an artifact to its surface syntax.
//!
//! Printing is a function of the artifact's graph: nodes and edges are written in [canonical
//! order](crate::graph::canonical_order), and every choice of form (inline terminals, omitted ports, sugar, line
//! breaks) depends only on the artifact. Each form is used only when parsing it back gives exactly the value printed;
//! otherwise the general form is printed, so parsing the output always rebuilds the artifact.

use std::{collections::HashMap, str::FromStr as _};

use saluki_metric_plan::{
    canonical::{contains_resolved_values, PlanHash, BINDING_MEMBER},
    ir::{
        ArtifactEdge, ArtifactNode, ArtifactPortRef, BindingName, Operand, OutputId, ParamType, PolicyArtifact,
        PortName, BLOB_MEMBER, IR_VERSION,
    },
    model::KindSet,
    operators::names,
    predicate::{Predicate, SetMatch, StringMatch, TagMatch},
};
use serde_json::{Map, Value};

use crate::{
    graph::{canonical_order, param_specs, positional_predicate, terminal_id, NodeShape, INPUT_PARENT},
    lexer::{self, is_invisible, DURATION_UNITS},
    parser::literal_type,
    PrintOptions, MAX_SOURCE_SIZE, MAX_TOKENS,
};

/// The indentation of one level.
const INDENT: usize = 2;

/// Prints an artifact.
///
/// The text breaks values and predicates across lines to fit `options.width`, unless that makes it longer than parsing
/// accepts: lines indented deeply, each holding one short list element, can take many times the bytes of the
/// artifact's JSON. Every value is then written on one line, which takes at most six times the bytes of its JSON (for
/// strings of characters written as escapes), so the text of any artifact within the artifact size limit parses back.
pub(crate) fn print(artifact: &PolicyArtifact, options: &PrintOptions) -> String {
    let text = layout(artifact, options, options.width);
    // Every token takes at least one byte, so only a long text can have too many.
    if text.len() <= MAX_SOURCE_SIZE && (text.len() <= MAX_TOKENS || lexer::lex(&text).is_ok()) {
        return text;
    }
    layout(artifact, options, usize::MAX)
}

/// Prints an artifact, breaking values and predicates across lines beyond `width` columns.
fn layout(artifact: &PolicyArtifact, options: &PrintOptions, width: usize) -> String {
    let shapes = artifact
        .nodes
        .iter()
        .map(|node| NodeShape::of(&node.op, &node.params))
        .collect::<Vec<_>>();
    let order = canonical_order(&artifact.nodes, &artifact.edges, &shapes);
    let graph = Graph::new(artifact, &shapes);
    let printer = Printer { width };

    let mut sections = Vec::new();

    let mut base = String::new();
    if artifact.ir_version != IR_VERSION {
        base.push_str(&format!("{}ir_version {}\n", pad(INDENT), artifact.ir_version));
    }
    base.push_str(&format!("{}base \"{}\"", pad(INDENT), artifact.base.program_hash));
    if !options.elide_fingerprints && !artifact.base.outputs.is_empty() {
        base.push_str(&format!("\n{}outputs {{\n", pad(INDENT)));
        for (output, fingerprint) in &artifact.base.outputs {
            base.push_str(&format!("{}{} = \"{}\",\n", pad(2 * INDENT), output, fingerprint));
        }
        base.push_str(&format!("{}}}", pad(INDENT)));
    }
    sections.push(base);

    let mut inputs = Vec::new();
    let mut dangling = Vec::new();
    let mut leaving: Vec<Vec<usize>> = vec![Vec::new(); artifact.nodes.len()];
    for &edge in &order.edges {
        match &artifact.edges[edge].from {
            ArtifactPortRef::Input(_) => inputs.push(edge),
            ArtifactPortRef::Node { node, .. } => match graph.index(node.as_str()) {
                Some(source) => leaving[source].push(edge),
                None => dangling.push(edge),
            },
            ArtifactPortRef::Exit(_) => dangling.push(edge),
        }
    }
    let statement = |edge: usize| {
        let edge_ref = &artifact.edges[edge];
        format!("{}{} -> {}", pad(INDENT), edge_ref.from, graph.target(edge))
    };
    if !inputs.is_empty() {
        sections.push(
            inputs
                .iter()
                .map(|&edge| statement(edge))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    for &node in &order.nodes {
        if !graph.inline[node] {
            sections.push(printer.node(&graph, node, &leaving[node]));
        }
    }
    if !dangling.is_empty() {
        sections.push(
            dangling
                .iter()
                .map(|&edge| statement(edge))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if !artifact.blobs.is_empty() {
        let mut blobs = format!("{}blobs {{\n", pad(INDENT));
        for (hash, value) in artifact.blobs.iter() {
            let key = format!("\"{}\" = ", hash);
            let at = At::line(2 * INDENT, 1).right(key.len());
            blobs.push_str(&format!(
                "{}{}{},\n",
                pad(2 * INDENT),
                key,
                printer.value(value, None, at)
            ));
        }
        blobs.push_str(&format!("{}}}", pad(INDENT)));
        sections.push(blobs);
    }

    format!("policy {} {{\n{}\n}}\n", artifact.scope, sections.join("\n\n"))
}

/// What printing needs to know about the artifact's graph beyond each node.
struct Graph<'a> {
    nodes: &'a [ArtifactNode],
    edges: &'a [ArtifactEdge],
    shapes: &'a [NodeShape],
    /// The position of the first node with each ID.
    index: HashMap<&'a str, usize>,
    /// Whether each node is printed inline, as the target of the one edge that enters it.
    inline: Vec<bool>,
    /// For each edge, whether its target is printed inline.
    inline_edge: Vec<bool>,
}

impl<'a> Graph<'a> {
    fn new(artifact: &'a PolicyArtifact, shapes: &'a [NodeShape]) -> Self {
        let nodes = &artifact.nodes[..];
        let edges = &artifact.edges[..];
        let mut index = HashMap::new();
        let mut count: HashMap<&str, usize> = HashMap::new();
        for (i, node) in nodes.iter().enumerate() {
            index.entry(node.id.as_str()).or_insert(i);
            *count.entry(node.id.as_str()).or_default() += 1;
        }
        let mut entering = vec![0usize; nodes.len()];
        let mut leaving = vec![0usize; nodes.len()];
        for edge in edges {
            if let ArtifactPortRef::Node { node, .. } = &edge.to {
                if let Some(&target) = index.get(node.as_str()) {
                    entering[target] += 1;
                }
            }
            if let ArtifactPortRef::Node { node, .. } = &edge.from {
                if let Some(&source) = index.get(node.as_str()) {
                    leaving[source] += 1;
                }
            }
        }

        let mut graph = Self {
            nodes,
            edges,
            shapes,
            index,
            inline: vec![false; nodes.len()],
            inline_edge: vec![false; edges.len()],
        };
        for (i, edge) in edges.iter().enumerate() {
            let ArtifactPortRef::Node { node, port } = &edge.to else {
                continue;
            };
            let Some(target) = graph.index(node.as_str()) else {
                continue;
            };
            let (parent, from_port) = match &edge.from {
                ArtifactPortRef::Input(name) => match PortName::new(name.as_str()) {
                    Ok(port) => (INPUT_PARENT, port),
                    Err(_) => continue,
                },
                ArtifactPortRef::Node { node, port } if graph.index(node.as_str()).is_some() => {
                    (node.as_str(), port.clone())
                }
                ArtifactPortRef::Node { .. } | ArtifactPortRef::Exit(_) => continue,
            };
            let unique = count.get(node.as_str()) == Some(&1);
            if unique
                && entering[target] == 1
                && leaving[target] == 0
                && shapes[target].default_input().as_ref() == Some(port)
                && terminal_form(&nodes[target]).is_some()
                && terminal_id(parent, &from_port).is_some_and(|id| id == nodes[target].id)
            {
                graph.inline[target] = true;
                graph.inline_edge[i] = true;
            }
        }
        graph
    }

    fn index(&self, id: &str) -> Option<usize> {
        self.index.get(id).copied()
    }

    /// Returns how the target of the edge at position `edge` is written.
    fn target(&self, edge: usize) -> String {
        let edge_ref = &self.edges[edge];
        match &edge_ref.to {
            ArtifactPortRef::Input(name) => format!("input.{}", name),
            ArtifactPortRef::Exit(name) => format!("exit.{}", name),
            ArtifactPortRef::Node { node, port } => {
                let target = self.index(node.as_str());
                if self.inline_edge[edge] {
                    if let Some(form) = target.and_then(|target| terminal_form(&self.nodes[target])) {
                        return form;
                    }
                }
                let default = match target {
                    Some(target) => self.shapes[target].default_input(),
                    None => NodeShape::default().default_input(),
                };
                // `emit` and `drop` alone are inline terminals, so nodes with those IDs always name the port.
                let keyword = matches!(node.as_str(), names::EMIT | names::DROP);
                if default.as_ref() == Some(port) && !keyword {
                    node.to_string()
                } else {
                    format!("{}.{}", node, port)
                }
            }
        }
    }
}

/// Returns the inline form of a terminal node, `emit <output>` or `drop`, if its operator and parameters are exactly
/// what that form stands for.
fn terminal_form(node: &ArtifactNode) -> Option<String> {
    match node.op.as_str() {
        names::DROP if node.params.is_empty() => Some("drop".to_string()),
        names::EMIT => {
            let mut params = node.params.iter();
            match (params.next(), params.next()) {
                (Some((name, Value::String(output))), None) if name.as_str() == "output" => {
                    OutputId::new(output.as_str())
                        .ok()
                        .map(|output| format!("emit {}", output))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// How a node's parameters are written.
enum Arguments<'a> {
    /// No parentheses: no parameters, or a `tee` whose ports are numbered by its edge lines.
    None,
    /// The sole predicate parameter, without its name.
    Positional(&'a Value),
    /// `name: value` pairs, with each value's declared type.
    Named(Vec<(&'a str, &'a Value, Option<&'static ParamType>)>),
}

/// Where printed text starts, and what follows it.
#[derive(Clone, Copy, Debug)]
struct At {
    /// The indentation of the line the text starts on, which continuation lines are indented relative to.
    indent: usize,

    /// The column the text starts at.
    col: usize,

    /// How many characters follow the text on its last line, such as a closing `)` or a separating `,`.
    trail: usize,
}

impl At {
    /// Text that starts a line indented by `indent`.
    fn line(indent: usize, trail: usize) -> Self {
        Self {
            indent,
            col: indent,
            trail,
        }
    }

    /// Text that starts `columns` further right.
    fn right(self, columns: usize) -> Self {
        Self {
            col: self.col + columns,
            ..self
        }
    }

    /// Text followed by `trail` characters.
    fn trail(self, trail: usize) -> Self {
        Self { trail, ..self }
    }
}

struct Printer {
    width: usize,
}

impl Printer {
    fn fits(&self, at: At, text: &str) -> bool {
        !text.contains('\n') && at.col + text.chars().count() + at.trail <= self.width
    }

    /// Prints the definition of the node at position `index`, with the edges that leave it in `edges`.
    fn node(&self, graph: &Graph<'_>, index: usize, edges: &[usize]) -> String {
        let node = &graph.nodes[index];
        let shape = &graph.shapes[index];

        let tee_sugar = node.op.as_str() == names::TEE && is_numbered_tee(node, edges, graph.edges);
        let arguments = if tee_sugar || (node.params.is_empty() && node.op.as_str() != names::TEE) {
            Arguments::None
        } else {
            let mut params = node.params.iter();
            match (positional_predicate(node.op.as_str()), params.next(), params.next()) {
                (Some(name), Some((param, value)), None) if param.as_str() == name => Arguments::Positional(value),
                _ => Arguments::Named(ordered_params(node)),
            }
        };

        let lines = edges
            .iter()
            .map(|&edge| {
                let port = match &graph.edges[edge].from {
                    ArtifactPortRef::Node { port, .. } => Some(port),
                    ArtifactPortRef::Input(_) | ArtifactPortRef::Exit(_) => None,
                };
                let label = port
                    .filter(|port| !tee_sugar && shape.default_output().as_ref() != Some(*port))
                    .map(|port| port.to_string());
                (label, graph.target(edge))
            })
            .collect::<Vec<_>>();
        let block = if lines.is_empty() { "" } else { " {" };

        let head = format!("{}node {} = {}", pad(INDENT), node.id, node.op);
        let inner = 2 * INDENT;
        let mut text = match arguments {
            Arguments::None => format!("{}{}", head, block),
            Arguments::Positional(predicate) => {
                let flat = format!("{}({}){}", head, self.flat_predicate(predicate), block);
                if self.fits(At::line(0, 0), &flat) {
                    flat
                } else {
                    format!(
                        "{}(\n{}{}\n{}){}",
                        head,
                        pad(inner),
                        self.predicate(predicate, At::line(inner, 0)),
                        pad(INDENT),
                        block
                    )
                }
            }
            Arguments::Named(params) => {
                let flat_params = params
                    .iter()
                    .map(|(name, value, ty)| format!("{}: {}", name, self.flat_value(value, *ty)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let flat = format!("{}({}){}", head, flat_params, block);
                if self.fits(At::line(0, 0), &flat) {
                    flat
                } else {
                    let mut text = format!("{}(\n", head);
                    for (name, value, ty) in &params {
                        let at = At::line(inner, 1).right(name.len() + 2);
                        text.push_str(&format!("{}{}: {},\n", pad(inner), name, self.value(value, *ty, at)));
                    }
                    text.push_str(&format!("{}){}", pad(INDENT), block));
                    text
                }
            }
        };

        if !lines.is_empty() {
            let label_width = lines
                .iter()
                .filter_map(|(label, _)| label.as_ref().map(String::len))
                .max()
                .unwrap_or(0);
            for (label, target) in &lines {
                text.push('\n');
                text.push_str(&pad(inner));
                if let Some(label) = label {
                    text.push_str(&format!("{:<width$} ", label, width = label_width));
                }
                text.push_str("-> ");
                text.push_str(target);
            }
            text.push_str(&format!("\n{}}}", pad(INDENT)));
        }
        text
    }

    // Values.

    /// Prints a value of declared type `ty` at `at`, breaking it across lines if it does not fit.
    fn value(&self, value: &Value, ty: Option<&'static ParamType>, at: At) -> String {
        let literal = literal_type(ty);
        if matches!(literal, Some(ParamType::Predicate)) {
            return self.predicate(value, at);
        }
        let flat = self.flat_value(value, ty);
        if self.fits(at, &flat) {
            return flat;
        }
        let inner = at.indent + INDENT;
        match value {
            Value::Array(values) if !values.is_empty() => {
                let element = element_type(literal);
                let mut text = "[\n".to_string();
                for value in values {
                    let element = self.value(value, element, At::line(inner, 1));
                    text.push_str(&format!("{}{},\n", pad(inner), element));
                }
                text.push_str(&format!("{}]", pad(at.indent)));
                text
            }
            Value::Object(members) if !members.is_empty() && call_form(members).is_none() => {
                let mut text = "{\n".to_string();
                for (key, member) in ordered_members(members, literal) {
                    let ty = member_type(literal, key);
                    let key = key_text(key);
                    let at = At::line(inner, 1).right(key.chars().count() + 2);
                    text.push_str(&format!("{}{}: {},\n", pad(inner), key, self.value(member, ty, at)));
                }
                text.push_str(&format!("{}}}", pad(at.indent)));
                text
            }
            _ => flat,
        }
    }

    /// Prints a value of declared type `ty` on one line.
    fn flat_value(&self, value: &Value, ty: Option<&'static ParamType>) -> String {
        let literal = literal_type(ty);
        if matches!(literal, Some(ParamType::Predicate)) {
            return self.flat_predicate(value);
        }
        match value {
            Value::Null => "null".to_string(),
            Value::Bool(value) => value.to_string(),
            Value::Number(number) => number.to_string(),
            Value::String(string) => {
                if matches!(literal, Some(ParamType::Duration)) && is_duration_token(string) {
                    string.clone()
                } else {
                    quote(string)
                }
            }
            Value::Array(values) => {
                let element = element_type(literal);
                let values = values
                    .iter()
                    .map(|value| self.flat_value(value, element))
                    .collect::<Vec<_>>();
                format!("[{}]", values.join(", "))
            }
            Value::Object(members) => {
                if let Some(call) = call_form(members) {
                    return call;
                }
                if members.is_empty() {
                    return "{}".to_string();
                }
                let members = ordered_members(members, literal)
                    .into_iter()
                    .map(|(key, member)| {
                        format!(
                            "{}: {}",
                            key_text(key),
                            self.flat_value(member, member_type(literal, key))
                        )
                    })
                    .collect::<Vec<_>>();
                format!("{{ {} }}", members.join(", "))
            }
        }
    }

    // Predicates.

    /// Prints a predicate at `at`, breaking it across lines at its top-level `and` or `or` if it does not fit.
    fn predicate(&self, value: &Value, at: At) -> String {
        let flat = self.flat_predicate(value);
        if self.fits(at, &flat) {
            return flat;
        }
        match classify(value) {
            Shape::Junction(junction, operands) if operands.len() >= 2 => {
                let word = junction.word();
                let last = operands.len() - 1;
                let mut text = String::new();
                for (i, operand) in operands.iter().enumerate() {
                    let trail = if i == last { at.trail } else { 0 };
                    if i == 0 {
                        text.push_str(&self.operand(operand, at.trail(trail)));
                    } else {
                        let at = At::line(at.indent, trail).right(word.len() + 1);
                        text.push_str(&format!("\n{}{} {}", pad(at.indent), word, self.operand(operand, at)));
                    }
                }
                text
            }
            Shape::Junction(junction, operands) => {
                let inner = at.indent + INDENT;
                let mut text = format!("{}(\n", junction.name());
                for operand in operands {
                    text.push_str(&format!(
                        "{}{},\n",
                        pad(inner),
                        self.predicate(operand, At::line(inner, 1))
                    ));
                }
                text.push_str(&format!("{})", pad(at.indent)));
                text
            }
            Shape::Not(operand) => format!("not {}", self.operand(operand, at.right(4))),
            Shape::Leaf(leaf) => self.leaf(&leaf, Some(at)),
            Shape::Raw => self.value(value, None, at),
        }
    }

    /// Prints an operand of `and`, `or`, or `not`, in parentheses if it is itself an `and` or `or`.
    fn operand(&self, value: &Value, at: At) -> String {
        if !is_infix(value) {
            return self.predicate(value, at);
        }
        let flat = format!("({})", self.flat_predicate(value));
        if self.fits(at, &flat) {
            return flat;
        }
        let inner = at.indent + INDENT;
        format!(
            "(\n{}{}\n{})",
            pad(inner),
            self.predicate(value, At::line(inner, 0)),
            pad(at.indent)
        )
    }

    fn flat_predicate(&self, value: &Value) -> String {
        match classify(value) {
            Shape::Junction(junction, operands) if operands.len() >= 2 => operands
                .iter()
                .map(|operand| self.flat_operand(operand))
                .collect::<Vec<_>>()
                .join(&format!(" {} ", junction.word())),
            Shape::Junction(junction, operands) => {
                let operands = operands
                    .iter()
                    .map(|operand| self.flat_predicate(operand))
                    .collect::<Vec<_>>();
                format!("{}({})", junction.name(), operands.join(", "))
            }
            Shape::Not(operand) => format!("not {}", self.flat_operand(operand)),
            Shape::Leaf(leaf) => self.leaf(&leaf, None),
            Shape::Raw => self.flat_value(value, None),
        }
    }

    fn flat_operand(&self, value: &Value) -> String {
        if is_infix(value) {
            format!("({})", self.flat_predicate(value))
        } else {
            self.flat_predicate(value)
        }
    }

    /// Prints a predicate leaf on one line if `at` is `None`, or else at `at`, breaking long lists.
    fn leaf(&self, leaf: &Leaf, at: Option<At>) -> String {
        match leaf {
            Leaf::Const(value) => value.to_string(),
            Leaf::Flag(flag) => format!("flag({})", bool_operand(flag)),
            Leaf::IsEmpty(list) => {
                let at = at.map(|at| at.right("is_empty(".len()).trail(at.trail + 1));
                format!("is_empty({})", self.strings_operand(list, at))
            }
            Leaf::Field(field, string_match) => {
                let prefix = format!("{} ", field);
                let at = at.map(|at| at.right(prefix.len()));
                format!("{}{}", prefix, self.string_match(string_match, at))
            }
            Leaf::HostUnset => "host is unset".to_string(),
            Leaf::HasKey(word, key) => format!("{}({})", word, quote(key)),
            Leaf::Tag(word, tag) => {
                let prefix = format!("{}({}) ", word, quote(&tag.key));
                let at = at.map(|at| at.right(prefix.chars().count()));
                format!("{}{}", prefix, self.string_match(&tag.value, at))
            }
            Leaf::KindIn(kinds) => {
                let kinds = kinds.iter().map(|kind| kind.as_str()).collect::<Vec<_>>();
                format!("kind in [{}]", kinds.join(", "))
            }
        }
    }

    fn string_match(&self, string_match: &StringMatch, at: Option<At>) -> String {
        let list = |prefix: &str, values: &[String]| {
            let at = at.map(|at| at.right(prefix.len()));
            format!("{}{}", prefix, self.strings(values, at))
        };
        match string_match {
            StringMatch::Eq(value) => format!("== {}", quote(value)),
            StringMatch::Prefix(value) => format!("starts_with {}", quote(value)),
            StringMatch::In(values) => list("in ", values),
            StringMatch::PrefixIn(values) => list("prefix_in ", values),
            StringMatch::Matches(SetMatch { values, prefix }) => {
                let flat = format!(
                    "matches(values: {}, prefix: {})",
                    self.strings_operand(values, None),
                    bool_operand(prefix)
                );
                let Some(at) = at.filter(|at| !self.fits(*at, &flat)) else {
                    return flat;
                };
                // One argument per line, each followed by a comma.
                let inner = at.indent + INDENT;
                let values_at = At::line(inner, 1).right("values: ".len());
                format!(
                    "matches(\n{}values: {},\n{}prefix: {},\n{})",
                    pad(inner),
                    self.strings_operand(values, Some(values_at)),
                    pad(inner),
                    bool_operand(prefix),
                    pad(at.indent)
                )
            }
        }
    }

    fn strings(&self, values: &[String], at: Option<At>) -> String {
        let list = Value::Array(values.iter().cloned().map(Value::String).collect());
        match at {
            Some(at) => self.value(&list, None, at),
            None => self.flat_value(&list, None),
        }
    }

    fn strings_operand(&self, operand: &Operand<Vec<String>>, at: Option<At>) -> String {
        match operand {
            Operand::Literal(values) => self.strings(values, at),
            Operand::Config { binding, .. } => config_call(binding),
        }
    }
}

fn bool_operand(operand: &Operand<bool>) -> String {
    match operand {
        Operand::Literal(value) => value.to_string(),
        Operand::Config { binding, .. } => config_call(binding),
    }
}

fn config_call(binding: &BindingName) -> String {
    format!("config({})", quote(binding.as_str()))
}

#[derive(Clone, Copy)]
enum Junction {
    All,
    Any,
}

impl Junction {
    /// The infix word.
    fn word(self) -> &'static str {
        match self {
            Self::All => "and",
            Self::Any => "or",
        }
    }

    /// The function name, used for fewer than two operands.
    fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
        }
    }
}

/// The form a predicate value is printed in.
enum Shape<'a> {
    /// `all` or `any` of a list of operands.
    Junction(Junction, &'a [Value]),
    /// `not` of an operand.
    Not(&'a Value),
    /// A leaf whose syntax parses back to exactly this value.
    Leaf(Leaf),
    /// Anything else, printed as a raw value.
    Raw,
}

fn classify(value: &Value) -> Shape<'_> {
    if let Some(members) = value.as_object() {
        let mut iter = members.iter();
        if let (Some((name, operand)), None) = (iter.next(), iter.next()) {
            // Where a predicate is expected, `true` and `false` are constant leaves, so a junction with a bare boolean
            // as an operand is printed raw, which keeps the boolean.
            match (name.as_str(), operand) {
                ("all", Value::Array(operands)) if !operands.iter().any(Value::is_boolean) => {
                    return Shape::Junction(Junction::All, operands)
                }
                ("any", Value::Array(operands)) if !operands.iter().any(Value::is_boolean) => {
                    return Shape::Junction(Junction::Any, operands)
                }
                ("not", operand) if !operand.is_boolean() => return Shape::Not(operand),
                _ => {}
            }
        }
    }
    typed_leaf(value).map_or(Shape::Raw, Shape::Leaf)
}

/// Returns the leaf `value` is, if its leaf syntax parses back to exactly `value`.
///
/// That excludes junctions, leaves that carry resolved values, which the syntax does not write, and values that read
/// as a leaf but are not written the way the leaf writes itself, such as kinds out of declaration order.
fn typed_leaf(value: &Value) -> Option<Leaf> {
    if contains_resolved_values(value) {
        return None;
    }
    let predicate = serde_json::from_value::<Predicate>(value.clone()).ok()?;
    if serde_json::to_value(&predicate).ok()? != *value {
        return None;
    }
    Leaf::of(predicate)
}

/// A predicate leaf, a predicate other than a junction, by the form its syntax takes.
enum Leaf {
    /// `true` or `false`.
    Const(bool),
    /// `flag(<operand>)`.
    Flag(Operand<bool>),
    /// `is_empty(<operand>)`.
    IsEmpty(Operand<Vec<String>>),
    /// `<field> <string match>`, for `name`, `intake_name`, and `host`.
    Field(&'static str, StringMatch),
    /// `host is unset`.
    HostUnset,
    /// `<word>("<key>")`, for `has_tag` and `has_origin_tag`.
    HasKey(&'static str, String),
    /// `<word>("<key>") <string match>`, for `tag` and `origin_tag`.
    Tag(&'static str, TagMatch),
    /// `kind in [<kind>, ...]`.
    KindIn(KindSet),
}

impl Leaf {
    /// Returns the leaf `predicate` is, or `None` if it is a junction.
    fn of(predicate: Predicate) -> Option<Self> {
        let leaf = match predicate {
            Predicate::Const(value) => Self::Const(value),
            Predicate::Flag(flag) => Self::Flag(flag),
            Predicate::IsEmpty(list) => Self::IsEmpty(list),
            Predicate::Name(string_match) => Self::Field("name", string_match),
            Predicate::IntakeName(string_match) => Self::Field("intake_name", string_match),
            Predicate::Host(string_match) => Self::Field("host", string_match),
            Predicate::HostUnset {} => Self::HostUnset,
            Predicate::HasTag(key) => Self::HasKey("has_tag", key),
            Predicate::HasOriginTag(key) => Self::HasKey("has_origin_tag", key),
            Predicate::Tag(tag) => Self::Tag("tag", tag),
            Predicate::OriginTag(tag) => Self::Tag("origin_tag", tag),
            Predicate::KindIn(kinds) => Self::KindIn(kinds),
            Predicate::All(_) | Predicate::Any(_) | Predicate::Not(_) => return None,
        };
        Some(leaf)
    }
}

/// Returns `true` if a predicate value prints as an infix `and` or `or`, which needs parentheses as an operand.
fn is_infix(value: &Value) -> bool {
    matches!(classify(value), Shape::Junction(_, operands) if operands.len() >= 2)
}

/// Returns `true` if a `tee` node's ports are exactly numbered by its edges, so `tee { -> a -> b }` prints it.
fn is_numbered_tee(node: &ArtifactNode, edges: &[usize], all_edges: &[ArtifactEdge]) -> bool {
    let mut params = node.params.iter();
    let ports = match (params.next(), params.next()) {
        (Some((name, Value::Number(ports))), None) if name.as_str() == "ports" => ports.as_u64(),
        _ => None,
    };
    ports == Some(edges.len() as u64)
        && edges
            .iter()
            .enumerate()
            .all(|(index, &edge)| match &all_edges[edge].from {
                ArtifactPortRef::Node { port, .. } => port.as_index().map(usize::from) == Some(index),
                ArtifactPortRef::Input(_) | ArtifactPortRef::Exit(_) => false,
            })
}

/// Returns a node's parameters in schema order, then any the schema does not declare, by name.
fn ordered_params(node: &ArtifactNode) -> Vec<(&str, &Value, Option<&'static ParamType>)> {
    let specs = param_specs(node.op.as_str()).unwrap_or_default();
    let mut params = Vec::new();
    for spec in specs {
        if let Some(value) = node.params.get(spec.name) {
            params.push((spec.name, value, Some(&spec.ty)));
        }
    }
    for (name, value) in node.params.iter() {
        if !specs.iter().any(|spec| spec.name == name.as_str()) {
            params.push((name.as_str(), value, None));
        }
    }
    params
}

/// Returns an object's members in record order if `ty` is a record, then the rest by name.
fn ordered_members<'a>(members: &'a Map<String, Value>, ty: Option<&'static ParamType>) -> Vec<(&'a str, &'a Value)> {
    let specs: &[_] = match ty {
        Some(ParamType::Record(specs)) => specs,
        _ => &[],
    };
    let mut ordered = Vec::new();
    for spec in specs {
        if let Some((key, value)) = members.get_key_value(spec.name) {
            ordered.push((key.as_str(), value));
        }
    }
    let mut rest = members
        .iter()
        .filter(|(key, _)| !specs.iter().any(|spec| spec.name == key.as_str()))
        .map(|(key, value)| (key.as_str(), value))
        .collect::<Vec<_>>();
    rest.sort_by(|a, b| a.0.cmp(b.0));
    ordered.extend(rest);
    ordered
}

fn element_type(ty: Option<&'static ParamType>) -> Option<&'static ParamType> {
    match ty {
        Some(ParamType::List(element) | ParamType::Set(element)) => Some(*element),
        _ => None,
    }
}

fn member_type(ty: Option<&'static ParamType>, key: &str) -> Option<&'static ParamType> {
    match ty {
        Some(ParamType::Record(specs)) => specs.iter().find(|spec| spec.name == key).map(|spec| &spec.ty),
        Some(ParamType::StringMap(element)) => Some(*element),
        _ => None,
    }
}

/// Returns the call form of a binding, `config("<binding>")`, or of a blob reference, `blob("<hash>")`, if the object
/// is exactly one.
fn call_form(members: &Map<String, Value>) -> Option<String> {
    let mut iter = members.iter();
    let (Some((name, Value::String(value))), None) = (iter.next(), iter.next()) else {
        return None;
    };
    match name.as_str() {
        BINDING_MEMBER => BindingName::new(value.as_str())
            .ok()
            .map(|binding| config_call(&binding)),
        BLOB_MEMBER => PlanHash::from_str(value).ok().map(|hash| format!("blob(\"{}\")", hash)),
        _ => None,
    }
}

/// Returns `true` if `value` is written exactly as the lexer reads a duration literal: digits, then a unit.
fn is_duration_token(value: &str) -> bool {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && DURATION_UNITS.contains(&&value[digits..])
}

/// Returns a record or map key as written: bare if it is a word, quoted otherwise.
fn key_text(key: &str) -> String {
    let mut bytes = key.bytes();
    let bare = bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if bare {
        key.to_string()
    } else {
        quote(key)
    }
}

/// Returns a string literal for `value`, escaping quotes, backslashes, and every character that does not print.
fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            c if c.is_control() || is_invisible(c) => quoted.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

fn pad(width: usize) -> String {
    " ".repeat(width)
}
