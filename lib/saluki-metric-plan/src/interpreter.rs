//! Deterministic single-threaded reference interpreter for [`MetricPlan`].
//!
//! This interpreter defines the exact executable semantics of every operator in the IR by directly invoking the pure
//! kernels in `saluki-metric-kernels`. Compiled execution engines and native pipeline lowering are tested
//! differentially against this interpreter.

use std::{collections::BTreeMap, time::Duration};

use saluki_core::data_model::{event::metric::Metric, tags::SharedTagSet};
use saluki_error::{generic_error, GenericError};
use saluki_metric_kernels::{
    aggregate::AggregateCore,
    filterlist::{PostAggregateFilterKernel, PrefixFilterKernel},
    host_tags::HostTagsKernel,
    mapper::MapperKernel,
    routing_filter::{MetricAllowlistKernel, MetricMirroring},
    tag_filter::TagFilterKernel,
    timestamp_split::split_for_no_aggregation,
};

use crate::{
    ir::{NodeId, Op, RouteFilterKind, DEFAULT_OUT_PORT, SPLIT_AGGREGATE_PORT, SPLIT_PASSTHROUGH_PORT},
    validate::ValidatedPlan,
};

/// Runtime host facts and live operational signals supplied to a running plan.
///
/// Policy settings live as literals inside the IR; only host facts (such as host tags) and dynamic signals (such as
/// mirroring activation) cross this boundary.
#[derive(Clone, Debug, Default)]
pub struct RuntimeBindings {
    /// Named host-tag sets (for example, `"host_tags"`). `None` or missing disables enrichment.
    pub host_tags: BTreeMap<String, SharedTagSet>,

    /// Named mirroring signals overriding a [`RouteFilterKind::Mirroring`] node's literal policy.
    pub mirroring_signals: BTreeMap<String, MetricMirroring>,
}

impl RuntimeBindings {
    /// Inserts or updates a named host-tags binding.
    pub fn with_host_tags(mut self, name: impl Into<String>, tags: SharedTagSet) -> Self {
        self.host_tags.insert(name.into(), tags);
        self
    }

    /// Inserts or updates a named mirroring signal binding.
    pub fn with_mirroring_signal(mut self, name: impl Into<String>, mirroring: MetricMirroring) -> Self {
        self.mirroring_signals.insert(name.into(), mirroring);
        self
    }
}

enum RuntimeNode {
    Source,
    Map(MapperKernel),
    PrefixFilter(PrefixFilterKernel),
    TimestampSplit,
    TagFilter(TagFilterKernel),
    Aggregate {
        core: AggregateCore<()>,
        flush_open_windows: bool,
    },
    PostAggregateFilter(PostAggregateFilterKernel),
    EnrichHostTags {
        binding: String,
    },
    RouteFilter {
        kind: RouteFilterKind,
        static_kernel: MetricAllowlistKernel,
    },
    Merge,
    Tee,
    Emit {
        target: String,
    },
    Drop,
}

/// Deterministic, single-threaded reference interpreter executing a [`ValidatedPlan`].
pub struct ReferenceInterpreter {
    topo_order: Vec<NodeId>,
    nodes: BTreeMap<NodeId, RuntimeNode>,
    /// Outgoing edges indexed by `(from_node, from_port) -> [to_node...]`.
    outgoing: BTreeMap<(NodeId, String), Vec<NodeId>>,
    /// Maps logical source stream names to their `Source` node IDs.
    source_nodes: BTreeMap<String, NodeId>,
    bindings: RuntimeBindings,
}

