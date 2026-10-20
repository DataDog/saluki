//! The telemetry contract: the nodes a host reads series from by name, and the operator each must run.
//!
//! A host whose readers select series by the component that reports them, such as a remapper shared with a native
//! mode, can only read a series from a node with the name it selects on, running the operator that reports it. The
//! host describes what it reads as a [`TelemetryContract`]: one [entry](ContractEntry) per series it reads, naming the
//! node and its operator. [`check`] finds which entries a plan meets, and [`findings`] warns, under
//! [`Rule::TelemetryContract`], about each node of the contract a plan lacks or runs as another operator. A plan with
//! more than the contract, such as a second aggregation under another name, gets nothing.
//!
//! The contract knows nothing of what a host does with the series; its messages name them for the reader.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Finding, Location, Rule};
use crate::ir::{NodeId, OperatorName, Plan};

/// What a host reads by name from the nodes of a plan.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryContract {
    /// What the host reads, one entry per series.
    pub entries: Vec<ContractEntry>,
}

/// One series a host reads from a node.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContractEntry {
    /// The series the host's reader produces, as messages name it.
    pub series: String,

    /// The node the host reads it from.
    pub node: NodeId,

    /// The operator the node must run to report it.
    pub operator: OperatorName,

    /// The series the node reports, which the host reads, as messages name it.
    pub reports: String,
}

/// Why a plan does not meet an entry of a contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Unmet {
    /// The plan has no node with the entry's name.
    NoNode {
        /// The node the entry names.
        node: NodeId,
    },

    /// The plan's node with the entry's name runs another operator.
    WrongOperator {
        /// The node the entry names.
        node: NodeId,

        /// The operator the node runs.
        operator: OperatorName,

        /// The operator the entry expects.
        expected: OperatorName,
    },
}

impl Unmet {
    /// Returns the node the entry names.
    pub fn node(&self) -> &NodeId {
        match self {
            Self::NoNode { node } | Self::WrongOperator { node, .. } => node,
        }
    }
}

/// Which entries of a contract a plan meets, in the contract's order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContractReport {
    /// The series of each entry the plan meets, with the node that reports it.
    pub met: Vec<(String, NodeId)>,

    /// The series of each entry the plan does not meet, with why.
    pub unmet: Vec<(String, Unmet)>,
}

impl ContractReport {
    /// Returns `true` if the plan meets every entry.
    pub fn is_met(&self) -> bool {
        self.unmet.is_empty()
    }
}

/// Checks which entries of `contract` `plan` meets: those whose node the plan has, running the entry's operator.
pub fn check(plan: &Plan, contract: &TelemetryContract) -> ContractReport {
    let operators = plan
        .nodes
        .iter()
        .map(|node| (&node.id, &node.op))
        .collect::<BTreeMap<_, _>>();
    let mut report = ContractReport::default();
    for entry in &contract.entries {
        match operators.get(&entry.node) {
            Some(&operator) if *operator == entry.operator => {
                report.met.push((entry.series.clone(), entry.node.clone()));
            }
            Some(&operator) => report.unmet.push((
                entry.series.clone(),
                Unmet::WrongOperator {
                    node: entry.node.clone(),
                    operator: operator.clone(),
                    expected: entry.operator.clone(),
                },
            )),
            None => report.unmet.push((
                entry.series.clone(),
                Unmet::NoNode {
                    node: entry.node.clone(),
                },
            )),
        }
    }
    report
}

