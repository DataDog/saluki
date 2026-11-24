//! Compiled single-region fused execution engine for [`MetricPlan`].
//!
//! Compiles a validated (and optimized) plan into a flat, index-addressed topological schedule with zero intermediate
//! channels and reusable per-step scratch buffers. Supports 0, 1, or N stateful [`AggregateCore`] operators natively
//! anywhere in the DAG.

use std::{collections::BTreeMap, time::Duration};

use saluki_core::data_model::event::metric::Metric;
use saluki_error::GenericError;
use saluki_metric_kernels::{
    aggregate::{AggregateContextSnapshotEntry, AggregateCore},
    filterlist::{PostAggregateFilterKernel, PrefixFilterKernel},
    host_tags::HostTagsKernel,
    mapper::MapperKernel,
    routing_filter::MetricAllowlistKernel,
    tag_filter::TagFilterKernel,
    timestamp_split::split_for_no_aggregation,
};

use crate::{
    interpreter::RuntimeBindings,
    ir::{NodeId, Op, RouteFilterKind, DEFAULT_OUT_PORT, SPLIT_AGGREGATE_PORT, SPLIT_PASSTHROUGH_PORT},
    optimize::{optimize_plan, OptimizationReport},
    validate::ValidatedPlan,
};

type StepIdx = usize;
type TargetIdx = usize;

enum CompiledOp {
    Source {
        next: Option<StepIdx>,
    },
    Map {
        kernel: MapperKernel,
        next: Option<StepIdx>,
    },
    PrefixFilter {
        kernel: PrefixFilterKernel,
        next: Option<StepIdx>,
    },
    TimestampSplit {
        agg_next: Option<StepIdx>,
        passthrough_next: Option<StepIdx>,
    },
    TagFilter {
        kernel: TagFilterKernel,
        next: Option<StepIdx>,
    },
    Aggregate {
        core: AggregateCore<()>,
        flush_open_windows: bool,
        next: Option<StepIdx>,
    },
    PostAggregateFilter {
        kernel: PostAggregateFilterKernel,
        next: Option<StepIdx>,
    },
    EnrichHostTags {
        binding: String,
        kernel: HostTagsKernel,
        next: Option<StepIdx>,
    },
    RouteFilter {
        kind: RouteFilterKind,
        kernel: MetricAllowlistKernel,
        next: Option<StepIdx>,
    },
    Merge {
        next: Option<StepIdx>,
    },
    Tee {
        targets: Vec<StepIdx>,
    },
    Emit {
        target_idx: TargetIdx,
    },
    Drop,
}

/// A compiled single-region fused metric execution engine.
///
/// All synchronous and stateful metric operators execute inside one flat topological loop with reusable scratch
/// buffers and no intermediate task hops.
pub struct FusedMetricEngine {
    steps: Vec<CompiledOp>,
    step_ids: Vec<NodeId>,
    inboxes: Vec<Vec<Metric>>,
    stream_to_step: BTreeMap<String, StepIdx>,
    target_names: Vec<String>,
    stateful_steps: Vec<StepIdx>,
    bindings: RuntimeBindings,
    optimization_report: OptimizationReport,
}

impl FusedMetricEngine {
    /// Optimizes and compiles `validated` into a [`FusedMetricEngine`].
    pub fn compile(validated: &ValidatedPlan, bindings: RuntimeBindings) -> Result<Self, GenericError> {
        let (optimized, optimization_report) = optimize_plan(validated);
        Self::compile_unoptimized(&optimized, bindings, optimization_report)
    }

