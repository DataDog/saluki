//! Native aggregation: the state machine that aggregates metrics into time buckets, and the decisions a host makes
//! around it.
//!
//! [`AggregateCore`] holds one entry per context and is driven by its host with explicit times, in whole Unix seconds:
//! [`admit`][AggregateCore::admit] inserts metrics when a batch arrives, and [`flush`][AggregateCore::flush] emits the
//! buckets closed by the flush time. The host owns the clock, the flush schedule, and delivery; the core owns every
//! decision about what is held and what is emitted, so every host aggregates alike.
//!
//! # Semantics
//!
//! - **Key.** A context is identified by its 64-bit identity hash; kind and metadata are not part of it. Contexts whose
//!   hashes collide merge into the first one inserted, which keeps its name and tags.
//! - **Buckets.** Buckets are aligned to Unix time and half-open, `[k·w, (k+1)·w)` for the window `w`. An inserted
//!   metric's untimestamped values collapse, merged by kind, into one point at the start of the bucket that contains
//!   the insertion time. Timestamped values keep their timestamps, unaligned.
//! - **Merge.** Into an existing entry, each point merges with the point at the same timestamp, or is added: counters
//!   and rates with equal intervals sum, gauges take the incoming value, sets unite, histograms gather samples, and
//!   distributions merge their sketches. A rate with another interval, or a value of another kind, replaces the entry's
//!   whole value set, closed buckets included. The entry keeps the metadata of its first insertion.
//! - **Context limit.** A metric for a new context is dropped while the core holds `context_limit` contexts, idle
//!   counters included; metrics for held contexts are always admitted. Dropping sets the breach flag, which only a
//!   flush that leaves fewer than `context_limit` contexts clears. A core that shares budgets of contexts with other
//!   cores counts its node's contexts instead of its own; see [Budgets](self#budgets).
//! - **Kind conflicts.** A metric whose kind, or rate interval, differs from its context's values replaces them, and
//!   the points they held are lost; [`AdmitOutcome`] counts them.
//! - **Flush.** A flush at time `t` emits every point whose bucket ended by the start of the bucket containing `t`. A
//!   host flushes only while the core holds contexts, so a flush time not reached while the core was empty is never
//!   recorded as a flush.
//! - **Idle counters.** At every flush after the first, each counter gains a zero at every bucket start `b` with
//!   `align(last flush) ≤ b < t`, `b + w - 1 < t`, and `b < last seen + expiry`, summed into any point already there.
//!   The rule does not ask whether the counter existed in those buckets, so a counter first seen after an idle period
//!   gains zeros back to the previous flush. A counter is removed at the first flush after `last seen + expiry` that
//!   leaves it without points; with no expiry, counters are never removed. Every other context is removed by the flush
//!   that empties it.
//! - **Emission.** Counters leave as rates over the window, zeros included. Histograms expand: first into a
//!   distribution under an optionally prefixed name, when they are copied to distributions, with sample weights
//!   truncated to integers, then into `<name>.<suffix>` per statistic, in order, where the count is a rate with its
//!   unit cleared and every other statistic a gauge. Every other kind leaves unchanged.
//! - **Final flush.** The last flush of a host treats every bucket as closed when it computes idle-counter zeros if
//!   `flush_open_windows` is set, but still emits only closed buckets. Open buckets are discarded.
//!
//! # Flushing
//!
//! [`AggregateCore::flush`] starts a [`Flush`], which visits one context per [`Flush::emit_next`] call, so a host that
//! delivers asynchronously can deliver each context's metrics before visiting the next one, and never holds more than
//! one context's expansion. [`AggregateCore::flush_into`] runs a whole flush into a synchronous sink.
//!
//! A host that keeps a flush across calls of its own, such as a kernel that yields between contexts, holds the flush
//! as an owned [`FlushState`] instead: [`AggregateCore::begin_flush`] starts it, [`AggregateCore::flush_step`] visits
//! one context, [`FlushState::take_counted`] takes what it has counted so far, and [`AggregateCore::end_flush`]
//! completes it. The same handle drives a **drain**, which emits every point held, whatever its bucket, and leaves the
//! core empty and as if it had never flushed ([`AggregateCore::begin_drain`]), and a **discard**, which removes
//! everything without emitting it ([`AggregateCore::begin_discard`]). Both remove each context as they visit it.
//!
//! # Parameters per call
//!
//! The bucket width defines the state, so it is fixed when the core is created ([`LineageParams`]). Every other
//! parameter applies to one call: the context limit to each admitted batch ([`AdmitParams`]), and the counter expiry,
//! histogram statistics, final-flush setting, and the context limit that clears the breach flag to each flush
//! ([`FlushParams`]). One core can therefore serve batches and flushes whose parameters differ, as when several
//! revisions of a program share one aggregation. [`AggregateParams::split`] divides a full set of parameters into these
//! parts; the core's [`admit`][AggregateCore::admit] and [`flush`][AggregateCore::flush] apply the parameters it was
//! created with.
//!
//! # Budgets
//!
//! A core created with [`AggregateCore::new`] admits by its own context count, as native aggregation does. A core
//! created with [`AggregateCore::with_budget`] instead shares a [`ContextBudget`] with other cores, such as other
//! instances of the same aggregation, or other aggregations of one host:
//!
//! - A metric for a new context is admitted while the node's budget counts fewer than `context_limit` contexts and the
//!   global budget has room; the new context then holds one unit of each. Metrics for held contexts are always
//!   admitted. With one core per budget, and a global capacity of at least `context_limit`, this is exactly native
//!   admission.
//! - Removing a context, by a flush, a drain, or a discard, releases its units, and a flush clears the breach flag if
//!   the node's budget then counts fewer than the flush's `context_limit` contexts and the global budget has room.
//! - Dropping the core releases every unit it holds, and removes its contexts from its context gauges, so that a host
//!   can discard a core, such as one rebuilt after a panic, without leaking its share of either budget.
//!
//! # Emission filters
//!
//! A flush whose [`FlushParams::filter`] is set skips the records that filter, downstream of aggregation, would drop,
//! and counts them per filter site in [`FlushOutcome::suppressed`]. Skipping happens only at emission, so everything
//! the core holds and every decision it makes are as without the filter; see [`EmissionFilter`].
//!
//! # Kernel
//!
//! [`AggregateKernel`] runs a core as a [`Kernel`][saluki_core::execution::Kernel]: it admits each batch at its
//! admission time, flushes at the turn's watermark, and drains, resets, and finishes in bounded, resumable steps.

