//! Native aggregation as a kernel.

use std::{any::Any, time::Duration};

use saluki_core::{
    data_model::event::Event,
    execution::{ClockReset, DrainPolicy, Kernel, LossClass, LossCount, PortIndex, Progress, Turn, WorkKind},
    topology::EventsBuffer,
};
use tracing::{debug, info, warn};

use super::{
    count_suppressed, AggregateCore, AggregateParams, AggregateStepParams, AggregateTelemetry, ContextBudget,
    ContextSnapshot, FlushParams, FlushState, LastSeen, SuppressedBySite,
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
///   It writes nothing, and never yields. A held context's last update, which counter expiry is measured from, is the
///   latest admission time it was admitted at ([`LastSeen::Latest`]): the batches of one admission point arrive in
///   stamp order, so with one upstream point this is the time of its last batch, as natively, and with several, a
///   batch stamped earlier that arrives late never moves a counter's expiry earlier. A batch without an admission
///   time cannot be bucketed, and is recorded as [`LossClass::Invalid`], as is any event that is not a metric.
///   Metrics dropped at the context limit are recorded as [`LossClass::Capacity`], and the first drop since the limit
///   was last clear logs a warning, as native aggregation does. A metric whose kind, or rate interval, differs from
///   the points its context holds replaces them; the replaced points are recorded as [`LossClass::Policy`], one event
///   per metric that replaced any.
/// - [`advance_time`][Kernel::advance_time] flushes at `floor(watermark)`: it emits every bucket that ended by then,
///   with idle-counter zeros. Without a watermark, or while the core holds nothing, nothing happens. A flush that
///   brings the contexts below the limit logs that metrics are accepted again, as native aggregation does.
/// - [`control`][Kernel::control] with a [`ClockReset`] flushes at `floor(watermark)` as `advance_time` does, then
///   drains: it emits every point still held, whatever its bucket, and the kernel continues empty, as if it had never
///   flushed, so its next flush adds no idle-counter zeros.
/// - [`control`][Kernel::control] with a [`ContextSnapshot`] appends an entry for every context held. It is
///   read-only, and may arrive while another call is yielded, which it does not disturb.
/// - [`control`][Kernel::control] with [`TakeSuppressed`] takes the counts of records skipped by an emission filter
///   since the last time; it too may arrive while another call is yielded, and does not disturb it.
/// - [`finish`][Kernel::finish] with [`DrainPolicy::Retire`] flushes at the watermark and then drains, as a reset
///   does, so the kernel holds nothing afterwards. With [`DrainPolicy::Shutdown`] it runs the final flush at the
///   watermark, as native aggregation's last flush, and then discards what remains, the open buckets, recording it as
///   [`LossClass::Shutdown`]: one event per context that held points, with its points. Each call records what it
///   discarded, so what the kernel [holds][Kernel::held] and what it recorded as lost always add up between calls.
/// - [`held`][Kernel::held] counts the contexts that hold points, and their points.
/// - [`publish_telemetry`][Kernel::publish_telemetry] writes what the core's telemetry counted since it last
///   published, when it was built [accumulating][AggregateTelemetry::accumulating], as a kernel in a region is.
///
/// # Budget
///
/// Flushes, drains, and discards visit one context at a time. The kernel charges [`WorkKind::StateWork`] one unit per
/// context visited and [`WorkKind::Events`] one unit per record emitted, and checks the budget only between contexts,
/// so one context's records, a histogram's whole expansion included, are never split across calls. When the budget is
/// exhausted the call returns [`Progress::Yielded`] and keeps its place: calling the same method again resumes it, a
/// reset included (see [`ClockReset`]). Every call visits at least one context, so a drain always progresses.
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
        }
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
        let pass = turn
            .watermark()
            .and_then(|watermark| self.core.begin_flush(watermark.as_unix_secs(), last, &params));
        if pass.is_some() {
            debug!("Flushing aggregated metrics...");
        }
        Operation {
            kind,
            pass,
            then,
            params,
        }
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

                let outcome = self.core.end_flush(pass);
                debug_assert!(
                    outcome.discarded == 0 && outcome.suppressed.is_empty(),
                    "a pass counts nothing after its last step"
                );
                // A shutdown's discard follows its final flush: nothing is accepted after it, so it reports no
                // recovery.
                let discarding = operation.kind == OperationKind::Shutdown && operation.then == Then::Done;
                if outcome.recovered && !discarding {
                    info!("Context limit no longer exceeded, metrics are being accepted again.");
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
        // A host never delivers a batch while a call is yielded. Should it, the pending pass completes first: it visits
        // contexts by their position in the core's table, which admitting could move.
        debug_assert!(
            self.pending.is_none(),
            "a host delivered a batch to a kernel whose call was yielded"
        );
        let mut skipped = SuppressedBySite::new();
        self.complete_pending(turn, &mut skipped);
        self.commit(skipped);

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
        let outcome = self.core.admit_with(admitted_at.as_unix_secs(), metrics, &params);

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
        if outcome.limit_reached {
            // First drop since the last recovery: warn once, as native aggregation does.
            warn!(
                context_limit = params.context_limit,
                "Context limit reached, dropping metrics. Consider increasing `aggregate_context_limit`."
            );
        }
        Progress::Done
    }

    fn advance_time(&mut self, turn: &mut Turn<'_>) -> Progress {
        self.call(turn, OperationKind::Tick)
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

#[cfg(test)]
mod tests;