    fn compile_unoptimized(
        validated: &ValidatedPlan, bindings: RuntimeBindings, optimization_report: OptimizationReport,
    ) -> Result<Self, GenericError> {
        let plan = validated.plan();
        let topo = validated.topo_order();

        let id_to_idx: BTreeMap<&NodeId, StepIdx> = topo.iter().enumerate().map(|(i, id)| (id, i)).collect();
        let nodes_by_id: BTreeMap<&NodeId, &crate::ir::Node> = plan.nodes.iter().map(|n| (&n.id, n)).collect();

        let mut outgoing: BTreeMap<(StepIdx, &str), Vec<StepIdx>> = BTreeMap::new();
        for edge in &plan.edges {
            let from_idx = id_to_idx[&edge.from];
            let to_idx = id_to_idx[&edge.to];
            outgoing
                .entry((from_idx, edge.from_port.as_str()))
                .or_default()
                .push(to_idx);
        }

        let mut steps = Vec::with_capacity(topo.len());
        let mut step_ids = Vec::with_capacity(topo.len());
        let mut stream_to_step = BTreeMap::new();
        let mut target_names = Vec::new();
        let mut stateful_steps = Vec::new();

        for (idx, node_id) in topo.iter().enumerate() {
            let node = nodes_by_id[node_id];
            let default_next = outgoing
                .get(&(idx, DEFAULT_OUT_PORT))
                .and_then(|targets| targets.first().copied());

            let compiled = match &node.op {
                Op::Source { stream } => {
                    stream_to_step.insert(stream.clone(), idx);
                    CompiledOp::Source { next: default_next }
                }
                Op::Map { profiles } => CompiledOp::Map {
                    kernel: MapperKernel::compile(profiles)?,
                    next: default_next,
                },
                Op::PrefixFilter {
                    metric_prefix,
                    metric_prefix_blocklist,
                    filter,
                } => {
                    let (kernel, _) =
                        PrefixFilterKernel::from_filter(metric_prefix.clone(), metric_prefix_blocklist.clone(), filter);
                    CompiledOp::PrefixFilter {
                        kernel,
                        next: default_next,
                    }
                }
                Op::TimestampSplit => {
                    let agg_next = outgoing
                        .get(&(idx, SPLIT_AGGREGATE_PORT))
                        .and_then(|t| t.first().copied());
                    let passthrough_next = outgoing
                        .get(&(idx, SPLIT_PASSTHROUGH_PORT))
                        .and_then(|t| t.first().copied());
                    CompiledOp::TimestampSplit {
                        agg_next,
                        passthrough_next,
                    }
                }
                Op::TagFilter {
                    entries,
                    value_allowlists,
                } => CompiledOp::TagFilter {
                    kernel: TagFilterKernel::new(entries, value_allowlists),
                    next: default_next,
                },
                Op::Aggregate {
                    window_duration_secs,
                    context_limit,
                    counter_expiry_secs,
                    flush_open_windows,
                    histogram,
                } => {
                    let expiry = (*counter_expiry_secs > 0).then(|| Duration::from_secs(*counter_expiry_secs));
                    stateful_steps.push(idx);
                    CompiledOp::Aggregate {
                        core: AggregateCore::new(*window_duration_secs, *context_limit, expiry, histogram.clone()),
                        flush_open_windows: *flush_open_windows,
                        next: default_next,
                    }
                }
                Op::PostAggregateFilter {
                    filter,
                    histogram_aggregates,
                    histogram_percentiles,
                } => CompiledOp::PostAggregateFilter {
                    kernel: PostAggregateFilterKernel::new(filter, histogram_aggregates, histogram_percentiles),
                    next: default_next,
                },
                Op::EnrichHostTags { binding } => {
                    let host_tags = bindings.host_tags.get(binding).cloned();
                    CompiledOp::EnrichHostTags {
                        binding: binding.clone(),
                        kernel: HostTagsKernel::new(host_tags),
                        next: default_next,
                    }
                }
                Op::RouteFilter { kind } => {
                    let kernel = compile_route_filter(kind, &bindings);
                    CompiledOp::RouteFilter {
                        kind: kind.clone(),
                        kernel,
                        next: default_next,
                    }
                }
                Op::Merge => CompiledOp::Merge { next: default_next },
                Op::Tee => {
                    let targets = outgoing.get(&(idx, DEFAULT_OUT_PORT)).cloned().unwrap_or_default();
                    CompiledOp::Tee { targets }
                }
                Op::Emit { target } => {
                    let target_idx = target_names.len();
                    target_names.push(target.clone());
                    CompiledOp::Emit { target_idx }
                }
                Op::Drop => CompiledOp::Drop,
            };

            steps.push(compiled);
            step_ids.push(node_id.clone());
        }

        let inboxes = (0..steps.len()).map(|_| Vec::new()).collect();

        Ok(Self {
            steps,
            step_ids,
            inboxes,
            stream_to_step,
            target_names,
            stateful_steps,
            bindings,
            optimization_report,
        })
    }

    /// Returns the optimization report produced when compiling this engine.
    pub fn optimization_report(&self) -> &OptimizationReport {
        &self.optimization_report
    }

    /// Returns the compiled step count.
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    /// Returns the number of stateful aggregate operators inside this region.
    pub fn stateful_count(&self) -> usize {
        self.stateful_steps.len()
    }

    /// Returns the node IDs in compiled topological execution order.
    pub fn step_ids(&self) -> &[NodeId] {
        &self.step_ids
    }

