//! The language reference the page links to: what a program may say, built from what decides it.
//!
//! The operators, their parameters (with their defaults, and which one may be passed first without its name) and ports,
//! the rules an author meets, and the limits come from the plan crate's registry, validator, and limits table, so they
//! are always the ones validation applies. The sources, the recipient directory, and the bindings come from
//! ADP's export, when it is at hand, so they are the ones this ADP accepts. The node IDs the example program's stages
//! get come from parsing it, so they are the ones the syntax derives. The examples, the shorthand and what each stands
//! for, and the recipes are written here, and the tests parse each one, lower each shorthand and its long form to the
//! same graph, and compile the recipes into the default program, so none of them shows something that does not
//! compile. The grammar's prose is in the page.

use std::collections::BTreeSet;

use saluki_metric_plan::{
    ir::{format_duration, ArtifactPortRef, ParamType, ValueType},
    limits::{self, LimitSpec, LimitValue},
    operators::{names, OperatorRegistry},
    validate::{BindingClass, Liveness, Rule, Severity},
};
use saluki_metric_plan_syntax::parse;
use serde::Serialize;
use serde_json::Value;

use crate::{compile::RecipientLabels, export::ExportBundle};

/// The reference, as the page renders it.
#[derive(Debug, Serialize)]
pub struct Reference {
    /// The host's side of the reference, if ADP's export is at hand: what this ADP accepts.
    pub host: Option<HostReference>,

    /// Why the host's side is missing, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_missing: Option<String>,

    /// A whole program, small, that shows each part of the language.
    pub example_program: &'static str,

    /// The nodes the example program describes, in the order parsing writes them, with the IDs the syntax gives them;
    /// empty if it does not parse.
    pub example_nodes: Vec<ExampleNode>,

    /// Each piece of shorthand, beside the longer form it stands for.
    pub sugar: Vec<SugarEntry>,

    /// Every operator a program may use, in the registry's order, then the ones only native configuration places.
    pub operators: Vec<OperatorEntry>,

    /// The predicate forms, each with an example.
    pub predicates: Vec<PredicateEntry>,

    /// One-line edits to the default program for common needs.
    pub recipes: Vec<RecipeEntry>,

    /// The validation rules an author is most likely to meet.
    pub rules: Vec<RuleEntry>,

    /// The limits on a program.
    pub limits: Vec<LimitEntry>,
}

/// What this ADP accepts: its sources, the recipient directory, and the bindings a program may read.
#[derive(Debug, Serialize)]
pub struct HostReference {
    /// The sources the default program reads, in name order: where each source's metrics enter, the streams a
    /// pipeline may start from.
    pub inputs: Vec<String>,

    /// The recipient directory, in name order: the names `emit` takes.
    pub recipients: Vec<RecipientEntry>,

    /// The configuration bindings a program may read, in name order.
    pub bindings: Vec<BindingEntry>,
}

/// An entry of the recipient directory.
#[derive(Debug, Serialize)]
pub struct RecipientEntry {
    /// The name, as `emit` writes it: bare, or a quoted URL.
    pub name: String,

    /// The endpoint it delivers to.
    pub endpoint: String,

    /// The demo's label for the endpoint, such as `Team B`, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A configuration binding: `config.<name>`.
#[derive(Debug, Serialize)]
pub struct BindingEntry {
    /// The binding's name.
    pub name: String,

    /// Its type, as the syntax writes values of it.
    #[serde(rename = "type")]
    pub ty: String,

    /// A fact or a signal.
    pub class: BindingClass,

    /// Whether its value changes while ADP runs.
    pub live: bool,

    /// Its current value, if the export carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// A node of the example program.
#[derive(Debug, Serialize)]
pub struct ExampleNode {
    /// Its ID: the one its `as` clause gives, or the one the syntax derives.
    pub id: String,

    /// Its operator.
    pub op: String,
}

/// A piece of shorthand, and the longer form it is one rewrite into.
#[derive(Debug, Serialize)]
pub struct SugarEntry {
    /// The shorthand, as it is usually written.
    pub form: &'static str,

    /// An example of it.
    pub sugar: &'static str,

    /// The same, in the longer form.
    pub core: &'static str,