use std::{fmt, mem::size_of, num::NonZeroU64, sync::Arc, time::Duration};

use saluki_core::{
    accounting::UsageExpr,
    data_model::event::metric::{context::Context, Metric},
    execution::StateBudget,
};
use tracing::trace;

mod emission;
use self::emission::count_suppressed;
pub use self::emission::{EmissionFilter, OutputKind, SiteIndex, Suppressed, SuppressedBySite};

mod histogram;
pub use self::histogram::{HistogramConfiguration, HistogramStatistic};

mod kernel;
pub use self::kernel::{AggregateKernel, TakeSuppressed};

mod snapshot;
pub use self::snapshot::{AggregateContextSnapshotEntry, AggregateMetricType, ContextSnapshot};

mod state;
use self::state::{AggregatedMetric, AggregationState, ContextLimit, FlushCursor, FlushMode, Insertion};

mod telemetry;
pub use self::telemetry::AggregateTelemetry;

/// The memory one held context takes in the state: its context handle and its aggregated values.
///
/// Values with more points than fit inline take more; see [`state_usage`].
pub const STATE_ENTRY_BYTES: usize = size_of::<Context>() + size_of::<AggregatedMetric>();

/// Returns the memory the state takes when it holds `context_limit` contexts.
///
/// Values with more points than fit inline, such as timestamped values spread over many buckets, take memory this does
/// not count.
pub fn state_usage(context_limit: usize) -> UsageExpr {
    UsageExpr::product(
        "aggregation state map",
        UsageExpr::sum(
            "context map entry",
            UsageExpr::struct_size::<Context>("context"),
            UsageExpr::struct_size::<AggregatedMetric>("aggregated metric"),
        ),
        UsageExpr::config("aggregate_context_limit", context_limit),
    )
}