    /// Resolves a logical source stream name to its dense step index.
    pub fn stream_index(&self, stream: &str) -> Option<StepIdx> {
        self.stream_to_step.get(stream).copied()
    }

    /// Returns the ordered emit target names corresponding to `target_idx` values passed to emit callbacks.
    pub fn target_names(&self) -> &[String] {
        &self.target_names
    }

    /// Returns `true` if any stateful aggregate operator currently holds retained contexts.
    pub fn has_buffered_state(&self) -> bool {
        self.stateful_steps
            .iter()
            .any(|&idx| matches!(&self.steps[idx], CompiledOp::Aggregate { core, .. } if !core.is_empty()))
    }

    /// Returns the shortest bucket window across all stateful aggregate operators, if any exist.
    pub fn shortest_window_secs(&self) -> Option<u64> {
        self.stateful_steps
            .iter()
            .filter_map(|&idx| match &self.steps[idx] {
                CompiledOp::Aggregate { core, .. } => Some(core.bucket_width_secs().get()),
                _ => None,
            })
            .min()
    }

    /// Returns the bucket widths (in seconds) of all stateful aggregate operators, keyed by node ID.
    pub fn stateful_window_widths(&self) -> BTreeMap<NodeId, u64> {
        let mut widths = BTreeMap::new();
        for &idx in &self.stateful_steps {
            if let CompiledOp::Aggregate { core, .. } = &self.steps[idx] {
                widths.insert(self.step_ids[idx].clone(), core.bucket_width_secs().get());
            }
        }
        widths
    }

    /// Extracts live [`AggregateCore`] state for every stateful node in `node_ids`, replacing them with empty cores.
    ///
    /// Used by generation switching to transfer unchanged stateful operators atomically across generations with zero
    /// data loss.
    pub fn take_stateful_cores(
        &mut self, node_ids: &std::collections::BTreeSet<NodeId>,
    ) -> BTreeMap<NodeId, AggregateCore<()>> {
        let mut taken = BTreeMap::new();
        for &idx in &self.stateful_steps {
            let id = &self.step_ids[idx];
            if !node_ids.contains(id) {
                continue;
            }
            if let CompiledOp::Aggregate { core, .. } = &mut self.steps[idx] {
                let replacement = AggregateCore::new(
                    core.bucket_width_secs(),
                    core.context_limit(),
                    None,
                    core.hist_config().clone(),
                );
                let live = std::mem::replace(core, replacement);
                taken.insert(id.clone(), live);
            }
        }
        taken
    }

    /// Restores transferred [`AggregateCore`] state into matching stateful nodes by node ID.
    pub fn restore_stateful_cores(&mut self, mut cores: BTreeMap<NodeId, AggregateCore<()>>) -> usize {
        let mut restored = 0;
        for &idx in &self.stateful_steps {
            let id = &self.step_ids[idx];
            if let Some(live) = cores.remove(id) {
                if let CompiledOp::Aggregate { core, .. } = &mut self.steps[idx] {
                    *core = live;
                    restored += 1;
                }
            }
        }
        restored
    }

    /// Snapshots retained contexts across all stateful aggregate operators in this region.
    pub fn snapshot_contexts(&self) -> Vec<AggregateContextSnapshotEntry> {
        let mut out = Vec::new();
        for &idx in &self.stateful_steps {
            if let CompiledOp::Aggregate { core, .. } = &self.steps[idx] {
                out.extend(core.snapshot_contexts());
            }
        }
        out
    }

    /// Updates runtime bindings (host tags and mirroring signals) in place without rebuilding the schedule.
    pub fn set_bindings(&mut self, bindings: RuntimeBindings) {
        self.bindings = bindings;
        for step in &mut self.steps {
            match step {
                CompiledOp::EnrichHostTags { binding, kernel, .. } => {
                    kernel.set_host_tags(self.bindings.host_tags.get(binding).cloned());
                }
                CompiledOp::RouteFilter { kind, kernel, .. } => {
                    *kernel = compile_route_filter(kind, &self.bindings);
                }
                _ => {}
            }
        }
    }

    /// Executes one batch entering at `stream_idx` at `timestamp_secs`.
    ///
    /// Every metric reaching an `Emit` step is handed synchronously to `emit(target_idx, metric)`.
    pub fn ingest(
        &mut self, stream_idx: StepIdx, timestamp_secs: u64, metrics: impl IntoIterator<Item = Metric>,
        mut emit: impl FnMut(TargetIdx, Metric),
    ) {
        self.inboxes[stream_idx].extend(metrics);
        self.run_from(stream_idx, timestamp_secs, &mut emit);
    }

