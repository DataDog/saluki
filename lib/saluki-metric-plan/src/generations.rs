//! Zero-loss live generation switching and exact bucket-boundary cuts for [`FusedMetricEngine`].
//!
//! Incoming batches are stamped at the single admission point with an [`AdmissionStamp`] (`timestamp_secs`, `epoch`,
//! `generation`)—side-carried rather than bloating shared `EventsBuffer` headers.
//!
//! When a new plan is installed:
//! - **Stateless-only changes** (for example, metric or tag filterlist updates) switch atomically in place and
//!   transfer every unchanged stateful [`AggregateCore`][saluki_metric_kernels::aggregate::AggregateCore] directly to
//!   the new generation.
//! - **Stateful changes** (for example, changing an aggregation window from 10s to 15s) schedule an exact cut at the
//!   next shared bucket boundary (`t_cut`, a multiple of the LCM of affected old and new window durations). Metrics
//!   stamped `< t_cut` route to the old generation; metrics stamped `>= t_cut` route to the new generation; at
//!   `t_cut`, the old generation drains all closed buckets and retires with zero dropped or duplicated metrics.

use std::collections::{BTreeMap, BTreeSet};

use saluki_core::data_model::event::metric::Metric;
use saluki_error::{generic_error, GenericError};

use crate::{
    effects::operator_spec,
    engine::FusedMetricEngine,
    interpreter::RuntimeBindings,
    ir::{NodeId, Op},
    telemetry::{OperatorTelemetry, RegionTelemetryDelta},
    validate::ValidatedPlan,
};

/// Monotonically increasing identifier for an installed plan generation.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct GenerationId(pub u64);

impl GenerationId {
    /// Returns the next generation ID.
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// Side-carried admission metadata stamped at the region's single admission point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionStamp {
    /// Wall-clock admission timestamp in Unix seconds (`t_a`).
    pub timestamp_secs: u64,

    /// Monotonically increasing transition epoch.
    pub epoch: u64,

    /// Generation that accepted this batch.
    pub generation: GenerationId,
}

/// Semantic diff between an active [`ValidatedPlan`] and a candidate replacement.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlanDiff {
    /// Stateful nodes (`Op::Aggregate`) with identical ID and parameters whose state transfers directly.
    pub unchanged_stateful: BTreeSet<NodeId>,

    /// Stateful nodes added, removed, or modified in parameters, requiring a bucket-boundary cut.
    pub changed_stateful: BTreeSet<NodeId>,

    /// Stateless nodes with identical ID and parameters.
    pub unchanged_stateless: BTreeSet<NodeId>,

    /// Stateless nodes added, removed, or modified.
    pub changed_stateless: BTreeSet<NodeId>,
}

impl PlanDiff {
    /// Returns `true` when installing the new plan requires a bucket-boundary cut rather than an immediate swap.
    pub fn requires_cut(&self) -> bool {
        !self.changed_stateful.is_empty()
    }
}

/// Computes the semantic diff between `old` and `new` validated plans.
pub fn diff_plans(old: &ValidatedPlan, new: &ValidatedPlan) -> PlanDiff {
    let old_nodes: BTreeMap<&NodeId, &Op> = old.plan().nodes.iter().map(|n| (&n.id, &n.op)).collect();
    let new_nodes: BTreeMap<&NodeId, &Op> = new.plan().nodes.iter().map(|n| (&n.id, &n.op)).collect();

    let all_ids: BTreeSet<&NodeId> = old_nodes.keys().chain(new_nodes.keys()).copied().collect();
    let mut diff = PlanDiff::default();

    for id in all_ids {
        match (old_nodes.get(id), new_nodes.get(id)) {
            (Some(old_op), Some(new_op)) => {
                let is_stateful = operator_spec(old_op).stateful || operator_spec(new_op).stateful;
                if old_op == new_op {
                    if is_stateful {
                        diff.unchanged_stateful.insert(id.clone());
                    } else {
                        diff.unchanged_stateless.insert(id.clone());
                    }
                } else if is_stateful {
                    diff.changed_stateful.insert(id.clone());
                } else {
                    diff.changed_stateless.insert(id.clone());
                }
            }
            (Some(op), None) | (None, Some(op)) => {
                if operator_spec(op).stateful {
                    diff.changed_stateful.insert(id.clone());
                } else {
                    diff.changed_stateless.insert(id.clone());
                }
            }
            (None, None) => {}
        }
    }

    diff
}

