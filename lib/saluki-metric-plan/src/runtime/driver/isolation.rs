//! The driver of an isolation slot: its state owners' instances, its watermark, its flush ticks, and the drains that
//! run their time-driven work.
//!
//! See [`MetricRegionDriver`][super::MetricRegionDriver]'s documentation, [Isolation slots](super#isolation-slots),
//! for the rules; this module implements them.

use std::{
    any::Any,
    collections::{BTreeMap, VecDeque},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    time::{Duration, Instant},
};

use saluki_core::{
    execution::{
        AdmissionPointId, AdmissionTime, BatchHeader, ClockReset, DrainPolicy, ExitSink, GenerationId, GenerationRef,
        LossClass, LossCount, PortIndex, Progress, RegionTurn, ScheduleHandle, Turn, WallClock,
    },
    topology::EventsBuffer,
};
use saluki_metric_kernels::aggregate::TakeSuppressed;
use tracing::{debug, error, warn};

use super::{
    add_loss, fold_loss, points_in, Cursor, Fault, KernelBinding, KernelControl, KernelTarget, LiveGeneration,
    MetricRegionDriver, NodeRef, Run,
};
use crate::{
    compile::{KernelIndex, KernelKey, LineageKey, ProgramKernel, StateParamsIndex},
    install::StateBinding,
    runtime::{
        kernels::{KernelSlot, PreparedKernel},
        state::{
            rules::{
                retiring_flush_time, shutdown_time, stamping_generation, Observation, OutputClock, RetiringRefs,
                TickDecision, TickGate, WatermarkTracker,
            },
            slab::{InstanceHandle, Slab},
            StateBudgets,
        },
        telemetry::StateTelemetry,
    },
};

/// The state of an isolation slot's region.
pub(super) struct StateSlot {
    /// The progress of the admission points upstream of the slot, and the watermark it makes.
    pub tracker: WatermarkTracker,
    /// Stamps the slot's time-driven output.
    pub output: OutputClock,
    /// Decides each flush tick.
    pub gate: TickGate,
    /// The schedule of the topology, read to decide the generation of time-driven output.
    pub schedule: ScheduleHandle,
    /// The wall clock the tick gate, the shutdown time, and the watermark lag read.
    pub wall: Arc<dyn WallClock>,
    /// The host's state budgets, which every state owner's kernel must draw on, if the host gave the region any.
    pub budgets: Option<StateBudgets>,
    /// The instances of the slot's state owners' state.
    pub instances: Slab<Instance>,
    /// The drain in progress, if any.
    pub drain: Option<DrainCursor>,
    /// The batch whose arrival started the drain in progress, admitted once the drain completes.
    pub stashed: Option<EventsBuffer>,
    /// Whether the region started, so that ticks are scheduled.
    pub started: bool,
    /// Whether the shutdown drain was queued.
    pub shutting_down: bool,
    /// The clock resets the region ran since it last published.
    pub clock_resets: u64,
    /// The gated ticks whose grace ended since the region last published.
    pub grace_expired: u64,
    /// The handles the region publishes its own telemetry to.
    pub telemetry: StateTelemetry,
}

impl StateSlot {
    pub fn new(
        output_point: AdmissionPointId, upstream: Vec<AdmissionPointId>, schedule: ScheduleHandle,
        wall: Arc<dyn WallClock>, budgets: Option<StateBudgets>, telemetry: StateTelemetry,
    ) -> Self {
        Self {
            tracker: WatermarkTracker::new(upstream),
            output: OutputClock::new(output_point),
            gate: TickGate::default(),
            schedule,
            wall,
            budgets,
            instances: Slab::default(),
            drain: None,
            stashed: None,
            started: false,
            shutting_down: false,
            clock_resets: 0,
            grace_expired: 0,
            telemetry,
        }
    }

    /// Returns `true` if the drain in progress refers to the instance.
    fn drain_refers_to(&self, handle: InstanceHandle) -> bool {
        self.drain.as_ref().is_some_and(|drain| {
            drain.current.as_ref().is_some_and(|step| step.instance == handle)
                || drain.queue.iter().any(|(queued, _)| *queued == handle)
        })
    }
}

/// One instance of a state owner's state: one lineage, created by one generation.
pub(super) struct Instance {
    /// The lineage.
    pub key: LineageKey,
    /// The generation that created it, which with the key identifies it: two instances of one lineage can coexist.
    pub created_by: GenerationId,
    /// The kernel.
    pub kernel: KernelSlot,
    /// The number of live generations whose programs bind it.
    pub users: usize,
    /// The flush interval of the latest generation that bound it, which a skipped tick moves its deadline by.
    pub interval: Duration,
    /// When it next ticks.
    pub tick: TickState,
    /// The flush time of its latest tick that fired in this epoch, which later flush times never fall below.
    pub last_flush: Option<AdmissionTime>,
    /// The generations retiring it at a cut, from its preparation until its final retirement.
    pub retiring: Option<Retiring>,
    /// Whether it made its final flush at a cut: it holds nothing, binds nothing new, and never ticks.
    pub retired: bool,
    /// Whether its last binder went while a drain referred to it: it is skipped, and freed once the drain ends.
    pub removed: bool,
}

/// When an instance next ticks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TickState {
    /// The region has not started: the first tick is one interval after it does.
    Unstarted,

    /// A tick is due at `at`.
    Scheduled { at: Instant },

    /// The tick due at `at` waits for the watermark to reach its wall-clock second, `second`, at most until `until`.
    Gated { at: Instant, second: u64, until: Instant },

    /// The instance never ticks again: it retired.
    Off,
}

/// The generations of an instance retiring at a cut, both held strongly: the one it served, whose generation its
/// output keeps, and the one replacing it, whose cut says when it retires.
pub(super) struct Retiring {
    pub owner: GenerationRef,
    pub by: GenerationRef,
}

