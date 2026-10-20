//! Formatting: a [syntax tree](crate::ast) to text, breaking values and predicates to fit the line width.
//!
//! Formatting is a function of the tree alone, never of its spans, and each form is written only when parsing it back
//! gives exactly the tree that was formatted. A predicate has one spelling: the sugar the grammar defines where it
//! parses back to the same value (`!=`, `is unset`, `in config.x`), the core form where it does not, and the raw JSON
//! shape of the value where the grammar has no predicate form for it (a junction of fewer than two operands, a bare
//! boolean operand, a literal flag, or a leaf that carries a resolved value).

use std::str::FromStr as _;

use saluki_metric_plan::{
    canonical::{contains_resolved_values, PlanHash, BINDING_MEMBER},
    ir::{is_identifier, BindingName, Operand, ParamType, BLOB_MEMBER},
    model::KindSet,
    operators::OperatorRegistry,
    predicate::{Predicate, SetMatch, StringMatch, TagMatch},
};
use serde_json::{Map, Value};

use crate::ast::{Head, Name, OpStage, Pipeline, Program, Ref, Sink, SinkKind, Stage, Statement, Tuple, WhereStage};

#[cfg(test)]
mod tests;

/// The indentation of one level.
const INDENT: usize = 2;

/// The units a duration literal ends in.
const DURATION_UNITS: [&str; 6] = ["h", "m", "s", "ms", "us", "ns"];

/// The words the grammar reserves where it expects them, which a bare recipient name is not written as. These include
/// the retired `fork`, which parsing still recognizes to say what replaced it.
const KEYWORDS: [&str; 8] = ["let", "as", "where", "fork", "emit", "output", "drop", "exit"];

/// The words that an operator of the same name is written with parentheses to be told apart from: those that start a
/// stage other than an operator or that parsing rejects there (the retired `fork`), and `as`, which names nodes.
const STAGE_WORDS: [&str; 6] = ["where", "fork", "emit", "drop", "exit", "as"];

/// Writes `program` as text, breaking lines beyond `width` columns. `usize::MAX` writes every value on one line.
///
/// The text depends only on the tree, never on its spans.
pub(crate) fn format(program: &Program, width: usize) -> String {
    let printer = Printer { width };
    let mut sections = Vec::new();

    let mut header = Vec::new();
    if let Some(version) = program.ir_version {
        header.push(format!("{}ir_version {}", pad(INDENT), version));
    }
    if !header.is_empty() {
        sections.push(header.join("\n"));
    }

    // The blob table is a header, as the grammar has it, so it comes before the statements.
    if let Some(blobs) = &program.blobs {
        let mut text = format!("{}blobs {{\n", pad(INDENT));
        for (hash, value) in blobs {
            let key = format!("{} = ", quote(hash));
            let at = At::line(2 * INDENT, 1).right(key.chars().count());
            text.push_str(&format!(
                "{}{}{},\n",
                pad(2 * INDENT),
                key,
                printer.value(value, None, at)
            ));
        }
        text.push_str(&format!("{}}}", pad(INDENT)));
        sections.push(text);
    }

    for statement in &program.statements {
        sections.push(printer.statement(statement));
    }

    if sections.is_empty() {
        "policy {\n}\n".to_string()
    } else {
        format!("policy {{\n{}\n}}\n", sections.join("\n\n"))
    }
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

/// An argument of an operator stage: its name, if it is written with one, its value, and the value's declared type.
struct Argument<'a> {
    name: Option<&'a str>,
    value: &'a Value,
    ty: Option<&'static ParamType>,
}

struct Printer {
    width: usize,
}

impl Printer {
    fn fits(&self, at: At, text: &str) -> bool {
        !text.contains('\n') && at.col + text.chars().count() + at.trail <= self.width
    }

    // Statements.

    fn statement(&self, statement: &Statement) -> String {
        match statement {
            Statement::Let { name, pipeline, .. } => self.pipeline(&format!("let {} = ", name.id), pipeline),
            Statement::Run { pipeline, .. } => self.pipeline("", pipeline),
        }
    }

