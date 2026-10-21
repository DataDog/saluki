//! Native aggregation as a kernel.

use std::{any::Any, sync::Arc, time::Duration};

use saluki_core::{
    data_model::event::{metric::Metric, Event},
    execution::{ClockReset, DrainPolicy, Kernel, LossClass, LossCount, PortIndex, Progress, Turn, WorkKind},
    topology::EventsBuffer,
};
use tracing::{debug, info, warn};

use super::{
    count_suppressed,
    lanes::{ContextCount, CountHeld, FlushRecord, Previous, Resolution, Restarted, TakeLaneFallbacks},
    AdmitOutcome, AdmitParams, AggregateCore, AggregateParams, AggregateStepParams, AggregateTelemetry, ContextBudget,
    ContextSnapshot, FlushParams, FlushState, LaneShared, LastSeen, SuppressedBySite,
};

/// Native aggregation as a [`Kernel`], driven by admission time and watermarks instead of a clock.
///
/// The kernel runs an [`AggregateCore`] with the semantics of the [module documentation][super], with two times
/// substituted for the native clock readings: a batch is admitted at its admission time, and a flush runs at the
/// turn's watermark, both floored to whole seconds. The kernel never schedules its own flushes; its host calls
/// [`advance_time`][Kernel::advance_time] on its flush schedule, with the watermark it flushes at.
///
/// # Calls
///
/// - [`consume`][Kernel::consume] admits the batch at `floor(t_a)`, with the context limit of the turn's parameters.
///   It writes nothing of its own, and never yields; what it writes while a flush is pending is the flush's output
///   (see [Admission while advancing](Self#admission-while-advancing)). A held context's last update, which counter
///   expiry is measured from, is the latest admission time it was admitted at ([`LastSeen::Latest`]): the batches of
///   one admission point arrive in stamp order, so with one upstream point this is the time of its last batch, as
///   natively, and with several, a batch stamped earlier that arrives late never moves a counter's expiry earlier. A
///   batch without an admission time cannot be bucketed, and is recorded as [`LossClass::Invalid`], as is any event
///   that is not a metric. Metrics dropped at the context limit are recorded as [`LossClass::Capacity`], and the first
///   drop since the limit was last clear logs a warning, as native aggregation does. A metric whose kind, or rate
///   interval, differs from the points its context holds replaces them; the replaced points are recorded as
///   [`LossClass::Policy`], one event per metric that replaced any.
/// - [`advance_time`][Kernel::advance_time] flushes at `floor(watermark)`: it emits every bucket that ended by then,
///   with idle-counter zeros. Without a watermark, or while the core holds nothing, nothing happens. A flush that
///   brings the contexts below the limit logs that metrics are accepted again, as native aggregation does. A kernel
///   not on lanes [admits while it advances](Self#admission-while-advancing).
/// - [`control`][Kernel::control] with a [`ClockReset`] flushes at `floor(watermark)` as `advance_time` does, then
///   drains: it emits every point still held, whatever its bucket, and the kernel continues empty, as if it had never
///   flushed, so its next flush adds no idle-counter zeros.
/// - [`control`][Kernel::control] with a [`ContextSnapshot`] appends an entry for every context held. It is
///   read-only, and may arrive while another call is yielded, which it does not disturb.
/// - [`control`][Kernel::control] with [`TakeSuppressed`] takes the counts of records skipped by an emission filter
///   since the last time; it too may arrive while another call is yielded, and does not disturb it.
/// - [`control`][Kernel::control] with [`RoomToAdmit`] or [`PassProgress`] answers whether new contexts fit without
///   anything being refused or the table growing, and how far a pending flush has come. Both are read-only, and may
///   arrive while another call is yielded, which they do not disturb. A kernel on lanes answers that nothing fits.
/// - [`finish`][Kernel::finish] with [`DrainPolicy::Retire`] flushes at the watermark and then drains, as a reset
///   does, so the kernel holds nothing afterwards. With [`DrainPolicy::Shutdown`] it runs the final flush at the
///   watermark, as native aggregation's last flush, and then discards what remains, the open buckets, recording it as
///   [`LossClass::Shutdown`]: one event per context that held points, with its points. Each call records what it
///   discarded, so what the kernel [holds][Kernel::held] and what it recorded as lost always add up between calls.
/// - [`held`][Kernel::held] counts the contexts that hold points, and their points.
/// - [`publish_telemetry`][Kernel::publish_telemetry] writes what the core's telemetry counted since it last
///   published, when it was built [accumulating][AggregateTelemetry::accumulating], as a kernel in a region is.
///
/// # Admission while advancing
///
/// A kernel not on lanes [admits while advancing][Kernel::admits_while_advancing]: its host may deliver batches while
/// a flush tick, an `advance_time`, is yielded. The flush is an [interleaved
/// flush][AggregateCore::begin_interleaved_flush], fixed when it starts, and each batch admitted while it is pending
/// is admitted [during it][AggregateCore::admit_during]: a metric whose context the flush has not visited yet makes it
/// visit that context first, writing what the visit releases to the primary port as output of the pending flush, which
/// the host delivers with it. So over the flush and the batches, the output, losses, state, breach flag and its log
/// lines, and suppressed counts are those of the flush completed before the first batch; only the order of records
/// differs. Records a visit on touch skips at emission are counted when the `consume` that visited returns.
///
/// While the flush is pending, nothing is refused and the context table never grows. A host asks [`RoomToAdmit`]
/// before it delivers a batch, and holds the batch until the flush completes if it does not fit. Should a batch reach
/// a new context that would be refused, or that the table has no room for without growing, `consume` completes the
/// flush itself, writing the rest of it, and then admits the metric; the flush tick stays pending, with nothing left
/// to do, and the host's next `advance_time`, which resumes it, returns [`Progress::Done`] at once without flushing
/// again. Visits by the flush's steps and on touch both count in [`PassProgress`].
///
/// A reset, a retirement, and shutdown still need the pending call completed first, and no batch arrives while they
/// are pending.
///
/// # Lanes
///
/// A kernel built [with lanes][Self::with_lanes] is one lane's instance of a lineage partitioned over several lanes,
/// and keeps the rules that couple every context of the lineage in the record the lanes share ([`LaneShared`]). On
/// every time-driven call whose turn carries a [lane tick][Turn::lane_tick], a flush tick or a closing flush (a reset,
/// a retirement, or shutdown), the kernel records its outcome in the record before it flushes: its idle-counter zeros
/// start from the latest earlier flush at which any lane held contexts, once the record knows it, and only the first
/// lane to flush contexts at the tick counts it in `aggregate_flushes_total`. The context limit warning is logged
/// once per episode across lanes, by the lane whose rejection starts it, and its recovery once, by the first lane
/// whose pass ends below the limit. A [`Restarted`] control message, which a host sends to an instance it rebuilt,
/// makes its first flush start from its own last flush, of which it has none, as a restarted aggregator's does.
///
/// The host of the lanes asks read-only questions with control messages, answered at once whatever pass is pending:
/// the flushes that started their zeros from the core's own last flush because the record could not say where the
/// lanes left them ([`TakeLaneFallbacks`]), how many contexts the core holds ([`ContextCount`]), and how many of
/// another instance's contexts it holds ([`CountHeld`]).
///
/// # Budget
///
/// Flushes, drains, and discards visit one context at a time. The kernel charges [`WorkKind::StateWork`] one unit per
/// context visited and [`WorkKind::Events`] one unit per record emitted, and checks the budget only between contexts,
/// so one context's records, a histogram's whole expansion included, are never split across calls. When the budget is
/// exhausted the call returns [`Progress::Yielded`] and keeps its place: calling the same method again resumes it, a
/// reset included (see [`ClockReset`]). Every call visits at least one context, so a drain always progresses, unless
/// the batches admitted during a flush tick visited every context left, when it completes the flush at once. Visits on
/// touch charge nothing: `consume` checks no budget.
///
/// # Parameters
///
/// A host that runs several revisions of a program over one kernel passes each call's parameters as
/// [`AggregateStepParams`] with [`Turn::with_param`]; a turn without them uses the parameters the kernel was built
/// with. Each batch is admitted with its own turn's parameters. A flush, reset, or finish takes the parameters of the
/// turn that starts it and applies them until it completes, however many calls that takes, its emission filter
/// included.
///
/// # Budgets and suppression
///
/// The kernel's core draws on the [`ContextBudget`] it was built with, which other kernels may share; see
/// [Budgets](super#budgets). Dropping the kernel, as a host does to rebuild it after a panic, releases its units and
/// removes its contexts from its gauges. A flush or drain with an emission filter skips the records the filter drops;
/// the kernel adds up what each of its calls skipped, per filter site, until a host takes the counts with
/// [`TakeSuppressed`] or [`take_suppressed`][Self::take_suppressed] to attribute them to the filter.
///
/// The counts follow what the host delivers. A call's counts are added when the call returns, a yielded call's
/// included, whose records the host delivers before resuming it; a call that panics adds nothing, as the host discards
/// everything such a call wrote, so the filter would never have received the records it skipped either. Counts not
/// taken are dropped with the kernel, so a host that drops a kernel, after a panic too, takes its counts first.
pub struct AggregateKernel {
    core: AggregateCore,
    defaults: AggregateStepParams,
    pending: Option<Operation>,
    suppressed: SuppressedBySite,
    /// Whether the kernel was rebuilt and has not flushed since: its next flush starts from its own last flush.
    restarted: bool,
    /// The flushes, on lanes, that started their idle-counter zeros from the core's own last flush because the lanes'
    /// record could not say where the lanes left them, since the host last took them ([`TakeLaneFallbacks`]).
    fallbacks: u64,
    /// The log lines about the context limit the kernel has written, for tests to compare.
    #[cfg(test)]
    limit_logs: LimitLogs,
    /// How far the last pass that completed had come when it completed, for tests to check.
    #[cfg(test)]
    last_completed: Option<PassProgress>,
}