/// What a drain runs for one instance.
#[derive(Clone, Copy, Debug)]
pub(super) enum DrainKind {
    /// A flush tick scheduled at `scheduled`, flushing at `flush`.
    Tick { scheduled: Instant, flush: AdmissionTime },

    /// A clock reset: a flush at the watermark before the reset, `pre`, then everything the instance holds, stamped at
    /// `stamp_at`. `in_publication_epoch` is whether `pre` is in the publication epoch of the cut retiring the
    /// instance, if any.
    Reset {
        pre: Option<AdmissionTime>,
        stamp_at: AdmissionTime,
        in_publication_epoch: bool,
    },

    /// The final retirement of an instance at its cut: a flush at the cut's time, then everything it holds.
    FinalRetire,

    /// The final flush at shutdown, at `at`, then the open buckets discarded.
    Shutdown { at: AdmissionTime },
}

/// A drain in progress: the instances it still runs, in order, and the one it is running.
pub(super) struct DrainCursor {
    pub queue: VecDeque<(InstanceHandle, DrainKind)>,
    pub current: Option<DrainStep>,
    /// The clock-reset entries not completed yet: the output's epoch moves on once the last one completes.
    pub resets: usize,
    /// Whether the host ended the call running the drain, after a panic: the next `advance_time`, or `finish` once
    /// input has ended, resumes it.
    pub orphaned: bool,
}

/// The drain of one instance, decided when it starts and kept across yields: one drain, one generation.
pub(super) struct DrainStep {
    pub instance: InstanceHandle,
    pub kind: DrainKind,
    /// The generation the output is stamped with, held for the whole drain.
    pub generation: GenerationRef,
    /// The instance's kernel index in that generation's program.
    pub kernel: usize,
    /// The position of the kernel's time-driven output entry in that generation's program.
    pub output: usize,
    /// That generation's state parameters for the kernel.
    pub params: Option<Arc<dyn Any + Send + Sync>>,
    /// The flush interval under that generation.
    pub interval: Duration,
    /// The flush time: the kernel's turn watermark.
    pub flush: Option<AdmissionTime>,
    /// The header the output is stamped with.
    pub stamp: BatchHeader,
    /// The slot's output clock once the stamp counts: the region's takes it when the step emits something, so that a
    /// step that emits nothing, such as the final retirement of an instance a reset drain already emptied, moves no
    /// later stamp.
    pub latch: OutputClock,
}

/// How a preparation binds one state owner of its program.
pub(super) enum StateResolution {
    /// To an instance the region holds.
    Retain(InstanceHandle),

    /// To a new instance built for the preparation.
    New(Box<PreparedKernel>, usize),
}

/// Returns the generation a drain of `handle` is stamped with: rule 3.2 of the time rules.
fn stamping(generations: &[LiveGeneration], state: &StateSlot, handle: InstanceHandle) -> Option<GenerationRef> {
    let instance = state.instances.get(handle)?;
    let retiring = instance.retiring.as_ref().map(|retiring| RetiringRefs {
        owner: &retiring.owner,
        by: &retiring.by,
    });
    let schedule = state.schedule.load();
    stamping_generation(
        &schedule,
        |id| binds(generations, id, handle),
        |id| state.tracker.switched(id),
        retiring,
    )
}

/// Returns `true` if the live generation `id` binds the instance.
fn binds(generations: &[LiveGeneration], id: GenerationId, handle: InstanceHandle) -> bool {
    generations
        .iter()
        .find(|live| live.id == id)
        .is_some_and(|live| live.kernels.contains(&KernelBinding::Instance(handle)))
}

/// Orders the drains one event runs together as the reference interpreter runs them: by the generation that stamps
/// each, the oldest first, then in plan order of their state owners in that generation's program (its kernels are in
/// plan order). The output clock latches each stamp to the latest before it, so the order decides the stamps of drains
/// that flush at different times.
fn in_drain_order(
    generations: &[LiveGeneration], state: &StateSlot, drains: Vec<(InstanceHandle, DrainKind)>,
) -> impl Iterator<Item = (InstanceHandle, DrainKind)> {
    let mut keyed = drains
        .into_iter()
        .map(|(handle, kind)| {
            let generation = stamping(generations, state, handle).map(|generation| generation.id());
            let position = generation
                .and_then(|id| generations.iter().find(|live| live.id == id))
                .and_then(|live| {
                    live.kernels
                        .iter()
                        .position(|binding| *binding == KernelBinding::Instance(handle))
                });
            ((generation, position), (handle, kind))
        })
        .collect::<Vec<_>>();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, drain)| drain)
}

impl MetricRegionDriver {
    /// Returns the isolation slot's state.
    fn state(&self) -> &StateSlot {
        self.state
            .as_deref()
            .expect("only an isolation slot's region runs its state")
    }

    /// Returns the isolation slot's state, mutably.
    fn state_mut(&mut self) -> &mut StateSlot {
        self.state
            .as_deref_mut()
            .expect("only an isolation slot's region runs its state")
    }

    /// Schedules every instance's first tick one interval after the region started.
    pub(super) fn start_isolated(&mut self, turn: &mut RegionTurn<'_>) {
        let now = turn.now();
        let state = self.state_mut();
        state.started = true;
        for (_, instance) in state.instances.iter_mut() {
            if instance.tick == TickState::Unstarted {
                instance.tick = TickState::Scheduled {
                    at: now + instance.interval,
                };
            }
        }
    }

