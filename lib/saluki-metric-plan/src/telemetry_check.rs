//! Static telemetry-contract analyzer for [`MetricPlan`].
//!
//! Checks whether a program's filters or namespace prefixing could drop or rename series that downstream systems
//! (such as internal telemetry contracts like `aggregator.*` or `datadog.agent.*`) expect to observe.

use std::fmt;

use saluki_core::data_model::{
    event::metric::{context::Context, Metric},
    tags::TagSet,
};
use saluki_metric_kernels::{filterlist::compile_metric_filter, routing_filter::MetricAllowlistKernel};

use crate::ir::{MetricPlan, NodeId, Op, RouteFilterKind};

/// A metric series name or representative probe expected to survive to a target emit sink.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelemetryContract {
    /// Human-readable contract name (for example, `"aggregator_telemetry"`).
    pub contract_name: String,

    /// Representative metric names that must not be dropped or renamed unexpectedly.
    pub expected_series: Vec<String>,
}

impl TelemetryContract {
    /// Creates a new `TelemetryContract`.
    pub fn new(contract_name: impl Into<String>, expected_series: Vec<String>) -> Self {
        Self {
            contract_name: contract_name.into(),
            expected_series,
        }
    }

    /// Returns a default contract covering standard internal health and aggregator series.
    pub fn default_internal_telemetry() -> Self {
        Self {
            contract_name: "internal_telemetry".to_string(),
            expected_series: vec![
                "datadog.agent.running".to_string(),
                "datadog.dogstatsd.running".to_string(),
                "aggregator.processed".to_string(),
                "aggregator.flushed".to_string(),
            ],
        }
    }
}

/// Reason a static telemetry-contract check produced a warning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelemetryWarningKind {
    /// A metric filterlist blocks the expected series.
    DroppedByFilterlist,
    /// A namespace prefix rewrites the expected series because its prefix is not in `metric_prefix_blocklist`.
    RenamedByNamespacePrefix {
        /// Prefix that would be prepended.
        prefix: String,
    },
    /// A routing allowlist filter drops the expected series.
    DroppedByRouteFilter,
}

/// Warning emitted when a plan could disrupt an expected telemetry contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelemetryWarning {
    /// Name of the violated contract.
    pub contract_name: String,
    /// Node in the plan that caused the warning.
    pub node_id: NodeId,
    /// Affected metric series name.
    pub series: String,
    /// Specific reason for the warning.
    pub kind: TelemetryWarningKind,
}

impl fmt::Display for TelemetryWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TelemetryWarningKind::DroppedByFilterlist => write!(
                f,
                "contract `{}`: node `{}` drops expected series `{}` via metric filterlist",
                self.contract_name, self.node_id, self.series
            ),
            TelemetryWarningKind::RenamedByNamespacePrefix { prefix } => write!(
                f,
                "contract `{}`: node `{}` prefixes expected series `{}` with `{prefix}`",
                self.contract_name, self.node_id, self.series
            ),
            TelemetryWarningKind::DroppedByRouteFilter => write!(
                f,
                "contract `{}`: node `{}` drops expected series `{}` via route filter",
                self.contract_name, self.node_id, self.series
            ),
        }
    }
}

/// Analyzes `plan` statically against `contracts` and returns warnings for any expected series that could be dropped
/// or renamed.
pub fn analyze_telemetry_contracts(plan: &MetricPlan, contracts: &[TelemetryContract]) -> Vec<TelemetryWarning> {
    let mut warnings = Vec::new();

    for node in &plan.nodes {
        match &node.op {
            Op::PrefixFilter {
                metric_prefix,
                metric_prefix_blocklist,
                filter,
            } => {
                let (blocklist, _) = compile_metric_filter(filter);
                for contract in contracts {
                    for series in &contract.expected_series {
                        let effective_name = if !metric_prefix.is_empty()
                            && !metric_prefix_blocklist.iter().any(|p| series.starts_with(p))
                        {
                            warnings.push(TelemetryWarning {
                                contract_name: contract.contract_name.clone(),
                                node_id: node.id.clone(),
                                series: series.clone(),
                                kind: TelemetryWarningKind::RenamedByNamespacePrefix {
                                    prefix: metric_prefix.clone(),
                                },
                            });
                            let mut prefixed = metric_prefix.clone();
                            if !prefixed.ends_with('.') {
                                prefixed.push('.');
                            }
                            prefixed.push_str(series);
                            prefixed
                        } else {
                            series.clone()
                        };

                        if blocklist.contains(&effective_name) {
                            warnings.push(TelemetryWarning {
                                contract_name: contract.contract_name.clone(),
                                node_id: node.id.clone(),
                                series: series.clone(),
                                kind: TelemetryWarningKind::DroppedByFilterlist,
                            });
                        }
                    }
                }
            }
            Op::PostAggregateFilter { filter, .. } => {
                let (blocklist, _) = compile_metric_filter(filter);
                for contract in contracts {
                    for series in &contract.expected_series {
                        if blocklist.contains(series) {
                            warnings.push(TelemetryWarning {
                                contract_name: contract.contract_name.clone(),
                                node_id: node.id.clone(),
                                series: series.clone(),
                                kind: TelemetryWarningKind::DroppedByFilterlist,
                            });
                        }
                    }
                }
            }
            Op::RouteFilter {
                kind: RouteFilterKind::Allowlist { names, prefixes },
            } => {
                let kernel = MetricAllowlistKernel::for_allowlist(names, prefixes);
                for contract in contracts {
                    for series in &contract.expected_series {
                        let probe = Metric::gauge(Context::from_parts(series.clone(), TagSet::default()), 1.0);
                        if !kernel.should_forward(&probe) {
                            warnings.push(TelemetryWarning {
                                contract_name: contract.contract_name.clone(),
                                node_id: node.id.clone(),
                                series: series.clone(),
                                kind: TelemetryWarningKind::DroppedByRouteFilter,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    warnings
}

#[cfg(test)]
mod tests {
    use saluki_metric_kernels::filterlist::MetricNameFilter;

    use super::*;

    #[test]
    fn warns_when_filterlist_or_prefix_impacts_contract_series() {
        let plan = MetricPlan::builder()
            .node(
                "src",
                Op::Source {
                    stream: "dogstatsd".to_string(),
                },
            )
            .node(
                "pf",
                Op::PrefixFilter {
                    metric_prefix: "acme".to_string(),
                    metric_prefix_blocklist: vec!["datadog.agent".to_string()],
                    filter: MetricNameFilter {
                        values: vec!["aggregator.".to_string()],
                        match_prefix: true,
                        prefix_rules: vec![],
                    },
                },
            )
            .node(
                "out",
                Op::Emit {
                    target: "primary".to_string(),
                },
            )
            .edge("src", "pf")
            .edge("pf", "out")
            .build();

        let contract = TelemetryContract::new(
            "core_telemetry",
            vec!["datadog.agent.running".to_string(), "aggregator.processed".to_string()],
        );
        let warnings = analyze_telemetry_contracts(&plan, &[contract]);

        assert!(warnings.iter().any(|w| {
            w.series == "aggregator.processed"
                && matches!(w.kind, TelemetryWarningKind::RenamedByNamespacePrefix { .. })
        }));
        assert!(!warnings.iter().any(|w| w.series == "datadog.agent.running"));
    }
}