/// The log lines about the context limit a kernel has written: that it was reached, and that it no longer is.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LimitLogs {
    pub(crate) reached: u64,
    pub(crate) recovered: u64,
}

/// A control message that takes the counts of records an [`AggregateKernel`] skipped at emission, by filter site.
///
/// The kernel moves into [`counts`][Self::counts] what its calls that returned since the last time skipped, and starts
/// counting from zero: a pass still in progress is counted up to the call that yielded it. One message sent to several
/// kernels in turn collects all of their counts, summing those of a site that several kernels counted. Taking the
/// counts changes nothing else, so a host may send the message while another call of the kernel is yielded, which it
/// does not disturb, or to a kernel a call of which panicked, before dropping it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TakeSuppressed {
    /// The counts taken so far.
    pub counts: SuppressedBySite,
}

/// A read-only control message that asks an aggregation instance whether `contexts` new contexts fit, so that a batch
/// admitted while a flush is pending can be refused nothing.
///
/// The kernel sets [`fits`][Self::fits] when that many new contexts fit in its table without growing it, and would
/// leave the contexts it holds strictly below the context limit of the turn's parameters and below what its shared
/// budgets allow, so that none of them can be refused. It answers whatever call is pending, which it does not
/// disturb. A host that interleaves admission with a pending flush ([`Kernel::admits_while_advancing`]) asks before it
/// delivers a batch that reaches the instance, with the batch's events that can reach it, and holds the batch until
/// the flush completes if they do not fit. A kernel that does not know the message leaves `fits` false, so the host
/// waits.
///
/// The answer reads the instance as it stands, and the shared budgets' counts, which other instances can change: it
/// holds for a batch delivered next, with the same parameters, while no other instance admits against the budgets in
/// between. Should a batch reach a context that does not fit anyway, the kernel completes the flush itself first, so
/// that nothing it admits or emits differs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RoomToAdmit {
    /// The new contexts asked about: an upper bound on those one batch can create.
    pub contexts: usize,

    /// Whether they fit; set by the kernel.
    pub fits: bool,
}