    /// Processes a batch that reached the isolation slot: observes its header, runs whatever it starts before it is
    /// admitted (a clock reset, final retirements, gated ticks now due), then admits it.
    pub(super) fn process_isolated(
        &mut self, turn: &mut RegionTurn<'_>, mut batch: EventsBuffer, exits: &mut ExitSink,
    ) -> Progress {
        let header = batch.take_header();
        let Self { generations, state, .. } = self;
        let state = state
            .as_deref_mut()
            .expect("only an isolation slot's region runs its state");
        let observation = state.tracker.observe(&header);
        let mut queue = VecDeque::new();
        let mut resets = 0;
        match observation {
            Observation::Fault => {
                self.reject(Fault::NotUpstream, &header, &batch);
                return Progress::Done;
            }
            Observation::EpochSwitched {
                point, previous, pre, ..
            } => {
                state.clock_resets += 1;
                let stamp_at = pre
                    .or(state.output.latest())
                    .or(header.admitted_at)
                    .unwrap_or(AdmissionTime::UNIX_EPOCH);
                let mut drains = Vec::new();
                for (handle, instance) in state.instances.iter() {
                    if instance.retired || instance.removed {
                        continue;
                    }
                    // Whether `pre` is in the cut's publication epoch, as the tracker stood before the header.
                    let in_publication_epoch = instance
                        .retiring
                        .as_ref()
                        .and_then(|retiring| retiring.by.cut())
                        .is_some_and(|cut| state.tracker.in_publication_epoch_with(cut, point, previous));
                    drains.push((
                        handle,
                        DrainKind::Reset {
                            pre,
                            stamp_at,
                            in_publication_epoch,
                        },
                    ));
                    resets += 1;
                }
                queue.extend(in_drain_order(generations, state, drains));
                if resets == 0 {
                    // Nothing to close: the new timeline starts now.
                    state.output.reset();
                }
                // A tick gated in the timeline that ended is dropped: its second belongs to the old timeline, and the
                // reset's drain closes everything the instance holds. Its next tick is one interval later.
                for handle in state.instances.handles() {
                    if let Some(instance) = state.instances.get_mut(handle) {
                        if let TickState::Gated { at, .. } = instance.tick {
                            instance.tick = TickState::Scheduled {
                                at: at + instance.interval,
                            };
                        }
                    }
                }
            }
            Observation::Ok => {}
        }

        // The crossing: once every upstream point has sent a header of the generation retiring an instance, every
        // batch of the generation it served has arrived, and it makes its final flush before this batch is admitted.
        let crossed = state
            .instances
            .iter()
            .filter(|(_, instance)| {
                instance
                    .retiring
                    .as_ref()
                    .is_some_and(|retiring| state.tracker.switched(retiring.by.id()))
                    && !instance.retired
                    && !instance.removed
            })
            .map(|(handle, _)| (handle, DrainKind::FinalRetire))
            .collect();
        queue.extend(in_drain_order(generations, state, crossed));

        // Gated ticks whose second the watermark has now reached fire before the batch, as natively they fired before
        // it arrived. A reset dropped every gated tick.
        let watermark = state.tracker.watermark();
        let released = state
            .instances
            .iter()
            .filter_map(|(handle, instance)| match instance.tick {
                TickState::Gated { at, second, .. } => state
                    .gate
                    .on_watermark(second, watermark, instance.last_flush)
                    .map(|flush| (handle, DrainKind::Tick { scheduled: at, flush })),
                _ => None,
            })
            .collect();
        queue.extend(in_drain_order(generations, state, released));

        if queue.is_empty() {
            return self.admit_isolated(turn, header, batch, exits);
        }
        if !batch.is_empty() {
            batch.set_header(header);
            state.stashed = Some(batch);
        }
        state.drain = Some(DrainCursor {
            queue,
            current: None,
            resets,
            orphaned: false,
        });
        self.continue_isolated(turn, exits)
    }

    /// Continues the drain in progress, then admits the batch that started it, if any.
    pub(super) fn continue_isolated(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        if self.run_drain(turn, exits) == Progress::Yielded {
            return Progress::Yielded;
        }
        match self.state_mut().stashed.take() {
            Some(mut batch) => {
                let header = batch.take_header();
                self.admit_isolated(turn, header, batch, exits)
            }
            None => Progress::Done,
        }
    }

    /// Admits a batch into the state of the instances its generation's program binds.
    fn admit_isolated(
        &mut self, turn: &mut RegionTurn<'_>, header: BatchHeader, batch: EventsBuffer, exits: &mut ExitSink,
    ) -> Progress {
        if batch.is_empty() {
            return Progress::Done;
        }
        let Some(index) = self.live_index_for(&header, &batch) else {
            return Progress::Done;
        };
        let Some(entry) = self.generations[index].code.entry_index(&header) else {
            self.reject(Fault::NoEntry, &header, &batch);
            return Progress::Done;
        };
        let Self {
            kernels,
            generations,
            scratch,
            cursor,
            state,
            ..
        } = self;
        let state = state
            .as_deref_mut()
            .expect("only an isolation slot's region runs its state");
        let LiveGeneration {
            id,
            program,
            code,
            layout,
            kernels: kernel_slots,
            decisions,
            counts,
            ..
        } = &mut generations[index];
        cursor.generation = Some(*id);
        Run {
            layout,
            kernel_slots,
            functions: &code.decisions,
            decisions,
            counts,
            slab: kernels,
            instances: Some(&mut state.instances),
            state_params: program.state_params(),
            scratch,
            cursor,
            header: &header,
            turn,
            exits,
        }
        .entry(&code.entries[entry], std::iter::once(batch));
        *cursor = Cursor::default();
        Progress::Done
    }