    /// Flushes every stateful `Aggregate` step in topological order at `current_time_secs`, propagating emitted metrics
    /// through all downstream operators (including any downstream chained `Aggregate`).
    pub fn flush(&mut self, current_time_secs: u64, shutdown: bool, mut emit: impl FnMut(TargetIdx, Metric)) {
        for idx in 0..self.steps.len() {
            if !self.inboxes[idx].is_empty() {
                let batch = std::mem::take(&mut self.inboxes[idx]);
                self.execute_step(idx, current_time_secs, batch, &mut emit);
            }

            let next_target = match &mut self.steps[idx] {
                CompiledOp::Aggregate {
                    core,
                    flush_open_windows,
                    next,
                } => {
                    if !core.is_empty() {
                        let flush_open = shutdown && *flush_open_windows;
                        if let Some(next_idx) = *next {
                            let dest = &mut self.inboxes[next_idx];
                            core.flush_into(current_time_secs, flush_open, |metric| dest.push(metric));
                        } else {
                            core.flush_into(current_time_secs, flush_open, |_| {});
                        }
                    }
                    *next
                }
                _ => None,
            };
            let _ = next_target;
        }
    }

    fn run_from(&mut self, start_idx: StepIdx, timestamp_secs: u64, emit: &mut impl FnMut(TargetIdx, Metric)) {
        for idx in start_idx..self.steps.len() {
            if self.inboxes[idx].is_empty() {
                continue;
            }
            let batch = std::mem::take(&mut self.inboxes[idx]);
            self.execute_step(idx, timestamp_secs, batch, emit);
        }
    }

