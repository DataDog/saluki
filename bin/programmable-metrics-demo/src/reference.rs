//! The language reference the page links to: what a program may say, built from what decides it.
//!
//! The operators, their parameters and ports, the rules an author meets, and the limits come from the plan crate's
//! registry, validator, and limits table, so they are always the ones validation applies. The scope's inputs, the
//! recipient directory, and the bindings come from ADP's export, when it is at hand, so they are the ones this ADP
//! accepts. The examples and recipes are written here, and the tests parse each one and compile the recipes into the
//! default program, so none of them shows something that does not compile. The grammar's prose is in the page.

use saluki_metric_plan::{
    ir::{format_duration, ParamType, ValueType},
    limits::{self, LimitSpec, LimitValue},
    operators::{names, OperatorRegistry},
    validate::{BindingClass, Catalog as _, Liveness, Rule, Severity},
};
use saluki_metric_plan_syntax::{parse_with, ParseOptions};
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

    /// A whole program, small, that shows every statement.
    pub example_program: &'static str,

    /// Every operator a program may use, in the registry's order, then the ones only native configuration places.
    pub operators: Vec<OperatorEntry>,

    /// The predicate forms, each with an example.
    pub predicates: Vec<PredicateEntry>,

    /// Edits to the default program for common needs.
    pub recipes: Vec<RecipeEntry>,

    /// The validation rules an author is most likely to meet.
    pub rules: Vec<RuleEntry>,

    /// The limits on a program.
    pub limits: Vec<LimitEntry>,
}

/// What this ADP accepts: the scope, its inputs, the recipient directory, and the bindings a program may read.
#[derive(Debug, Serialize)]
pub struct HostReference {
    /// The scope a program replaces.
    pub scope: String,

    /// The scope's inputs, in name order: where each source's metrics enter.
    pub inputs: Vec<String>,

    /// The recipient directory, in name order: the names `emit to [...]` takes.
    pub recipients: Vec<RecipientEntry>,

    /// The configuration bindings a program may read, in name order.
    pub bindings: Vec<BindingEntry>,

    /// Whether a program may bind facts and signals only, writing its policy settings as literals.
    pub detached: bool,

    /// Whether a program may hold at most one aggregation.
    pub single_aggregation: bool,
}

/// An entry of the recipient directory.
#[derive(Debug, Serialize)]
pub struct RecipientEntry {
    /// The name, as `emit to [...]` writes it: bare, or a quoted URL.
    pub name: String,

    /// The endpoint it delivers to.
    pub endpoint: String,

    /// The demo's label for the endpoint, such as "Team B", if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A configuration binding: `config("<name>")`.
#[derive(Debug, Serialize)]
pub struct BindingEntry {
    /// The binding's name.
    pub name: String,

    /// Its type, as the syntax writes values of it.
    #[serde(rename = "type")]
    pub ty: String,

    /// A fact, a signal, or a policy setting.
    pub class: BindingClass,

    /// Whether its value changes while ADP runs.
    pub live: bool,

    /// Whether a program may bind it: not a policy setting when programs are detached.
    pub bindable: bool,

    /// Its current value, if the export carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// An operator.
#[derive(Debug, Serialize)]
pub struct OperatorEntry {
    /// Its name, as `node <id> = <name>(...)` writes it.
    pub name: &'static str,

    /// What it does, in one sentence.
    pub summary: &'static str,

    /// Whether a program may write it; when not, only native configuration places it.
    pub authorable: bool,

    /// Its parameters, in declared order.
    pub params: Vec<ParamEntry>,

    /// An example node, if the operator may be written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<&'static str>,

    /// The example's input ports, in order: what `<node>.<port>` names as a target.
    pub inputs: Vec<String>,

    /// The example's output ports, in order: what the node's block names as `<port> -> ...`.
    pub outputs: Vec<String>,

    /// How the ports depend on the parameters, where they do.
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

    /// Whether it may be bound to configuration with `config("<name>")`.
    pub bindable: bool,
}

/// A predicate form.
#[derive(Debug, Serialize)]
pub struct PredicateEntry {
    /// What it tests.
    pub tests: &'static str,