/// Computes the earliest shared bucket boundary strictly greater than `now_secs` that aligns with every window width
/// in `window_widths_secs`.
///
/// If `window_widths_secs` is empty, returns `now_secs.saturating_add(1)`.
pub fn compute_shared_cut_boundary(window_widths_secs: &[u64], now_secs: u64) -> u64 {
    let mut stride: u64 = 1;
    for &width in window_widths_secs {
        if width > 0 {
            stride = lcm(stride, width);
        }
    }

    let next_multiple = ((now_secs / stride).saturating_add(1)).saturating_mul(stride);
    next_multiple.max(now_secs.saturating_add(1))
}

const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

const fn lcm(a: u64, b: u64) -> u64 {
    if a == 0 || b == 0 {
        return 0;
    }
    (a / gcd(a, b)).saturating_mul(b)
}

/// Result of installing a new plan generation via [`GenerationManager::install`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallOutcome {
    /// Plan switched atomically immediately (stateless changes only), transferring unchanged stateful cores.
    SwitchedImmediately {
        /// Newly active generation ID.
        generation: GenerationId,
        /// Number of unchanged stateful cores transferred without reset.
        reused_stateful_nodes: usize,
    },

    /// Stateful change detected; cut scheduled at the exact shared bucket boundary `cut_timestamp_secs`.
    ScheduledCut {
        /// Generation ID that will take over at `cut_timestamp_secs`.
        generation: GenerationId,
        /// Exact Unix timestamp (seconds) where traffic cuts from old to new generation.
        cut_timestamp_secs: u64,
    },
}

struct ActiveGeneration {
    id: GenerationId,
    plan: ValidatedPlan,
    engine: FusedMetricEngine,
}

struct PendingCut {
    cut_timestamp_secs: u64,
    next: ActiveGeneration,
}

/// Manages live plan generations, admission stamping, state transfer, bucket-boundary cuts, and rollback.
pub struct GenerationManager {
    active: ActiveGeneration,
    pending_cut: Option<PendingCut>,
    last_known_good: Option<ValidatedPlan>,
    bindings: RuntimeBindings,
    epoch: u64,
    next_gen_id: GenerationId,
    max_admitted_ts: u64,
    pending_telemetry: RegionTelemetryDelta,
}

impl GenerationManager {
    /// Creates a new `GenerationManager` starting at generation `1`.
    pub fn new(initial_plan: ValidatedPlan, bindings: RuntimeBindings) -> Result<Self, GenericError> {
        let engine = FusedMetricEngine::compile(&initial_plan, bindings.clone())?;
        Ok(Self {
            active: ActiveGeneration {
                id: GenerationId(1),
                plan: initial_plan,
                engine,
            },
            pending_cut: None,
            last_known_good: None,
            bindings,
            epoch: 1,
            next_gen_id: GenerationId(2),
            max_admitted_ts: 0,
            pending_telemetry: RegionTelemetryDelta::default(),
        })
    }

    /// Returns the currently active generation ID.
    pub fn active_generation(&self) -> GenerationId {
        self.active.id
    }

    /// Returns the current transition epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the scheduled cut timestamp if a stateful cut is currently pending.
    pub fn pending_cut_timestamp(&self) -> Option<u64> {
        self.pending_cut.as_ref().map(|c| c.cut_timestamp_secs)
    }

    /// Returns a reference to the active compiled engine.
    pub fn active_engine(&self) -> &FusedMetricEngine {
        &self.active.engine
    }

    /// Snapshots retained contexts across all stateful aggregate operators in the active generation (and any staged
    /// generation if it has already admitted metrics).
    pub fn snapshot_contexts(&self) -> Vec<saluki_metric_kernels::aggregate::AggregateContextSnapshotEntry> {
        let mut out = self.active.engine.snapshot_contexts();
        if let Some(pending) = &self.pending_cut {
            out.extend(pending.next.engine.snapshot_contexts());
        }
        out
    }