/// Returns the memory a snapshot of `context_limit` held contexts takes.
///
/// A snapshot is built while the state stays live, so a host that serves snapshots needs this in addition to
/// [`state_usage`].
pub fn snapshot_usage(context_limit: usize) -> UsageExpr {
    UsageExpr::product(
        "retained context snapshot",
        UsageExpr::struct_size::<AggregateContextSnapshotEntry>("snapshot entry"),
        UsageExpr::config("aggregate_context_limit", context_limit),
    )
}

/// The parameters of native aggregation.
///
/// [`split`][Self::split] divides them by when they apply; see [Parameters per call](self#parameters-per-call).
#[derive(Clone, Debug)]
pub struct AggregateParams {
    /// The bucket width, in seconds.
    pub window: NonZeroU64,

    /// The most contexts held at once, idle counters included.
    ///
    /// With `0`, every metric is dropped.
    pub context_limit: usize,

    /// How long after it was last updated an idle counter keeps emitting zeros, in seconds.
    ///
    /// `None` keeps counters forever: they still gain zeros for the closed buckets before they were last updated, but
    /// never after, and they keep counting against the context limit.
    pub counter_expiry: Option<NonZeroU64>,

    /// Whether the final flush treats every bucket as closed when it computes idle-counter zeros.
    ///
    /// A final flush emits only closed buckets either way, so this has no effect on what is emitted.
    pub flush_open_windows: bool,

    /// The statistics computed for each histogram, and whether histograms are also emitted as distributions.
    pub histograms: HistogramConfiguration,
}

impl AggregateParams {
    /// Divides the parameters by when they apply: those that define the state, those that apply to each admitted batch,
    /// and those that apply to each flush.
    ///
    /// The context limit applies both to admission and, as the limit below which a flush clears the breach flag, to
    /// flushes. The flush parameters filter nothing at emission.
    pub fn split(self) -> (LineageParams, AdmitParams, FlushParams) {
        let Self {
            window,
            context_limit,
            counter_expiry,
            flush_open_windows,
            histograms,
        } = self;
        (
            LineageParams { window },
            AdmitParams { context_limit },
            FlushParams {
                counter_expiry,
                flush_open_windows,
                histograms,
                context_limit,
                filter: None,
            },
        )
    }
}

/// The parameters that define aggregation state: two cores with equal lineage parameters bucket alike.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LineageParams {
    /// The bucket width, in seconds.
    pub window: NonZeroU64,
}

/// The parameters that apply to each admitted batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmitParams {
    /// The most contexts held at once, idle counters included: a metric for a new context is dropped while the core
    /// holds this many.
    ///
    /// With `0`, every metric is dropped.
    pub context_limit: usize,
}

/// The parameters that apply to each flush.
#[derive(Clone)]
pub struct FlushParams {
    /// How long after it was last updated an idle counter keeps emitting zeros, in seconds, or `None` to keep counters
    /// forever; see [`AggregateParams::counter_expiry`].
    pub counter_expiry: Option<NonZeroU64>,

    /// Whether the final flush treats every bucket as closed when it computes idle-counter zeros; see
    /// [`AggregateParams::flush_open_windows`].
    pub flush_open_windows: bool,

    /// The statistics computed for each histogram, and whether histograms are also emitted as distributions.
    pub histograms: HistogramConfiguration,

    /// The context limit below which a flush clears the breach flag.
    pub context_limit: usize,

    /// A filter downstream of aggregation whose drops the flush applies at emission, if any; see [`EmissionFilter`].
    ///
    /// A flush or drain with a filter skips the records it drops and counts them in [`FlushOutcome::suppressed`];
    /// without one, every record is emitted.
    pub filter: Option<Arc<dyn EmissionFilter>>,
}

impl fmt::Debug for FlushParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlushParams")
            .field("counter_expiry", &self.counter_expiry)
            .field("flush_open_windows", &self.flush_open_windows)
            .field("histograms", &self.histograms)
            .field("context_limit", &self.context_limit)
            .field("filter", &self.filter.as_ref().map(|filter| filter.site()))
            .finish()
    }
}