    /// An example.
    pub example: &'static str,
}

/// An edit for a common need.
#[derive(Debug, Serialize)]
pub struct RecipeEntry {
    /// The need.
    pub title: &'static str,

    /// Where the text goes, and what it does.
    pub how: &'static str,

    /// The text, with the policy `tee`'s node as what it feeds.
    pub source: String,
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

/// The node a recipe's text feeds, as the default program names its policy `tee`.
const NEXT: &str = "metrics_policy";

/// A whole program that uses every statement: a scope input, a node with a block of edges, inline terminals, `emit to`
/// a recipient, and a comment.
pub const EXAMPLE_PROGRAM: &str = r#"policy metrics {
  # Each source's metrics enter at an input of the scope.
  input.dogstatsd -> drop_debug

  # A node: an ID, an operator, its parameters, then a block of edges.
  node drop_debug = split(name starts_with "demo.debug.") {
    true  -> drop
    false -> fanout
  }

  # A tee copies each metric to every branch.
  node fanout = tee {
    -> emit to [primary]
    -> checkout_only
  }

  node checkout_only = split(
    name starts_with "demo.checkout." and tag("env") == "prod"
  ) {
    true  -> emit to ["http://127.0.0.1:17102"]
    false -> drop
  }
}
"#;

/// An example node of each operator a program may write, in the registry's order. Edges lead to `next`, a node the
/// reader's program has.
const OPERATOR_EXAMPLES: &[(&str, &str)] = &[
    (
        names::DOGSTATSD_MAPPER,
        r#"node mapper = dogstatsd_mapper(
  profiles: [{
    name: "app",
    prefix: "app.",
    mappings: [{
      match: "app.*.requests",
      match_type: "wildcard",
      name: "app.requests",
      tags: [{ name: "service", value: "$1" }],
    }],
  }],
  cache_size: 1000,
  string_interner_bytes: 65536,
) {
  -> next
}"#,
    ),
    (
        names::NAMESPACE_PREFIX,
        r#"node prefix = namespace_prefix(namespace: "demo", exclusions: ["datadog."]) {
  -> next
}"#,
    ),
    (
        names::TIMESTAMP_SPLIT,
        r#"node by_timestamp = timestamp_split(window: 10s) {
  default     -> next
  passthrough -> drop
}"#,
    ),
    (
        names::TAG_PROJECTION,
        r#"node tags = tag_projection(
  rules: [{ metric_name: "demo.checkout.requests", action: "exclude", tags: ["pod"] }],
  value_rules: [],
  cache_capacity: 1000,
) {
  -> next
}"#,
    ),
    (
        names::AGGREGATE,
        r#"node agg = aggregate(
  window: 10s,
  flush_interval: 10s,
  context_limit: 100000,
  counter_expiry: 5m,
  flush_open_windows: false,
  histogram_aggregates: ["max", "median", "avg", "count"],
  histogram_percentiles: [95],
  copy_to_distribution: false,
  copy_to_distribution_prefix: "",
) {
  -> next
}"#,
    ),
    (names::HOST_ENRICHMENT, "node enrich = host_enrichment {\n  -> next\n}"),
    (
        names::HOST_TAGS,
        r#"node host_tags = host_tags(tags: ["env:demo"], deadline: 30s) {
  -> next
}"#,
    ),
    (
        names::SPLIT,
        r#"node checkout = split(name starts_with "demo.checkout.") {
  true  -> next
  false -> drop
}"#,
    ),
    (names::TEE, "node copies = tee {\n  -> next\n  -> emit to [primary]\n}"),
    (names::MERGE, "node joined = merge(inputs: 2) {\n  -> next\n}"),
    (names::EMIT, "node to_primary = emit(to: [\"primary\"])"),
    (names::DROP, "node discard = drop"),
];

/// How an operator's ports depend on its parameters.
const PORTS_NOTES: &[(&str, &str)] = &[
    (
        names::TEE,
        "One output port per branch, numbered from 0. Without parentheses, `tee { -> a -> b }` numbers them in order.",
    ),
    (
        names::MERGE,
        "One input port per input, named 0 to `inputs` − 1: an edge names one as `joined.0`.",
    ),
    (
        names::EMIT,
        "Usually written inline as a target: `-> emit to [primary]`, where directory names may be bare, or `-> emit \
         <output>` for an output by ID. Written as a node, its `to` is a list of strings: `emit(to: [\"primary\"])`.",
    ),
    (names::DROP, "Usually written inline as a target: `-> drop`."),
];