    /// Returns whether either the active or pending generation holds buffered stateful metrics.
    pub fn has_buffered_state(&self) -> bool {
        self.active.engine.has_buffered_state()
            || self
                .pending_cut
                .as_ref()
                .is_some_and(|c| c.next.engine.has_buffered_state())
    }

    /// Returns the total number of stateful operators in the active generation (or pending cut).
    pub fn has_stateful_operators(&self) -> bool {
        self.active.engine.stateful_count() > 0
            || self
                .pending_cut
                .as_ref()
                .is_some_and(|c| c.next.engine.stateful_count() > 0)
    }

    /// Drains accumulated per-operator telemetry and live-reload update counts since the last drain.
    ///
    /// When `include_gauges` is `true`, rule counts (including `0` for pruned no-op filter nodes) and active-context
    /// snapshots are included so RAR/status gauges initialize and reset cleanly.
    pub fn take_telemetry(&mut self, include_gauges: bool) -> RegionTelemetryDelta {
        let mut delta = std::mem::take(&mut self.pending_telemetry);
        let mut active_ops = self.active.engine.take_telemetry(include_gauges);
        if include_gauges {
            let present: BTreeSet<NodeId> = active_ops.iter().map(|(id, _)| id.clone()).collect();
            for node in &self.active.plan.plan().nodes {
                if present.contains(&node.id) {
                    continue;
                }
                match &node.op {
                    Op::PrefixFilter { .. } => {
                        active_ops.push((
                            node.id.clone(),
                            OperatorTelemetry::PrefixFilter {
                                dropped_points: 0,
                                rule_count: 0,
                            },
                        ));
                    }
                    Op::TagFilter { .. } => {
                        active_ops.push((
                            node.id.clone(),
                            OperatorTelemetry::TagFilter {
                                tags_filtered: 0,
                                cache_hits: 0,
                                cache_misses: 0,
                                cache_evictions: 0,
                                rule_count: 0,
                            },
                        ));
                    }
                    _ => {}
                }
            }
        }
        delta.operators.extend(active_ops);
        delta
    }

    /// Updates runtime bindings on both the active generation and any staged cut generation.
    pub fn set_bindings(&mut self, bindings: RuntimeBindings) {
        self.bindings = bindings.clone();
        self.active.engine.set_bindings(bindings.clone());
        if let Some(pending) = self.pending_cut.as_mut() {
            pending.next.engine.set_bindings(bindings);
        }
    }

    /// Installs `new_plan` at `now_secs`.
    ///
    /// Stateless-only changes switch atomically in place while transferring all unchanged stateful cores.
    /// Stateful parameter changes schedule an exact cut at the next shared bucket boundary.
    pub fn install(&mut self, new_plan: ValidatedPlan, now_secs: u64) -> Result<InstallOutcome, GenericError> {
        // Cancel any unreached previous pending cut before staging the newer plan.
        self.pending_cut = None;

        let diff = diff_plans(&self.active.plan, &new_plan);
        let mut new_engine = FusedMetricEngine::compile(&new_plan, self.bindings.clone())?;
        let new_id = self.next_gen_id;
        self.next_gen_id = self.next_gen_id.next();

        if !diff.requires_cut() {
            record_filter_reconfig_updates(&self.active.plan, &new_plan, &mut self.pending_telemetry);
            self.pending_telemetry
                .operators
                .extend(self.active.engine.take_telemetry(false));

            let transferred = self.active.engine.take_stateful_cores(&diff.unchanged_stateful);
            let reused_stateful_nodes = new_engine.restore_stateful_cores(transferred);

            let previous = std::mem::replace(
                &mut self.active,
                ActiveGeneration {
                    id: new_id,
                    plan: new_plan,
                    engine: new_engine,
                },
            );
            self.last_known_good = Some(previous.plan);
            self.epoch = self.epoch.saturating_add(1);

            return Ok(InstallOutcome::SwitchedImmediately {
                generation: new_id,
                reused_stateful_nodes,
            });
        }

        // Compute the shared bucket boundary across ALL stateful windows in both generations so that every upstream
        // and downstream stateful operator closes cleanly at `cut_timestamp_secs` (even in chained multi-aggregate
        // DAGs), and clamp against `max_admitted_ts` so backward clock skew can never place `t_cut` behind an
        // already-admitted bucket.
        let mut widths: Vec<u64> = self.active.engine.stateful_window_widths().into_values().collect();
        widths.extend(new_engine.stateful_window_widths().into_values());
        let cut_timestamp_secs = compute_shared_cut_boundary(&widths, now_secs.max(self.max_admitted_ts));

        // Remember the active plan as last-known-good before staging the cut.
        self.last_known_good = Some(self.active.plan.clone());
        self.pending_cut = Some(PendingCut {
            cut_timestamp_secs,
            next: ActiveGeneration {
                id: new_id,
                plan: new_plan,
                engine: new_engine,
            },
        });

        Ok(InstallOutcome::ScheduledCut {
            generation: new_id,
            cut_timestamp_secs,
        })
    }