/// The parameters an [`AggregateKernel`] applies to one call, which a host passes with
/// [`Turn::with_param`][saluki_core::execution::Turn::with_param].
///
/// A kernel uses the parameters it was built with for a call whose turn carries none.
#[derive(Clone, Debug)]
pub struct AggregateStepParams {
    /// The parameters of admission, for a batch.
    pub admit: AdmitParams,

    /// The parameters of a flush, a drain, a reset, or a final flush.
    pub flush: FlushParams,

    /// How often the host flushes: it schedules the next flush this long after one that ran with these parameters.
    ///
    /// The kernel itself never schedules flushes; the interval travels with the other parameters for its host.
    pub flush_interval: Duration,
}

/// The budgets of contexts a core draws on: one shared by every core of a host, and one shared by every core of the
/// same node.
///
/// Each context a core created with [`AggregateCore::with_budget`] holds takes one unit of each budget; see
/// [Budgets](self#budgets). The node budget is normally unlimited: each batch's context limit caps the contexts it
/// counts, while the global budget's capacity caps the contexts of every core of the host together.
#[derive(Clone, Debug)]
pub struct ContextBudget {
    /// The budget every core of the host draws on, which bounds the contexts held in all of them together.
    pub global: StateBudget,

    /// The budget every core of one node draws on, which counts the node's contexts across its cores.
    pub node: StateBudget,
}

impl ContextBudget {
    /// Returns budgets that only one core draws on, both unlimited: the core is bounded by its own context limit alone,
    /// as native aggregation is.
    pub fn exclusive() -> Self {
        Self {
            global: StateBudget::unlimited(),
            node: StateBudget::unlimited(),
        }
    }

    /// Releases the units `contexts` removed contexts held.
    fn release(&self, contexts: usize) {
        let units = u64::try_from(contexts).unwrap_or(u64::MAX);
        if units > 0 {
            self.global.release(units);
            self.node.release(units);
        }
    }
}

/// What admitting a batch did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdmitOutcome {
    /// The metrics inserted.
    pub accepted: usize,

    /// The metrics dropped because the context limit was reached.
    pub dropped: usize,

    /// The points the dropped metrics carried.
    pub dropped_points: usize,

    /// Whether this batch dropped the first metric since the breach flag was last clear, which a host reports once.
    pub limit_reached: bool,

    /// The metrics whose values replaced the points their context held, because their kind, or their rate interval,
    /// differed from those points'.
    ///
    /// The replaced points are lost, closed buckets not yet flushed included. A metric whose context held no points,
    /// such as an idle counter, replaces nothing and is not counted.
    pub replaced: usize,

    /// The points those metrics replaced.
    pub replaced_points: usize,
}

/// What a completed flush, drain, or discard did.
///
/// A host that takes what a pass counted as it goes, with [`FlushState::take_counted`], finds here only what the pass
/// counted after the last take: the outcomes of the takes and of the end add up to what the whole pass did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FlushOutcome {
    /// Whether the flush cleared the breach flag: new contexts are admitted again.
    ///
    /// Only the end of a pass can clear it, so an outcome taken before the end has never recovered.
    pub recovered: bool,

    /// The contexts a discard removed while they held points; `0` for a flush or a drain, which emit what they remove.
    pub discarded: usize,

    /// The points those contexts held.
    pub discarded_points: usize,

    /// The records the flush or drain skipped because its [emission filter][FlushParams::filter] drops them, by the
    /// filter's site; empty without a filter, or when it skipped nothing.
    pub suppressed: SuppressedBySite,
}