impl ReferenceInterpreter {
    /// Instantiates a `ReferenceInterpreter` from a validated plan and initial runtime bindings.
    pub fn new(validated: &ValidatedPlan, bindings: RuntimeBindings) -> Result<Self, GenericError> {
        let plan = validated.plan();
        let mut nodes = BTreeMap::new();
        let mut source_nodes = BTreeMap::new();

        for node in &plan.nodes {
            let runtime = match &node.op {
                Op::Source { stream } => {
                    source_nodes.insert(stream.clone(), node.id.clone());
                    RuntimeNode::Source
                }
                Op::Map { profiles } => RuntimeNode::Map(MapperKernel::compile(profiles)?),
                Op::PrefixFilter {
                    metric_prefix,
                    metric_prefix_blocklist,
                    filter,
                } => {
                    let (kernel, _) =
                        PrefixFilterKernel::from_filter(metric_prefix.clone(), metric_prefix_blocklist.clone(), filter);
                    RuntimeNode::PrefixFilter(kernel)
                }
                Op::TimestampSplit => RuntimeNode::TimestampSplit,
                Op::TagFilter {
                    entries,
                    value_allowlists,
                } => RuntimeNode::TagFilter(TagFilterKernel::new(entries, value_allowlists)),
                Op::Aggregate {
                    window_duration_secs,
                    context_limit,
                    counter_expiry_secs,
                    flush_open_windows,
                    histogram,
                } => {
                    let expiry = (*counter_expiry_secs > 0).then(|| Duration::from_secs(*counter_expiry_secs));
                    RuntimeNode::Aggregate {
                        core: AggregateCore::new(*window_duration_secs, *context_limit, expiry, histogram.clone()),
                        flush_open_windows: *flush_open_windows,
                    }
                }
                Op::PostAggregateFilter {
                    filter,
                    histogram_aggregates,
                    histogram_percentiles,
                } => RuntimeNode::PostAggregateFilter(PostAggregateFilterKernel::new(
                    filter,
                    histogram_aggregates,
                    histogram_percentiles,
                )),
                Op::EnrichHostTags { binding } => RuntimeNode::EnrichHostTags {
                    binding: binding.clone(),
                },
                Op::RouteFilter { kind } => {
                    let static_kernel = compile_route_filter(kind, &bindings);
                    RuntimeNode::RouteFilter {
                        kind: kind.clone(),
                        static_kernel,
                    }
                }
                Op::Merge => RuntimeNode::Merge,
                Op::Tee => RuntimeNode::Tee,
                Op::Emit { target } => RuntimeNode::Emit { target: target.clone() },
                Op::Drop => RuntimeNode::Drop,
            };

            nodes.insert(node.id.clone(), runtime);
        }

        let mut outgoing: BTreeMap<(NodeId, String), Vec<NodeId>> = BTreeMap::new();
        for edge in &plan.edges {
            outgoing
                .entry((edge.from.clone(), edge.from_port.clone()))
                .or_default()
                .push(edge.to.clone());
        }

        Ok(Self {
            topo_order: validated.topo_order().to_vec(),
            nodes,
            outgoing,
            source_nodes,
            bindings,
        })
    }

    /// Replaces the runtime bindings and refreshes signal-bound route filters.
    pub fn set_bindings(&mut self, bindings: RuntimeBindings) {
        self.bindings = bindings;
        for node in self.nodes.values_mut() {
            if let RuntimeNode::RouteFilter { kind, static_kernel } = node {
                *static_kernel = compile_route_filter(kind, &self.bindings);
            }
        }
    }

    /// Returns a reference to the current runtime bindings.
    pub fn bindings(&self) -> &RuntimeBindings {
        &self.bindings
    }

    /// Feeds `metrics` into the named `stream` at `timestamp_secs` and returns all metrics emitted immediately by
    /// terminal `Emit` nodes, grouped by target name.
    pub fn ingest(
        &mut self, stream: &str, timestamp_secs: u64, metrics: Vec<Metric>,
    ) -> Result<BTreeMap<String, Vec<Metric>>, GenericError> {
        let Some(source_id) = self.source_nodes.get(stream).cloned() else {
            return Err(generic_error!("unknown source stream `{stream}`"));
        };

        let mut inboxes: BTreeMap<NodeId, Vec<Metric>> = BTreeMap::new();
        inboxes.insert(source_id, metrics);

        let mut emitted: BTreeMap<String, Vec<Metric>> = BTreeMap::new();
        self.propagate_inboxes(timestamp_secs, &mut inboxes, &mut emitted);

        Ok(emitted)
    }

    /// Flushes every stateful `Aggregate` node in topological order at `current_time_secs` and returns all metrics
    /// reaching terminal `Emit` nodes, grouped by target name.
    ///
    /// When `shutdown` is `true`, each `Aggregate` node respects its `flush_open_windows` setting. When `shutdown` is
    /// `false`, only closed buckets are flushed.
    pub fn flush(&mut self, current_time_secs: u64, shutdown: bool) -> BTreeMap<String, Vec<Metric>> {
        let mut inboxes: BTreeMap<NodeId, Vec<Metric>> = BTreeMap::new();
        let mut emitted: BTreeMap<String, Vec<Metric>> = BTreeMap::new();
        let topo = self.topo_order.clone();

        for node_id in &topo {
            // First process any metrics that arrived from upstream nodes (for example, an upstream aggregate).
            if let Some(incoming) = inboxes.remove(node_id) {
                self.execute_node(node_id, current_time_secs, incoming, &mut inboxes, &mut emitted);
            }

            // Then, if this node is an Aggregate, flush its buckets and route them downstream.
            let flushed = match self.nodes.get_mut(node_id) {
                Some(RuntimeNode::Aggregate {
                    core,
                    flush_open_windows,
                }) => {
                    let mut out = Vec::new();
                    if !core.is_empty() {
                        let flush_open = shutdown && *flush_open_windows;
                        core.flush_into(current_time_secs, flush_open, |metric| out.push(metric));
                    }
                    out
                }
                _ => Vec::new(),
            };

            if !flushed.is_empty() {
                self.route_out(node_id, DEFAULT_OUT_PORT, flushed, &mut inboxes);
            }
        }

        emitted
    }