    /// Rolls back to the last-known-good plan using the same atomic/cut transition rules.
    pub fn rollback(&mut self, now_secs: u64) -> Result<InstallOutcome, GenericError> {
        let Some(lkg) = self.last_known_good.clone() else {
            return Err(generic_error!("no last-known-good plan available for rollback"));
        };
        self.install(lkg, now_secs)
    }

    /// Admits and processes a batch on `stream` at `timestamp_secs`, returning its [`AdmissionStamp`].
    ///
    /// If a stateful cut is pending and `timestamp_secs >= cut_timestamp_secs`, the cut completes atomically: the old
    /// generation drains all closed buckets at `cut_timestamp_secs` and retires before this batch executes on the new
    /// generation.
    pub fn ingest(
        &mut self, stream: &str, timestamp_secs: u64, metrics: impl IntoIterator<Item = Metric>,
        mut emit: impl FnMut(&str, Metric),
    ) -> Result<AdmissionStamp, GenericError> {
        if let Some(cut_ts) = self.pending_cut_timestamp() {
            if timestamp_secs >= cut_ts {
                self.complete_cut(cut_ts, false, &mut emit);
            }
        }

        let stream_idx = self
            .active
            .engine
            .stream_index(stream)
            .ok_or_else(|| generic_error!("active generation does not declare stream `{stream}`"))?;

        self.max_admitted_ts = self.max_admitted_ts.max(timestamp_secs);
        self.active
            .engine
            .ingest_named(stream_idx, timestamp_secs, metrics, emit);

        Ok(AdmissionStamp {
            timestamp_secs,
            epoch: self.epoch,
            generation: self.active.id,
        })
    }

    /// Flushes stateful operators at `current_time_secs` (or drains all generations on `shutdown`).
    pub fn flush(&mut self, current_time_secs: u64, shutdown: bool, mut emit: impl FnMut(&str, Metric)) {
        if let Some(cut_ts) = self.pending_cut_timestamp() {
            if current_time_secs >= cut_ts || shutdown {
                self.complete_cut(cut_ts, shutdown, &mut emit);
            }
        }

        self.active.engine.flush_named(current_time_secs, shutdown, emit);
    }

    fn complete_cut(&mut self, cut_timestamp_secs: u64, shutdown: bool, emit: &mut impl FnMut(&str, Metric)) {
        let Some(mut pending) = self.pending_cut.take() else {
            return;
        };

        // Drain all buckets closed up to `cut_timestamp_secs` from the retiring generation FIRST, so upstream
        // aggregates flush through any unchanged downstream aggregates before state is transferred.
        if self.active.engine.has_buffered_state() {
            self.active.engine.flush_named(cut_timestamp_secs, shutdown, emit);
        }

        record_filter_reconfig_updates(&self.active.plan, &pending.next.plan, &mut self.pending_telemetry);
        self.pending_telemetry
            .operators
            .extend(self.active.engine.take_telemetry(false));

        // Transfer any remaining state (such as idle counter keep-alive entries) from unchanged stateful nodes.
        let diff = diff_plans(&self.active.plan, &pending.next.plan);
        if !diff.unchanged_stateful.is_empty() {
            let transferred = self.active.engine.take_stateful_cores(&diff.unchanged_stateful);
            pending.next.engine.restore_stateful_cores(transferred);
        }

        self.active = pending.next;
        self.epoch = self.epoch.saturating_add(1);
    }
}