    /// Prints a statement's pipeline after `prefix`, on one line if it has at most two stages and fits, and otherwise
    /// with the head on the first line and each stage on a line of its own.
    fn pipeline(&self, prefix: &str, pipeline: &Pipeline) -> String {
        let indent = INDENT;
        let stages = pipeline.stages.len() + usize::from(pipeline.end.is_some());
        if stages <= 2 {
            let mut flat = format!("{}{}{}", pad(indent), prefix, self.flat_head(&pipeline.head));
            for stage in &pipeline.stages {
                flat.push_str(&format!(" | {}", self.flat_stage(stage)));
            }
            if let Some(sink) = &pipeline.end {
                flat.push_str(&format!(" | {}", sink_text(sink)));
            }
            if self.fits(At::line(0, 0), &flat) {
                return flat;
            }
        }

        let mut text = format!(
            "{}{}{}",
            pad(indent),
            prefix,
            self.head(&pipeline.head, At::line(indent, 0).right(prefix.len()))
        );
        let bar = indent + INDENT;
        for stage in &pipeline.stages {
            text.push_str(&format!(
                "\n{}| {}",
                pad(bar),
                self.stage(stage, bar + INDENT, bar + 2 * INDENT)
            ));
        }
        if let Some(sink) = &pipeline.end {
            text.push_str(&format!("\n{}| {}", pad(bar), sink_text(sink)));
        }
        text
    }

    fn flat_head(&self, head: &Head) -> String {
        match head {
            Head::Ref(reference) => ref_text(reference),
            Head::Tuple(tuple) => self.flat_tuple(tuple),
        }
    }

    /// Prints a head at `at`, breaking a tuple to one item per line if it does not fit.
    fn head(&self, head: &Head, at: At) -> String {
        match head {
            Head::Ref(reference) => ref_text(reference),
            Head::Tuple(tuple) => {
                let flat = self.flat_tuple(tuple);
                if self.fits(at, &flat) {
                    return flat;
                }
                let mut text = "(\n".to_string();
                for item in &tuple.items {
                    text.push_str(&format!("{}{},\n", pad(at.indent + INDENT), ref_text(item)));
                }
                text.push_str(&format!("{}){}", pad(at.indent), as_clause(&tuple.id)));
                text
            }
        }
    }

    fn flat_tuple(&self, tuple: &Tuple) -> String {
        let items = tuple.items.iter().map(ref_text).collect::<Vec<_>>();
        format!("({}){}", items.join(", "), as_clause(&tuple.id))
    }

    // Stages.

    fn flat_stage(&self, stage: &Stage) -> String {
        match stage {
            Stage::Op(op) => {
                let args = self.arguments(op);
                let as_ = as_clause(&op.id);
                if args.is_empty() {
                    // An operator named like a sink or `where` takes parentheses, which make it an operator.
                    let parens = if STAGE_WORDS.contains(&op.op.as_str()) {
                        "()"
                    } else {
                        ""
                    };
                    return format!("{}{}{}", op.op, parens, as_);
                }
                let args = args.iter().map(|arg| self.flat_argument(arg)).collect::<Vec<_>>();
                format!("{}({}){}", op.op, args.join(", "), as_)
            }
            Stage::Where(stage) => format!(
                "where {}{}",
                self.flat_predicate(&stage.predicate),
                as_clause(&stage.id)
            ),
        }
    }

    /// Prints a stage whose text starts at column `lead`, on one line if it fits. A broken `where` continues its
    /// junction's lines at `junction`.
    fn stage(&self, stage: &Stage, lead: usize, junction: usize) -> String {
        let flat = self.flat_stage(stage);
        if self.fits(At::line(lead, 0), &flat) {
            return flat;
        }
        match stage {
            // Without arguments there is nothing to break.
            Stage::Op(op) if op.params.is_empty() => flat,
            Stage::Op(op) => self.broken_op(op, lead),
            Stage::Where(stage) => self.broken_where(stage, lead, junction),
        }
    }