/// A read-only control message that asks an aggregation instance how far its pending flush has come.
///
/// The kernel sets [`total`][Self::total] to the contexts the pending flush visits, those its table held when the
/// flush started, and [`visited`][Self::visited] to those it has visited so far, by its cursor or because admission
/// reached them first. It answers whatever call is pending, which it does not disturb. Both stay zero when no flush is
/// pending, and in a kernel that does not know the message. A host that paces a flush over time reads it when the
/// flush starts and after each step.
///
/// `visited` equals `total` once every context has been visited, which a batch can do before the flush's next step.
/// The flush is still pending then: the host completes it with one more call, which emits nothing more.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PassProgress {
    /// The contexts the pending flush has visited.
    pub visited: usize,

    /// The contexts the pending flush visits in all.
    pub total: usize,
}

impl AggregateKernel {
    /// Creates a kernel holding no contexts.
    ///
    /// `params` are the kernel's bucket width and its parameters for calls whose turn carries none, and
    /// `flush_interval` the flush interval its host schedules flushes at. The kernel draws contexts from `budget` (see
    /// [`AggregateCore::with_budget`]), and reports its telemetry through `telemetry`; see [`AggregateTelemetry`].
    pub fn new(
        params: AggregateParams, flush_interval: Duration, budget: ContextBudget, telemetry: AggregateTelemetry,
    ) -> Self {
        let (_, admit, flush) = params.clone().split();
        Self {
            core: AggregateCore::with_budget(params, budget, telemetry).with_last_seen(LastSeen::Latest),
            defaults: AggregateStepParams {
                admit,
                flush,
                flush_interval,
            },
            pending: None,
            suppressed: SuppressedBySite::new(),
            restarted: false,
            fallbacks: 0,
            #[cfg(test)]
            limit_logs: LimitLogs::default(),
            #[cfg(test)]
            last_completed: None,
        }
    }