fn record_filter_reconfig_updates(old: &ValidatedPlan, new: &ValidatedPlan, delta: &mut RegionTelemetryDelta) {
    let old_ops: BTreeMap<&NodeId, &Op> = old.plan().nodes.iter().map(|n| (&n.id, &n.op)).collect();
    for node in &new.plan().nodes {
        let Some(prev) = old_ops.get(&node.id) else {
            continue;
        };
        if *prev == &node.op {
            continue;
        }
        match &node.op {
            Op::PrefixFilter { .. } => delta.filterlist_updates.push((node.id.clone(), 1)),
            Op::TagFilter { .. } => delta.tag_filterlist_updates.push((node.id.clone(), 1)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use saluki_core::data_model::event::metric::{Metric, MetricValues};
    use saluki_metric_kernels::filterlist::MetricNameFilter;

    use super::*;
    use crate::{
        lowering::{lower_native_pipeline, NativePipelineConfig, STREAM_DOGSTATSD, TARGET_PRIMARY},
        validate::validate_plan,
    };

    fn plan_with_window_expiry_and_filter(
        window_secs: u64, counter_expiry_secs: u64, blocked: &[&str],
    ) -> ValidatedPlan {
        let config = NativePipelineConfig {
            dogstatsd_enabled: true,
            aggregate_window_duration_secs: NonZeroU64::new(window_secs).unwrap(),
            aggregate_counter_expiry_secs: counter_expiry_secs,
            aggregate_flush_open_windows: true,
            metric_filter: MetricNameFilter {
                values: blocked.iter().map(|s| s.to_string()).collect(),
                match_prefix: false,
                prefix_rules: vec![],
            },
            ..NativePipelineConfig::default()
        };
        validate_plan(&lower_native_pipeline(&config)).unwrap()
    }

    fn plan_with_window_and_filter(window_secs: u64, blocked: &[&str]) -> ValidatedPlan {
        plan_with_window_expiry_and_filter(window_secs, 0, blocked)
    }

    fn sum_rate_counts(metrics: &[Metric], name: &str) -> f64 {
        metrics
            .iter()
            .filter(|m| m.context().name() == name)
            .map(|m| match m.values() {
                MetricValues::Rate(points, _) => points.into_iter().map(|(_, v)| v).sum::<f64>(),
                _ => 0.0,
            })
            .sum()
    }

    #[test]
    fn stateless_reload_switches_atomically_and_preserves_inflight_aggregate_state() {
        let mut mgr = GenerationManager::new(plan_with_window_and_filter(10, &[]), RuntimeBindings::default()).unwrap();

        // Admit a counter into bucket [10, 20) in generation 1.
        let mut emitted = Vec::new();
        let s1 = mgr
            .ingest(STREAM_DOGSTATSD, 12, vec![Metric::counter("hits", 3.0)], |t, m| {
                if t == TARGET_PRIMARY {
                    emitted.push(m);
                }
            })
            .unwrap();
        assert_eq!(s1.generation, GenerationId(1));

        // Install a stateless filterlist update (block `noise`) mid-bucket at t=15.
        let outcome = mgr.install(plan_with_window_and_filter(10, &["noise"]), 15).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::SwitchedImmediately {
                generation: GenerationId(2),
                reused_stateful_nodes: 1,
            }
        );

        // Admit another `hits` sample and a newly blocked `noise` sample in generation 2 (still bucket [10, 20)).
        let s2 = mgr
            .ingest(
                STREAM_DOGSTATSD,
                16,
                vec![Metric::counter("hits", 7.0), Metric::counter("noise", 99.0)],
                |t, m| {
                    if t == TARGET_PRIMARY {
                        emitted.push(m);
                    }
                },
            )
            .unwrap();
        assert_eq!(s2.generation, GenerationId(2));

        // Flush at t=20: `hits` equals 3.0 + 7.0 = 10.0 (zero loss across reload!), `noise` was dropped.
        mgr.flush(20, false, |t, m| {
            if t == TARGET_PRIMARY {
                emitted.push(m);
            }
        });
        assert_eq!(sum_rate_counts(&emitted, "hits"), 10.0);
        assert_eq!(sum_rate_counts(&emitted, "noise"), 0.0);
    }

    #[test]
    fn stateful_window_change_executes_exact_lcm_cut_with_zero_loss_or_duplication() {
        // Start with a 10s aggregation window.
        let mut mgr = GenerationManager::new(plan_with_window_and_filter(10, &[]), RuntimeBindings::default()).unwrap();
        let mut emitted = Vec::new();

        // Admit samples into [10, 20).
        mgr.ingest(STREAM_DOGSTATSD, 12, vec![Metric::counter("req", 2.0)], |t, m| {
            if t == TARGET_PRIMARY {
                emitted.push(m);
            }
        })
        .unwrap();

        // Request a stateful change to a 15s window at t=14 -> LCM(10, 15) = 30, so t_cut = 30!
        let outcome = mgr.install(plan_with_window_and_filter(15, &[]), 14).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::ScheduledCut {
                generation: GenerationId(2),
                cut_timestamp_secs: 30,
            }
        );

        // Continue admitting samples across [10, 20), [20, 30), and then into [30, 45) across the exact cut at t=30!
        let timeline = [
            (18, 3.0, GenerationId(1)),
            (22, 5.0, GenerationId(1)),
            (29, 7.0, GenerationId(1)),
            (30, 11.0, GenerationId(2)), // exact cut boundary -> generation 2 takes over!
            (35, 13.0, GenerationId(2)),
            (44, 17.0, GenerationId(2)),
        ];

        for (ts, val, expected_gen) in timeline {
            let stamp = mgr
                .ingest(STREAM_DOGSTATSD, ts, vec![Metric::counter("req", val)], |t, m| {
                    if t == TARGET_PRIMARY {
                        emitted.push(m);
                    }
                })
                .unwrap();
            assert_eq!(stamp.generation, expected_gen, "wrong generation at t={ts}");
        }

        // Flush at t=45 closes [30, 45) on generation 2.
        mgr.flush(45, false, |t, m| {
            if t == TARGET_PRIMARY {
                emitted.push(m);
            }
        });

        // Total admitted across the cut: 2 + 3 + 5 + 7 + 11 + 13 + 17 = 58.0.
        // Zero loss, zero duplication!
        assert_eq!(sum_rate_counts(&emitted, "req"), 58.0);

        // Verify exact bucket intervals: pre-cut rates have 10s interval, post-cut rate has 15s interval!
        let mut intervals: Vec<u64> = emitted
            .iter()
            .filter_map(|m| match m.values() {
                MetricValues::Rate(_, d) => Some(d.as_secs()),
                _ => None,
            })
            .collect();
        intervals.sort();
        assert_eq!(intervals, vec![10, 15]);
    }

    #[test]
    fn rollback_restores_last_known_good_plan() {
        let mut mgr = GenerationManager::new(plan_with_window_and_filter(10, &[]), RuntimeBindings::default()).unwrap();
        mgr.install(plan_with_window_and_filter(10, &["blocked"]), 10).unwrap();
        assert_eq!(mgr.active_generation(), GenerationId(2));

        let rolled_back = mgr.rollback(12).unwrap();
        assert_eq!(
            rolled_back,
            InstallOutcome::SwitchedImmediately {
                generation: GenerationId(3),
                reused_stateful_nodes: 1,
            }
        );

        let mut emitted = Vec::new();
        mgr.ingest(STREAM_DOGSTATSD, 12, vec![Metric::counter("blocked", 4.0)], |t, m| {
            if t == TARGET_PRIMARY {
                emitted.push(m);
            }
        })
        .unwrap();
        mgr.flush(20, false, |t, m| {
            if t == TARGET_PRIMARY {
                emitted.push(m);
            }
        });
        assert_eq!(sum_rate_counts(&emitted, "blocked"), 4.0);
    }

    #[test]
    fn chained_two_aggregate_cut_preserves_every_sample_when_upstream_window_changes() {
        use saluki_metric_kernels::histogram::HistogramConfiguration;

        use crate::ir::MetricPlan;

        let make_chained = |w1: u64, w2: u64| {
            let plan = MetricPlan::builder()
                .node(
                    "in",
                    Op::Source {
                        stream: STREAM_DOGSTATSD.to_string(),
                    },
                )
                .node(
                    "agg1",
                    Op::Aggregate {
                        window_duration_secs: NonZeroU64::new(w1).unwrap(),
                        context_limit: 100,
                        counter_expiry_secs: 0,
                        flush_open_windows: true,
                        histogram: HistogramConfiguration::default(),
                    },
                )
                .node(
                    "agg2",
                    Op::Aggregate {
                        window_duration_secs: NonZeroU64::new(w2).unwrap(),
                        context_limit: 100,
                        counter_expiry_secs: 0,
                        flush_open_windows: true,
                        histogram: HistogramConfiguration::default(),
                    },
                )
                .node(
                    "out",
                    Op::Emit {
                        target: TARGET_PRIMARY.to_string(),
                    },
                )
                .edge("in", "agg1")
                .edge("agg1", "agg2")
                .edge("agg2", "out")
                .build();
            validate_plan(&plan).unwrap()
        };

        // Start with 10s -> 60s chained aggregates; change only upstream agg1 to 15s while agg2 stays 60s.
        let mut mgr = GenerationManager::new(make_chained(10, 60), RuntimeBindings::default()).unwrap();
        let mut emitted = Vec::new();

        mgr.ingest(STREAM_DOGSTATSD, 12, vec![Metric::gauge("temp", 10.0)], |_, m| {
            emitted.push(m)
        })
        .unwrap();

        // Cut boundary must align with ALL stateful windows: lcm(10, 15, 60) = 60.
        let outcome = mgr.install(make_chained(15, 60), 15).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::ScheduledCut {
                generation: GenerationId(2),
                cut_timestamp_secs: 60,
            }
        );

        // Admit another sample into [20, 30) on gen 1, then cross the cut at t=65 on gen 2!
        mgr.ingest(STREAM_DOGSTATSD, 25, vec![Metric::gauge("temp", 20.0)], |_, m| {
            emitted.push(m)
        })
        .unwrap();
        let post_cut = mgr
            .ingest(STREAM_DOGSTATSD, 65, vec![Metric::gauge("temp", 30.0)], |_, m| {
                emitted.push(m)
            })
            .unwrap();
        assert_eq!(post_cut.generation, GenerationId(2));

        // Gen 1's [0, 60) rollup must have drained cleanly at the cut.
        assert_eq!(emitted.len(), 1);

        // Flush at t=120 closes [60, 120) on gen 2.
        mgr.flush(120, false, |_, m| emitted.push(m));
        assert_eq!(emitted.len(), 2);
    }

    #[test]
    fn backward_clock_skew_before_install_never_cuts_behind_admitted_buckets() {
        let mut mgr = GenerationManager::new(plan_with_window_and_filter(10, &[]), RuntimeBindings::default()).unwrap();
        let mut emitted = Vec::new();

        // Admit a sample at t=55.
        mgr.ingest(STREAM_DOGSTATSD, 55, vec![Metric::counter("req", 5.0)], |_, m| {
            emitted.push(m)
        })
        .unwrap();

        // Clock jumps backward to t=20 when installing a 15s window; cut must still be > 55 -> lcm(10,15)=30 -> 60!
        let outcome = mgr.install(plan_with_window_and_filter(15, &[]), 20).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::ScheduledCut {
                generation: GenerationId(2),
                cut_timestamp_secs: 60,
            }
        );

        // At t=60, gen 1 flushes [50, 60) with exact value 5.0 and gen 2 accepts t=62 with value 8.0.
        mgr.ingest(STREAM_DOGSTATSD, 62, vec![Metric::counter("req", 8.0)], |_, m| {
            emitted.push(m)
        })
        .unwrap();
        mgr.flush(75, false, |_, m| emitted.push(m));
        assert_eq!(sum_rate_counts(&emitted, "req"), 13.0);
    }

    mod proptest_generations {
        use proptest::prelude::*;

        use super::*;

        #[derive(Clone, Debug)]
        enum GenAction {
            Ingest { dt: u64, value: u16 },
            Flush { dt: u64 },
            StatelessReload { dt: u64, block_other: bool },
            WindowCut { dt: u64, new_window: u64 },
        }

        fn action_strategy() -> impl Strategy<Value = GenAction> {
            prop_oneof![
                (0u64..8, 1u16..50).prop_map(|(dt, value)| GenAction::Ingest { dt, value }),
                (1u64..15).prop_map(|dt| GenAction::Flush { dt }),
                (0u64..5, any::<bool>()).prop_map(|(dt, block_other)| GenAction::StatelessReload { dt, block_other }),
                (
                    0u64..5,
                    prop_oneof![
                        Just(5u64),
                        Just(10u64),
                        Just(12u64),
                        Just(15u64),
                        Just(20u64),
                        Just(30u64)
                    ]
                )
                    .prop_map(|(dt, new_window)| GenAction::WindowCut { dt, new_window }),
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            /// Deterministic simulation: across arbitrary sequences of continuous counter samples, flushes,
            /// stateless filterlist reloads, and stateful window-width cuts, `GenerationManager` must conserve
            /// 100% of admitted counter values with zero loss and zero duplication.
            #[test]
            fn continuous_input_across_random_cuts_and_reloads_conserves_exact_total(
                initial_window in prop_oneof![Just(5u64), Just(10u64), Just(15u64)],
                counter_expiry in prop_oneof![Just(0u64), Just(20u64), Just(40u64)],
                actions in prop::collection::vec(action_strategy(), 1..40),
            ) {
                let mut current_window = initial_window;
                let mut block_other = false;
                let mut mgr = GenerationManager::new(
                    plan_with_window_expiry_and_filter(current_window, counter_expiry, &[]),
                    RuntimeBindings::default(),
                )
                .unwrap();

                let mut now = 1_700_000_000u64;
                let mut expected_total: f64 = 0.0;
                let mut emitted: Vec<Metric> = Vec::new();

                for action in actions {
                    match action {
                        GenAction::Ingest { dt, value } => {
                            now += dt;
                            let v = f64::from(value);
                            expected_total += v;
                            mgr.ingest(STREAM_DOGSTATSD, now, vec![Metric::counter("req", v)], |t, m| {
                                if t == TARGET_PRIMARY {
                                    emitted.push(m);
                                }
                            })
                            .unwrap();
                        }
                        GenAction::Flush { dt } => {
                            now += dt;
                            mgr.flush(now, false, |t, m| {
                                if t == TARGET_PRIMARY {
                                    emitted.push(m);
                                }
                            });
                        }
                        GenAction::StatelessReload { dt, block_other: next_block } => {
                            now += dt;
                            block_other = next_block;
                            let blocked: &[&str] = if block_other { &["noise"] } else { &[] };
                            mgr.install(
                                plan_with_window_expiry_and_filter(current_window, counter_expiry, blocked),
                                now,
                            )
                            .unwrap();
                        }
                        GenAction::WindowCut { dt, new_window } => {
                            now += dt;
                            current_window = new_window;
                            let blocked: &[&str] = if block_other { &["noise"] } else { &[] };
                            mgr.install(
                                plan_with_window_expiry_and_filter(current_window, counter_expiry, blocked),
                                now,
                            )
                            .unwrap();
                        }
                    }
                }

                // Advance past any pending cut and drain on shutdown.
                if let Some(cut_ts) = mgr.pending_cut_timestamp() {
                    now = now.max(cut_ts + 1);
                }
                mgr.flush(now + 60, true, |t, m| {
                    if t == TARGET_PRIMARY {
                        emitted.push(m);
                    }
                });

                let actual_total = sum_rate_counts(&emitted, "req");
                prop_assert_eq!(actual_total, expected_total, "counter total must be conserved across reloads/cuts");
            }
        }
    }
}
