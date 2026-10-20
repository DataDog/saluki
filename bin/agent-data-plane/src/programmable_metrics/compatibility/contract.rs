//! The telemetry contract built from the remapper's rules: what the default program meets.

use saluki_core::observability::metrics::RemapperRule;
use saluki_metric_plan::{
    ir::{Node, Plan},
    operators::{OperatorRegistry, Recorder, ReportedSeries},
    validate::{contract, ContractEntry, ContractReport, TelemetryContract},
};

use super::roles;

/// The prefix the remapper's rules give the series ADP reports.
const SOURCE_PREFIX: &str = "adp.";

/// What a remapper rule selects its source series by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Scope<'a> {
    /// The component reporting the series, by its `component_id` label.
    Component(&'a str),

    /// The cache reporting the series, by its `cache_id` label.
    Cache(&'a str),
}

/// A rule scoped to a component or cache that no node of the plan reports for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Unclaimed {
    /// The series the rule produces.
    pub series: &'static str,

    /// The component or cache it selects.
    pub scope: Scope<'static>,
}

/// Returns what `rule` selects its source series by, and the series' name and other selector tags, if it is scoped to
/// a component or a cache.
fn scope_of(rule: &RemapperRule) -> Option<(Scope<'static>, &'static str, Vec<&'static str>)> {
    let name = rule.source_name().strip_prefix(SOURCE_PREFIX)?;
    let mut scope = None;
    let mut tags = Vec::new();
    for tag in rule.source_tags() {
        if let Some(component) = tag.strip_prefix("component_id:") {
            scope = Some(Scope::Component(component));
        } else if let Some(cache) = tag.strip_prefix("cache_id:") {
            scope = Some(Scope::Cache(cache));
        } else {
            tags.push(*tag);
        }
    }
    Some((scope?, name, tags))
}

/// Returns `true` if `node`, running its operator, reports the series `name` with `tags` in `scope`.
fn reports(registry: &OperatorRegistry, node: &Node, scope: Scope<'_>, name: &str, tags: &[&str]) -> bool {
    let role = roles::of(&node.id, node.op.as_str());
    let operator = registry.get(node.op.as_str()).map(|def| def.reports()).unwrap_or(&[]);
    let series = operator.iter().chain(role.map(|role| role.series).unwrap_or(&[]));
    let matches = |series: &&ReportedSeries| series.name == name && series.has_tags(tags.iter().copied());
    match scope {
        Scope::Component(component) => {
            node.id.to_string() == component
                && series
                    .filter(|series| series.recorder != Recorder::Cache)
                    .any(|series| matches(&series))
        }
        Scope::Cache(cache) => {
            role.and_then(|role| role.cache_id) == Some(cache)
                && series
                    .filter(|series| series.recorder == Recorder::Cache)
                    .any(|series| matches(&series))
        }
    }
}

/// Walks `rules`, returning an entry for each rule scoped to a component or cache that a node of `plan` reports for,
/// and each such rule no node reports for.
pub(super) fn claim(
    plan: &Plan, registry: &OperatorRegistry, rules: &[RemapperRule],
) -> (Vec<ContractEntry>, Vec<Unclaimed>) {
    let mut entries = Vec::new();
    let mut unclaimed = Vec::new();
    for rule in rules {
        let Some((scope, name, tags)) = scope_of(rule) else {
            continue;
        };
        match plan
            .nodes
            .iter()
            .find(|node| reports(registry, node, scope, name, &tags))
        {
            Some(node) => entries.push(ContractEntry {
                series: rule.remapped_name().to_string(),
                node: node.id.clone(),
                operator: node.op.clone(),
                reports: name.to_string(),
            }),
            None => unclaimed.push(Unclaimed {
                series: rule.remapped_name(),
                scope,
            }),
        }
    }
    (entries, unclaimed)
}

/// Returns the telemetry contract `default_program` meets: an entry for every rule of `rules` scoped to a component or
/// cache that a node of the default program reports for, naming that node and its operator.
///
/// Rules selecting what a host component reports, such as the DogStatsD source's, are not in the contract: no program
/// can remove a host component.
pub fn contract(default_program: &Plan, registry: &OperatorRegistry, rules: &[RemapperRule]) -> TelemetryContract {
    TelemetryContract {
        entries: claim(default_program, registry, rules).0,
    }
}

/// Returns which entries of `contract` `plan` meets.
pub fn report(plan: &Plan, contract: &TelemetryContract) -> ContractReport {
    contract::check(plan, contract)
}
