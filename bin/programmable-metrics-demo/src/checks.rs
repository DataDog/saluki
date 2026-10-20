//! Structural checks of an artifact against ADP's export bundle, with the plan crate's operator registry and catalog.
//!
//! These checks are what can be said about an artifact before it is composed into a plan: that it has an IR version
//! ADP accepts, refers only to outputs and configuration bindings the catalog declares and to sources the exported
//! program reads, and gives every node parameters its operator accepts. They are not validation: whether the composed plan is valid (types, duplicate contributions, bounds,
//! authority) is the validator's to say, on the composed plan, and whether ADP can run its state owners where the
//! artifact puts them is placement's, so an artifact that passes them can still be rejected.
//!
//! They are the fallback for an ADP that does not serve its default program (`GET
//! /programmable-metrics/default-program`, since stage 5): with the default program in hand, compiling composes the
//! artifact into it, validates the composed plan, and diffs it against the active plan instead
//! ([`preview`](crate::preview)), which covers each of these checks with ADP's own verdict.

use std::collections::BTreeSet;

use saluki_metric_plan::{
    canonical::BINDING_MEMBER,
    ir::{ArtifactPortRef, BindingName, OutputId},
    operators::{names, ConfigureError, OperatorRegistry},
    validate::Catalog as _,
};
use saluki_metric_plan_syntax::{Parsed, Span};
use serde_json::Value;

use crate::export::ExportBundle;

/// How serious a finding is.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// ADP would reject the artifact, or install it in a state the author probably does not want.
    Error,

    /// ADP would install the artifact, but reports something about it.
    Warning,
}

/// One finding of the structural checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Finding {
    /// How serious it is.
    pub severity: Severity,

    /// What is wrong, and how to fix it.
    pub message: String,

    /// Where in the source, when it can be placed.
    pub span: Option<Span>,
}

impl Finding {
    fn error(message: String, span: Option<Span>) -> Self {
        Self {
            severity: Severity::Error,
            message,
            span,
        }
    }
}

/// Returns the span of the first occurrence of `text` in `source`, if any.
fn find(source: &str, text: &str) -> Option<Span> {
    source.find(text).map(|start| Span::new(start, start + text.len()))
}

/// Lists up to ten names, for a message.
fn some_names<'a, I>(names: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let mut names = names.into_iter().collect::<Vec<_>>();
    names.sort_unstable();
    let more = names.len().saturating_sub(10);
    names.truncate(10);
    let mut list = names
        .iter()
        .map(|name| format!("`{}`", name))
        .collect::<Vec<_>>()
        .join(", ");
    if more > 0 {
        list.push_str(&format!(", and {} more", more));
    }
    if list.is_empty() {
        list.push_str("none");
    }
    list
}

/// Collects the binding names a parameter value refers to (`{"config": "<binding>"}` at any depth).
fn binding_references<'a>(value: &'a Value, found: &mut Vec<&'a str>) {
    match value {
        Value::Object(members) => {
            if let Some(Value::String(binding)) = members.get(BINDING_MEMBER) {
                found.push(binding);
            }
            for member in members.values() {
                binding_references(member, found);
            }
        }
        Value::Array(values) => values.iter().for_each(|value| binding_references(value, found)),
        _ => {}
    }
}

/// Checks a parsed artifact against an export bundle.
pub fn check_against_export(source: &str, parsed: &Parsed, export: &ExportBundle) -> Vec<Finding> {
    let artifact = &parsed.artifact;
    let catalog = &export.catalog;
    let mut findings = Vec::new();

    if let Some(versions) = &export.ir_versions {
        let (lowest, highest) = versions.accepted;
        if !(lowest..=highest).contains(&artifact.ir_version) {
            findings.push(Finding::error(
                format!(
                    "IR version {} is not accepted by this ADP, which accepts versions {} through {}.",
                    artifact.ir_version, lowest, highest
                ),
                find(source, "ir_version"),
            ));
        }
    }

    let registry = OperatorRegistry::native();
    for node in &artifact.nodes {
        let node_span = parsed.spans.node(node.id.as_str());
        match registry.configure(node.op.as_str(), &node.params) {
            Err(ConfigureError::UnknownOperator { name, known }) => findings.push(Finding::error(
                format!("`{}` is not a registered operator. The operators are: {}.", name, known),
                node_span,
            )),
            Err(ConfigureError::InvalidParams { operator, errors }) => {
                for error in errors {
                    findings.push(Finding::error(
                        format!(
                            "Node `{}` (`{}`): parameter `{}`: {}",
                            node.id, operator, error.path, error.message
                        ),
                        parsed.spans.param(node.id.as_str(), &error.path).or(node_span),
                    ));
                }
            }
            Ok(_) => {}
        }

        if node.op.as_str() == names::EMIT {
            if let Some(Value::String(output)) = node.params.get("output") {
                let span = parsed.spans.param(node.id.as_str(), "output").or(node_span);
                // A malformed output ID is reported by the registry above.
                if let Ok(id) = output.parse::<OutputId>() {
                    if catalog.output(&id).is_none() {
                        findings.push(Finding::error(
                            format!(
                                "Node `{}` emits to `{}`, which ADP does not declare. Declared outputs: {}.",
                                node.id,
                                output,
                                some_names(export.output_ids.iter().map(String::as_str))
                            ),
                            span,
                        ));
                    }
                }
            }
        }

        for (param, value) in node.params.iter() {
            let mut references = Vec::new();
            binding_references(value, &mut references);
            for binding in references {
                let declared = binding
                    .parse::<BindingName>()
                    .ok()
                    .is_some_and(|name| catalog.binding(&name).is_some());
                if !declared {
                    findings.push(Finding::error(
                        format!(
                            "Node `{}` refers to the configuration binding `{}`, which ADP does not declare. \
                             Declared bindings: {}.",
                            node.id,
                            binding,
                            some_names(export.binding_names.iter().map(String::as_str))
                        ),
                        parsed.spans.param(node.id.as_str(), param.as_str()).or(node_span),
                    ));
                }
            }
        }
    }

    // Without the default program at hand, the sources are those the exported program reads.
    let sources = export
        .artifact
        .artifact
        .edges
        .iter()
        .filter_map(|edge| match &edge.from {
            ArtifactPortRef::Input(input) => Some(input.as_str()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    for (index, edge) in artifact.edges.iter().enumerate() {
        if let ArtifactPortRef::Input(input) = &edge.from {
            if !sources.contains(input.as_str()) {
                findings.push(Finding::error(
                    format!(
                        "The default program has no source `{}`. Edges leave the sources as `input.<name>`: {}.",
                        input,
                        some_names(sources.iter().copied())
                    ),
                    parsed.spans.edge(index),
                ));
            }
        }
    }

    findings
}
