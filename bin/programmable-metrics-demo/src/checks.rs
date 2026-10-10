//! Structural checks of an artifact against ADP's export bundle, with the plan crate's operator registry and catalog.
//!
//! These checks are what can be said about an artifact before it is composed into a plan: that it replaces the
//! exported scope, was written against the current default program, refers only to outputs, configuration bindings,
//! and scope inputs the catalog declares (with matching fingerprints), gives every node parameters its operator
//! accepts, and keeps no state (ADP does not accept stateful operators in artifacts yet). They are not validation:
//! whether the composed plan is valid (types, duplicate contributions, bounds, authority) is the validator's to say, on
//! the composed plan, so an artifact that passes them can still be rejected.
//!
//! They are the fallback for an ADP that does not serve its default program (`GET
//! /programmable-metrics/default-program`, since stage 5): with the default program in hand, compiling composes the
//! artifact into it, validates the composed plan, and diffs it against the active plan instead
//! ([`preview`](crate::preview)), which covers each of these checks with ADP's own verdict.

use std::collections::BTreeSet;

use saluki_metric_plan::{
    canonical::BINDING_MEMBER,
    ir::{ArtifactPortRef, BindingName, OutputId},
    operators::{names, ConfigureError, OperatorRegistry, StateContract},
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

    fn warning(message: String, span: Option<Span>) -> Self {
        Self {
            severity: Severity::Warning,
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

    if artifact.scope.as_str() != export.scope {
        findings.push(Finding::error(
            format!(
                "This artifact replaces the scope `{}`, but ADP exports the scope `{}`, which is the one artifacts may \
                 replace. Write `policy {} {{ ... }}`.",
                artifact.scope, export.scope, export.scope
            ),
            find(source, artifact.scope.as_str()),
        ));
    }

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

    let base = artifact.base.program_hash.to_string();
    if &artifact.base.program_hash != export.program_hash() {
        findings.push(Finding::warning(
            format!(
                "The base {} is not the current default program, {}. ADP installs the artifact but reports it as \
                 stale: the default program changed since this artifact was derived from it. Load the default program \
                 again and reapply your change to rebase it.",
                base,
                export.program_hash()
            ),
            find(source, &base),
        ));
    }

    // ADP checks the fingerprint of every output an `emit` of the artifact delivers to, and ignores the others its base
    // records: an output the artifact no longer emits to cannot be removed under it.
    let emitted = artifact
        .nodes
        .iter()
        .filter(|node| node.op.as_str() == names::EMIT)
        .filter_map(|node| node.params.get("output")?.as_str()?.parse::<OutputId>().ok())
        .collect::<BTreeSet<_>>();
    for (output, fingerprint) in artifact
        .base
        .outputs
        .iter()
        .filter(|(output, _)| emitted.contains(*output))
    {
        let span = find(source, &fingerprint.to_string()).or_else(|| find(source, output.as_str()));
        match catalog.output(output) {
            None => findings.push(Finding::error(
                format!(
                    "ADP does not declare the output `{}`. An artifact that refers to a removed output makes the plan \
                     invalid, and the current generation keeps running.",
                    output
                ),
                span,
            )),
            Some(declared) if &declared.fingerprint != fingerprint => findings.push(Finding::error(
                format!(
                    "The output `{}` has fingerprint {} in ADP, not {}: its binding changed since this artifact was \
                     derived, and ADP treats it as a removed output. Write `{} = \"{}\"` after rebasing.",
                    output, declared.fingerprint, fingerprint, output, declared.fingerprint
                ),
                span,
            )),
            Some(_) => {}
        }
    }

    let registry = OperatorRegistry::native();
    for node in &artifact.nodes {
        let node_span = parsed.spans.node(node.id.as_str());
        // A node the artifact keeps from the export as it is, operator and parameters, is the default program's own,
        // which ADP does not check for state.
        let exported = export
            .artifact
            .artifact
            .nodes
            .iter()
            .any(|exported| exported.id == node.id && exported.op == node.op && exported.params == node.params);
        // Whether the node keeps state between batches: from its contract when its parameters configure, and by its
        // operator otherwise (aggregation is the stateful operator an author is likely to write). An opaque host's
        // state is its host component's: whether an artifact may place one is the validator's to say, on the
        // composed plan.
        let stateful = match registry.configure(node.op.as_str(), &node.params) {
            Err(ConfigureError::UnknownOperator { name, known }) => {
                findings.push(Finding::error(
                    format!("`{}` is not a registered operator. The operators are: {}.", name, known),
                    node_span,
                ));
                false
            }
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
                node.op.as_str() == names::AGGREGATE
            }
            Ok(configured) => {
                let contract = configured.contract();
                let host_owned = contract.state == StateContract::Opaque && contract.boundary.is_fence();
                contract.state != StateContract::None && !host_owned
            }
        };
        if stateful && !exported {
            findings.push(Finding::error(
                format!(
                    "Node `{}` (`{}`) keeps state between batches, and ADP does not accept stateful operators in \
                     artifacts yet. Remove the node, or keep the processing in native configuration.",
                    node.id, node.op
                ),
                node_span,
            ));
        }

        if node.op.as_str() == names::EMIT {
            if let Some(Value::String(output)) = node.params.get("output") {
                let span = parsed.spans.param(node.id.as_str(), "output").or(node_span);
                // A malformed output ID is reported by the registry above.
                if let Ok(id) = output.parse::<OutputId>() {
                    match catalog.output(&id) {
                        // The check of the base's outputs above reports it.
                        None if artifact.base.outputs.contains_key(&id) => {}
                        None => findings.push(Finding::error(
                            format!(
                                "Node `{}` emits to `{}`, which ADP does not declare. Declared outputs: {}.",
                                node.id,
                                output,
                                some_names(export.output_ids.iter().map(String::as_str))
                            ),
                            span,
                        )),
                        Some(declared) if !artifact.base.outputs.contains_key(&id) => findings.push(Finding::error(
                            format!(
                                "Node `{}` emits to `{}`, which the artifact's `outputs` does not list. Add `{} = \
                                 \"{}\"` to `outputs`.",
                                node.id, output, output, declared.fingerprint
                            ),
                            span,
                        )),
                        Some(_) => {}
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

    for (index, edge) in artifact.edges.iter().enumerate() {
        if let ArtifactPortRef::Input(input) = &edge.from {
            if catalog.scope_input(&artifact.scope, input).is_none() {
                findings.push(Finding::error(
                    format!(
                        "The scope `{}` has no input `{}` in ADP's catalog. Edges leave the scope's inputs as \
                         `input.<name>`.",
                        artifact.scope, input
                    ),
                    parsed.spans.edge(index),
                ));
            }
        }
    }

    findings
}