    fn propagate_inboxes(
        &mut self, timestamp_secs: u64, inboxes: &mut BTreeMap<NodeId, Vec<Metric>>,
        emitted: &mut BTreeMap<String, Vec<Metric>>,
    ) {
        let topo = self.topo_order.clone();
        for node_id in &topo {
            let Some(incoming) = inboxes.remove(node_id) else {
                continue;
            };
            if incoming.is_empty() {
                continue;
            }
            self.execute_node(node_id, timestamp_secs, incoming, inboxes, emitted);
        }
    }

    fn execute_node(
        &mut self, node_id: &NodeId, timestamp_secs: u64, incoming: Vec<Metric>,
        inboxes: &mut BTreeMap<NodeId, Vec<Metric>>, emitted: &mut BTreeMap<String, Vec<Metric>>,
    ) {
        let Some(node) = self.nodes.get_mut(node_id) else {
            return;
        };

        match node {
            RuntimeNode::Source | RuntimeNode::Merge => {
                self.route_out(node_id, DEFAULT_OUT_PORT, incoming, inboxes);
            }
            RuntimeNode::Tee => {
                if let Some(targets) = self.outgoing.get(&(node_id.clone(), DEFAULT_OUT_PORT.to_string())) {
                    if let Some((last, rest)) = targets.split_last() {
                        for target in rest {
                            inboxes
                                .entry(target.clone())
                                .or_default()
                                .extend(incoming.iter().cloned());
                        }
                        inboxes.entry(last.clone()).or_default().extend(incoming);
                    }
                }
            }
            RuntimeNode::Map(kernel) => {
                let mut out = incoming;
                for metric in &mut out {
                    kernel.apply(metric);
                }
                self.route_out(node_id, DEFAULT_OUT_PORT, out, inboxes);
            }
            RuntimeNode::PrefixFilter(kernel) => {
                let mut out = incoming;
                out.retain_mut(|metric| kernel.apply(metric));
                self.route_out(node_id, DEFAULT_OUT_PORT, out, inboxes);
            }
            RuntimeNode::TimestampSplit => {
                let mut agg = Vec::new();
                let mut passthrough = Vec::new();
                for metric in incoming {
                    let outcome = split_for_no_aggregation(metric);
                    if let Some(m) = outcome.aggregate {
                        agg.push(m);
                    }
                    if let Some(m) = outcome.passthrough {
                        passthrough.push(m);
                    }
                }
                if !agg.is_empty() {
                    self.route_out(node_id, SPLIT_AGGREGATE_PORT, agg, inboxes);
                }
                if !passthrough.is_empty() {
                    self.route_out(node_id, SPLIT_PASSTHROUGH_PORT, passthrough, inboxes);
                }
            }
            RuntimeNode::TagFilter(kernel) => {
                let mut out = incoming;
                for metric in &mut out {
                    kernel.apply(metric);
                }
                self.route_out(node_id, DEFAULT_OUT_PORT, out, inboxes);
            }
            RuntimeNode::Aggregate { core, .. } => {
                for metric in incoming {
                    let _ = core.insert(timestamp_secs, metric);
                }
            }
            RuntimeNode::PostAggregateFilter(kernel) => {
                let mut out = incoming;
                out.retain(|metric| kernel.should_keep(metric));
                self.route_out(node_id, DEFAULT_OUT_PORT, out, inboxes);
            }
            RuntimeNode::EnrichHostTags { binding } => {
                let mut out = incoming;
                if let Some(tags) = self.bindings.host_tags.get(binding) {
                    let kernel = HostTagsKernel::new(Some(tags.clone()));
                    for metric in &mut out {
                        kernel.apply(metric);
                    }
                }
                self.route_out(node_id, DEFAULT_OUT_PORT, out, inboxes);
            }
            RuntimeNode::RouteFilter { static_kernel, .. } => {
                let mut out = incoming;
                out.retain(|metric| static_kernel.should_forward(metric));
                self.route_out(node_id, DEFAULT_OUT_PORT, out, inboxes);
            }
            RuntimeNode::Emit { target } => {
                emitted.entry(target.clone()).or_default().extend(incoming);
            }
            RuntimeNode::Drop => {}
        }
    }