/// A flush, drain, or discard in progress, owned by its host.
///
/// Created by [`AggregateCore::begin_flush`], [`begin_drain`][AggregateCore::begin_drain], or
/// [`begin_discard`][AggregateCore::begin_discard], driven one context at a time by [`AggregateCore::flush_step`], and
/// completed by [`AggregateCore::end_flush`], or given up with [`AggregateCore::abandon_flush`]. It carries the
/// parameters it started with, so a flush resumed in a later call applies the same ones throughout.
///
/// Until the flush ends, its core must not admit anything: the flush visits contexts by their position in the core's
/// table, which an insertion can move. A state is only meaningful to the core that created it.
#[must_use = "a flush must be ended or abandoned"]
#[derive(Debug)]
pub struct FlushState {
    cursor: FlushCursor,
    was_breached: bool,
    params: FlushParams,
}

impl FlushState {
    /// Returns the time this flush runs at, in whole Unix seconds, or `None` for a drain or a discard, which do not
    /// depend on the time.
    pub fn flush_time(&self) -> Option<u64> {
        match self.cursor.mode() {
            FlushMode::Closed => Some(self.cursor.current_time()),
            FlushMode::All | FlushMode::Discard => None,
        }
    }

    /// Returns the parameters the flush applies.
    pub fn params(&self) -> &FlushParams {
        &self.params
    }

    /// Takes what the pass has counted since it started, or since the last take: the contexts a discard removed while
    /// they held points, with their points, and the records its emission filter skipped, by site.
    ///
    /// The outcome taken has never [`recovered`][FlushOutcome::recovered], which only the end of the pass decides, and
    /// [`end_flush`][AggregateCore::end_flush] then reports only what the pass counts after this take. A host that
    /// delivers what the steps emitted before it starts more steps can take the counts with that delivery, so that
    /// what it reports matches what it delivered, even for a pass that never ends, as when the host discards the core
    /// part way through.
    pub fn take_counted(&mut self) -> FlushOutcome {
        let counts = self.cursor.take_counts();
        let mut suppressed = SuppressedBySite::new();
        if let Some(filter) = &self.params.filter {
            count_suppressed(&mut suppressed, filter.site(), counts.suppressed);
        }
        FlushOutcome {
            recovered: false,
            discarded: counts.discarded,
            discarded_points: counts.discarded_points,
            suppressed,
        }
    }
}

/// Native aggregation, driven by a host.
///
/// See the [module documentation][self] for the semantics. Times are whole Unix seconds read from the host's clock:
/// the arrival time of each batch, and the time of each flush.
pub struct AggregateCore {
    state: AggregationState,
    budget: ContextBudget,
    /// Whether the core draws on `budget`, shared with other cores, rather than admitting by its own context count.
    shared: bool,
    admit: AdmitParams,
    flush: FlushParams,
}

impl AggregateCore {
    /// Creates an empty core that admits by its own context count, as native aggregation does.
    ///
    /// The core's budget is [exclusive][ContextBudget::exclusive], and dropping it changes nothing: its context
    /// gauges keep their values, as native aggregation's do when it stops. The core reports its telemetry through
    /// `telemetry`; see [`AggregateTelemetry`].
    pub fn new(params: AggregateParams, telemetry: AggregateTelemetry) -> Self {
        Self::create(params, ContextBudget::exclusive(), false, telemetry)
    }

    /// Creates an empty core that draws on `budget`, which it may share with other cores; see
    /// [Budgets](self#budgets).
    ///
    /// The core's bucket width is `params.window`; the rest of `params` is what [`admit`][Self::admit] and
    /// [`flush`][Self::flush] apply. Dropping the core releases the units it holds and removes its contexts from its
    /// context gauges. The core reports its telemetry through `telemetry`; see [`AggregateTelemetry`].
    pub fn with_budget(params: AggregateParams, budget: ContextBudget, telemetry: AggregateTelemetry) -> Self {
        Self::create(params, budget, true, telemetry)
    }

    fn create(params: AggregateParams, budget: ContextBudget, shared: bool, telemetry: AggregateTelemetry) -> Self {
        let (lineage, admit, flush) = params.split();
        Self {
            state: AggregationState::new(lineage.window, telemetry),
            budget,
            shared,
            admit,
            flush,
        }
    }

    /// Returns the parameters that define the core's state.
    pub fn lineage(&self) -> LineageParams {
        LineageParams {
            window: self.state.bucket_width_secs(),
        }
    }