    /// Prints an operator stage with each argument on a line of its own.
    fn broken_op(&self, op: &OpStage, lead: usize) -> String {
        let inner = lead + INDENT;
        let mut text = format!("{}(\n", op.op);
        let args = self.arguments(op);
        // A lone positional argument, such as a `split`'s predicate, takes no comma after it.
        let comma = if matches!(args.as_slice(), [Argument { name: None, .. }]) {
            ""
        } else {
            ","
        };
        for arg in args {
            match arg.name {
                Some(name) => {
                    let at = At::line(inner, 1).right(name.len() + 2);
                    text.push_str(&format!(
                        "{}{}: {},\n",
                        pad(inner),
                        name,
                        self.value(arg.value, arg.ty, at)
                    ));
                }
                None => {
                    let trail = comma.len();
                    text.push_str(&format!(
                        "{}{}{}\n",
                        pad(inner),
                        self.value(arg.value, arg.ty, At::line(inner, trail)),
                        comma
                    ));
                }
            }
        }
        text.push_str(&format!("{}){}", pad(lead), as_clause(&op.id)));
        text
    }

    /// Prints a `where` stage with its predicate broken, its continuation lines indented under the stage.
    fn broken_where(&self, stage: &WhereStage, lead: usize, junction: usize) -> String {
        let as_ = as_clause(&stage.id);
        if matches!(classify(&stage.predicate), Shape::Junction(..)) {
            // The lines of a junction continue at `junction`, deeper than the stage, and an `as` takes a line of its
            // own after them, so that it names the stage rather than the last operand.
            let at = At {
                indent: junction,
                col: lead + "where ".len(),
                trail: 0,
            };
            let predicate = self.predicate(&stage.predicate, at);
            return match &stage.id {
                Some(id) => format!("where {}\n{}as {}", predicate, pad(junction), id.id),
                None => format!("where {}", predicate),
            };
        }
        // The closing bracket of a list, a record, or a set match aligns with the stage.
        let at = At {
            indent: lead,
            col: lead + "where ".len(),
            trail: as_.len(),
        };
        format!("where {}{}", self.predicate(&stage.predicate, at), as_)
    }