    /// Makes the kernel one lane's instance of a lineage that runs on several lanes, sharing `lanes` with the other
    /// lanes' instances; see [`AggregateCore::with_lanes`].
    ///
    /// Its host then sets the turn's [lane tick][saluki_core::execution::Turn::with_lane_tick] on every time-driven
    /// call, from which the kernel resolves each flush in the shared record; see [Lanes](Self#lanes).
    pub fn with_lanes(mut self, lanes: Arc<LaneShared>) -> Self {
        self.core = self.core.with_lanes(lanes);
        self
    }

    /// Takes the counts of records skipped by an emission filter in the calls that returned since the last time, by
    /// filter site; see [`TakeSuppressed`].
    pub fn take_suppressed(&mut self) -> SuppressedBySite {
        std::mem::take(&mut self.suppressed)
    }

    /// Returns the core the kernel runs.
    pub fn core(&self) -> &AggregateCore {
        &self.core
    }

    /// Returns the parameters the kernel applies to calls whose turn carries none.
    #[cfg(test)]
    pub fn defaults(&self) -> &AggregateStepParams {
        &self.defaults
    }

    /// Returns `true` if a call yielded and its work is still pending.
    #[cfg(test)]
    pub fn has_pending_work(&self) -> bool {
        self.pending.is_some()
    }

    /// Returns `true` if a call yielded and its pass is still in progress, rather than completed by a batch that
    /// arrived while the call was pending.
    #[cfg(test)]
    pub fn has_pass_in_progress(&self) -> bool {
        self.pending.as_ref().is_some_and(|operation| operation.pass.is_some())
    }

    /// Returns the log lines about the context limit the kernel has written.
    #[cfg(test)]
    pub(crate) fn limit_logs(&self) -> LimitLogs {
        self.limit_logs
    }

    /// Takes how far the last pass that completed had come when it completed.
    #[cfg(test)]
    pub(crate) fn take_last_completed(&mut self) -> Option<PassProgress> {
        self.last_completed.take()
    }

    /// Returns the core the kernel runs, to change it.
    #[cfg(test)]
    pub(crate) fn core_mut(&mut self) -> &mut AggregateCore {
        &mut self.core
    }

    /// Returns `true` if the pending call, if any, is a flush tick that batches may be admitted during.
    fn admits_during_pending(&self) -> bool {
        self.core.lanes().is_none()
            && self
                .pending
                .as_ref()
                .is_some_and(|operation| operation.kind == OperationKind::Tick)
    }

    /// Returns how far the pending pass has come, or zeros if no call is pending.
    fn pass_progress(&self) -> PassProgress {
        self.pending.as_ref().map_or_else(PassProgress::default, |operation| {
            operation.pass.as_ref().map_or(operation.ended, FlushState::progress)
        })
    }

