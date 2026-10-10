//! References to what the catalog declares: outputs, configuration bindings, and scope inputs.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::{
    catalog::{Catalog, Liveness},
    graph::{emit_output, Graph},
    types::{describe, Types},
    Finding, Location, Rule, ValidationReport,
};
use crate::{
    canonical::to_canonical_string,
    ir::{BindingName, InputName, Operand, ParamType, ParamValue, Plan, PortRef, ScopeName, ValueType},
    operators::ConfiguredOperator,
    predicate::{Predicate, StringMatch},
};

/// The type of a predicate's boolean operands, such as a `flag`.
const PREDICATE_FLAG: ParamType = ParamType::Bool;

/// The type of a predicate's list operands, such as the values of a `matches`.
const PREDICATE_LIST: ParamType = ParamType::List(&ParamType::String);

pub(super) fn check(
    graph: &Graph<'_>, types: &Types, plan: &Plan, catalog: &dyn Catalog, report: &mut ValidationReport,
) {
    check_outputs(graph, plan, catalog, report);
    check_bindings(graph, catalog, report);
    check_scopes(graph, types, plan, catalog, report);
    check_scope_closure(graph, plan, report);
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
        if output.fingerprint != known.fingerprint {
            differences.push(format!(
                "its fingerprint is {}, but the host's binding has fingerprint {}",
                output.fingerprint, known.fingerprint
            ));
        }
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
                    if known.liveness == Liveness::Live && op.contract().boundary.is_fence() {
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

/// Checks that every scope is closed: its region, every node reachable from its inputs without entering one of its
/// exits, is fed from outside only through its inputs; every exit is fed from inside the region and leads out of it;
/// and no node is in the regions of two scopes.
pub(super) fn check_scope_closure(graph: &Graph<'_>, plan: &Plan, report: &mut ValidationReport) {
    let mut owner: BTreeMap<usize, &ScopeName> = BTreeMap::new();
    for (scope, declaration) in &plan.scopes {
        let at = || Location::Scope(scope.clone());
        let inputs = declaration
            .inputs
            .values()
            .filter_map(|port| graph.output_port(&port.node, &port.port))
            .collect::<BTreeSet<_>>();
        let mut exits = BTreeMap::new();
        for (name, port) in &declaration.exits {
            match graph.input_port(&port.node, &port.port) {
                Some(endpoint) => {
                    exits.insert(endpoint, name);
                }
                None => report.push(Finding::new(
                    Rule::ScopeClosed,
                    at(),
                    format!(
                        "exit `{}` refers to `{}`, which is not an input port of the plan",
                        name, port
                    ),
                )),
            }
        }

        // The region: every node reachable from the inputs without entering an exit.
        let mut region = BTreeSet::new();
        let mut fed_exits = BTreeSet::new();
        let mut pending = inputs
            .iter()
            .filter_map(|&(node, port)| graph.target(node, port))
            .collect::<Vec<_>>();
        while let Some(endpoint) = pending.pop() {
            if exits.contains_key(&endpoint) {
                fed_exits.insert(endpoint);
                continue;
            }
            let node = endpoint.0;
            if !region.insert(node) {
                continue;
            }
            let outputs = graph.op(node).map_or(0, |op| op.ports().outputs.len());
            pending.extend((0..outputs).filter_map(|port| graph.target(node, port)));
        }

        for &node in &region {
            let input_ports = graph.op(node).map_or(0, |op| op.ports().inputs.len());
            for port in 0..input_ports {
                let Some(from) = graph.source(node, port) else { continue };
                if region.contains(&from.0) || inputs.contains(&from) {
                    continue;
                }
                report.push(Finding::new(
                    Rule::ScopeClosed,
                    at(),
                    format!(
                        "`{}.{}` is inside the scope, but is fed from `{}.{}`, outside it; feed the scope only \
                         through its inputs, or declare that port as one of them",
                        graph.id(node),
                        graph.input_name((node, port)),
                        graph.id(from.0),
                        graph.output_name(from)
                    ),
                ));
            }
            if let Some(other) = owner.insert(node, scope) {
                report.push(Finding::new(
                    Rule::ScopeClosed,
                    at(),
                    format!(
                        "node `{}` is in both this scope and scope `{}`; scopes must not overlap",
                        graph.id(node),
                        other
                    ),
                ));
            }
        }

        for (endpoint, name) in &exits {
            let port = format!("{}.{}", graph.id(endpoint.0), graph.input_name(*endpoint));
            if region.contains(&endpoint.0) {
                report.push(Finding::new(
                    Rule::ScopeClosed,
                    at(),
                    format!(
                        "exit `{}` enters `{}`, which is inside the scope; an exit leads out of its scope",
                        name, port
                    ),
                ));
            } else if !fed_exits.contains(endpoint) {
                report.push(Finding::new(
                    Rule::ScopeClosed,
                    at(),
                    format!(
                        "exit `{}` enters `{}`, which nothing inside the scope feeds; connect the exit to the scope, \
                         or remove it",
                        name, port
                    ),
                ));
            }
        }
    }
}

/// Checks that every scope input names an output port of the plan, without the catalog.
///
/// [`check`] reports the same finding while it checks scope inputs against the catalog. Validation of a rewritten plan
/// runs this alone: a rewrite never changes a plan's scopes, but it removes nodes, and must not remove one a scope's
/// input names. (Scope closure, which also runs there, reports exits that name no input port.)
pub(super) fn check_scope_input_ports(graph: &Graph<'_>, plan: &Plan, report: &mut ValidationReport) {
    for (scope, declaration) in &plan.scopes {
        for (input, port) in &declaration.inputs {
            if graph.output_port(&port.node, &port.port).is_none() {
                report.push(missing_scope_input(scope, input, port));
            }
        }
    }
}

fn missing_scope_input(scope: &ScopeName, input: &InputName, port: &PortRef) -> Finding {
    Finding::new(
        Rule::ScopeInputs,
        Location::Scope(scope.clone()),
        format!(
            "input `{}` refers to `{}`, which is not an output port of the plan",
            input, port
        ),
    )
}

fn check_scopes(graph: &Graph<'_>, types: &Types, plan: &Plan, catalog: &dyn Catalog, report: &mut ValidationReport) {
    for (scope, declaration) in &plan.scopes {
        for (input, port) in &declaration.inputs {
            let at = Location::Scope(scope.clone());
            let Some(endpoint) = graph.output_port(&port.node, &port.port) else {
                report.push(missing_scope_input(scope, input, port));
                continue;
            };
            let Some(declared) = catalog.scope_input(scope, input) else {
                report.push(Finding::new(
                    Rule::ScopeInputs,
                    at,
                    format!("input `{}` is not an input of this scope that the host declares", input),
                ));
                continue;
            };
            if let Some(carried) = types.outputs[endpoint.0][endpoint.1] {
                if !declared.accepts(&carried) {
                    report.push(Finding::new(
                        Rule::ScopeInputs,
                        at,
                        format!(
                            "input `{}` is declared to carry {}, but `{}` can carry {}",
                            input,
                            describe(&declared),
                            port,
                            describe(&carried)
                        ),
                    ));
                }
            }
        }
    }
}