    /// What the rewrite does.
    pub means: &'static str,
}

/// An operator.
#[derive(Debug, Serialize)]
pub struct OperatorEntry {
    /// Its name, as a stage writes it.
    pub name: &'static str,

    /// What it does, in one sentence.
    pub summary: &'static str,

    /// Whether a program may write it; when not, only native configuration places it.
    pub authorable: bool,

    /// Its parameters, in declared order.
    pub params: Vec<ParamEntry>,

    /// An example pipeline that uses it, if the operator may be written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<&'static str>,

    /// The example's input ports, in order.
    pub inputs: Vec<String>,

    /// The example's output ports, in order: what `<name>.<port>` picks when there are several.
    pub outputs: Vec<String>,

    /// How the operator is usually written, and how its ports depend on its parameters, where they do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports_note: Option<&'static str>,
}

/// A parameter of an operator.
#[derive(Debug, Serialize)]
pub struct ParamEntry {
    /// Its name.
    pub name: &'static str,

    /// Its type, as the syntax writes values of it.
    #[serde(rename = "type")]
    pub ty: String,

    /// Whether it must be given.
    pub required: bool,

    /// Whether it may be bound to configuration with `config.<name>`.
    pub bindable: bool,

    /// Whether it may be passed first, without its name.
    pub positional: bool,

    /// The value it takes when it is left out, as the syntax writes it, if it has a default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// A predicate form.
#[derive(Debug, Serialize)]
pub struct PredicateEntry {
    /// What it tests.
    pub tests: &'static str,

    /// An example.
    pub example: &'static str,
}

/// A one-line edit for a common need.
#[derive(Debug, Serialize)]
pub struct RecipeEntry {
    /// The need.
    pub title: &'static str,

    /// What the line does.
    pub how: &'static str,

    /// The stage the line adds.
    pub stage: &'static str,

    /// The line, as it goes into the default program: a stage on its own line, at the end of the stream every
    /// destination reads, `let metrics = ...`.
    pub line: String,

    /// The line of the default program just above where the line goes.
    pub above: String,

    /// The line of the default program just below where the line goes, past any blank lines.
    pub below: String,
}

/// A validation rule.
#[derive(Debug, Serialize)]
pub struct RuleEntry {
    /// The rule's name, as diagnostics show it.
    pub name: &'static str,

    /// `error` refuses the program; `warning` lets it through.
    pub severity: &'static str,

    /// What it asks of a program, in an author's terms.
    pub means: &'static str,

    /// Whether text that parses already meets it, so that only an artifact written as JSON can break it.
    pub met_by_syntax: bool,
}

/// A limit on a program.
#[derive(Debug, Serialize)]
pub struct LimitEntry {
    /// The limit's name.
    pub name: &'static str,

    /// Its value.
    pub value: String,

    /// What it bounds.
    pub description: &'static str,
}

/// Where a recipe's line goes when the default program is not at hand: the end of a `let metrics` that ends in host
/// tags, above its first use.
const RECIPE_CONTEXT: (&str, &str) = (
    "    | host_tags(config.host.tags, deadline: config.host.tags_deadline)",
    "  metrics | emit primary",
);

/// A whole program that shows each part of the language: sources, a `let`, stages, `where`, streams used more
/// than once, a tuple, sinks, and comments.
pub const EXAMPLE_PROGRAM: &str = r#"policy {
  # A `let` names a stream: here, DogStatsD's metrics, filtered, then aggregated.
  let dsd = dogstatsd
    | where not (name starts_with "demo.debug.")
    | aggregate(window: 10s, flush_interval: 10s)

  # A stream used more than once is copied to each use: `dsd` feeds this pipeline and the tuple below.
  dsd | emit output dsd_client_telemetry

  # A tuple merges streams.
  let metrics = (dsd, otlp) | host_tags(["env:demo"], deadline: 30s)