    /// Returns a stage's arguments: the positional one first, without its name, then the others in the order of the
    /// operator's schema, then any the schema does not declare, by name.
    fn arguments<'a>(&self, op: &'a OpStage) -> Vec<Argument<'a>> {
        let specs = OperatorRegistry::native()
            .get(op.op.as_str())
            .map(|def| def.params())
            .unwrap_or_default();
        let ty_of = |name: &str| specs.iter().find(|spec| spec.name == name).map(|spec| &spec.ty);

        let mut args = Vec::new();
        let positional = op
            .positional
            .as_ref()
            .and_then(|name| op.params.get(name.as_str()).map(|value| (name.as_str(), value)));
        if let Some((name, value)) = positional {
            args.push(Argument {
                name: None,
                value,
                ty: ty_of(name),
            });
        }
        let written = |name: &str| positional.is_some_and(|(positional, _)| positional == name);
        for spec in specs {
            if written(spec.name) {
                continue;
            }
            if let Some(value) = op.params.get(spec.name) {
                args.push(Argument {
                    name: Some(spec.name),
                    value,
                    ty: Some(&spec.ty),
                });
            }
        }
        for (name, value) in op.params.iter() {
            if !written(name.as_str()) && !specs.iter().any(|spec| spec.name == name.as_str()) {
                args.push(Argument {
                    name: Some(name.as_str()),
                    value,
                    ty: None,
                });
            }
        }
        args
    }

    fn flat_argument(&self, arg: &Argument<'_>) -> String {
        match arg.name {
            Some(name) => format!("{}: {}", name, self.flat_value(arg.value, arg.ty)),
            None => self.flat_value(arg.value, arg.ty),
        }
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
            Shape::Junction(junction, operands) => {
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
            Shape::Not(operand) => format!("not {}", self.operand(operand, at.right(4))),
            Shape::Leaf(leaf) => self.leaf(&leaf, Some(at)),
            Shape::Raw => raw_binding(value).unwrap_or_else(|| self.value(value, None, at)),
        }
    }

    /// Prints an operand of `and`, `or`, or `not`, in parentheses if precedence or the tree's shape needs them.
    fn operand(&self, value: &Value, at: At) -> String {
        if !parenthesizes(value) {
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
            Shape::Junction(junction, operands) => operands
                .iter()
                .map(|operand| self.flat_operand(operand))
                .collect::<Vec<_>>()
                .join(&format!(" {} ", junction.word())),
            Shape::Not(operand) => format!("not {}", self.flat_operand(operand)),
            Shape::Leaf(leaf) => self.leaf(&leaf, None),
            Shape::Raw => raw_binding(value).unwrap_or_else(|| self.flat_value(value, None)),
        }
    }

    fn flat_operand(&self, value: &Value) -> String {
        if parenthesizes(value) {
            format!("({})", self.flat_predicate(value))
        } else {
            self.flat_predicate(value)
        }
    }

    /// Prints a predicate leaf on one line if `at` is `None`, or else at `at`, breaking long lists.
    fn leaf(&self, leaf: &Leaf, at: Option<At>) -> String {
        match leaf {
            Leaf::Const(value) => value.to_string(),
            Leaf::Flag(binding) => config_ref(binding),
            Leaf::IsEmpty(list) => {
                let at = at.map(|at| at.right("is_empty(".len()).trail(at.trail + 1));
                format!("is_empty({})", self.strings_operand(list, at))
            }
            Leaf::Field(field, string_match, negated) => {
                let prefix = format!("{} ", field);
                let at = at.map(|at| at.right(prefix.len()));
                format!("{}{}", prefix, self.string_match(string_match, *negated, at))
            }
            Leaf::HostUnset(negated) => format!("host is {}", if *negated { "set" } else { "unset" }),
            Leaf::HasKey(word, key, negated) => {
                format!("{} is {}", tag_key(word, key), if *negated { "unset" } else { "set" })
            }
            Leaf::Tag(word, tag, negated) => {
                let prefix = format!("{} ", tag_key(word, &tag.key));
                let at = at.map(|at| at.right(prefix.chars().count()));
                format!("{}{}", prefix, self.string_match(&tag.value, *negated, at))
            }
            Leaf::KindIn(kinds) => {
                let kinds = kinds.iter().map(|kind| kind.as_str()).collect::<Vec<_>>();
                format!("kind in [{}]", kinds.join(", "))
            }
        }
    }

    fn string_match(&self, string_match: &StringMatch, negated: bool, at: Option<At>) -> String {
        let list = |prefix: &str, values: &[String]| {
            let at = at.map(|at| at.right(prefix.len()));
            format!("{}{}", prefix, self.strings(values, at))
        };
        match string_match {
            StringMatch::Eq(value) => format!("{} {}", if negated { "!=" } else { "==" }, quote(value)),
            StringMatch::Prefix(value) => format!("starts_with {}", quote(value)),
            StringMatch::In(values) => list("in ", values),
            StringMatch::PrefixIn(values) => list("starts_with ", values),
            StringMatch::Matches(SetMatch {
                values: Operand::Config { binding, .. },
                prefix: Operand::Literal(prefix),
            }) => format!("{} {}", if *prefix { "starts_with" } else { "in" }, config_ref(binding)),
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
            Operand::Config { binding, .. } => config_ref(binding),
        }
    }
}

// Pieces of statements.

fn ref_text(reference: &Ref) -> String {
    match &reference.port {
        Some((port, _)) => format!("{}.{}", reference.name.id, port),
        None => reference.name.id.to_string(),
    }
}

/// Returns ` as <id>` for a node ID the author wrote, and nothing otherwise.
fn as_clause(id: &Option<Name>) -> String {
    match id {
        Some(name) => format!(" as {}", name.id),
        None => String::new(),
    }
}

