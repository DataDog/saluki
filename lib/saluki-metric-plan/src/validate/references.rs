//! References to what the catalog declares: outputs and configuration bindings.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::{
    catalog::{Catalog, Liveness},
    graph::{emit_output, Graph},
    types::describe,
    Finding, Location, Rule, ValidationReport,
};
use crate::{
    canonical::to_canonical_string,
    ir::{BindingName, Operand, ParamType, ParamValue, Plan, ValueType},
    operators::{names, ConfiguredOperator, EMIT_TO},
    predicate::{Predicate, StringMatch},
};

/// The type of a predicate's boolean operands, such as a `flag`.
const PREDICATE_FLAG: ParamType = ParamType::Bool;

/// The type of a predicate's list operands, such as the values of a `matches`.
const PREDICATE_LIST: ParamType = ParamType::List(&ParamType::String);

pub(super) fn check(graph: &Graph<'_>, plan: &Plan, catalog: &dyn Catalog, report: &mut ValidationReport) {
    check_outputs(graph, plan, catalog, report);
    check_bindings(graph, catalog, report);
}

fn check_outputs(graph: &Graph<'_>, plan: &Plan, catalog: &dyn Catalog, report: &mut ValidationReport) {
    let mut declared = BTreeSet::new();
    for output in &plan.outputs {
        let at = Location::Output(output.id.clone());
        if !declared.insert(&output.id) {
            report.push(Finding::new(
                Rule::UniqueOutputIds,
                at,
                "another output already has this ID; declare each output once",
            ));
            continue;
        }

        let Some(known) = catalog.output(&output.id) else {
            report.push(Finding::new(
                Rule::CatalogOutput,
                at,
                "this output is not declared by the host: plans can only deliver to outputs that native configuration \
                 or an approved resources artifact declares",
            ));
            continue;
        };
        let mut differences = Vec::new();
        if output.accepts != known.accepts {
            differences.push(format!(
                "it accepts {}, but the host's output accepts {}",
                describe(&output.accepts),
                describe(&known.accepts)
            ));
        }
        let recipients = output.recipients.iter().collect::<BTreeSet<_>>();
        let known_recipients = known.recipients.iter().collect::<BTreeSet<_>>();
        if recipients != known_recipients {
            differences.push("its recipients differ from the host's".to_string());
        }
        if !differences.is_empty() {
            report.push(Finding::new(
                Rule::CatalogOutput,
                at,
                format!(
                    "this output does not match the host's declaration: {}; the output's binding changed since \
                     the plan was written, so write the plan against the current one",
                    differences.join("; ")
                ),
            ));
        }
    }

    let mut emitted = BTreeSet::new();
    for node in 0..graph.nodes.len() {
        // An `emit` addressed to recipients is an artifact's: composition resolves it into its output.
        if graph
            .op(node)
            .is_some_and(|op| op.name() == names::EMIT && op.params().get(EMIT_TO).is_some())
        {
            report.push(Finding::new(
                Rule::DeclaredOutput,
                graph.location(node),
                "this `emit` names recipients, which a plan cannot: composing an artifact resolves the recipients an \
                 `emit` names into the output whose recipients they are, so every `emit` of a plan names its output",
            ));
            continue;
        }
        let Some(output) = graph.op(node).and_then(emit_output) else {
            continue;
        };
        emitted.insert(output);
        if !declared.contains(output) {
            report.push(Finding::new(
                Rule::DeclaredOutput,
                graph.location(node),
                format!(
                    "this `emit` delivers to `{}`, which the plan does not declare in `outputs`",
                    output
                ),
            ));
        }
    }
    for output in &plan.outputs {
        if !emitted.contains(&output.id) {
            report.push(Finding::new(
                Rule::UnusedOutput,
                Location::Output(output.id.clone()),
                "no `emit` delivers to this output",
            ));
        }
    }
}

/// One use of a configuration binding in a node's parameters.
pub(crate) struct BindingUse<'a> {
    /// The binding.
    pub binding: &'a BindingName,

    /// The type the binding must supply where it is used.
    pub expected: ParamType,

    /// The binding's resolved value, if the plan carries one.
    pub resolved: Option<Value>,

    /// Where in the parameters the binding is, such as `predicate.intake_name.values`.
    pub path: String,
}

/// Returns every configuration binding in a configured operator's parameters, in parameter schema order.
pub(crate) fn binding_uses(op: &ConfiguredOperator) -> Vec<BindingUse<'_>> {
    let mut uses = Vec::new();
    for spec in op.def().params() {
        if let Some(value) = op.params().get(spec.name) {
            visit_bindings(&spec.ty, value, spec.name.to_string(), &mut uses);
        }
    }
    uses
}