    /// Returns the budgets the core draws on.
    ///
    /// A core created with [`new`][Self::new] has an exclusive budget it does not draw on: it admits by its own
    /// context count.
    pub fn budget(&self) -> &ContextBudget {
        &self.budget
    }

    /// Returns `true` if the core draws on its budget, shared with other cores, rather than admitting by its own
    /// context count: it was created with [`with_budget`][Self::with_budget].
    #[cfg(test)]
    pub fn shares_budget(&self) -> bool {
        self.shared
    }

    /// Returns `true` if the core holds no contexts.
    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// Returns the number of contexts the core holds, idle counters included.
    pub fn len(&self) -> usize {
        self.state.len()
    }

    /// Returns the context limit the core was created with, which [`admit`][Self::admit] applies.
    pub fn context_limit(&self) -> usize {
        self.admit.context_limit
    }

    /// Returns `true` if a metric was dropped at the context limit since the last flush that left fewer contexts than
    /// the limit.
    pub fn context_limit_breached(&self) -> bool {
        self.state.context_limit_breached()
    }

    /// Returns the number of held contexts that hold points, and the number of points they hold.
    ///
    /// A host that stops discards these: they are the open buckets, plus any closed bucket not yet flushed.
    pub fn held_points(&self) -> (usize, usize) {
        self.state.held_points()
    }

    /// Returns a snapshot of every held context, with the kind of its values and its unit.
    pub fn snapshot_contexts(&self) -> Vec<AggregateContextSnapshotEntry> {
        self.state.snapshot_contexts()
    }

    /// Admits a batch of metrics that arrived at `now`, in whole Unix seconds, with the context limit the core was
    /// created with.
    ///
    /// See [`admit_with`][Self::admit_with].
    pub fn admit<I>(&mut self, now: u64, metrics: I) -> AdmitOutcome
    where
        I: IntoIterator<Item = Metric>,
    {
        let params = self.admit;
        self.admit_with(now, metrics, &params)
    }

    /// Admits a batch of metrics that arrived at `now`, in whole Unix seconds, with the given parameters.
    ///
    /// Each metric is inserted in order; one for a new context is dropped, and counted as dropped, while the core holds
    /// `params.context_limit` contexts, or, for a core that shares budgets, while its node's budget counts that many or
    /// the global budget is exhausted.
    pub fn admit_with<I>(&mut self, now: u64, metrics: I, params: &AdmitParams) -> AdmitOutcome
    where
        I: IntoIterator<Item = Metric>,
    {
        let limit = context_limit(&self.budget, self.shared, params.context_limit);
        let mut outcome = AdmitOutcome::default();
        for metric in metrics {
            let was_breached = self.state.context_limit_breached();
            let points = metric.values().len();
            match self.state.insert(now, metric, limit) {
                Insertion::Inserted { replaced_points } => {
                    outcome.accepted += 1;
                    if replaced_points > 0 {
                        outcome.replaced += 1;
                        outcome.replaced_points += replaced_points;
                    }
                }
                Insertion::Rejected => {
                    trace!("Dropping metric due to context limit.");
                    if !was_breached {
                        // First drop since the last recovery.
                        outcome.limit_reached = true;
                    }
                    self.state.telemetry().increment_events_dropped();
                    outcome.dropped += 1;
                    outcome.dropped_points += points;
                }
            }
        }
        outcome
    }