    /// Starts the operation of `kind`, with the flush parameters of the turn, or the kernel's own.
    fn start(&mut self, turn: &Turn<'_>, kind: OperationKind) -> Operation {
        let params = match turn.param::<AggregateStepParams>() {
            Some(step) => step.flush.clone(),
            None => self.defaults.flush.clone(),
        };
        let (last, then) = match kind {
            OperationKind::Tick => (false, Then::Done),
            OperationKind::Reset | OperationKind::Retire => (false, Then::Drain),
            OperationKind::Shutdown => (true, Then::Discard),
        };
        let now = turn.watermark().map(|watermark| watermark.as_unix_secs());
        let pass = match self.resolve_lane(turn, now) {
            Some(record) => now.and_then(|now| self.core.begin_flush_with(now, last, &params, record)),
            // A flush tick off lanes is the one call that batches may be admitted during.
            None if kind == OperationKind::Tick && self.core.lanes().is_none() => {
                now.and_then(|now| self.core.begin_interleaved_flush(now, &params))
            }
            None => now.and_then(|now| self.core.begin_flush(now, last, &params)),
        };
        if pass.is_some() {
            self.restarted = false;
            debug!("Flushing aggregated metrics...");
        }
        Operation {
            kind,
            pass,
            then,
            params,
            ended: PassProgress::default(),
        }
    }

    /// Records the outcome of a time-driven call flushing at `now`, if any, in the record the lanes share, when the
    /// kernel runs on lanes and its host says which shared flush the call is: a flush holding contexts when the core
    /// holds any and the call flushes. Returns how the flush starts its idle-counter zeros and whether it counts.
    ///
    /// A flush that starts its zeros from the core's own last flush because the record cannot say where the lanes left
    /// them is counted for the host ([`TakeLaneFallbacks`]).
    fn resolve_lane(&mut self, turn: &Turn<'_>, now: Option<u64>) -> Option<FlushRecord> {
        let (lanes, tick) = (self.core.lanes()?, turn.lane_tick()?);
        let outcome = match now {
            Some(now) if !self.core.is_empty() => Resolution::Flushed(now),
            _ => Resolution::Empty,
        };
        let mut record = lanes.resolve(tick, outcome);
        if self.restarted {
            // A rebuilt instance's first flush is a first flush: it adds no zeros, whatever the other lanes flushed.
            record.previous = Previous::Unresolved;
        } else if record.previous == Previous::Unresolved && outcome != Resolution::Empty {
            self.fallbacks += 1;
        }
        Some(record)
    }

    /// Runs the operation of `kind`: resumes the pending one if it is of that kind, or starts a new one.
    fn call(&mut self, turn: &mut Turn<'_>, kind: OperationKind) -> Progress {
        let mut skipped = SuppressedBySite::new();
        let progress = self.resume_or_start(turn, kind, &mut skipped);
        self.commit(skipped);
        progress
    }

    /// Runs the operation of `kind`, adding what its passes skip at emission to `skipped`.
    fn resume_or_start(
        &mut self, turn: &mut Turn<'_>, kind: OperationKind, skipped: &mut SuppressedBySite,
    ) -> Progress {
        if let Some(operation) = self.pending.take() {
            // A host resumes a yielded call before it calls any other method. Should it not, the pending work runs to
            // completion first, so that nothing is lost and no two passes ever share the core.
            let resumed = operation.kind == kind;
            debug_assert!(
                resumed,
                "a host called another method of a kernel whose call was yielded"
            );
            let progress = self.run(turn, operation, skipped);
            if resumed || progress == Progress::Yielded {
                return progress;
            }
        }
        let operation = self.start(turn, kind);
        self.run(turn, operation, skipped)
    }

    /// Adds what a call skipped at emission to the counts a host takes, once the call returns.
    ///
    /// A host discards everything a call that panics wrote, so the filter would never have received the records such
    /// a call skipped: they are counted with the call's output, or not at all.
    fn commit(&mut self, skipped: SuppressedBySite) {
        for (site, suppressed) in skipped {
            count_suppressed(&mut self.suppressed, site, suppressed);
        }
    }