fn check_bindings(graph: &Graph<'_>, catalog: &dyn Catalog, report: &mut ValidationReport) {
    let mut resolved_values: BTreeMap<&BindingName, (String, String)> = BTreeMap::new();
    for node in 0..graph.nodes.len() {
        let Some(op) = graph.op(node) else { continue };
        for found in binding_uses(op) {
            let at = graph.location(node);
            match catalog.binding(found.binding) {
                None => report.push(Finding::new(
                    Rule::KnownBinding,
                    at.clone(),
                    format!(
                        "parameter `{}` is bound to `{}`, which is not a configuration binding the host declares",
                        found.path, found.binding
                    ),
                )),
                Some(known) => {
                    if !ValueType::from(&found.expected).admits(&known.ty) {
                        report.push(Finding::new(
                            Rule::BindingType,
                            at.clone(),
                            format!(
                                "parameter `{}` needs a {}, but the binding `{}` supplies a {}",
                                found.path, found.expected, found.binding, known.ty
                            ),
                        ));
                    }
                    if known.liveness == Liveness::Live && op.contract().boundary.is_host() {
                        report.push(Finding::new(
                            Rule::LiveBindingOnHost,
                            at.clone(),
                            format!(
                                "parameter `{}` is bound to the live binding `{}`, but `{}` runs in a host \
                                 component, which applies changes only at restart",
                                found.path,
                                found.binding,
                                op.name()
                            ),
                        ));
                    }
                }
            }

            let Some(resolved) = found
                .resolved
                .as_ref()
                .and_then(|value| to_canonical_string(value).ok())
            else {
                continue;
            };
            let place = format!("parameter `{}` of node `{}`", found.path, graph.id(node));
            match resolved_values.get(found.binding) {
                Some((first, first_place)) if *first != resolved => report.push(Finding::new(
                    Rule::ConsistentBinding,
                    at,
                    format!(
                        "the binding `{}` resolves to {} in {}, but to {} in {}; every use of a binding has the same \
                         value",
                        found.binding, resolved, place, first, first_place
                    ),
                )),
                Some(_) => {}
                None => {
                    resolved_values.insert(found.binding, (resolved, place));
                }
            }
        }
    }
}

/// Collects every configuration binding in a parameter value of type `ty`, with the type each binding must supply.
fn visit_bindings<'a>(ty: &ParamType, value: &'a ParamValue, path: String, uses: &mut Vec<BindingUse<'a>>) {
    match (ty, value) {
        (ParamType::Bindable(inner), ParamValue::Config { binding, resolved }) => uses.push(BindingUse {
            binding,
            expected: **inner,
            resolved: resolved.as_deref().map(ParamValue::encode),
            path,
        }),
        (ParamType::Bindable(inner), value) => visit_bindings(inner, value, path, uses),
        (ParamType::List(element) | ParamType::Set(element), ParamValue::List(values) | ParamValue::Set(values)) => {
            for (i, value) in values.iter().enumerate() {
                visit_bindings(element, value, format!("{}[{}]", path, i), uses);
            }
        }
        (ParamType::StringMap(element), ParamValue::StringMap(members)) => {
            for (key, value) in members {
                visit_bindings(element, value, format!("{}.{}", path, key), uses);
            }
        }
        (ParamType::Record(specs), ParamValue::Record(members)) => {
            for spec in specs.iter() {
                if let Some(value) = members.get(spec.name) {
                    visit_bindings(&spec.ty, value, format!("{}.{}", path, spec.name), uses);
                }
            }
        }
        (ParamType::Predicate, ParamValue::Predicate(predicate)) => visit_predicate(predicate, path, uses),
        _ => {}
    }
}

fn visit_operand<'a, T>(operand: &'a Operand<T>, expected: ParamType, path: String, uses: &mut Vec<BindingUse<'a>>)
where
    T: serde::Serialize,
{
    if let Operand::Config { binding, resolved } = operand {
        uses.push(BindingUse {
            binding,
            expected,
            resolved: resolved.as_ref().and_then(|value| serde_json::to_value(value).ok()),
            path,
        });
    }
}

fn visit_string_match<'a>(matcher: &'a StringMatch, path: String, uses: &mut Vec<BindingUse<'a>>) {
    if let StringMatch::Matches(set) = matcher {
        visit_operand(&set.values, PREDICATE_LIST, format!("{}.values", path), uses);
        visit_operand(&set.prefix, PREDICATE_FLAG, format!("{}.prefix", path), uses);
    }
}

fn visit_predicate<'a>(predicate: &'a Predicate, path: String, uses: &mut Vec<BindingUse<'a>>) {
    match predicate {
        Predicate::Flag(flag) => visit_operand(flag, PREDICATE_FLAG, format!("{}.flag", path), uses),
        Predicate::IsEmpty(list) => visit_operand(list, PREDICATE_LIST, format!("{}.is_empty", path), uses),
        Predicate::Name(matcher) => visit_string_match(matcher, format!("{}.name", path), uses),
        Predicate::IntakeName(matcher) => visit_string_match(matcher, format!("{}.intake_name", path), uses),
        Predicate::Host(matcher) => visit_string_match(matcher, format!("{}.host", path), uses),
        Predicate::Tag(tag) => visit_string_match(&tag.value, format!("{}.tag", path), uses),
        Predicate::OriginTag(tag) => visit_string_match(&tag.value, format!("{}.origin_tag", path), uses),
        Predicate::All(operands) | Predicate::Any(operands) => {
            let junction = if matches!(predicate, Predicate::All(_)) {
                "all"
            } else {
                "any"
            };
            for (i, operand) in operands.iter().enumerate() {
                visit_predicate(operand, format!("{}.{}[{}]", path, junction, i), uses);
            }
        }
        Predicate::Not(operand) => visit_predicate(operand, format!("{}.not", path), uses),
        Predicate::Const(_)
        | Predicate::HostUnset {}
        | Predicate::HasTag(_)
        | Predicate::HasOriginTag(_)
        | Predicate::KindIn(_) => {}
    }
}