fn sink_text(sink: &Sink) -> String {
    let text = match &sink.kind {
        SinkKind::Recipients { names, .. } => match names.as_slice() {
            [name] => format!("emit {}", recipient(name)),
            names => {
                let names = names.iter().map(|name| recipient(name)).collect::<Vec<_>>();
                format!("emit [{}]", names.join(", "))
            }
        },
        SinkKind::Output { output, .. } => format!("emit output {}", output),
        SinkKind::Drop => "drop".to_string(),
    };
    format!("{}{}", text, as_clause(&sink.id))
}

/// Returns a recipient as written: bare if it is an identifier that is not a keyword, quoted otherwise.
fn recipient(name: &str) -> String {
    if is_identifier(name) && !KEYWORDS.contains(&name) {
        name.to_string()
    } else {
        quote(name)
    }
}

// Pieces of values and predicates.

fn bool_operand(operand: &Operand<bool>) -> String {
    match operand {
        Operand::Literal(value) => value.to_string(),
        Operand::Config { binding, .. } => config_ref(binding),
    }
}

/// Returns a configuration binding as written, `config.<binding>`.
fn config_ref(binding: &BindingName) -> String {
    format!("config.{}", binding.as_str())
}

/// Returns a tag leaf's subject: `tags.<key>`, or `tags["<key>"]` when the key is not an identifier.
fn tag_key(word: &str, key: &str) -> String {
    if is_identifier(key) {
        format!("{}.{}", word, key)
    } else {
        format!("{}[{}]", word, quote(key))
    }
}

#[derive(Clone, Copy, PartialEq)]
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
}

/// Returns `true` if `value` needs parentheses as an operand of `and`, `or`, or `not`: it is a junction. Under `not`
/// one always does, as does an `or` under an `and`; an `and` under an `and`, or an `or` under an `or`, would merge with
/// its parent when parsed back; and an `and` under an `or`, which precedence keeps apart, is clearer to read with them.
fn parenthesizes(value: &Value) -> bool {
    matches!(classify(value), Shape::Junction(..))
}

/// The form a predicate value is printed in.
enum Shape<'a> {
    /// `all` or `any` of at least two operands, none of them a bare boolean.
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
            // as an operand is printed raw, which keeps the boolean. A junction of fewer than two operands has no
            // infix form, and `all(...)` and `any(...)` are not written, so it is raw too.
            match (name.as_str(), operand) {
                ("all", Value::Array(operands)) if operands.len() >= 2 && !operands.iter().any(Value::is_boolean) => {
                    return Shape::Junction(Junction::All, operands)
                }
                ("any", Value::Array(operands)) if operands.len() >= 2 && !operands.iter().any(Value::is_boolean) => {
                    return Shape::Junction(Junction::Any, operands)
                }
                ("not", operand) if !operand.is_boolean() => {
                    // `not` of a leaf that has a negated spelling is that spelling.
                    return match typed_leaf(operand).and_then(Leaf::negated) {
                        Some(leaf) => Shape::Leaf(leaf),
                        None => Shape::Not(operand),
                    };
                }
                _ => {}
            }
        }
    }
    typed_leaf(value).map_or(Shape::Raw, Shape::Leaf)
}

/// Returns the leaf `value` is, if its leaf syntax parses back to exactly `value`.
///
/// That excludes junctions, leaves that carry resolved values, which the syntax does not write, a flag that is a
/// literal, which has no spelling that is not the constant leaf, and values that read as a leaf but are not written the
/// way the leaf writes itself, such as kinds out of declaration order.
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