    /// Runs `operation` until it completes, or until the turn's budget is exhausted, when it is kept pending, adding
    /// what its passes skip at emission to `skipped`.
    fn run(&mut self, turn: &mut Turn<'_>, mut operation: Operation, skipped: &mut SuppressedBySite) -> Progress {
        loop {
            if let Some(mut pass) = operation.pass.take() {
                let visited_all = self.step(turn, &mut pass);
                // What the pass counted so far goes with what it emitted in this call, so that a host that drops the
                // kernel between calls loses neither: what it discarded is recorded as lost now, and what it skipped is
                // counted with the call.
                let counted = pass.take_counted();
                if counted.discarded > 0 {
                    turn.counters().record_loss(
                        LossClass::Shutdown,
                        counted.discarded as u64,
                        counted.discarded_points as u64,
                    );
                }
                for (site, suppressed) in counted.suppressed {
                    count_suppressed(skipped, site, suppressed);
                }
                if !visited_all {
                    operation.pass = Some(pass);
                    self.pending = Some(operation);
                    return Progress::Yielded;
                }

                #[cfg(test)]
                {
                    self.last_completed = Some(pass.progress());
                }
                let outcome = self.core.end_flush(pass);
                debug_assert!(
                    outcome.discarded == 0 && outcome.suppressed.is_empty(),
                    "a pass counts nothing after its last step"
                );
                // A shutdown's discard follows its final flush: nothing is accepted after it, so it reports no
                // recovery.
                let discarding = operation.kind == OperationKind::Shutdown && operation.then == Then::Done;
                let recovered = match self.core.lanes() {
                    // Across lanes, the episode ends once, at the first pass that ends below the shared limit.
                    Some(lanes) => {
                        !discarding
                            && self.core.is_below_context_limit(operation.params.context_limit)
                            && lanes.clear_breach()
                    }
                    None => outcome.recovered && !discarding,
                };
                if recovered {
                    self.log_recovered();
                }
            }

            operation.pass = match std::mem::replace(&mut operation.then, Then::Done) {
                Then::Done => return Progress::Done,
                Then::Drain => self.core.begin_drain(&operation.params),
                Then::Discard => self.core.begin_discard(&operation.params),
            };
        }
    }

    /// Visits contexts of `pass` until it has visited them all, returning `true`, or until the turn's budget is
    /// exhausted, returning `false`. It visits at least one context.
    fn step(&mut self, turn: &mut Turn<'_>, pass: &mut FlushState) -> bool {
        loop {
            let mut emitted = 0;
            let visited = self.core.flush_step(pass, |metric| {
                turn.push(PortIndex::PRIMARY, Event::Metric(metric));
                emitted += 1;
            });
            if !visited {
                return true;
            }

            let budget = turn.budget_mut();
            budget.charge(WorkKind::StateWork, 1);
            budget.charge(WorkKind::Events, emitted);
            if budget.is_exhausted() {
                return false;
            }
        }
    }

    /// Logs that the context limit is no longer exceeded, as native aggregation does at the end of the flush that ends
    /// a breach.
    fn log_recovered(&mut self) {
        #[cfg(test)]
        {
            self.limit_logs.recovered += 1;
        }
        info!("Context limit no longer exceeded, metrics are being accepted again.");
    }

    /// Logs that the context limit was reached, as native aggregation does at the first drop since it was last clear.
    fn log_limit_reached(&mut self, context_limit: usize) {
        #[cfg(test)]
        {
            self.limit_logs.reached += 1;
        }
        warn!(
            context_limit,
            "Context limit reached, dropping metrics. Consider increasing `aggregate_context_limit`."
        );
    }

