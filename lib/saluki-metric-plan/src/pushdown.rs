//! Whole-program ingress drop predicate pushdown derived from [`ValidatedPlan`].
//!
//! Because a [`MetricPlan`][crate::ir::MetricPlan] exposes the complete downstream operator graph statically,
//! `DogStatsDIngressPushdown::compile_from_plan` derives a sound early-drop predicate that DogStatsD ingress executes
//! on raw metric names *before* tag parsing, origin resolution, `StringInterner` interning, `ContextResolver` lookup,
//! and channel dispatch.
//!
//! Pushdown is derived conservatively: if an active `Op::Map` (with mapper profiles) or a fan-out `Op::Tee` sits
//! upstream of `Op::PrefixFilter` on the DogStatsD path, pushdown disables itself so semantics are preserved
//! unconditionally.

use std::collections::BTreeMap;

use saluki_core::data_model::event::metric::MetricValues;
use saluki_metric_kernels::filterlist::{compile_metric_filter, Blocklist};

use crate::{
    ir::{NodeId, Op, DEFAULT_OUT_PORT},
    lowering::STREAM_DOGSTATSD,
    validate::ValidatedPlan,
};

/// Compiled DogStatsD early-drop predicate derived from a [`ValidatedPlan`].
pub struct DogStatsDIngressPushdown {
    metric_prefix: String,
    metric_prefix_blocklist: Vec<String>,
    blocklist: Option<Blocklist>,
    prefix_filter_node: Option<NodeId>,
}

impl Default for DogStatsDIngressPushdown {
    fn default() -> Self {
        Self::disabled()
    }
}

impl DogStatsDIngressPushdown {
    /// Returns a no-op pushdown that never drops metrics early.
    pub fn disabled() -> Self {
        Self {
            metric_prefix: String::new(),
            metric_prefix_blocklist: Vec::new(),
            blocklist: None,
            prefix_filter_node: None,
        }
    }

    /// Derives a sound DogStatsD early-drop predicate by walking the dominating linear prefix of the `dogstatsd`
    /// stream inside `validated`.
    pub fn compile_from_plan(validated: &ValidatedPlan) -> Self {
        let plan = validated.plan();
        let Some(source_node) = plan
            .nodes
            .iter()
            .find(|n| matches!(&n.op, Op::Source { stream } if stream == STREAM_DOGSTATSD))
        else {
            return Self::disabled();
        };

        let nodes_by_id: BTreeMap<&NodeId, &crate::ir::Node> = plan.nodes.iter().map(|n| (&n.id, n)).collect();
        let mut outgoing: BTreeMap<(&NodeId, &str), Vec<&NodeId>> = BTreeMap::new();
        for edge in &plan.edges {
            outgoing
                .entry((&edge.from, edge.from_port.as_str()))
                .or_default()
                .push(&edge.to);
        }

        let mut current = outgoing
            .get(&(&source_node.id, DEFAULT_OUT_PORT))
            .and_then(|targets| (targets.len() == 1).then_some(targets[0]));

        while let Some(node_id) = current {
            let Some(node) = nodes_by_id.get(node_id) else {
                break;
            };

            match &node.op {
                Op::Map { profiles } => {
                    if !profiles.is_empty() {
                        // Active mapper profiles can rewrite metric names; do not push down blocklist drops.
                        return Self::disabled();
                    }
                }
                Op::PrefixFilter {
                    metric_prefix,
                    metric_prefix_blocklist,
                    filter,
                } => {
                    let (blocklist, _) = compile_metric_filter(filter);
                    if blocklist.is_empty() {
                        return Self::disabled();
                    }
                    return Self {
                        metric_prefix: metric_prefix.clone(),
                        metric_prefix_blocklist: metric_prefix_blocklist.clone(),
                        blocklist: Some(blocklist),
                        prefix_filter_node: Some(node.id.clone()),
                    };
                }
                // Stop once we reach any branching, stateful, or non-prefix operator.
                _ => break,
            }

            current = outgoing
                .get(&(node_id, DEFAULT_OUT_PORT))
                .and_then(|targets| (targets.len() == 1).then_some(targets[0]));
        }

        Self::disabled()
    }