/// A predicate leaf, a predicate other than a junction or a `not`, by the form its syntax takes.
enum Leaf {
    /// `true` or `false`.
    Const(bool),
    /// `config.<binding>`: a flag is written only when it is bound.
    Flag(BindingName),
    /// `is_empty(<operand>)`.
    IsEmpty(Operand<Vec<String>>),
    /// `<field> <string match>`, for `name`, `intake_name`, and `host`; `!=` when negated, which only `==` has.
    Field(&'static str, StringMatch, bool),
    /// `host is unset`, or `host is set` when negated.
    HostUnset(bool),
    /// `<word>.<key> is set`, for `tags` and `origin_tags`, or `is unset` when negated.
    HasKey(&'static str, String, bool),
    /// `<word>.<key> <string match>`, for `tags` and `origin_tags`; `!=` when negated, which only `==` has.
    Tag(&'static str, TagMatch, bool),
    /// `kind in [<kind>, ...]`.
    KindIn(KindSet),
}

impl Leaf {
    /// Returns the leaf `predicate` is, or `None` if it is a junction, a `not`, or has no leaf syntax.
    fn of(predicate: Predicate) -> Option<Self> {
        let leaf = match predicate {
            Predicate::Const(value) => Self::Const(value),
            Predicate::Flag(Operand::Config { binding, .. }) => Self::Flag(binding),
            Predicate::Flag(Operand::Literal(_)) => return None,
            Predicate::IsEmpty(list) => Self::IsEmpty(list),
            Predicate::Name(string_match) => Self::Field("name", string_match, false),
            Predicate::IntakeName(string_match) => Self::Field("intake_name", string_match, false),
            Predicate::Host(string_match) => Self::Field("host", string_match, false),
            Predicate::HostUnset {} => Self::HostUnset(false),
            Predicate::HasTag(key) => Self::HasKey("tags", key, false),
            Predicate::HasOriginTag(key) => Self::HasKey("origin_tags", key, false),
            Predicate::Tag(tag) => Self::Tag("tags", tag, false),
            Predicate::OriginTag(tag) => Self::Tag("origin_tags", tag, false),
            Predicate::KindIn(kinds) => Self::KindIn(kinds),
            Predicate::All(_) | Predicate::Any(_) | Predicate::Not(_) => return None,
        };
        Some(leaf)
    }

    /// Returns the leaf that is `not` of this one, if the syntax has a spelling for it: `!=`, `is unset`, `is set`.
    fn negated(self) -> Option<Self> {
        match self {
            Self::Field(field, string_match @ StringMatch::Eq(_), false) => {
                Some(Self::Field(field, string_match, true))
            }
            Self::HostUnset(false) => Some(Self::HostUnset(true)),
            Self::HasKey(word, key, false) => Some(Self::HasKey(word, key, true)),
            Self::Tag(
                word,
                TagMatch {
                    key,
                    value: value @ StringMatch::Eq(_),
                },
                false,
            ) => Some(Self::Tag(word, TagMatch { key, value }, true)),
            _ => None,
        }
    }
}

// Pieces of values.

/// Returns a parameter type with any configuration binding unwrapped: the type its literal values have.
fn literal_type(ty: Option<&'static ParamType>) -> Option<&'static ParamType> {
    match ty {
        Some(ParamType::Bindable(inner)) => Some(*inner),
        ty => ty,
    }
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

/// Returns the call form of a binding, `config.<binding>`, or of a blob reference, `blob("<hash>")`, if the object is
/// exactly one.
fn call_form(members: &Map<String, Value>) -> Option<String> {
    let mut iter = members.iter();
    let (Some((name, Value::String(value))), None) = (iter.next(), iter.next()) else {
        return None;
    };
    match name.as_str() {
        BINDING_MEMBER => BindingName::new(value.as_str())
            .ok()
            .map(|binding| config_ref(&binding)),
        BLOB_MEMBER => PlanHash::from_str(value).ok().map(|hash| format!("blob(\"{}\")", hash)),
        _ => None,
    }
}

/// Returns the record form of a binding written raw where a predicate is expected, `{ config: "<binding>" }`: there,
/// `config.<binding>` is a flag, which parses to a different value.
fn raw_binding(value: &Value) -> Option<String> {
    let members = value.as_object()?;
    match (members.get(BINDING_MEMBER), members.len()) {
        (Some(Value::String(binding)), 1) if call_form(members).is_some() => {
            Some(format!("{{ {}: {} }}", key_text(BINDING_MEMBER), quote(binding)))
        }
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

/// Returns `true` for the characters that format text without showing: the bidirectional controls, the zero-width
/// characters, the line and paragraph separators, and the soft hyphen.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{00AD}'
    )
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