  # Each use of `metrics` is a branch that gets every metric, and each ends in a sink.
  metrics | emit primary
  metrics
    | where name starts_with "demo.checkout." and tags.env == "prod"
    | emit "http://127.0.0.1:17102"
}
"#;

/// Each piece of shorthand: its name, an example, the same in the longer form it stands for, and what the rewrite does.
/// Each example and its longer form describe the same graph, with the same node IDs; the `as` clauses make the IDs
/// line up.
const SUGAR: &[(&str, &str, &str, &str)] = &[
    (
        "`e | where p`",
        "dogstatsd\n  | where name starts_with \"demo.\" as keep\n  | emit primary",
        "let keep = dogstatsd | split(name starts_with \"demo.\")\nkeep.false | drop\nkeep.true | emit primary",
        "A `split` on `p` whose `false` port leads to `drop`: the pipeline carries on from its `true` port.",
    ),
    (
        "`a != b`",
        "dogstatsd\n  | where tags.env != \"dev\" as kept\n  | emit primary",
        "dogstatsd\n  | where not (tags.env == \"dev\") as kept\n  | emit primary",
        "`not (a == b)`, inside a predicate.",
    ),
    (
        "`tags.k is unset`",
        "dogstatsd\n  | where tags.env is unset as untagged\n  | emit primary",
        "dogstatsd\n  | where not (tags.env is set) as untagged\n  | emit primary",
        "`not (tags.k is set)`, inside a predicate. `host is unset`, `origin_tags.k is unset` likewise.",
    ),
];

/// An example pipeline of each operator a program may write, in the registry's order, starting from the sources and
/// ending in sinks.
const OPERATOR_EXAMPLES: &[(&str, &str)] = &[
    (
        names::DOGSTATSD_MAPPER,
        r#"dogstatsd
  | dogstatsd_mapper(profiles: [{
      name: "app",
      prefix: "app.",
      mappings: [{ match: "app.*.requests", match_type: "wildcard", name: "app.requests", tags: [{ name: "service", value: "$1" }] }],
    }])
  | emit primary"#,
    ),
    (
        names::NAMESPACE_PREFIX,
        r#"dogstatsd
  | namespace_prefix("demo", exclusions: ["datadog."])
  | emit primary"#,
    ),
    (
        names::TIMESTAMP_SPLIT,
        r#"let by_time = dogstatsd | timestamp_split(10s)
let aggregated = by_time.default | aggregate(window: 10s, flush_interval: 10s)
(aggregated, by_time.passthrough) | emit primary"#,
    ),
    (
        names::TAG_PROJECTION,
        r#"dogstatsd
  | tag_projection(rules: [{ metric_name: "demo.checkout.requests", action: "exclude", tags: ["pod"] }])
  | emit primary"#,
    ),
    (
        names::AGGREGATE,
        r#"dogstatsd
  | aggregate(window: 10s, flush_interval: 10s)
  | emit primary"#,
    ),
    (names::HOST_ENRICHMENT, "otlp | host_enrichment | emit primary"),
    (
        names::HOST_TAGS,
        r#"otlp
  | host_tags(["env:demo"], deadline: 30s)
  | emit primary"#,
    ),
    (
        names::SPLIT,
        r#"let checkout = dogstatsd | split(name starts_with "demo.checkout.")
checkout.true | emit primary
checkout.false | emit mrf"#,
    ),
    (
        names::TEE,
        r#"let copies = dogstatsd | tee(ports: 2)
copies.0 | emit primary
copies.1 | emit mrf"#,
    ),
    (names::MERGE, "(dogstatsd, otlp) | emit primary"),
    (names::EMIT, "dogstatsd | emit [primary, \"http://127.0.0.1:17102\"]"),
    (names::DROP, "dogstatsd | drop"),
];

/// How an operator is usually written, and how its ports depend on its parameters.
const PORTS_NOTES: &[(&str, &str)] = &[
    (
        names::TIMESTAMP_SPLIT,
        "Two output ports: name the stream with `let`, then read `<name>.default` and `<name>.passthrough`.",
    ),
    (
        names::SPLIT,
        "Usually written as `where p`, which drops what does not match. With both ports used, name the stream with \
         `let`, then read `<name>.true` and `<name>.false`.",
    ),
    (
        names::TEE,
        "Usually implied: a stream used more than once is copied to each use. Written as a stage, it has one output \
         port per copy, numbered from 0: `<name>.0`, `<name>.1`.",
    ),
    (
        names::MERGE,
        "Written as a tuple: `(a, b, ...)` merges the streams, `a` into port 0, `b` into port 1, and so on.",
    ),
    (
        names::EMIT,
        "Written as a sink: `emit <recipient>` or `emit [<recipient>, ...]` for names of the recipient directory, \
         bare or a quoted URL, or `emit output <output>` for an output ADP declares, by its ID.",
    ),
    (names::DROP, "Written as a sink: `| drop`."),
];

/// Each predicate form, with an example.
const PREDICATES: &[(&str, &str)] = &[
    ("The metric's name", r#"name == "demo.checkout.requests""#),
    ("Not that name", r#"name != "demo.checkout.requests""#),
    ("A prefix of the name", r#"name starts_with "demo.debug.""#),
    ("The name, among several", r#"name in ["demo.a", "demo.b"]"#),
    (
        "The name, among several prefixes",
        r#"name starts_with ["demo.a.", "demo.b."]"#,
    ),
    (
        "The name, among a list a binding holds (`starts_with` takes one too)",
        "name in config.host.tags",
    ),
    (
        "The name as the client sent it, before mapping and prefixing",
        r#"intake_name starts_with "app.""#,
    ),
    ("The host", r#"host == "web-1""#),
    ("No host, or a host", "host is unset"),
    ("A tag is present (`is unset`: absent)", "tags.env is set"),
    (
        "A tag whose key is not an identifier",
        r#"tags["app.kubernetes.io/name"] is set"#,
    ),
    ("A tag's value", r#"tags.team == "platform""#),
    ("A tag's value, among several", r#"tags.env in ["prod", "staging"]"#),
    ("An origin tag is present", "origin_tags.pod is set"),
    ("An origin tag's value", r#"origin_tags.pod starts_with "web-""#),
    ("The metric's kind", "kind in [counter, gauge]"),
    (
        "A boolean signal or fact, by its binding",
        "config.multi_region_failover.metric_mirroring.enabled",
    ),
    ("An empty list", "is_empty(config.host.tags)"),
    ("Always, or never", "true"),
    (
        "Combinations: `not` binds tightest, then `and`, then `or`",
        r#"tags.env is unset or (tags.env == "prod" and name starts_with "demo.")"#,
    ),
    (
        "A set of names or prefixes, in the long form (prints as `in` or `starts_with`)",
        r#"name matches(values: ["demo.a", "demo.b"], prefix: false)"#,
    ),
];

/// One-line edits to the default program for common needs: a title, what the line does, and the stage it appends to
/// the stream every destination reads, `let metrics = ...`.
const RECIPES: &[(&str, &str, &str)] = &[
    (
        "Drop metrics by name",
        "Keeps what does not match, so the `demo.debug.*` metrics reach no destination.",
        r#"where not (name starts_with "demo.debug.")"#,
    ),
    (
        "Drop metrics by tag",
        "Keeps every metric but those tagged `env:dev`, including the ones without an `env` tag.",
        r#"where tags.env != "dev""#,
    ),
    (
        "Drop metrics without a host",
        "Keeps only the metrics that carry a host.",
        "where host is set",
    ),
    (
        "Remove a tag from one metric",
        "A tag projection rule removes the listed tags from the metric it names, before every destination.",
        r#"tag_projection(rules: [{ metric_name: "demo.checkout.requests", action: "exclude", tags: ["pod"] }])"#,
    ),
];

/// The rules an author is most likely to meet, in the validator's order, with what each asks of a program and whether
/// the syntax already meets it for any text that parses.
const RULES: &[(Rule, &str, bool)] = &[
    (Rule::KnownOperator, "Every stage names an operator listed here.", false),
    (
        Rule::ValidParameters,
        "Every parameter has its operator's type; required ones are given and no others.",
        false,
    ),
    (
        Rule::EdgeReferences,
        "Every edge names a node and port that exist.",
        false,
    ),
    (
        Rule::OneEdgePerPort,
        "Each input port has one edge in, each output port at most one out. A pipeline joins one stage to the next, a \
         tuple merges, and using a stream twice copies it, so text always does.",
        true,
    ),
    (
        Rule::PathsEndInEmitOrDrop,
        "Every path ends in `emit` or `drop`. Every pipeline that is no `let` ends in a sink, and every stream and \
         port is used, so text always does.",
        true,
    ),
    (
        Rule::Acyclic,
        "No path leads back to a node it came from. A name is defined before it is used, so text cannot loop.",
        true,
    ),
    (Rule::Reachable, "Every node is reached from an input.", false),
    (
        Rule::PortTypes,
        "Each node accepts what reaches it, such as an aggregation only metrics that can be aggregated.",
        false,
    ),
    (
        Rule::DeclaredOutput,
        "Every `emit output` names an output ADP declares, and every `emit` recipients of the directory.",
        false,
    ),
    (
        Rule::KnownBinding,
        "Every `config.<name>` names a binding listed here.",
        false,
    ),
    (
        Rule::DuplicateContribution,
        "No metric reaches one recipient twice, as by two uses of a stream that both emit to it.",
        false,
    ),
    (
        Rule::SplitWindow,
        "A `timestamp_split`'s window equals that of the aggregation it feeds.",
        false,
    ),
    (
        Rule::TrustedHost,
        "A program holds no operator that only native configuration places, such as `source`.",
        false,
    ),
    (
        Rule::PredicateCanMatch,
        "A `where`'s or `split`'s predicate can match something that reaches it.",
        false,
    ),
    (
        Rule::TelemetryContract,
        "Every node ADP reads the Agent's series from by name is in the program, by that name and operator, such as \
         the `aggregate` named `dsd_agg`; otherwise a warning names the series that will be absent.",
        false,
    ),
];

/// The limits an author can reach.
const LIMITS: &[LimitSpec] = &[
    limits::ARTIFACT_NODES,
    limits::PREDICATE_LEAVES,
    limits::TEE_FAN_OUT,
    limits::WORK_PER_INPUT,
    limits::ARTIFACT_SIZE,
    limits::ARTIFACT_MEMORY,
    limits::MAX_IMPLIED_OUTPUTS,
    limits::BUCKET_WIDTHS,
];

/// Returns a recipe's stage as a line of the default program, at the indentation of a stage of `let metrics`.
pub(crate) fn recipe_line(stage: &str) -> String {
    format!("    | {}", stage)
}

/// Returns the lines of the default program around where a recipe's line goes, the one above it and the first one
/// below it that is not blank: in the default program `export` prints when it is at hand, else in a typical one.
fn recipe_context(export: Option<&ExportBundle>, labels: &RecipientLabels) -> (String, String) {
    const MARK: &str = "where true";
    let found = export.and_then(|export| {
        let printed = crate::compile::default_program_source(export, labels);
        let edited = crate::scenario::edits::append_to_metrics(&printed, MARK).ok()?;
        let lines = edited.lines().collect::<Vec<_>>();
        let at = lines.iter().position(|line| *line == recipe_line(MARK))?;
        let above = at.checked_sub(1).map(|above| lines[above])?;
        let below = lines[at + 1..].iter().find(|line| !line.trim().is_empty())?;
        Some((above.to_string(), below.to_string()))
    });
    found.unwrap_or_else(|| (RECIPE_CONTEXT.0.to_string(), RECIPE_CONTEXT.1.to_string()))
}

/// Builds the reference, with the host's side from `export` when it is at hand.
pub fn reference(export: Option<&ExportBundle>, labels: &RecipientLabels, host_missing: Option<String>) -> Reference {
    Reference {
        host: export.map(|export| host_reference(export, labels)),
        host_missing: if export.is_some() { None } else { host_missing },
        example_program: EXAMPLE_PROGRAM,
        example_nodes: example_nodes(),
        sugar: SUGAR
            .iter()
            .map(|&(form, sugar, core, means)| SugarEntry {
                form,
                sugar,
                core,
                means,
            })
            .collect(),
        operators: operators(),
        predicates: PREDICATES
            .iter()
            .map(|&(tests, example)| PredicateEntry { tests, example })
            .collect(),
        recipes: {
            let (above, below) = recipe_context(export, labels);
            RECIPES
                .iter()
                .map(|&(title, how, stage)| RecipeEntry {
                    title,
                    how,
                    stage,
                    line: recipe_line(stage),
                    above: above.clone(),
                    below: below.clone(),
                })
                .collect()
        },
        rules: RULES
            .iter()
            .map(|&(rule, means, met_by_syntax)| RuleEntry {
                name: rule.as_str(),
                severity: match rule.severity() {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                },
                means,
                met_by_syntax,
            })
            .collect(),
        limits: LIMITS
            .iter()
            .map(|spec| LimitEntry {
                name: spec.name,
                value: limit_value(spec.default),
                description: spec.description,
            })
            .collect(),
    }
}

fn host_reference(export: &ExportBundle, labels: &RecipientLabels) -> HostReference {
    let catalog = &export.catalog;
    HostReference {
        // The sources the exported program reads.
        inputs: export
            .artifact
            .artifact
            .edges
            .iter()
            .filter_map(|edge| match &edge.from {
                ArtifactPortRef::Input(input) => Some(input.to_string()),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        recipients: catalog
            .recipients()
            .map(|(name, recipient)| {
                let name = name.to_string();
                RecipientEntry {
                    label: labels.of(&recipient.endpoint).map(str::to_string),
                    name: if name.contains("://") {
                        format!("\"{}\"", name)
                    } else {
                        name
                    },
                    endpoint: recipient.endpoint.clone(),
                }
            })
            .collect(),
        bindings: catalog
            .bindings()
            .map(|(name, binding)| BindingEntry {
                name: name.to_string(),
                ty: value_type(&binding.ty),
                class: binding.class,
                live: binding.liveness == Liveness::Live,
                value: binding.value.clone(),
            })
            .collect(),
    }
}

/// Returns the example program's nodes, as parsing it names them, or none if it does not parse.
fn example_nodes() -> Vec<ExampleNode> {
    parse(EXAMPLE_PROGRAM)
        .map(|parsed| {
            parsed
                .artifact
                .nodes
                .iter()
                .map(|node| ExampleNode {
                    id: node.id.to_string(),
                    op: node.op.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn operators() -> Vec<OperatorEntry> {
    let registry = OperatorRegistry::native();
    let mut entries = registry
        .iter()
        .map(|def| {
            let example = OPERATOR_EXAMPLES
                .iter()
                .find(|(name, _)| *name == def.name())
                .map(|(_, example)| *example);
            let (inputs, outputs) = example
                .and_then(|example| example_ports(def.name(), example))
                .unwrap_or_default();
            OperatorEntry {
                name: def.name(),
                summary: def.summary(),
                authorable: def.authorable(),
                params: def
                    .params()
                    .iter()
                    .map(|param| {
                        let (ty, bindable) = match &param.ty {
                            ParamType::Bindable(inner) => (*inner, true),
                            ty => (ty, false),
                        };
                        let ty = ValueType::from(ty);
                        ParamEntry {
                            name: param.name,
                            ty: value_type(&ty),
                            required: param.required,
                            bindable,
                            positional: param.positional,
                            default: param
                                .default
                                .and_then(|json| serde_json::from_str::<Value>(json).ok())
                                .map(|value| syntax_value(&value, &ty)),
                        }
                    })
                    .collect(),
                example,
                inputs,
                outputs,
                ports_note: PORTS_NOTES
                    .iter()
                    .find(|(name, _)| *name == def.name())
                    .map(|(_, note)| *note),
            }
        })
        .collect::<Vec<_>>();
    // The operators a program may write first, then the host's.
    entries.sort_by_key(|entry| !entry.authorable);
    entries
}

/// Wraps an example (statements, as they appear in a program's body) in a program, for parsing alone.
pub(crate) fn example_wrapper(example: &str) -> String {
    let body = example.replace('\n', "\n  ");
    format!("policy {{\n  {body}\n}}\n")
}

/// Returns the input and output ports of the first node of operator `op` that an example describes, as the operator
/// gives them for the example's parameters.
pub(crate) fn example_ports(op: &str, example: &str) -> Option<(Vec<String>, Vec<String>)> {
    let parsed = parse(&example_wrapper(example)).ok()?;
    let node = parsed.artifact.nodes.iter().find(|node| node.op.as_str() == op)?;
    let configured = OperatorRegistry::native()
        .configure(node.op.as_str(), &node.params)
        .ok()?;
    let ports = configured.ports();
    Some((
        ports.inputs.iter().map(|port| port.name.to_string()).collect(),
        ports.outputs.iter().map(|port| port.name.to_string()).collect(),
    ))
}

/// Writes a parameter's default value, given as JSON, as the syntax writes values of its type: durations bare, lists
/// and records with spaces after their commas.
fn syntax_value(value: &Value, ty: &ValueType) -> String {
    let element = |ty: &ValueType| match ty {
        ValueType::List { element } | ValueType::Set { element } | ValueType::StringMap { element } => {
            Some((**element).clone())
        }
        ValueType::Bindable { inner } => Some((**inner).clone()),
        _ => None,
    };
    match (value, ty) {
        (_, ValueType::Bindable { inner }) => syntax_value(value, inner),
        (Value::String(duration), ValueType::Duration) => duration.clone(),
        (Value::Array(values), _) => {
            let element = element(ty).unwrap_or(ValueType::String);
            format!(
                "[{}]",
                values
                    .iter()
                    .map(|value| syntax_value(value, &element))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        (Value::Object(members), _) if members.is_empty() => "{}".to_string(),
        (Value::Object(members), _) => format!(
            "{{ {} }}",
            members
                .iter()
                .map(|(name, value)| {
                    let member = match ty {
                        ValueType::Record { members } => members
                            .iter()
                            .find(|member| member.name == name.as_str())
                            .map(|member| member.ty.clone()),
                        _ => element(ty),
                    };
                    format!(
                        "{}: {}",
                        name,
                        syntax_value(value, &member.unwrap_or(ValueType::String))
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        (value, _) => value.to_string(),
    }
}

/// Writes a type as the syntax writes its values.
fn value_type(ty: &ValueType) -> String {
    match ty {
        ValueType::Bool => "true | false".to_string(),
        ValueType::Integer { min, max } if *min == 0 && *max >= MAX_SAFE => "integer ≥ 0".to_string(),
        ValueType::Integer { min, max } if *max >= MAX_SAFE => format!("integer ≥ {}", min),
        ValueType::Integer { min, max } => format!("integer {}..{}", min, max),
        ValueType::Decimal => "decimal, as a string: \"0.25\"".to_string(),
        ValueType::String => "string".to_string(),
        ValueType::Duration => "duration: 10s, 5m".to_string(),
        ValueType::Enum { choices } => choices
            .iter()
            .map(|choice| format!("\"{}\"", choice))
            .collect::<Vec<_>>()
            .join(" | "),
        ValueType::List { element } => format!("[{}, ...]", value_type(element)),
        ValueType::Set { element } => format!("[{}, ...], distinct", value_type(element)),
        ValueType::StringMap { element } => format!("{{ <name>: {}, ... }}", value_type(element)),
        ValueType::Predicate => "predicate".to_string(),
        ValueType::OutputRef => "output ID".to_string(),
        ValueType::CredentialRef => "credential name".to_string(),
        ValueType::Bindable { inner } => format!("{}, or config.<name>", value_type(inner)),
        ValueType::Record { members } => format!(
            "{{ {} }}",
            members
                .iter()
                .map(|member| format!(
                    "{}{}: {}",
                    member.name,
                    if member.required { "" } else { "?" },
                    value_type(&member.ty)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ValueType::PortType => "port type".to_string(),
    }
}

/// The largest integer a parameter's range can name, above which a range is unbounded to an author.
const MAX_SAFE: i64 = (1 << 53) - 1;

fn limit_value(value: LimitValue) -> String {
    match value {
        LimitValue::Count(count) => count.to_string(),
        LimitValue::Bytes(bytes) if bytes % (1024 * 1024) == 0 => format!("{} MiB", bytes / (1024 * 1024)),
        LimitValue::Bytes(bytes) if bytes % 1024 == 0 => format!("{} KiB", bytes / 1024),
        LimitValue::Bytes(bytes) => format!("{} bytes", bytes),
        LimitValue::Duration(duration) => format_duration(duration).unwrap_or_else(|| format!("{:?}", duration)),
        LimitValue::Durations(durations) => durations
            .iter()
            .map(|duration| format_duration(*duration).unwrap_or_else(|| format!("{:?}", duration)))
            .collect::<Vec<_>>()
            .join(", "),
        LimitValue::Ratio(ratio) => ratio.to_string(),
    }
}

#[cfg(test)]
mod tests;