/// Each predicate form, with an example.
const PREDICATES: &[(&str, &str)] = &[
    ("The metric's name", r#"name == "demo.checkout.requests""#),
    ("A prefix of the name", r#"name starts_with "demo.debug.""#),
    ("The name, among several", r#"name in ["demo.a", "demo.b"]"#),
    (
        "The name, among several prefixes",
        r#"name prefix_in ["demo.a.", "demo.b."]"#,
    ),
    (
        "The name as the client sent it, before mapping and prefixing",
        r#"intake_name starts_with "app.""#,
    ),
    ("The host", r#"host == "web-1""#),
    ("No host", "host is unset"),
    ("A tag is present", r#"has_tag("env")"#),
    ("A tag's value", r#"tag("team") == "platform""#),
    (
        "A tag's value, by prefix or among several",
        r#"tag("env") in ["prod", "staging"]"#,
    ),
    ("An origin tag is present", r#"has_origin_tag("pod")"#),
    ("An origin tag's value", r#"origin_tag("pod") starts_with "web-""#),
    ("The metric's kind", "kind in [counter, gauge]"),
    (
        "A set of names or prefixes, as one parameter",
        r#"name matches(values: ["demo.a", "demo.b."], prefix: true)"#,
    ),
    (
        "A boolean signal or fact",
        r#"flag(config("multi_region_failover.metric_mirroring.enabled"))"#,
    ),
    ("An empty list", "is_empty([])"),
    ("Always, or never", "true"),
    (
        "Combinations: `not` binds tightest, then `and`, then `or`",
        r#"not has_tag("env") or (tag("env") == "prod" and name starts_with "demo.")"#,
    ),
    (
        "Any number of operands",
        r#"any(name starts_with "a.", name starts_with "b.", has_tag("keep"))"#,
    ),
];

/// Edits to the default program for common needs, each a node to put between enrichment and the policy `tee`; `{next}`
/// stands for the `tee`.
const RECIPES: &[(&str, &str, &str)] = &[
    (
        "Drop metrics by name",
        "Put the node above the policy `tee`, and point the edge that fed `metrics_policy` at it.",
        r#"node drop_debug = split(name starts_with "demo.debug.") {
  true  -> drop
  false -> {next}
}"#,
    ),
    (
        "Drop metrics by tag",
        "As above: what matches goes nowhere, everything else carries on to the policy.",
        r#"node drop_dev = split(tag("env") == "dev") {
  true  -> drop
  false -> {next}
}"#,
    ),
    (
        "Send some metrics only to the primary endpoint",
        "As above: what matches leaves for the recipient named here alone and skips the policy `tee`, so it \
         reaches no other destination. Any name of the recipient directory, or several, works the same way.",
        r#"node checkout_only = split(tag("team") == "checkout") {
  true  -> emit to [primary]
  false -> {next}
}"#,
    ),
    (
        "Remove a tag from one metric",
        "As above: a tag projection rule excludes the listed tags from the metric it names.",
        r#"node strip_pod = tag_projection(
  rules: [{ metric_name: "demo.checkout.requests", action: "exclude", tags: ["pod"] }],
  value_rules: [],
  cache_capacity: 1000,
) {
  -> {next}
}"#,
    ),
];

/// The rules an author is most likely to meet, in the validator's order, with what each asks of a program.
const RULES: &[(Rule, &str)] = &[
    (Rule::KnownOperator, "Every node names an operator listed here."),
    (
        Rule::ValidParameters,
        "Every parameter has its operator's type; required ones are given and no others.",
    ),
    (Rule::EdgeReferences, "Every edge names a node and port that exist."),
    (
        Rule::OneEdgePerPort,
        "Each input port has one edge in, each output port at most one out: use `tee` to copy and `merge` to join.",
    ),
    (
        Rule::PathsEndInEmitOrDrop,
        "Every output port has an edge, so every path ends in `emit` or `drop`.",
    ),
    (Rule::Acyclic, "No path leads back to a node it came from."),
    (Rule::Reachable, "Every node is reached from an input."),
    (
        Rule::PortTypes,
        "Each node accepts what reaches it, such as an aggregation only metrics that can be aggregated.",
    ),
    (
        Rule::DeclaredOutput,
        "Every `emit` names an output ADP declares, or recipients in the directory.",
    ),
    (
        Rule::KnownBinding,
        "Every `config(\"...\")` names a binding listed here.",
    ),
    (
        Rule::DuplicateContribution,
        "No metric reaches one recipient twice, as by two branches of a `tee` that both emit to it.",
    ),
    (Rule::SingleAggregation, "A program holds at most one `aggregate`."),
    (
        Rule::SplitWindow,
        "A `timestamp_split`'s window equals that of the aggregation it feeds.",
    ),
    (
        Rule::TrustedHost,
        "A program holds no operator that only native configuration places, such as `source`.",
    ),
    (
        Rule::PredicateCanMatch,
        "A `split`'s predicate can match something that reaches it.",
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

/// Builds the reference, with the host's side from `export` when it is at hand.
pub fn reference(export: Option<&ExportBundle>, labels: &RecipientLabels, host_missing: Option<String>) -> Reference {
    Reference {
        host: export.map(|export| host_reference(export, labels)),
        host_missing: if export.is_some() { None } else { host_missing },
        example_program: EXAMPLE_PROGRAM,
        operators: operators(),
        predicates: PREDICATES
            .iter()
            .map(|&(tests, example)| PredicateEntry { tests, example })
            .collect(),
        recipes: RECIPES
            .iter()
            .map(|&(title, how, source)| RecipeEntry {
                title,
                how,
                source: source.replace("{next}", NEXT),
            })
            .collect(),
        rules: RULES
            .iter()
            .map(|&(rule, means)| RuleEntry {
                name: rule.as_str(),
                severity: match rule.severity() {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                },
                means,
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
    let detached = catalog.detached_programs();
    let scope = export.scope.parse().ok();
    HostReference {
        scope: export.scope.clone(),
        inputs: scope
            .iter()
            .flat_map(|scope| catalog.scope_inputs(scope))
            .map(|(name, _)| name.to_string())
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
                bindable: !detached || binding.class != BindingClass::Policy,
                value: binding.value.clone(),
            })
            .collect(),
        detached,
        single_aggregation: catalog.single_aggregation(),
    }
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
            let (inputs, outputs) = example.and_then(example_ports).unwrap_or_default();
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
                        ParamEntry {
                            name: param.name,
                            ty: value_type(&ValueType::from(ty)),
                            required: param.required,
                            bindable,
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

/// Wraps an operator's example node in a program that feeds it from an input and gives it the `next` node it leads to:
/// the program the tests parse and the ports are read from.
pub(crate) fn example_wrapper(example: &str) -> String {
    let id = example
        .strip_prefix("node ")
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default();
    let input = if example.contains("= merge(") {
        format!("  input.dogstatsd -> {id}.0\n  input.otlp -> {id}.1\n")
    } else {
        format!("  input.dogstatsd -> {id}\n")
    };
    let body = example.replace('\n', "\n  ");
    format!(
        "policy metrics {{\n  base \"sha256:{zeros}\"\n{input}\n  {body}\n\n  node next = tee {{\n    -> drop\n  }}\n}}\n",
        zeros = "0".repeat(64),
    )
}

/// Returns the input and output ports of the node an example defines, as its operator gives them for the example's
/// parameters.
pub(crate) fn example_ports(example: &str) -> Option<(Vec<String>, Vec<String>)> {
    let parsed = parse_with(&example_wrapper(example), &ParseOptions::default()).ok()?;
    let id = example.strip_prefix("node ")?.split_whitespace().next()?;
    let node = parsed.artifact.nodes.iter().find(|node| node.id.as_str() == id)?;
    let configured = OperatorRegistry::native()
        .configure(node.op.as_str(), &node.params)
        .ok()?;
    let ports = configured.ports();
    Some((
        ports.inputs.iter().map(|port| port.name.to_string()).collect(),
        ports.outputs.iter().map(|port| port.name.to_string()).collect(),
    ))
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
        ValueType::Bindable { inner } => format!("{}, or config(\"...\")", value_type(inner)),
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