    /// Admits `metrics`, which arrived at `now`, with `params`, while the flush tick `operation` is pending: during its
    /// pass, if it is still in progress, writing what visits on touch release to the primary port as the flush's
    /// output, and adding what they skip at emission to `skipped`.
    ///
    /// If admission completes the pass, the operation stays pending with nothing left to do, so that the host's next
    /// `advance_time`, which resumes it, completes it at once.
    fn admit_during_tick(
        &mut self, turn: &mut Turn<'_>, now: u64, metrics: impl Iterator<Item = Metric>, params: &AdmitParams,
        skipped: &mut SuppressedBySite,
    ) -> AdmitOutcome {
        let operation = self.pending.as_mut().expect("a flush tick is pending");
        debug_assert_eq!(
            operation.kind,
            OperationKind::Tick,
            "batches arrive only during a flush tick"
        );
        let started = operation.pass.as_ref().map(FlushState::progress);
        let (outcome, ended) = self
            .core
            .admit_during(now, metrics, params, &mut operation.pass, |metric| {
                turn.push(PortIndex::PRIMARY, Event::Metric(metric))
            });
        if let Some(pass) = operation.pass.as_mut() {
            for (site, suppressed) in pass.take_counted().suppressed {
                count_suppressed(skipped, site, suppressed);
            }
        }
        let Some(ended) = ended else {
            return outcome;
        };

        // Admission completed the pass: it visited every context it started with.
        let total = started.map_or(0, |started| started.total);
        operation.ended = PassProgress { visited: total, total };
        debug_assert!(
            ended.discarded == 0 && operation.then == Then::Done,
            "a flush tick discards nothing and is done when its pass is"
        );
        for (site, suppressed) in ended.suppressed {
            count_suppressed(skipped, site, suppressed);
        }
        #[cfg(test)]
        {
            self.last_completed = Some(PassProgress { visited: total, total });
        }
        if ended.recovered {
            self.log_recovered();
        }
        outcome
    }

    /// Completes pending work whatever the budget, for a batch that arrived while it was pending, adding what it skips
    /// at emission to `skipped`.
    fn complete_pending(&mut self, turn: &mut Turn<'_>, skipped: &mut SuppressedBySite) {
        while let Some(operation) = self.pending.take() {
            // Each run visits at least one context, so this ends.
            self.run(turn, operation, skipped);
        }
    }
}

impl Kernel for AggregateKernel {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        // A host delivers a batch while a call is yielded only if it is a flush tick, which admission continues
        // during. Should it deliver one during any other call, the pending pass completes first: it visits contexts by
        // their position in the core's table, which admitting could move.
        let during_tick = self.admits_during_pending();
        debug_assert!(
            self.pending.is_none() || during_tick,
            "a host delivered a batch to a kernel whose call was yielded and does not admit while it is"
        );
        let mut skipped = SuppressedBySite::new();
        if !during_tick {
            self.complete_pending(turn, &mut skipped);
            self.commit(std::mem::take(&mut skipped));
        }

        let batch = std::mem::replace(input, EventsBuffer::with_capacity(0));
        let Some(admitted_at) = turn.admitted_at() else {
            // Without an admission time there is no bucket to put the batch in.
            let events = batch.len() as u64;
            let points = batch
                .into_iter()
                .map(|event| event.try_as_metric().map_or(0, |metric| metric.values().len() as u64))
                .sum();
            turn.counters().record_loss(LossClass::Invalid, events, points);
            return Progress::Done;
        };

        let params = match turn.param::<AggregateStepParams>() {
            Some(step) => step.admit,
            None => self.defaults.admit,
        };
        let mut invalid = 0;
        let metrics = batch.into_iter().filter_map(|event| {
            let metric = event.try_into_metric();
            if metric.is_none() {
                invalid += 1;
            }
            metric
        });
        let outcome = if during_tick {
            self.admit_during_tick(turn, admitted_at.as_unix_secs(), metrics, &params, &mut skipped)
        } else {
            self.core.admit_with(admitted_at.as_unix_secs(), metrics, &params)
        };
        // What visits on touch skipped is counted with the call that wrote what they emitted.
        self.commit(skipped);