    fn execute_step(
        &mut self, idx: StepIdx, timestamp_secs: u64, mut batch: Vec<Metric>, emit: &mut impl FnMut(TargetIdx, Metric),
    ) {
        match &mut self.steps[idx] {
            CompiledOp::Source { next } | CompiledOp::Merge { next } => {
                if let Some(next_idx) = *next {
                    self.inboxes[next_idx].extend(batch);
                }
            }
            CompiledOp::Tee { targets } => {
                if let Some((&last, rest)) = targets.split_last() {
                    for &target in rest {
                        self.inboxes[target].extend(batch.iter().cloned());
                    }
                    self.inboxes[last].extend(batch);
                }
            }
            CompiledOp::Map { kernel, next } => {
                for metric in &mut batch {
                    kernel.apply(metric);
                }
                if let Some(next_idx) = *next {
                    self.inboxes[next_idx].extend(batch);
                }
            }
            CompiledOp::PrefixFilter { kernel, next } => {
                batch.retain_mut(|metric| kernel.apply(metric));
                if let Some(next_idx) = *next {
                    self.inboxes[next_idx].extend(batch);
                }
            }
            CompiledOp::TimestampSplit {
                agg_next,
                passthrough_next,
            } => {
                for metric in batch {
                    let outcome = split_for_no_aggregation(metric);
                    if let (Some(m), Some(next_idx)) = (outcome.aggregate, *agg_next) {
                        self.inboxes[next_idx].push(m);
                    }
                    if let (Some(m), Some(next_idx)) = (outcome.passthrough, *passthrough_next) {
                        self.inboxes[next_idx].push(m);
                    }
                }
            }
            CompiledOp::TagFilter { kernel, next } => {
                for metric in &mut batch {
                    kernel.apply(metric);
                }
                if let Some(next_idx) = *next {
                    self.inboxes[next_idx].extend(batch);
                }
            }
            CompiledOp::Aggregate { core, .. } => {
                for metric in batch {
                    let _ = core.insert(timestamp_secs, metric);
                }
            }
            CompiledOp::PostAggregateFilter { kernel, next } => {
                batch.retain(|metric| kernel.should_keep(metric));
                if let Some(next_idx) = *next {
                    self.inboxes[next_idx].extend(batch);
                }
            }
            CompiledOp::EnrichHostTags { kernel, next, .. } => {
                if kernel.is_active() {
                    for metric in &mut batch {
                        kernel.apply(metric);
                    }
                }
                if let Some(next_idx) = *next {
                    self.inboxes[next_idx].extend(batch);
                }
            }
            CompiledOp::RouteFilter { kernel, next, .. } => {
                batch.retain(|metric| kernel.should_forward(metric));
                if let Some(next_idx) = *next {
                    self.inboxes[next_idx].extend(batch);
                }
            }
            CompiledOp::Emit { target_idx } => {
                let target_idx = *target_idx;
                for metric in batch {
                    emit(target_idx, metric);
                }
            }
            CompiledOp::Drop => {}
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
        event::metric::context::Context,
        tags::{SharedTagSet, Tag},
    };
    use saluki_metric_kernels::{
        filterlist::MetricNameFilter,
        histogram::HistogramConfiguration,
        routing_filter::MetricMirroring,
        tag_filter::{FilterAction, MetricTagFilterEntry},
    };

    use super::*;
    use crate::{
        interpreter::ReferenceInterpreter,
        ir::MetricPlan,
        lowering::{
            lower_native_pipeline, NativePipelineConfig, RoutingPolicySpec, HOST_TAGS_BINDING, MRF_SIGNAL_BINDING,
            STREAM_DOGSTATSD, STREAM_OTLP,
        },
        validate::validate_plan,
    };

    fn collect_engine_outputs(
        engine: &mut FusedMetricEngine, stream: &str, ts: u64, batch: Vec<Metric>, flush_ts: Option<u64>,
    ) -> BTreeMap<String, Vec<Metric>> {
        let mut emitted: BTreeMap<String, Vec<Metric>> = BTreeMap::new();
        let names = engine.target_names().to_vec();
        let idx = engine.stream_index(stream).expect("stream exists");
        engine.ingest(idx, ts, batch, |t_idx, metric| {
            emitted.entry(names[t_idx].clone()).or_default().push(metric);
        });
        if let Some(ft) = flush_ts {
            engine.flush(ft, false, |t_idx, metric| {
                emitted.entry(names[t_idx].clone()).or_default().push(metric);
            });
        }
        emitted
    }

    #[test]
    fn differential_fused_engine_matches_interpreter_on_full_native_plan() {
        let config = NativePipelineConfig {
            dogstatsd_enabled: true,
            passthrough_streams: vec![STREAM_OTLP.to_string()],
            metric_prefix: "svc".to_string(),
            metric_prefix_blocklist: vec!["datadog.".to_string()],
            metric_filter: MetricNameFilter {
                values: vec!["svc.drop_me".to_string()],
                match_prefix: false,
                prefix_rules: vec![],
            },
            tag_filter_entries: vec![MetricTagFilterEntry {
                metric_name: "svc.hits".to_string(),
                action: FilterAction::Exclude,
                tags: vec!["ephemeral".to_string()],
            }],
            host_tags_enabled: true,
            routing_policies: vec![RoutingPolicySpec {
                target: "routing_0".to_string(),
                metric_allowlist: vec!["svc.hits".to_string()],
                metric_prefix_allowlist: vec![],
            }],
            mrf_branch_configured: true,
            mrf_enabled: true,
            mrf_mirroring: MetricMirroring {
                enabled: false,
                allowlist: vec![],
            },
            ..NativePipelineConfig::default()
        };

        let validated = validate_plan(&lower_native_pipeline(&config)).unwrap();
        let bindings = RuntimeBindings::default()
            .with_host_tags(HOST_TAGS_BINDING, SharedTagSet::from_iter([Tag::from("az:us-east-1a")]))
            .with_mirroring_signal(
                MRF_SIGNAL_BINDING,
                MetricMirroring {
                    enabled: true,
                    allowlist: vec!["svc.hits".to_string()],
                },
            );

        let mut interp = ReferenceInterpreter::new(&validated, bindings.clone()).unwrap();
        let mut engine = FusedMetricEngine::compile(&validated, bindings).unwrap();

        let batch = vec![
            Metric::counter(Context::from_static_parts("hits", &["ephemeral:1", "env:prod"]), 4.0),
            Metric::gauge("drop_me", 1.0),
            Metric::gauge("ts.value", (1000, 9.0)),
        ];

        let mut expected = interp.ingest(STREAM_DOGSTATSD, 12, batch.clone()).unwrap();
        for (target, metrics) in interp.flush(20, false) {
            expected.entry(target).or_default().extend(metrics);
        }

        let mut actual = collect_engine_outputs(&mut engine, STREAM_DOGSTATSD, 12, batch, Some(20));

        for list in expected.values_mut().chain(actual.values_mut()) {
            list.sort_by(|a, b| a.context().name().cmp(b.context().name()));
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn supports_zero_one_and_two_stateful_operators_natively() {
        // 0 stateful operators (pure routing).
        let zero_plan = MetricPlan::builder()
            .node(
                "in",
                Op::Source {
                    stream: "otlp".to_string(),
                },
            )
            .node(
                "out",
                Op::Emit {
                    target: "primary".to_string(),
                },
            )
            .edge("in", "out")
            .build();
        let zero_validated = validate_plan(&zero_plan).unwrap();
        let mut zero_engine = FusedMetricEngine::compile(&zero_validated, RuntimeBindings::default()).unwrap();
        assert_eq!(zero_engine.stateful_count(), 0);
        assert_eq!(zero_engine.shortest_window_secs(), None);
        let out = collect_engine_outputs(&mut zero_engine, "otlp", 10, vec![Metric::gauge("g", 1.0)], None);
        assert_eq!(out["primary"].len(), 1);

        // 2 chained stateful operators (10s -> 60s).
        let two_plan = MetricPlan::builder()
            .node(
                "in",
                Op::Source {
                    stream: "dsd".to_string(),
                },
            )
            .node(
                "agg1",
                Op::Aggregate {
                    window_duration_secs: NonZeroU64::new(10).unwrap(),
                    context_limit: 100,
                    counter_expiry_secs: 0,
                    flush_open_windows: true,
                    histogram: HistogramConfiguration::default(),
                },
            )
            .node(
                "agg2",
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
            .edge("in", "agg1")
            .edge("agg1", "agg2")
            .edge("agg2", "out")
            .build();
        let two_validated = validate_plan(&two_plan).unwrap();
        let mut two_engine = FusedMetricEngine::compile(&two_validated, RuntimeBindings::default()).unwrap();
        assert_eq!(two_engine.stateful_count(), 2);
        assert_eq!(two_engine.shortest_window_secs(), Some(10));

        let idx = two_engine.stream_index("dsd").unwrap();
        let mut emitted = Vec::new();
        two_engine.ingest(idx, 12, vec![Metric::gauge("temp", 5.0)], |_, m| emitted.push(m));
        assert!(emitted.is_empty());
        two_engine.flush(20, false, |_, m| emitted.push(m));
        assert!(emitted.is_empty());
        two_engine.flush(60, false, |_, m| emitted.push(m));
        assert_eq!(emitted.len(), 1);
    }

    mod proptest_sim {
        use proptest::prelude::*;
        use saluki_core::data_model::event::metric::MetricValues;
        use saluki_metric_kernels::tag_filter::{MetricTagValueAllowlistEntry, TagValueMismatchAction};

        use super::*;
        use crate::lowering::TARGET_PRIMARY;

        #[derive(Clone, Debug)]
        enum SimMetricKind {
            Counter(u16, bool),
            Gauge(u16, bool),
            Histogram(u16),
            Distribution(u16),
            Set(u8),
        }

        #[derive(Clone, Debug)]
        struct SimMetricSpec {
            name_idx: usize,
            tag_a: u8,
            tag_b: u8,
            kind: SimMetricKind,
        }

        #[derive(Clone, Debug)]
        enum SimEvent {
            Ingest {
                to_otlp: bool,
                dt: u64,
                metrics: Vec<SimMetricSpec>,
            },
            Flush {
                dt: u64,
            },
            ToggleMrf {
                enabled: bool,
                allow_all: bool,
            },
            ToggleHostTags {
                active: bool,
            },
        }

        const NAMES: &[&str] = &["hits", "latency", "drop_me", "datadog.agent.up", "special"];

        fn build_metric(spec: &SimMetricSpec) -> Metric {
            let name = NAMES[spec.name_idx % NAMES.len()];
            let t1 = format!("ephemeral:{}", spec.tag_a % 3);
            let t2 = format!("env:e{}", spec.tag_b % 2);
            let mut tags = saluki_core::data_model::tags::TagSet::default();
            tags.insert_tag(t1.as_str());
            tags.insert_tag(t2.as_str());
            let ctx = Context::from_parts(name.to_string(), tags);

            match spec.kind {
                SimMetricKind::Counter(v, true) => Metric::counter(ctx, (1_700_000_000u64, f64::from(v) + 1.0)),
                SimMetricKind::Counter(v, false) => Metric::counter(ctx, f64::from(v) + 1.0),
                SimMetricKind::Gauge(v, true) => Metric::gauge(ctx, (1_700_000_000u64, f64::from(v))),
                SimMetricKind::Gauge(v, false) => Metric::gauge(ctx, f64::from(v)),
                SimMetricKind::Histogram(v) => Metric::histogram(ctx, f64::from(v) + 1.0),
                SimMetricKind::Distribution(v) => Metric::distribution(ctx, f64::from(v) + 1.0),
                SimMetricKind::Set(s) => Metric::set(ctx, format!("v{}", s % 4)),
            }
        }

        fn metric_spec_strategy() -> impl Strategy<Value = SimMetricSpec> {
            let kind = prop_oneof![
                (0u16..50, any::<bool>()).prop_map(|(v, ts)| SimMetricKind::Counter(v, ts)),
                (0u16..50, any::<bool>()).prop_map(|(v, ts)| SimMetricKind::Gauge(v, ts)),
                (0u16..50).prop_map(SimMetricKind::Histogram),
                (0u16..50).prop_map(SimMetricKind::Distribution),
                (0u8..4).prop_map(SimMetricKind::Set),
            ];
            (0usize..NAMES.len(), 0u8..3, 0u8..2, kind).prop_map(|(name_idx, tag_a, tag_b, kind)| SimMetricSpec {
                name_idx,
                tag_a,
                tag_b,
                kind,
            })
        }

        fn sim_event_strategy() -> impl Strategy<Value = SimEvent> {
            prop_oneof![
                (
                    any::<bool>(),
                    0u64..8,
                    prop::collection::vec(metric_spec_strategy(), 1..6)
                )
                    .prop_map(|(to_otlp, dt, metrics)| SimEvent::Ingest { to_otlp, dt, metrics }),
                (1u64..16).prop_map(|dt| SimEvent::Flush { dt }),
                (any::<bool>(), any::<bool>())
                    .prop_map(|(enabled, allow_all)| SimEvent::ToggleMrf { enabled, allow_all }),
                any::<bool>().prop_map(|active| SimEvent::ToggleHostTags { active }),
            ]
        }

        fn canonical_key(m: &Metric) -> (String, Vec<String>, &'static str, String) {
            let mut tags: Vec<String> = m.context().tags().into_iter().map(|t| t.as_str().to_string()).collect();
            tags.sort();
            let kind = match m.values() {
                MetricValues::Counter(_) => "counter",
                MetricValues::Rate(_, _) => "rate",
                MetricValues::Gauge(_) => "gauge",
                MetricValues::Set(_) => "set",
                MetricValues::Histogram(_) => "histogram",
                MetricValues::Distribution(_) => "distribution",
            };
            (m.context().name().to_string(), tags, kind, format!("{:?}", m.values()))
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// Deterministic simulation: across randomized native pipeline configurations, mixed DogStatsD/OTLP
            /// streams, timestamped & non-timestamped metrics, binding toggles, and flush schedules, the optimized
            /// compiled `FusedMetricEngine` must produce the exact same metrics on every output target as the
            /// unoptimized `ReferenceInterpreter`.
            #[test]
            fn fused_engine_matches_reference_interpreter_across_random_timelines(
                no_agg in any::<bool>(),
                use_prefix in any::<bool>(),
                use_filter in any::<bool>(),
                use_tag_filter in any::<bool>(),
                window_secs in prop_oneof![Just(5u64), Just(10u64), Just(15u64)],
                counter_expiry in prop_oneof![Just(0u64), Just(20u64)],
                mrf_enabled in any::<bool>(),
                events in prop::collection::vec(sim_event_strategy(), 1..25),
            ) {
                let prefix = if use_prefix { "svc".to_string() } else { String::new() };
                let blocked_name = if use_prefix { "svc.drop_me" } else { "drop_me" };
                let hits_name = if use_prefix { "svc.hits" } else { "hits" };

                let config = NativePipelineConfig {
                    dogstatsd_enabled: true,
                    passthrough_streams: vec![STREAM_OTLP.to_string()],
                    metric_prefix: prefix,
                    metric_prefix_blocklist: vec!["datadog.".to_string()],
                    metric_filter: if use_filter {
                        MetricNameFilter {
                            values: vec![blocked_name.to_string()],
                            match_prefix: false,
                            prefix_rules: vec![],
                        }
                    } else {
                        MetricNameFilter::default()
                    },
                    no_aggregation_pipeline: no_agg,
                    tag_filter_entries: if use_tag_filter {
                        vec![MetricTagFilterEntry {
                            metric_name: hits_name.to_string(),
                            action: FilterAction::Exclude,
                            tags: vec!["ephemeral".to_string()],
                        }]
                    } else {
                        vec![]
                    },
                    tag_value_allowlists: if use_tag_filter {
                        vec![MetricTagValueAllowlistEntry {
                            metric_prefix: hits_name.to_string(),
                            tag_name: "env".to_string(),
                            values: vec!["e0".to_string()],
                            on_miss: TagValueMismatchAction::Replace,
                            replacement: "other".to_string(),
                        }]
                    } else {
                        vec![]
                    },
                    aggregate_window_duration_secs: NonZeroU64::new(window_secs).unwrap(),
                    aggregate_counter_expiry_secs: counter_expiry,
                    aggregate_flush_open_windows: true,
                    host_tags_enabled: true,
                    routing_policies: vec![RoutingPolicySpec {
                        target: "routing_0".to_string(),
                        metric_allowlist: vec![hits_name.to_string()],
                        metric_prefix_allowlist: vec!["datadog.".to_string()],
                    }],
                    mrf_branch_configured: true,
                    mrf_enabled,
                    mrf_mirroring: MetricMirroring {
                        enabled: false,
                        allowlist: vec![],
                    },
                    ..NativePipelineConfig::default()
                };

                let validated = validate_plan(&lower_native_pipeline(&config)).unwrap();
                let mut bindings = RuntimeBindings::default()
                    .with_host_tags(HOST_TAGS_BINDING, SharedTagSet::from_iter([Tag::from("host:sim")]));

                let mut interp = ReferenceInterpreter::new(&validated, bindings.clone()).unwrap();
                let mut engine = FusedMetricEngine::compile(&validated, bindings.clone()).unwrap();
                let target_names = engine.target_names().to_vec();

                let mut now = 1_700_000_000u64;

                let assert_step_eq =
                    |ts: u64, expected: BTreeMap<String, Vec<Metric>>, actual: BTreeMap<String, Vec<Metric>>| {
                        let mut all_targets: std::collections::BTreeSet<String> = expected.keys().cloned().collect();
                        all_targets.extend(actual.keys().cloned());
                        for target in all_targets {
                            let mut exp: Vec<_> =
                                expected.get(&target).into_iter().flatten().map(canonical_key).collect();
                            let mut act: Vec<_> =
                                actual.get(&target).into_iter().flatten().map(canonical_key).collect();
                            exp.sort();
                            act.sort();
                            prop_assert_eq!(act, exp, "mismatch on target `{}` at t={}", target, ts);
                        }
                        Ok(())
                    };

                for event in events {
                    match event {
                        SimEvent::Ingest { to_otlp, dt, metrics } => {
                            now += dt;
                            let stream = if to_otlp { STREAM_OTLP } else { STREAM_DOGSTATSD };
                            let batch: Vec<Metric> = metrics.iter().map(build_metric).collect();

                            let expected = interp.ingest(stream, now, batch.clone()).unwrap();
                            let mut actual: BTreeMap<String, Vec<Metric>> = BTreeMap::new();
                            let s_idx = engine.stream_index(stream).unwrap();
                            engine.ingest(s_idx, now, batch, |t_idx, m| {
                                actual.entry(target_names[t_idx].clone()).or_default().push(m);
                            });
                            assert_step_eq(now, expected, actual)?;
                        }
                        SimEvent::Flush { dt } => {
                            now += dt;
                            let expected = interp.flush(now, false);
                            let mut actual: BTreeMap<String, Vec<Metric>> = BTreeMap::new();
                            engine.flush(now, false, |t_idx, m| {
                                actual.entry(target_names[t_idx].clone()).or_default().push(m);
                            });
                            assert_step_eq(now, expected, actual)?;
                        }
                        SimEvent::ToggleMrf { enabled, allow_all } => {
                            let policy = MetricMirroring {
                                enabled,
                                allowlist: if allow_all { vec![] } else { vec![hits_name.to_string()] },
                            };
                            bindings.mirroring_signals.insert(MRF_SIGNAL_BINDING.to_string(), policy);
                            interp.set_bindings(bindings.clone());
                            engine.set_bindings(bindings.clone());
                        }
                        SimEvent::ToggleHostTags { active } => {
                            if active {
                                bindings.host_tags.insert(
                                    HOST_TAGS_BINDING.to_string(),
                                    SharedTagSet::from_iter([Tag::from("host:sim")]),
                                );
                            } else {
                                bindings.host_tags.remove(HOST_TAGS_BINDING);
                            }
                            interp.set_bindings(bindings.clone());
                            engine.set_bindings(bindings.clone());
                        }
                    }
                }

                // Final shutdown flush must match identically on every target (including TARGET_PRIMARY).
                let expected = interp.flush(now + 1, true);
                let mut actual: BTreeMap<String, Vec<Metric>> = BTreeMap::new();
                engine.flush(now + 1, true, |t_idx, m| {
                    actual.entry(target_names[t_idx].clone()).or_default().push(m);
                });
                let _ = TARGET_PRIMARY;
                assert_step_eq(now + 1, expected, actual)?;
            }
        }
    }
}