    /// Starts a flush at `now`, in whole Unix seconds, with the parameters the core was created with, or returns `None`
    /// if the core holds no contexts.
    ///
    /// An empty core is not flushed, and so a flush time reached while it is empty is not recorded: the next flush
    /// computes idle-counter zeros from the last flush that ran. `last` marks the host's final flush, which applies
    /// `flush_open_windows`.
    ///
    /// The flush must be [finished][Flush::finish], or [abandoned][Flush::abandon] if the host cannot deliver what it
    /// emitted, before the core is used again; the borrow enforces it.
    pub fn flush(&mut self, now: u64, last: bool) -> Option<Flush<'_>> {
        let params = self.flush.clone();
        let state = self.start(FlushMode::Closed, now, last, params)?;
        Some(Flush { core: self, state })
    }

    /// Runs a whole flush at `now` into `sink`, or does nothing and returns `None` if the core holds no contexts.
    ///
    /// See [`flush`][Self::flush].
    pub fn flush_into<F>(&mut self, now: u64, last: bool, mut sink: F) -> Option<FlushOutcome>
    where
        F: FnMut(Metric),
    {
        let mut flush = self.flush(now, last)?;
        while flush.emit_next(&mut sink) {}
        Some(flush.finish())
    }

    /// Starts a flush at `now`, in whole Unix seconds, with the given parameters, or returns `None` if the core holds
    /// no contexts.
    ///
    /// The flush emits the closed buckets, as [`flush`][Self::flush] does, and `last` marks the host's final flush. The
    /// returned state is driven by [`flush_step`][Self::flush_step], and must be ended with
    /// [`end_flush`][Self::end_flush] or [`abandon_flush`][Self::abandon_flush] before anything is admitted.
    pub fn begin_flush(&mut self, now: u64, last: bool, params: &FlushParams) -> Option<FlushState> {
        self.start(FlushMode::Closed, now, last, params.clone())
    }

    /// Starts a drain, or returns `None` if the core holds no contexts.
    ///
    /// A drain emits every point the core holds, whatever its bucket, closed or open, expanded by `params`, and removes
    /// every context, idle counters included, without adding idle-counter zeros. Each step removes the context it
    /// visits and releases its budget units, so the work and the memory of a drain are spread over its steps. Once it
    /// ends, the core is empty and as if it had never flushed, so its next flush adds no zeros; a core that holds
    /// nothing is left as if it had never flushed right away. It is driven like a flush; see
    /// [`begin_flush`][Self::begin_flush].
    pub fn begin_drain(&mut self, params: &FlushParams) -> Option<FlushState> {
        self.start(FlushMode::All, 0, false, params.clone())
    }

    /// Starts a discard, or returns `None` if the core holds no contexts.
    ///
    /// A discard removes every context without emitting anything, one a step as a drain does, and its outcome counts
    /// the contexts that held points and their points, which a host records as lost. Once it ends, the core is empty
    /// and as if it had never flushed; a core that holds nothing is left as if it had never flushed right away. Of
    /// `params`, only the context limit applies, to clear the breach flag. It is driven like a flush; see
    /// [`begin_flush`][Self::begin_flush].
    pub fn begin_discard(&mut self, params: &FlushParams) -> Option<FlushState> {
        self.start(FlushMode::Discard, 0, false, params.clone())
    }

    fn start(&mut self, mode: FlushMode, now: u64, last: bool, params: FlushParams) -> Option<FlushState> {
        if self.state.is_empty() {
            if mode != FlushMode::Closed {
                // A drain or a discard ends the core's timeline whether or not the core holds anything: no later flush
                // may add idle-counter zeros back to a flush before it, nor compare its time with one.
                self.state.forget_flushes();
            }
            return None;
        }

        // Remember if the context limit had been surpassed before this flush.
        let was_breached = self.state.context_limit_breached();
        let cursor = match mode {
            FlushMode::Closed => self
                .state
                .start_flush(now, last && params.flush_open_windows, params.counter_expiry),
            FlushMode::All | FlushMode::Discard => self.state.start_drain(mode),
        };
        Some(FlushState {
            cursor,
            was_breached,
            params,
        })
    }

    /// Visits the next context of the flush in progress, emitting what the visit releases into `sink`, except records
    /// the flush's [emission filter][FlushParams::filter] drops, which it counts for
    /// [`take_counted`][FlushState::take_counted] or [`end_flush`][Self::end_flush] to report.
    ///
    /// Returns `false`, without emitting anything, once every context has been visited. One call emits the records of
    /// one context, so a host that bounds its work per call checks its budget between calls.
    pub fn flush_step<F>(&mut self, state: &mut FlushState, sink: F) -> bool
    where
        F: FnMut(Metric),
    {
        let FlushState { cursor, params, .. } = state;
        let budget = self.shared.then_some(&self.budget);
        self.state
            .flush_next(cursor, &params.histograms, params.filter.as_deref(), budget, sink)
    }

    /// Completes a flush once [`flush_step`][Self::flush_step] has returned `false`: removes the contexts it emptied or
    /// expired, releasing their budget units, clears the breach flag if fewer contexts than the flush's context limit
    /// remain, and records the flush.
    ///
    /// A flush is recorded as the last flush, and counted in `aggregate_flushes_total`. A drain or a discard, which
    /// removed each context as it visited it, leaves the core as if it had never flushed, and is not counted as a
    /// flush. The outcome holds what the pass counted since the last [take][FlushState::take_counted], if any.
    pub fn end_flush(&mut self, mut state: FlushState) -> FlushOutcome {
        let counted = state.take_counted();
        let FlushState {
            cursor,
            was_breached,
            params,
        } = state;
        let mode = cursor.mode();
        let limit = context_limit(&self.budget, self.shared, params.context_limit);
        self.state.finish_flush(cursor, limit);
        if mode == FlushMode::Closed {
            self.state.telemetry().increment_flushes();
        }
        FlushOutcome {
            recovered: was_breached && !self.state.context_limit_breached(),
            ..counted
        }
    }

    /// Gives up on a flush after a failed delivery: nothing more is removed or recorded.
    ///
    /// The contexts a flush visited keep the state the visits left them in, so the next flush computes idle-counter
    /// zeros from the previous one and removes what this one would have; those a drain or a discard visited are already
    /// removed. An abandoned flush still counts in `aggregate_flushes_total`, as the native flush that stopped at a
    /// failed delivery did.
    pub fn abandon_flush(&mut self, state: FlushState) {
        if state.cursor.mode() == FlushMode::Closed {
            self.state.telemetry().increment_flushes();
        }
    }
}