    /// Runs time-driven work: resumes a drain a panic ended, or fires, gates, or skips every tick now due.
    pub(super) fn advance_isolated(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        if let Some(drain) = self.state_mut().drain.as_mut() {
            // Only a drain a panic ended can be left when the host calls this: a yielded one is resumed.
            drain.orphaned = false;
            return self.continue_isolated(turn, exits);
        }
        let now = turn.now();
        let Self { generations, state, .. } = self;
        let state = state
            .as_deref_mut()
            .expect("only an isolation slot's region runs its state");
        let wall = state.wall.now();
        let watermark = state.tracker.watermark();
        // Ticks whose grace ended fire before the ticks due now, as the interpreter fires them.
        let (mut expired, mut fired) = (Vec::new(), Vec::new());
        for handle in state.instances.handles() {
            let bound = stamping(generations, state, handle).is_some();
            let StateSlot {
                instances,
                gate,
                grace_expired,
                ..
            } = &mut *state;
            let Some(instance) = instances.get_mut(handle) else {
                continue;
            };
            if instance.retired || instance.removed {
                continue;
            }
            match instance.tick {
                TickState::Scheduled { at } if at <= now => {
                    match gate.due(bound, watermark, wall, instance.last_flush) {
                        // Skipped work moves on by its interval, so that the deadline that fired is never kept.
                        TickDecision::Skip => {
                            instance.tick = TickState::Scheduled {
                                at: at + instance.interval,
                            }
                        }
                        TickDecision::Fire { flush } => {
                            fired.push((handle, DrainKind::Tick { scheduled: at, flush }));
                        }
                        TickDecision::Gate { second } => {
                            instance.tick = TickState::Gated {
                                at,
                                second,
                                until: now + gate.grace(),
                            };
                        }
                    }
                }
                TickState::Gated { at, until, .. } if until <= now => {
                    match gate.on_grace(watermark, instance.last_flush) {
                        Some(flush) if bound => {
                            *grace_expired += 1;
                            expired.push((handle, DrainKind::Tick { scheduled: at, flush }));
                        }
                        _ => {
                            instance.tick = TickState::Scheduled {
                                at: at + instance.interval,
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let queue = in_drain_order(generations, state, expired)
            .chain(in_drain_order(generations, state, fired))
            .collect::<VecDeque<_>>();
        if queue.is_empty() {
            return Progress::Done;
        }
        state.drain = Some(DrainCursor {
            queue,
            current: None,
            resets: 0,
            orphaned: false,
        });
        self.continue_isolated(turn, exits)
    }

    /// Finishes the isolation slot after its input ended: completes a drain a panic ended, then the shutdown drain of
    /// every instance.
    pub(super) fn finish_isolated(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        if let Some(drain) = self.state_mut().drain.as_mut() {
            drain.orphaned = false;
            if self.continue_isolated(turn, exits) == Progress::Yielded {
                return Progress::Yielded;
            }
        }
        let Self { generations, state, .. } = self;
        let state = state
            .as_deref_mut()
            .expect("only an isolation slot's region runs its state");
        if std::mem::replace(&mut state.shutting_down, true) {
            return Progress::Done;
        }
        // Every upstream point's data has arrived: close at the later of the latest admission time and the wall clock.
        let end = shutdown_time(state.tracker.max_latest(), state.wall.now());
        let drains = state
            .instances
            .iter()
            .filter(|(_, instance)| !instance.retired && !instance.removed)
            .map(|(handle, _)| (handle, DrainKind::Shutdown { at: end }))
            .collect();
        let queue = in_drain_order(generations, state, drains).collect::<VecDeque<_>>();
        if queue.is_empty() {
            return Progress::Done;
        }
        state.drain = Some(DrainCursor {
            queue,
            current: None,
            resets: 0,
            orphaned: false,
        });
        self.continue_isolated(turn, exits)
    }

    /// Returns when the isolation slot next has time-driven work: at once for a drain a panic ended, never while a
    /// drain is pending otherwise, and else the earliest tick deadline or end of a grace.
    pub(super) fn next_deadline_isolated(&self) -> Option<Instant> {
        let state = self.state();
        if let Some(drain) = &state.drain {
            return drain.orphaned.then(|| self.now.unwrap_or_else(Instant::now));
        }
        state
            .instances
            .iter()
            .filter(|(_, instance)| !instance.retired && !instance.removed)
            .filter_map(|(_, instance)| match instance.tick {
                TickState::Scheduled { at } => Some(at),
                TickState::Gated { until, .. } => Some(until),
                TickState::Unstarted | TickState::Off => None,
            })
            .min()
    }

    /// Runs the drain in progress until it completes, returning [`Progress::Done`], or until the turn's budget runs
    /// out, returning [`Progress::Yielded`].
    fn run_drain(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        loop {
            let Some(drain) = self.state_mut().drain.as_mut() else {
                return Progress::Done;
            };
            if drain.current.is_none() {
                let Some((handle, kind)) = drain.queue.pop_front() else {
                    self.state_mut().drain = None;
                    self.collect_instances();
                    return Progress::Done;
                };
                let step = self.start_step(handle, kind);
                let state = self.state_mut();
                let drain = state.drain.as_mut().expect("the drain is in progress");
                match step {
                    Some(step) => drain.current = Some(step),
                    None => {
                        if matches!(kind, DrainKind::Reset { .. }) {
                            drain.resets -= 1;
                            if drain.resets == 0 {
                                state.output.reset();
                            }
                        }
                        continue;
                    }
                }
            }
            if self.run_step(turn, exits) == Progress::Yielded {
                return Progress::Yielded;
            }
            self.complete_step();
        }
    }

    /// Decides the drain of one instance: its generation, flush time, parameters, and stamp. Returns `None` for an
    /// instance there is nothing to drain for: gone, retired, removed, or bound by no published generation, which was
    /// created by a generation prepared and not published, so it holds nothing. A tick that does not run moves on by
    /// the instance's interval, so that the deadline that fired is never kept.
    fn start_step(&mut self, handle: InstanceHandle, kind: DrainKind) -> Option<DrainStep> {
        let step = self.decide_step(handle, kind);
        if step.is_none() {
            if let DrainKind::Tick { scheduled, .. } = kind {
                let state = self.state_mut();
                if let Some(instance) = state.instances.get_mut(handle).filter(|instance| !instance.retired) {
                    instance.tick = TickState::Scheduled {
                        at: scheduled + instance.interval,
                    };
                }
            }
        }
        step
    }

    fn decide_step(&mut self, handle: InstanceHandle, kind: DrainKind) -> Option<DrainStep> {
        let Self { generations, state, .. } = self;
        let state = state
            .as_deref_mut()
            .expect("only an isolation slot's region runs its state");
        let instance = state.instances.get(handle)?;
        if instance.removed || instance.retired {
            return None;
        }
        let generation = match kind {
            // A final retirement is stamped with the generation the instance served.
            DrainKind::FinalRetire => instance.retiring.as_ref().map(|retiring| Arc::clone(&retiring.owner)),
            _ => stamping(generations, state, handle),
        }?;
        let instance = state.instances.get(handle)?;
        let Some(live) = generations.iter().find(|live| live.id == generation.id()) else {
            debug_assert!(false, "a drain's generation is live in the region");
            return None;
        };
        let Some(kernel) = live
            .kernels
            .iter()
            .position(|binding| *binding == KernelBinding::Instance(handle))
        else {
            debug_assert!(false, "a drain's generation binds the instance");
            return None;
        };
        let code = live.code.state_kernel(kernel)?;
        let params = code
            .params
            .and_then(|params| live.program.state_param(StateParamsIndex::new(params as u16)))
            .map(|entry| Arc::clone(&entry.params));
        let interval = live
            .program
            .kernel(KernelIndex::new(kernel as u16))
            .and_then(|kernel| kernel.lineage.as_ref())
            .map_or(instance.interval, |lineage| lineage.flush_interval);

        // A retiring instance never closes or fills a bucket at or after its cut, within the cut's publication epoch.
        // With one upstream point, every stamp of that epoch before the crossing is before the cut, so its flush times
        // are too, and the clamp only guards: a flush time past the cut there is a stamping or ordering fault.
        let cut = instance.retiring.as_ref().and_then(|retiring| retiring.by.cut());
        let before_cut = |flush: AdmissionTime, in_epoch: bool| cut.is_none_or(|cut| !in_epoch || flush <= cut.at);
        let (flush, stamp_at) = match kind {
            DrainKind::Tick { flush, .. } => {
                let in_epoch = cut.is_some_and(|cut| state.tracker.in_publication_epoch(cut));
                debug_assert!(
                    before_cut(flush, in_epoch),
                    "a retiring instance's tick never passes its cut in the cut's publication epoch"
                );
                let flush = retiring_flush_time(flush, cut, in_epoch);
                (Some(flush), flush)
            }
            DrainKind::Reset {
                pre,
                stamp_at,
                in_publication_epoch,
            } => {
                debug_assert!(
                    pre.is_none_or(|pre| before_cut(pre, in_publication_epoch)),
                    "a retiring instance's reset never flushes past its cut in the cut's publication epoch"
                );
                let pre = pre.map(|pre| retiring_flush_time(pre, cut, in_publication_epoch));
                (pre, pre.unwrap_or(stamp_at))
            }
            DrainKind::FinalRetire => {
                // The cut is armed before any generation it publishes is stamped, so a crossing always has one.
                let at = match cut {
                    Some(cut) => cut.at,
                    None => {
                        debug_assert!(false, "an instance retires only at an armed cut");
                        state.tracker.watermark().unwrap_or(AdmissionTime::UNIX_EPOCH)
                    }
                };
                (Some(at), at)
            }
            DrainKind::Shutdown { at } => {
                let at = cut.map_or(at, |cut| at.min(cut.at));
                (Some(at), at)
            }
        };
        let mut latch = state.output;
        let stamp = latch.stamp(stamp_at, Arc::clone(&generation));
        Some(DrainStep {
            instance: handle,
            kind,
            generation,
            kernel,
            output: code.output,
            params,
            interval,
            flush,
            stamp,
            latch,
        })
    }

    /// Runs the current drain step's kernel call once, and its time-driven output through the generation's entry.
    fn run_step(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress {
        let Self {
            kernels,
            generations,
            scratch,
            cursor,
            state,
            ..
        } = self;
        let state = state
            .as_deref_mut()
            .expect("only an isolation slot's region runs its state");
        let step = state
            .drain
            .as_ref()
            .and_then(|drain| drain.current.as_ref())
            .expect("a drain step is in progress");
        let Some(index) = generations.iter().position(|live| live.id == step.generation.id()) else {
            return Progress::Done;
        };
        let Some(instance) = state.instances.get_mut(step.instance) else {
            return Progress::Done;
        };

        *cursor = Cursor {
            generation: Some(step.generation.id()),
            node: Some(NodeRef::Kernel(step.kernel)),
            instance: Some(step.instance),
        };
        let slot = &mut instance.kernel;
        let pushed = generations[index].telemetry.has_pushed_filter(step.kernel);
        let (progress, suppressed) = {
            let KernelSlot {
                context, kernel, ports, ..
            } = slot;
            context.accounted(|| {
                let mut kernel_turn =
                    Turn::new(&step.stamp, turn.now(), turn.budget_mut(), ports, &mut scratch.counters)
                        .with_watermark(step.flush);
                if let Some(params) = &step.params {
                    kernel_turn = kernel_turn.with_param(params.as_ref());
                }
                let progress = match step.kind {
                    DrainKind::Tick { .. } => kernel.advance_time(&mut kernel_turn),
                    DrainKind::Reset { .. } => kernel.control(&mut kernel_turn, &mut ClockReset),
                    DrainKind::FinalRetire => kernel.finish(&mut kernel_turn, DrainPolicy::Retire),
                    DrainKind::Shutdown { .. } => kernel.finish(&mut kernel_turn, DrainPolicy::Shutdown),
                };
                // What the call skipped at emission, for the filter R5b pushed into it: a read-only message, which
                // leaves a yielded call in place. A call that panics never gets here, and its records are discarded.
                let mut take = TakeSuppressed::default();
                if pushed {
                    kernel.control(&mut kernel_turn, &mut take);
                }
                (progress, take.counts)
            })
        };
        cursor.instance = None;

        // The kernel's output runs the generation's entry for it, which hands it to the exit its edge leaves by.
        let LiveGeneration {
            program,
            code,
            layout,
            kernels: kernel_slots,
            decisions,
            counts,
            ..
        } = &mut generations[index];
        let mut events = 0u64;
        let output = slot
            .ports
            .drain(PortIndex::PRIMARY)
            .inspect(|buffer| events += buffer.len() as u64);
        let entry = &code.entries[step.output];
        cursor.node = None;
        Run {
            layout,
            kernel_slots,
            functions: &code.decisions,
            decisions,
            counts,
            slab: kernels,
            instances: None,
            state_params: program.state_params(),
            scratch,
            cursor,
            header: &step.stamp,
            turn,
            exits,
        }
        .entry(entry, output);
        if events > 0 {
            // The output's admission time is the latest of what the slot emitted, never of a step that emitted nothing.
            state.output = step.latch;
        }
        let live = &mut generations[index];
        live.counts.kernel_ports[live.layout.kernel_ports[step.kernel]] += events;
        fold_loss(&mut live.counts.kernel_loss[step.kernel], &mut scratch.counters);
        // Each suppressed record is one the filter would have received and sent to its `drop`: counted in the
        // filter's own series, under the generation whose filter judged it.
        for (_, suppressed) in suppressed {
            add_loss(
                &mut live.counts.pushed[step.kernel],
                suppressed.records,
                suppressed.points,
            );
        }
        *cursor = Cursor::default();
        progress
    }

    /// Completes the current drain step: moves its instance's tick on, forgets its flushes after a reset, or retires
    /// it, and starts the output's next epoch after a clock reset's last instance.
    fn complete_step(&mut self) {
        let state = self.state_mut();
        let Some(drain) = state.drain.as_mut() else { return };
        let Some(step) = drain.current.take() else { return };
        let reset_done = matches!(step.kind, DrainKind::Reset { .. }) && {
            drain.resets -= 1;
            drain.resets == 0
        };
        if let Some(instance) = state.instances.get_mut(step.instance) {
            complete_instance(instance, &step);
        }
        if reset_done {
            state.output.reset();
        }
    }

    /// Frees every instance no live generation binds and no drain refers to, counting what one still held as invalid
    /// loss of its node: an instance holds nothing once its lineage retired.
    pub(super) fn collect_instances(&mut self) {
        let Self {
            state, invalid, name, ..
        } = self;
        let Some(state) = state.as_deref_mut() else { return };
        if state.drain.is_some() {
            return;
        }
        for handle in state.instances.handles() {
            let unused = state
                .instances
                .get(handle)
                .is_some_and(|instance| instance.users == 0 && instance.retiring.is_none());
            if !unused {
                continue;
            }
            let Some(instance) = state.instances.remove(handle) else {
                continue;
            };
            let held = instance.kernel.kernel.held();
            if held != LossCount::default() {
                warn!(
                    region = %name,
                    node = %instance.key.node,
                    contexts = held.events,
                    points = held.points,
                    "Freed an instance of a state owner's state that still held points, which only an installation \
                     that removes a state owner without a cut leaves; counting them as invalid loss."
                );
                add_loss(&mut invalid.pending, held.events, held.points);
            }
            debug!(region = %name, node = %instance.key.node, created_by = %instance.created_by, "Freed a lineage.");
        }
    }

    /// Releases a generation's uses of the instances it binds; an instance a drain refers to is marked removed and
    /// freed once the drain ends.
    pub(super) fn release_instances(&mut self, bindings: &[KernelBinding]) {
        let Some(state) = self.state.as_deref_mut() else { return };
        for binding in bindings {
            let KernelBinding::Instance(handle) = *binding else {
                continue;
            };
            let referred = state.drain_refers_to(handle);
            if let Some(instance) = state.instances.get_mut(handle) {
                instance.users = instance.users.saturating_sub(1);
                if instance.users == 0 && referred {
                    instance.removed = true;
                }
            }
        }
        self.collect_instances();
    }

    /// Undoes what an aborted generation's preparation did to the state: the retiring marks it set, and the instances
    /// it created, which no other generation can bind yet.
    pub(super) fn abort_state(&mut self, generation: GenerationId) {
        let Some(state) = self.state.as_deref_mut() else { return };
        for handle in state.instances.handles() {
            let referred = state.drain_refers_to(handle);
            let Some(instance) = state.instances.get_mut(handle) else {
                continue;
            };
            if instance
                .retiring
                .as_ref()
                .is_some_and(|retiring| retiring.by.id() == generation)
            {
                instance.retiring = None;
            }
            if instance.created_by == generation && referred {
                instance.removed = true;
            }
        }
    }

    /// Resolves how a preparation binds a state owner's kernel: to the instance its explicit binding names, or else to
    /// the region's instance of its lineage that is not retiring, or else to the new instance the preparation brings.
    pub(super) fn resolve_state(
        &self, kernel: &ProgramKernel, bindings: &[StateBinding], supplied: &mut BTreeMap<KernelKey, PreparedKernel>,
    ) -> Result<StateResolution, String> {
        let lineage = kernel
            .lineage
            .as_ref()
            .ok_or_else(|| format!("kernel `{}` is no state owner's", kernel.key.node))?;
        let state = self.state();
        let binding = bindings.iter().find(|binding| match binding {
            StateBinding::Retain { node, key, .. } | StateBinding::New { node, key } => {
                node == &kernel.key.node && key == &lineage.key
            }
            StateBinding::Retire { .. } => false,
        });
        let new = |supplied: &mut BTreeMap<_, _>| {
            supplied
                .remove(&kernel.key)
                .map(|prepared| StateResolution::New(Box::new(prepared), kernel.ports.len()))
                .ok_or_else(|| {
                    format!(
                        "the state owner `{}` needs a new instance of its lineage, and the preparation brought no \
                         kernel for it",
                        kernel.key.node
                    )
                })
        };
        match binding {
            Some(StateBinding::Retain { from, .. }) => self
                .instance_of(*from, &lineage.key)
                .map(StateResolution::Retain)
                .ok_or_else(|| {
                    format!(
                        "the state owner `{}` retains the instance of its lineage that generation {} uses, which \
                         the region does not hold",
                        kernel.key.node, from
                    )
                }),
            Some(StateBinding::New { .. }) => new(supplied),
            Some(StateBinding::Retire { .. }) => unreachable!("retirements are no bindings of a kernel"),
            None => {
                let shared = state.instances.iter().find(|(_, instance)| {
                    instance.key == lineage.key
                        && instance.users > 0
                        && instance.retiring.is_none()
                        && !instance.retired
                        && !instance.removed
                });
                match shared {
                    Some((handle, _)) => Ok(StateResolution::Retain(handle)),
                    None => new(supplied),
                }
            }
        }
    }

    /// Returns the instance of `key` the live generation `generation` binds, if any.
    pub(super) fn instance_of(&self, generation: GenerationId, key: &LineageKey) -> Option<InstanceHandle> {
        let state = self.state();
        let live = self.generations.iter().find(|live| live.id == generation)?;
        live.kernels.iter().find_map(|binding| match *binding {
            KernelBinding::Instance(handle) => state
                .instances
                .get(handle)
                .filter(|instance| &instance.key == key && !instance.retired)
                .map(|_| handle),
            KernelBinding::Slab(_) => None,
        })
    }

    /// Adds a new instance of a state owner's state, created by `generation`, ticking in the phase of another instance
    /// of its node, or one interval from now.
    pub(super) fn insert_instance(
        &mut self, generation: GenerationId, kernel: &ProgramKernel, prepared: PreparedKernel, ports: usize,
        now: Instant,
    ) -> InstanceHandle {
        let lineage = kernel
            .lineage
            .as_ref()
            .expect("only a state owner's kernel has an instance");
        let state = self.state_mut();
        let phase = state
            .instances
            .iter()
            .filter(|(_, instance)| instance.key.node == lineage.key.node && !instance.retired)
            .find_map(|(_, instance)| match instance.tick {
                TickState::Scheduled { at } | TickState::Gated { at, .. } => Some(at),
                TickState::Unstarted | TickState::Off => None,
            });
        let tick = match (state.started, phase) {
            (false, _) => TickState::Unstarted,
            (true, Some(at)) => TickState::Scheduled { at },
            (true, None) => TickState::Scheduled {
                at: now + lineage.flush_interval,
            },
        };
        state.instances.insert(Instance {
            key: lineage.key.clone(),
            created_by: generation,
            kernel: KernelSlot::new(prepared, ports),
            users: 1,
            interval: lineage.flush_interval,
            tick,
            last_flush: None,
            retiring: None,
            retired: false,
            removed: false,
        })
    }

    /// Delivers a control message to every live instance the target selects, then replies with it.
    pub(super) fn kernel_control_isolated(&mut self, turn: &mut RegionTurn<'_>, control: KernelControl) {
        let KernelControl {
            target,
            mut message,
            reply,
        } = control;
        let Self {
            generations,
            state,
            scratch,
            cursor,
            ..
        } = self;
        let state = state
            .as_deref_mut()
            .expect("only an isolation slot's region runs its state");
        for handle in state.instances.handles() {
            let generation = stamping(generations, state, handle);
            let Some(instance) = state.instances.get_mut(handle) else {
                continue;
            };
            let selected = match &target {
                KernelTarget::Node(node) => &instance.key.node == node,
                KernelTarget::Stateful => true,
            };
            if !selected || instance.retired || instance.removed {
                continue;
            }
            // A panic is charged to the generation whose time-driven output the instance's state would go to, or the
            // one that created it, and to the node it runs.
            let charged = generation.map_or(instance.created_by, |generation| generation.id());
            let node = generations
                .iter()
                .find(|live| live.id == charged)
                .and_then(|live| {
                    live.kernels
                        .iter()
                        .position(|binding| *binding == KernelBinding::Instance(handle))
                })
                .map(NodeRef::Kernel);
            *cursor = Cursor {
                generation: Some(charged),
                node,
                instance: Some(handle),
            };
            let slot = &mut instance.kernel;
            let header = BatchHeader::default();
            let progress = slot.context.accounted(|| {
                let mut kernel_turn = Turn::new(
                    &header,
                    turn.now(),
                    turn.budget_mut(),
                    &mut slot.ports,
                    &mut scratch.counters,
                );
                slot.kernel.control(&mut kernel_turn, message.as_mut())
            });
            debug_assert_eq!(
                progress,
                Progress::Done,
                "a read-only control message completes at once"
            );
            if !slot.ports.is_empty() {
                // A read-only message writes nothing; anything written is dropped as invalid.
                let lost = slot
                    .ports
                    .drain(PortIndex::PRIMARY)
                    .fold(LossCount::default(), |mut lost, buffer| {
                        lost.events += buffer.len() as u64;
                        lost.points += points_in(&buffer);
                        lost
                    });
                scratch
                    .counters
                    .record_loss(LossClass::Invalid, lost.events, lost.points);
                slot.ports.clear();
            }
        }
        *cursor = Cursor::default();
        reply(message);
    }

    /// Recovers an isolation slot from a panic: charges it to the generation whose call ran, rebuilds the instance
    /// whose kernel panicked empty, and keeps the rest of a drain the panic ended for the next call to resume.
    ///
    /// Returns the instance that panicked, if a state owner's kernel did.
    pub(super) fn recover_state(&mut self, cursor: &Cursor) -> Option<InstanceHandle> {
        let state = self.state.as_deref_mut()?;
        state.stashed = None;
        let panicked = cursor.instance;
        if let Some(drain) = state.drain.as_mut() {
            drain.orphaned = true;
            // The instance that panicked restarts empty, so its drain is complete; any other one resumes its call.
            if let Some(step) = drain.current.take_if(|step| Some(step.instance) == panicked) {
                let reset_done = matches!(step.kind, DrainKind::Reset { .. }) && {
                    drain.resets -= 1;
                    drain.resets == 0
                };
                if let Some(instance) = state.instances.get_mut(step.instance) {
                    complete_instance(instance, &step);
                }
                if reset_done {
                    state.output.reset();
                }
            }
        }
        for (_, instance) in state.instances.iter_mut() {
            instance.kernel.ports.clear();
        }
        panicked
    }

    /// Rebuilds an instance whose kernel panicked, empty, counting what it held as invalid loss of its node in the
    /// generation `charged`; returns why it could not be rebuilt, if it could not.
    pub(super) fn rebuild_instance(&mut self, handle: InstanceHandle, charged: usize) -> Result<(), String> {
        let Self { generations, state, .. } = self;
        let Some(state) = state.as_deref_mut() else {
            return Ok(());
        };
        let Some(instance) = state.instances.get_mut(handle) else {
            return Ok(());
        };
        let kernel = &mut instance.kernel;
        let held = catch_unwind(AssertUnwindSafe(|| kernel.kernel.held())).unwrap_or_default();
        let live = &mut generations[charged];
        if let Some(index) = live
            .kernels
            .iter()
            .position(|binding| *binding == KernelBinding::Instance(handle))
        {
            live.counts.kernel_loss[index].record_loss(LossClass::Invalid, held.events, held.points);
        }
        kernel.rebuild()
    }

    /// Returns the live generations that bind an instance.
    pub(super) fn binders(&self, handle: InstanceHandle) -> Vec<usize> {
        (0..self.generations.len())
            .filter(|&index| {
                self.generations[index]
                    .kernels
                    .contains(&KernelBinding::Instance(handle))
            })
            .collect()
    }

    /// Publishes the isolation slot's own telemetry, and what its state owners' kernels accumulated.
    pub(super) fn publish_state(&mut self) {
        let Some(state) = self.state.as_deref_mut() else { return };
        for (_, instance) in state.instances.iter_mut() {
            let slot = &mut instance.kernel;
            slot.context.accounted(|| slot.kernel.publish_telemetry());
        }
        let telemetry = &state.telemetry;
        if state.clock_resets > 0 {
            telemetry
                .clock_resets
                .increment(std::mem::take(&mut state.clock_resets));
        }
        if state.grace_expired > 0 {
            telemetry
                .tick_grace_expired
                .increment(std::mem::take(&mut state.grace_expired));
        }
        let lineages = state
            .instances
            .iter()
            .filter(|(_, instance)| !instance.retired && !instance.removed)
            .count();
        telemetry.lineages.set(lineages as f64);
        if let Some(watermark) = state.tracker.watermark() {
            let wall = state.wall.now();
            let lag = wall.as_unix_nanos().saturating_sub(watermark.as_unix_nanos());
            telemetry.watermark_lag.set(lag as f64 / 1e9);
        }
    }

    /// Returns `true` if the isolation slot can take input: not while a drain is left to complete.
    pub(super) fn accepts_isolated(&self) -> bool {
        self.state().drain.is_none()
    }

    /// Checks that a state owner's kernel was built with the host's state budgets, if the region has any.
    pub(super) fn check_budget(&self, prepared: &PreparedKernel) -> Result<(), String> {
        let state = self.state();
        let Some(budgets) = &state.budgets else {
            return Ok(());
        };
        let shares = prepared
            .context()
            .state_budget()
            .is_some_and(|budget| budget.global.shares_with(budgets.global()));
        if shares {
            Ok(())
        } else {
            Err(format!(
                "the kernel of state owner `{}` was not built with the host's state budget; build state owners' \
                 kernels with `KernelBuildContext::with_state_budget(StateBudgets::context_budget(node))`",
                prepared.key().node
            ))
        }
    }

    /// Logs a generation a region keeps running after a state owner's kernel panicked under it.
    pub(super) fn log_rolling_back(&self, index: usize, node: &str, reason: &str) {
        let generation = &self.generations[index];
        error!(
            region = %self.name,
            generation = %generation.id,
            node,
            reason,
            "A state owner's kernel panicked under an artifact's generation; its state restarted empty, the \
             generation keeps processing its input, and a rollback is requested."
        );
    }
}

/// Completes the drain of one instance: a tick moves its deadline on by the interval of the generation it ran under,
/// keeping its phase; a reset forgets its flushes; a final retirement retires it.
fn complete_instance(instance: &mut Instance, step: &DrainStep) {
    instance.interval = step.interval;
    match step.kind {
        DrainKind::Tick { scheduled, .. } => {
            if !instance.retired {
                instance.tick = TickState::Scheduled {
                    at: scheduled + step.interval,
                };
            }
            instance.last_flush = step.flush;
        }
        DrainKind::Reset { .. } => instance.last_flush = None,
        DrainKind::FinalRetire => {
            instance.retired = true;
            instance.retiring = None;
            instance.tick = TickState::Off;
        }
        DrainKind::Shutdown { .. } => {}
    }
}