        if invalid > 0 {
            turn.counters().record_loss(LossClass::Invalid, invalid, 0);
        }
        if outcome.dropped > 0 {
            turn.counters().record_loss(
                LossClass::Capacity,
                outcome.dropped as u64,
                outcome.dropped_points as u64,
            );
        }
        if outcome.replaced > 0 {
            // A conflicting kind or rate interval replaces what the context held, as native aggregation does.
            turn.counters().record_loss(
                LossClass::Policy,
                outcome.replaced as u64,
                outcome.replaced_points as u64,
            );
        }
        let limit_reached = match self.core.lanes() {
            // Across lanes, the first rejection of an episode in any lane warns.
            Some(lanes) => outcome.dropped > 0 && lanes.try_breach(),
            None => outcome.limit_reached,
        };
        if limit_reached {
            // First drop since the last recovery: warn once, as native aggregation does.
            self.log_limit_reached(params.context_limit);
        }
        Progress::Done
    }

    fn advance_time(&mut self, turn: &mut Turn<'_>) -> Progress {
        self.call(turn, OperationKind::Tick)
    }

    fn admits_while_advancing(&self) -> bool {
        // On lanes, a flush resolves in the record the lanes share, at once: it does not interleave.
        self.core.lanes().is_none()
    }

    fn control(&mut self, turn: &mut Turn<'_>, control: &mut dyn Any) -> Progress {
        if let Some(snapshot) = control.downcast_mut::<ContextSnapshot>() {
            // Read-only: whatever pass is pending keeps its place.
            snapshot.entries.extend(self.core.snapshot_contexts());
            return Progress::Done;
        }
        if let Some(take) = control.downcast_mut::<TakeSuppressed>() {
            // Only the counts of calls that returned move: whatever pass is pending keeps its place.
            for (site, suppressed) in self.take_suppressed() {
                count_suppressed(&mut take.counts, site, suppressed);
            }
            return Progress::Done;
        }
        if control.is::<ClockReset>() {
            return self.call(turn, OperationKind::Reset);
        }
        if control.is::<Restarted>() {
            // Only an instance on lanes reads where its zeros start from outside.
            self.restarted = self.core.lanes().is_some();
            return Progress::Done;
        }
        // The lanes' host's read-only questions: whatever pass is pending keeps its place.
        if let Some(take) = control.downcast_mut::<TakeLaneFallbacks>() {
            take.flushes += std::mem::take(&mut self.fallbacks);
            return Progress::Done;
        }
        if let Some(count) = control.downcast_mut::<ContextCount>() {
            count.contexts = Some(self.core.len());
            return Progress::Done;
        }
        if let Some(count) = control.downcast_mut::<CountHeld>() {
            let held = count
                .entries
                .iter()
                .filter(|entry| self.core.holds(entry.context()))
                .count();
            count.held += held as u64;
            return Progress::Done;
        }
        // The interleaved flush's read-only questions: whatever pass is pending keeps its place.
        if let Some(room) = control.downcast_mut::<RoomToAdmit>() {
            let context_limit = match turn.param::<AggregateStepParams>() {
                Some(step) => step.admit.context_limit,
                None => self.defaults.admit.context_limit,
            };
            room.fits = self.core.lanes().is_none() && self.core.has_room_for(room.contexts, context_limit);
            return Progress::Done;
        }
        if let Some(progress) = control.downcast_mut::<PassProgress>() {
            *progress = self.pass_progress();
            return Progress::Done;
        }
        Progress::Done
    }

    fn finish(&mut self, turn: &mut Turn<'_>, policy: DrainPolicy) -> Progress {
        let kind = match policy {
            DrainPolicy::Retire => OperationKind::Retire,
            DrainPolicy::Shutdown => OperationKind::Shutdown,
        };
        self.call(turn, kind)
    }

    fn held(&self) -> LossCount {
        let (contexts, points) = self.core.held_points();
        LossCount {
            events: contexts as u64,
            points: points as u64,
        }
    }

    fn publish_telemetry(&mut self) {
        self.core.publish_telemetry();
    }
}

/// Work a call of the kernel started and has not completed: the pass in progress, and what follows it.
struct Operation {
    kind: OperationKind,
    pass: Option<FlushState>,
    then: Then,
    params: FlushParams,
    /// How far the flush tick's pass had come when a batch admitted during it completed it, which the operation keeps
    /// reporting until the host resumes it.
    ended: PassProgress,
}

/// What a call of the kernel does with what it holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationKind {
    /// A flush at the watermark, from `advance_time`.
    Tick,

    /// A flush at the watermark, then a drain, from a `ClockReset`.
    Reset,

    /// A flush at the watermark, then a drain, from `finish` when retiring.
    Retire,

    /// The final flush at the watermark, then a discard, from `finish` when shutting down.
    Shutdown,
}

/// The pass that follows the one in progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Then {
    Done,
    Drain,
    Discard,
}

#[cfg(all(test, not(feature = "loom")))]
mod lane_tests;
#[cfg(test)]
mod tests;