    fn route_out(&self, from: &NodeId, port: &str, metrics: Vec<Metric>, inboxes: &mut BTreeMap<NodeId, Vec<Metric>>) {
        if metrics.is_empty() {
            return;
        }
        if let Some(targets) = self.outgoing.get(&(from.clone(), port.to_string())) {
            if let Some(target) = targets.first() {
                inboxes.entry(target.clone()).or_default().extend(metrics);
            }
        }
    }
}

fn compile_route_filter(kind: &RouteFilterKind, bindings: &RuntimeBindings) -> MetricAllowlistKernel {
    match kind {
        RouteFilterKind::Allowlist { names, prefixes } => MetricAllowlistKernel::for_allowlist(names, prefixes),
        RouteFilterKind::Mirroring {
            enabled,
            policy,
            signal_binding,
        } => {
            let effective = signal_binding
                .as_ref()
                .and_then(|name| bindings.mirroring_signals.get(name))
                .unwrap_or(policy);
            MetricAllowlistKernel::for_mirroring(*enabled, effective)
        }
        RouteFilterKind::SeriesAllowlist { enabled, metrics } => {
            MetricAllowlistKernel::for_series_allowlist(*enabled, metrics)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use saluki_core::data_model::{
        event::metric::{context::Context, MetricValues},
        tags::{SharedTagSet, Tag},
    };
    use saluki_metric_kernels::{
        filterlist::MetricNameFilter,
        histogram::HistogramConfiguration,
        routing_filter::MetricMirroring,
        tag_filter::{FilterAction, MetricTagFilterEntry},
    };

    use super::*;
    use crate::{ir::MetricPlan, validate::validate_plan};

    #[test]
    fn executes_full_dogstatsd_graph_with_timestamp_split_and_routing() {
        let plan = MetricPlan::builder()
            .node(
                "dsd_in",
                Op::Source {
                    stream: "dogstatsd".to_string(),
                },
            )
            .node(
                "prefix",
                Op::PrefixFilter {
                    metric_prefix: "acme".to_string(),
                    metric_prefix_blocklist: vec!["datadog.".to_string()],
                    filter: MetricNameFilter {
                        values: vec!["acme.dropped".to_string()],
                        match_prefix: false,
                        prefix_rules: vec![],
                    },
                },
            )
            .node("split", Op::TimestampSplit)
            .node(
                "tag_filter",
                Op::TagFilter {
                    entries: vec![MetricTagFilterEntry {
                        metric_name: "acme.hits".to_string(),
                        action: FilterAction::Exclude,
                        tags: vec!["noisy".to_string()],
                    }],
                    value_allowlists: vec![],
                },
            )
            .node(
                "agg",
                Op::Aggregate {
                    window_duration_secs: NonZeroU64::new(10).unwrap(),
                    context_limit: 1000,
                    counter_expiry_secs: 0,
                    flush_open_windows: true,
                    histogram: HistogramConfiguration::default(),
                },
            )
            .node("rejoin", Op::Merge)
            .node(
                "host_tags",
                Op::EnrichHostTags {
                    binding: "host_tags".to_string(),
                },
            )
            .node("fanout", Op::Tee)
            .node(
                "primary",
                Op::Emit {
                    target: "primary".to_string(),
                },
            )
            .node(
                "mrf_filter",
                Op::RouteFilter {
                    kind: RouteFilterKind::Mirroring {
                        enabled: true,
                        policy: MetricMirroring {
                            enabled: false,
                            allowlist: vec![],
                        },
                        signal_binding: Some("mrf".to_string()),
                    },
                },
            )
            .node(
                "mrf",
                Op::Emit {
                    target: "mrf".to_string(),
                },
            )
            .edge("dsd_in", "prefix")
            .edge("prefix", "split")
            .edge_port("split", SPLIT_AGGREGATE_PORT, "tag_filter")
            .edge("tag_filter", "agg")
            .edge("agg", "rejoin")
            .edge_port("split", SPLIT_PASSTHROUGH_PORT, "rejoin")
            .edge("rejoin", "host_tags")
            .edge("host_tags", "fanout")
            .edge("fanout", "primary")
            .edge("fanout", "mrf_filter")
            .edge("mrf_filter", "mrf")
            .build();

        let validated = validate_plan(&plan).expect("plan should be valid");
        let bindings = RuntimeBindings::default()
            .with_host_tags("host_tags", SharedTagSet::from_iter([Tag::from("region:us-east-1")]))
            .with_mirroring_signal(
                "mrf",
                MetricMirroring {
                    enabled: true,
                    allowlist: vec!["acme.hits".to_string()],
                },
            );

        let mut interp = ReferenceInterpreter::new(&validated, bindings).expect("interpreter init");

        // Ingest: 1 dropped metric, 2 non-timestamped counters (same series after tag drop), 1 timestamped gauge.
        let immediate = interp
            .ingest(
                "dogstatsd",
                12,
                vec![
                    Metric::counter("dropped", 1.0),
                    Metric::counter(Context::from_static_parts("hits", &["noisy:a", "env:prod"]), 2.0),
                    Metric::counter(Context::from_static_parts("hits", &["noisy:b", "env:prod"]), 3.0),
                    Metric::gauge("ts_gauge", (1000, 9.0)),
                ],
            )
            .expect("ingest");

        // Timestamped gauge bypasses aggregation and arrives immediately on `primary`, enriched with host tags,
        // while `mrf` drops it because its allowlist only permits `acme.hits`.
        assert_eq!(immediate.get("primary").map(Vec::len), Some(1));
        assert_eq!(immediate["primary"][0].context().name(), "acme.ts_gauge");
        assert!(immediate["primary"][0]
            .context()
            .tags()
            .has_tag(Tag::from("region:us-east-1")));
        assert!(immediate.get("mrf").is_none_or(Vec::is_empty));

        // Flush at t=20 closes bucket [10, 20): merged `acme.hits` arrives on BOTH `primary` and `mrf`!
        let flushed = interp.flush(20, false);
        assert_eq!(flushed.get("primary").map(Vec::len), Some(1));
        assert_eq!(flushed.get("mrf").map(Vec::len), Some(1));
        let hit = &flushed["primary"][0];
        assert_eq!(hit.context().name(), "acme.hits");
        assert!(hit.context().tags().has_tag(Tag::from("env:prod")));
        assert!(hit.context().tags().has_tag(Tag::from("region:us-east-1")));
        assert!(!hit.context().tags().has_tag(Tag::from("noisy:a")));
        assert!(matches!(hit.values(), MetricValues::Rate(_, _)));
    }

    #[test]
    fn supports_chained_stateful_aggregations_natively() {
        // Two chained aggregations: 10s primary rollup -> 60s secondary rollup.
        let plan = MetricPlan::builder()
            .node(
                "src",
                Op::Source {
                    stream: "dogstatsd".to_string(),
                },
            )
            .node(
                "agg_10s",
                Op::Aggregate {
                    window_duration_secs: NonZeroU64::new(10).unwrap(),
                    context_limit: 100,
                    counter_expiry_secs: 0,
                    flush_open_windows: true,
                    histogram: HistogramConfiguration::default(),
                },
            )
            .node(
                "agg_60s",
                Op::Aggregate {
                    window_duration_secs: NonZeroU64::new(60).unwrap(),
                    context_limit: 100,
                    counter_expiry_secs: 0,
                    flush_open_windows: true,
                    histogram: HistogramConfiguration::default(),
                },
            )
            .node(
                "out",
                Op::Emit {
                    target: "rollup".to_string(),
                },
            )
            .edge("src", "agg_10s")
            .edge("agg_10s", "agg_60s")
            .edge("agg_60s", "out")
            .build();

        let validated = validate_plan(&plan).expect("valid chained aggregate plan");
        assert_eq!(validated.stateful_node_count(), 2);

        let mut interp = ReferenceInterpreter::new(&validated, RuntimeBindings::default()).expect("init");
        assert!(interp
            .ingest("dogstatsd", 12, vec![Metric::gauge("temp", 10.0)])
            .unwrap()
            .is_empty());

        // Flush at t=20: agg_10s flushes [10, 20) into agg_60s, whose [0, 60) window is still open.
        let at_20 = interp.flush(20, false);
        assert!(at_20.is_empty());

        // Flush at t=60: agg_60s closes [0, 60) and emits the rolled-up gauge!
        let at_60 = interp.flush(60, false);
        assert_eq!(at_60.get("rollup").map(Vec::len), Some(1));
        assert_eq!(at_60["rollup"][0].context().name(), "temp");
    }
}