/// Returns the warnings for what `plan` does not meet of `contract`: one per node of the contract the plan lacks, at
/// the plan, or runs as another operator, at the node, each listing every series it loses.
pub fn findings(plan: &Plan, contract: &TelemetryContract) -> Vec<Finding> {
    let report = check(plan, contract);
    // Group the series by node, keeping the contract's order of nodes and of series within a node.
    let mut nodes: Vec<(&Unmet, Vec<&str>)> = Vec::new();
    for (series, unmet) in &report.unmet {
        match nodes.iter_mut().find(|(seen, _)| seen.node() == unmet.node()) {
            Some((_, lost)) => {
                if !lost.contains(&series.as_str()) {
                    lost.push(series);
                }
            }
            None => nodes.push((unmet, vec![series])),
        }
    }
    nodes
        .into_iter()
        .map(|(unmet, lost)| {
            let lost = lost
                .iter()
                .map(|series| format!("`{}`", series))
                .collect::<Vec<_>>()
                .join(", ");
            match unmet {
                Unmet::NoNode { node } => Finding::new(
                    Rule::TelemetryContract,
                    Location::Plan,
                    format!(
                        "{} will be absent: no node is named `{}`; the host reads them from a node of that name",
                        lost, node
                    ),
                ),
                Unmet::WrongOperator {
                    node,
                    operator,
                    expected,
                } => Finding::new(
                    Rule::TelemetryContract,
                    Location::Node {
                        id: node.clone(),
                        port: None,
                    },
                    format!(
                        "`{}` runs `{}`, not `{}`; {} will be absent",
                        node, operator, expected, lost
                    ),
                ),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::validate::Severity;

    fn entry(series: &str, node: &str, operator: &str, reports: &str) -> ContractEntry {
        ContractEntry {
            series: series.to_string(),
            node: node.parse().unwrap(),
            operator: OperatorName::new(operator).unwrap(),
            reports: reports.to_string(),
        }
    }

    fn contract() -> TelemetryContract {
        TelemetryContract {
            entries: vec![
                entry("aggregator.flush", "dsd_agg", "aggregate", "aggregate_flushed_total"),
                entry(
                    "aggregator.number_of_flush",
                    "dsd_agg",
                    "aggregate",
                    "aggregate_flushes_total",
                ),
                entry(
                    "no_aggregation.flush",
                    "dsd_no_agg_split",
                    "timestamp_split",
                    "aggregate_passthrough_flushes_total",
                ),
            ],
        }
    }

    /// A plan of `nodes`, each `(id, operator)`, without edges: the contract reads only names and operators.
    fn plan(nodes: &[(&str, &str)]) -> Plan {
        let nodes = nodes
            .iter()
            .map(|(id, op)| json!({"id": id, "op": op}))
            .collect::<Vec<_>>();
        serde_json::from_value(json!({
            "ir_version": crate::ir::IR_VERSION,
            "nodes": nodes,
            "edges": [],
        }))
        .unwrap()
    }

    #[test]
    fn a_plan_with_every_node_of_the_contract_under_its_operator_meets_it() {
        let plan = plan(&[
            ("dsd_no_agg_split", "timestamp_split"),
            ("dsd_agg", "aggregate"),
            ("second_agg", "aggregate"),
        ]);
        let report = check(&plan, &contract());
        assert!(report.is_met(), "{:?}", report);
        assert_eq!(report.met.len(), 3);
        assert!(findings(&plan, &contract()).is_empty());
    }

    #[test]
    fn a_missing_node_gets_one_warning_at_the_plan_listing_every_series_it_loses() {
        let plan = plan(&[("dsd_no_agg_split", "timestamp_split"), ("agg", "aggregate")]);
        let findings = findings(&plan, &contract());
        assert_eq!(findings.len(), 1, "{:?}", findings);
        let finding = &findings[0];
        assert_eq!(finding.rule, Rule::TelemetryContract);
        assert_eq!(finding.severity, Severity::Warning);
        assert_eq!(finding.location, Location::Plan);
        assert_eq!(
            finding.message,
            "`aggregator.flush`, `aggregator.number_of_flush` will be absent: no node is named `dsd_agg`; the host \
             reads them from a node of that name"
        );
    }

    #[test]
    fn a_node_running_another_operator_gets_one_warning_at_the_node() {
        let plan = plan(&[("dsd_no_agg_split", "timestamp_split"), ("dsd_agg", "split")]);
        let report = check(&plan, &contract());
        assert_eq!(
            report.met,
            vec![("no_aggregation.flush".to_string(), "dsd_no_agg_split".parse().unwrap())]
        );
        let findings = findings(&plan, &contract());
        assert_eq!(findings.len(), 1, "{:?}", findings);
        assert_eq!(
            findings[0].location,
            Location::Node {
                id: "dsd_agg".parse().unwrap(),
                port: None
            }
        );
        assert_eq!(
            findings[0].message,
            "`dsd_agg` runs `split`, not `aggregate`; `aggregator.flush`, `aggregator.number_of_flush` will be absent"
        );
    }

    #[test]
    fn an_empty_contract_is_met_by_any_plan() {
        let plan = plan(&[("x", "split")]);
        assert!(check(&plan, &TelemetryContract::default()).is_met());
    }

    #[test]
    fn a_contract_round_trips_through_json() {
        let contract = contract();
        let value = serde_json::to_value(&contract).unwrap();
        assert_eq!(
            value["entries"][0],
            json!({
                "series": "aggregator.flush",
                "node": "dsd_agg",
                "operator": "aggregate",
                "reports": "aggregate_flushed_total",
            })
        );
        assert_eq!(serde_json::from_value::<TelemetryContract>(value).unwrap(), contract);
    }
}