impl Drop for AggregateCore {
    fn drop(&mut self) {
        // A core that shares budgets gives back its share of them, and takes its contexts out of the gauges it shares
        // with the other cores of its host. A native core is dropped only when its host stops, and leaves both as they
        // are.
        if self.shared {
            let removed = self.state.release_all();
            self.budget.release(removed);
        }
    }
}

/// Returns the limit a core with `budget`, shared or not, applies with the context limit `limit`.
fn context_limit(budget: &ContextBudget, shared: bool, limit: usize) -> ContextLimit<'_> {
    if shared {
        ContextLimit::shared(limit, budget)
    } else {
        ContextLimit::exclusive(limit)
    }
}

/// A flush in progress, borrowing its core.
///
/// Each [`emit_next`][Self::emit_next] call visits one held context and emits the metrics of its closed buckets, so a
/// host can deliver them before the next call. A flush ends with [`finish`][Self::finish], which removes the contexts
/// the flush emptied or expired and records the flush, or with [`abandon`][Self::abandon] when the host failed to
/// deliver: the contexts visited keep the state the visit left them in, and the flush is not recorded, so the next
/// flush computes idle-counter zeros from the previous one and removes what this one would have.
#[must_use = "a flush must be finished or abandoned"]
pub struct Flush<'a> {
    core: &'a mut AggregateCore,
    state: FlushState,
}

impl Flush<'_> {
    /// Visits the next held context, emitting the metrics of its closed buckets into `sink`.
    ///
    /// Returns `false`, without emitting anything, once every context has been visited.
    pub fn emit_next<F>(&mut self, sink: F) -> bool
    where
        F: FnMut(Metric),
    {
        self.core.flush_step(&mut self.state, sink)
    }

    /// Completes the flush: removes the contexts it emptied or expired, clears the breach flag if fewer contexts than
    /// the limit remain, and records the flush time.
    ///
    /// The host finishes a flush once [`emit_next`][Self::emit_next] has returned `false`, after visiting every
    /// context.
    pub fn finish(self) -> FlushOutcome {
        let Self { core, state } = self;
        core.end_flush(state)
    }

    /// Gives up on the flush after a failed delivery: nothing more is removed or recorded.
    pub fn abandon(self) {
        let Self { core, state } = self;
        core.abandon_flush(state);
    }
}

#[cfg(test)]
mod reference;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod compatibility_tests;