    /// Returns `true` if early drop pushdown is active.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.blocklist.is_some()
    }

    /// Returns the node ID of the `Op::PrefixFilter` that owns pushed-down dropped points, if any.
    pub fn prefix_filter_node(&self) -> Option<&NodeId> {
        self.prefix_filter_node.as_ref()
    }

    /// Returns `true` if a DogStatsD packet with `raw_name` and `values` will definitely be dropped by
    /// `Op::PrefixFilter` and can therefore be discarded before tag parsing and `ContextResolver` lookup.
    #[inline]
    pub fn should_drop_metric(&self, raw_name: &str, values: &MetricValues, scratch: &mut String) -> bool {
        let Some(blocklist) = self.blocklist.as_ref() else {
            return false;
        };

        let effective = self.effective_name(raw_name, scratch);
        if !blocklist.contains(effective) {
            return false;
        }

        if matches!(values, MetricValues::Histogram(_)) && blocklist.has_descendant_exception(effective) {
            return false;
        }

        true
    }

    #[inline]
    fn effective_name<'a>(&self, raw_name: &'a str, scratch: &'a mut String) -> &'a str {
        if self.metric_prefix.is_empty()
            || self
                .metric_prefix_blocklist
                .iter()
                .any(|excluded| raw_name.starts_with(excluded))
        {
            raw_name
        } else {
            scratch.clear();
            scratch.push_str(&self.metric_prefix);
            scratch.push_str(raw_name);
            scratch.as_str()
        }
    }
}

#[cfg(test)]
mod tests {
    use saluki_metric_kernels::{
        filterlist::{MetricNameFilter, MetricPrefixRule},
        mapper::{DogStatsDMapperProfile, DogStatsDMetricMapping},
    };

    use super::*;
    use crate::{
        lowering::{lower_native_pipeline, NativePipelineConfig},
        validate::validate_plan,
    };

    #[test]
    fn derives_blocklist_pushdown_with_namespace_prefix_and_histogram_exception() {
        let config = NativePipelineConfig {
            dogstatsd_enabled: true,
            metric_prefix: "acme.".to_string(),
            metric_prefix_blocklist: vec!["datadog.".to_string()],
            metric_filter: MetricNameFilter {
                values: vec!["acme.dropped".to_string()],
                match_prefix: false,
                prefix_rules: vec![MetricPrefixRule {
                    prefix: "acme.hist".to_string(),
                    except_exact: vec!["acme.hist.max".to_string()],
                    except_prefix: vec![],
                }],
            },
            ..NativePipelineConfig::default()
        };
        let validated = validate_plan(&lower_native_pipeline(&config)).unwrap();
        let pushdown = DogStatsDIngressPushdown::compile_from_plan(&validated);
        assert!(pushdown.is_active());

        let mut scratch = String::new();
        let counter = MetricValues::counter(1.0);
        let histogram = MetricValues::histogram(1.0);

        // Prefixed exact drop.
        assert!(pushdown.should_drop_metric("dropped", &counter, &mut scratch));
        assert!(!pushdown.should_drop_metric("kept", &counter, &mut scratch));

        // Histogram with descendant exception is preserved; counter on same prefix is dropped.
        assert!(pushdown.should_drop_metric("hist", &counter, &mut scratch));
        assert!(!pushdown.should_drop_metric("hist", &histogram, &mut scratch));
    }

    #[test]
    fn disables_pushdown_when_active_mapper_profiles_exist() {
        let config = NativePipelineConfig {
            dogstatsd_enabled: true,
            mapper_profiles: vec![DogStatsDMapperProfile {
                name: "p".to_string(),
                prefix: "foo.".to_string(),
                mappings: vec![DogStatsDMetricMapping {
                    metric_match: "foo.*".to_string(),
                    match_type: "wildcard".to_string(),
                    name: "bar.$1".to_string(),
                    tags: Default::default(),
                }],
            }],
            metric_filter: MetricNameFilter {
                values: vec!["bar.a".to_string()],
                match_prefix: false,
                prefix_rules: vec![],
            },
            ..NativePipelineConfig::default()
        };
        let validated = validate_plan(&lower_native_pipeline(&config)).unwrap();
        let pushdown = DogStatsDIngressPushdown::compile_from_plan(&validated);
        assert!(!pushdown.is_active());
    }
}
