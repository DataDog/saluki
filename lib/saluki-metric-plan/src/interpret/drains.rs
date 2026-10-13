//! Time-driven work of isolated state owners: what a stamp makes due before its batch is admitted (reset drains,
//! final retirements at a cut, gated ticks), flush ticks, and the end of input.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use saluki_core::execution::{AdmissionPointId, AdmissionTime, BatchHeader, ClockReset, DrainPolicy, GenerationId};

use super::{
    port_count, program::Stamp, time_driven_header, with_exec, Drain, DrainKind, Exec, HostFinish, HostOutput,
    HostTick, InstallOutcome, InterpretErrorKind, KernelCall, LocalWork, Program, Queue, Run,
};
use crate::equivalence::ContextIdentity;
use crate::runtime::state::rules::{
    retiring_flush_time, shutdown_time, stamping_generation, Observation, OutputClock, TickDecision, WindowStep,
};

impl Run<'_, '_> {
    /// Lets every isolated state owner upstream of a stamp's admission point observe it, and runs what the stamp
    /// makes due before its batch is admitted, in order: the reset windows it opens or closes (or, for a state owner
    /// over points of several clocks, the reset drains of a clock reset), the final retirement of lineages whose
    /// replacement every upstream point has now switched to, and the gated ticks the stamp releases.
    pub(super) fn before_admission(
        &mut self, header: &BatchHeader, stamp: &Stamp, now: AdmissionTime,
    ) -> Result<(), InterpretErrorKind> {
        let Some(point) = header.admission_point else {
            return Ok(());
        };
        if stamp.reset {
            self.check_reset_supported(point)?;
        }
        // The state owners observe the point's progress: in arrival order, its latest time, which a stamp earlier than
        // it leaves where it is.
        let progress = BatchHeader {
            admitted_at: Some(stamp.latest),
            ..header.clone()
        };
        let header = &progress;

        let mut resets = Vec::new();
        for index in self.instances.indices() {
            let Some(instance) = self.instances.get_mut(index) else {
                continue;
            };
            if !instance.observes(point) {
                continue;
            }
            let one_clock = self
                .slots
                .get(&instance.output_clock())
                .is_some_and(|slot| slot.one_clock);
            let latest = instance.tracker.max_latest();
            match instance.tracker.observe(header) {
                Observation::Ok => {}
                // Over the lanes of one clock, the slot's reset window decides what a reset does.
                Observation::EpochSwitched { .. } if one_clock => {}
                Observation::EpochSwitched {
                    point, previous, pre, ..
                } => {
                    // Whether the instance's timeline before this stamp is the cut's, as it stood before the stamp
                    // moved it.
                    let in_publication_epoch = instance
                        .retiring
                        .as_ref()
                        .and_then(|retiring| retiring.by.cut())
                        .is_some_and(|cut| instance.tracker.in_publication_epoch_with(cut, point, previous));
                    resets.push((index, pre, latest, in_publication_epoch));
                }
                Observation::Fault => {
                    return Err(InterpretErrorKind::Internal {
                        message: format!("a state owner rejected the stamp of its upstream point {}", point.get()),
                    })
                }
            }
        }

        // Each slot over the lanes of one clock steps its reset window: the stamp may open one, or close the epochs
        // every upstream point has now left.
        let keys = self
            .slots
            .keys()
            .filter(|key| key.contains(&point))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            let Some(slot) = self.slots.get_mut(&key) else {
                continue;
            };
            if matches!(slot.tracker.observe(header), Observation::Fault) || !slot.one_clock {
                continue;
            }
            match slot.window.step(&slot.tracker, header) {
                WindowStep::None | WindowStep::Route(_) => {}
                WindowStep::Opened { .. } => self.open_window(&key),
                WindowStep::Closed { epochs } => self.close_window(&key, epochs, header.admitted_at, now)?,
            }
        }

        // Over points of several clocks, a reset closes everything each lineage fed by the point holds, in the
        // timeline that ended.
        if !resets.is_empty() {
            let mut drains = Vec::with_capacity(resets.len());
            let mut clocks = BTreeSet::new();
            for (index, pre, latest, in_publication_epoch) in resets {
                let Some(instance) = self.instances.get(index) else {
                    continue;
                };
                clocks.insert(instance.output_clock());
                let cut = instance.retiring.as_ref().and_then(|retiring| retiring.by.cut());
                let flush = pre.map(|pre| retiring_flush_time(pre, cut, in_publication_epoch));
                let stamp_at = flush.or(latest).unwrap_or(stamp.admitted_at);
                if let Some(drain) = self.drain(index, DrainKind::Reset, flush, stamp_at) {
                    drains.push(drain);
                }
            }
            self.run_drains(drains, now)?;
            for key in &clocks {
                if let Some(clock) = self.output_clocks.get_mut(key) {
                    clock.reset();
                }
            }
            for index in self.instances.indices() {
                if let Some(instance) = self
                    .instances
                    .get_mut(index)
                    .filter(|instance| clocks.contains(&instance.output_clock()))
                {
                    instance.last_flush = None;
                    instance.gated = None;
                }
            }
        }

        // Every upstream point of a retiring lineage has switched: no batch of the generation it served can still come,
        // so it makes its final flush at the cut before the batch that switched it is admitted. A lineage inside a
        // reset window waits for the window's close, where its reset drain is its final retirement.
        let crossing = self
            .instances
            .indices()
            .into_iter()
            .filter(|&index| {
                self.instances.get(index).is_some_and(|instance| {
                    instance.ticks()
                        && instance.retiring.as_ref().is_some_and(|retiring| {
                            retiring.by.cut().is_some() && instance.tracker.switched(retiring.by.id())
                        })
                })
            })
            .collect::<Vec<_>>();
        if !crossing.is_empty() {
            self.retire(&crossing, now)?;
        }

        // Gated ticks the stamp's watermark reached.
        let mut released = Vec::new();
        for index in self.instances.indices() {
            let one_clock = self.one_clock(index);
            let Some(instance) = self.instances.get_mut(index) else {
                continue;
            };
            let Some((second, _)) = instance.gated else { continue };
            if let Some(flush) = self
                .gate
                .on_watermark(second, instance.watermark(one_clock), instance.last_flush)
            {
                instance.gated = None;
                released.push((index, flush));
            }
        }
        self.fire_ticks(released, now)
    }

    /// Returns `true` if the instance at `index` retires at a cut and holds data of the cut's publication epoch, which
    /// it never flushes past the cut in. Over the lanes of one clock, that is the instance's own epoch, whatever epoch
    /// each lane has reached, as a state lane decides it; a lane that has stamped nothing yet does not make it another.
    /// Over points of several clocks, every point must be in the epoch the cut recorded for it.
    pub(super) fn in_publication_epoch(&self, index: usize) -> bool {
        let one_clock = self.one_clock(index);
        let Some(instance) = self.instances.get(index) else {
            return false;
        };
        let Some(cut) = instance.retiring.as_ref().and_then(|retiring| retiring.by.cut()) else {
            return false;
        };
        if one_clock {
            instance.tracker.is_publication_epoch(cut, instance.epoch)
        } else {
            instance.tracker.in_publication_epoch(cut)
        }
    }

    /// Returns `true` if the instance at `index` belongs to a slot over the lanes of one clock.
    pub(super) fn one_clock(&self, index: usize) -> bool {
        self.instances
            .get(index)
            .and_then(|instance| self.slots.get(&instance.output_clock()))
            .is_some_and(|slot| slot.one_clock)
    }

    /// Opens or extends the reset window of the slot `key`: every instance holding an older epoch's data is reset-
    /// retiring from now on. It fires no further tick, and a tick waiting at its gate is dropped, as a reset drops it.
    fn open_window(&mut self, key: &[AdmissionPointId]) {
        let Some(target) = self.slots.get(key).map(|slot| slot.window.target()) else {
            return;
        };
        for index in self.instances.indices() {
            if let Some(instance) = self.instances.get_mut(index) {
                if !instance.retired && instance.output_clock() == key && instance.epoch != target {
                    instance.reset_retiring = true;
                    instance.gated = None;
                }
            }
        }
    }

    /// Closes epochs of the slot `key`'s reset window, oldest first, each with its watermark: every instance holding
    /// a closed epoch's data runs the reset drain at that watermark (a cut-retiring one never past its cut, if the
    /// epoch is the cut's publication epoch), and the slot's output clock starts a new epoch after each. Each lineage
    /// then continues in the oldest epoch still open, or the window's target once it is closed: through its instance
    /// of that epoch, which every generation binding the lineage binds from now on, or else through its drained
    /// instance, which is empty, as if it had never flushed.
    ///
    /// A cut-retiring lineage whose drained epoch was the cut's publication epoch has had its last batch of the
    /// generation it served: every newer-epoch stamp is the cut's new generation, so every point has switched to it,
    /// and the stamp's crossing check retires it right after, its final flush finding it empty. Its reset drain is its
    /// final retirement.
    fn close_window(
        &mut self, key: &[AdmissionPointId], epochs: Vec<(u32, Option<AdmissionTime>)>,
        admitted_at: Option<AdmissionTime>, now: AdmissionTime,
    ) -> Result<(), InterpretErrorKind> {
        for (epoch, watermark) in epochs {
            let mut drains = Vec::new();
            for index in self.instances.indices() {
                let Some(instance) = self
                    .instances
                    .get(index)
                    .filter(|instance| !instance.retired && instance.output_clock() == key && instance.epoch == epoch)
                else {
                    continue;
                };
                let Some(slot) = self.slots.get(key) else { continue };
                let cut = instance.retiring.as_ref().and_then(|retiring| retiring.by.cut());
                let publication = cut.is_some_and(|cut| slot.tracker.is_publication_epoch(cut, epoch));
                let flush = watermark.map(|watermark| retiring_flush_time(watermark, cut, publication));
                let stamp_at = flush
                    .or_else(|| slot.tracker.max_latest_in(epoch))
                    .or(admitted_at)
                    .unwrap_or(now);
                if let Some(drain) = self.drain(index, DrainKind::Reset, flush, stamp_at) {
                    drains.push(drain);
                }
            }
            self.run_drains(drains, now)?;
            if let Some(clock) = self.output_clocks.get_mut(key) {
                clock.reset();
            }
        }

        let Some(slot) = self.slots.get(key) else {
            return Ok(());
        };
        let (target, open) = (slot.window.target(), slot.window.is_open());
        let next = slot.window.older().first().copied().filter(|_| open).unwrap_or(target);
        let held = |epoch: u32| epoch == target || slot.window.older().contains(&epoch);
        let mut lineages = BTreeMap::<usize, Vec<usize>>::new();
        for index in self.instances.indices() {
            if let Some(instance) = self
                .instances
                .get(index)
                .filter(|instance| !instance.retired && instance.output_clock() == key)
            {
                lineages.entry(instance.root(index)).or_default().push(index);
            }
        }
        let closed = lineages
            .values()
            .flatten()
            .filter(|&&index| self.instances.get(index).is_some_and(|instance| !held(instance.epoch)))
            .copied()
            .collect::<BTreeSet<_>>();
        for (root, members) in lineages {
            debug_assert!(
                members.contains(&root),
                "a lineage whose bound instance retired at a cut has no instance of a newer epoch"
            );
            if !members.contains(&root) {
                continue;
            }
            let survivor = members
                .iter()
                .copied()
                .find(|&index| self.instances.get(index).is_some_and(|instance| instance.epoch == next));
            match survivor {
                Some(survivor) if survivor != root && closed.contains(&root) => self.rebind(root, survivor),
                Some(_) => {}
                None if closed.contains(&root) => {
                    if let Some(instance) = self.instances.get_mut(root) {
                        instance.epoch = next;
                        instance.last_flush = None;
                        instance.gated = None;
                        if let Some(admitted) = &mut instance.admitted {
                            admitted.clear();
                        }
                    }
                }
                None => {}
            }
            for index in members {
                if index != root && closed.contains(&index) && Some(index) != survivor {
                    self.instances.remove(index);
                }
            }
        }
        for index in self.instances.indices() {
            if let Some(instance) = self
                .instances
                .get_mut(index)
                .filter(|instance| !instance.retired && instance.output_clock() == key)
            {
                instance.reset_retiring = open && instance.epoch != target;
            }
        }
        Ok(())
    }

    /// Moves a lineage from its bound instance `from` to its instance of a newer epoch `to`: every program that binds
    /// `from` binds `to`, which keeps the lineage's cut, and `from`, drained, is dropped.
    fn rebind(&mut self, from: usize, to: usize) {
        for program in self.programs.values_mut() {
            for node in &mut program.nodes {
                if let Exec::State { instance, .. } = &mut node.exec {
                    if *instance == Some(from) {
                        *instance = Some(to);
                    }
                }
            }
        }
        let retiring = self
            .instances
            .get_mut(from)
            .and_then(|instance| instance.retiring.take());
        for index in self.instances.indices() {
            if let Some(instance) = self.instances.get_mut(index) {
                if instance.primary == Some(from) {
                    instance.primary = (index != to).then_some(to);
                }
            }
        }
        if let Some(instance) = self.instances.get_mut(to) {
            instance.primary = None;
            instance.last_flush = None;
            instance.gated = None;
            if retiring.is_some() {
                instance.retiring = retiring;
            }
        }
        self.instances.remove(from);
    }

    /// Fails if a clock reset at `point` reaches a state owner that is not isolated, whose host handles resets itself.
    pub(super) fn check_reset_supported(&self, point: AdmissionPointId) -> Result<(), InterpretErrorKind> {
        for program in self.programs.values() {
            for (position, node) in program.nodes.iter().enumerate() {
                if node.is_local_state() && program.upstream[position].contains(&point) {
                    return Err(InterpretErrorKind::Unsupported {
                        capability: format!(
                            "a clock reset at admission point `{}` upstream of `{}`, a state owner that does not run \
                             behind an isolation fence,",
                            self.points.name(point),
                            node.id
                        ),
                        stage: 10,
                    });
                }
            }
        }
        Ok(())
    }

    /// Runs the final retirement of retiring instances: a flush at the cut's time, then everything they hold, under the
    /// generation they served; they then hold nothing, and stay only until the plans that bind them retire.
    pub(super) fn retire(&mut self, instances: &[usize], now: AdmissionTime) -> Result<(), InterpretErrorKind> {
        let mut drains = Vec::new();
        for &index in instances {
            let Some(instance) = self.instances.get(index) else {
                continue;
            };
            let Some(at) = instance
                .retiring
                .as_ref()
                .and_then(|retiring| retiring.by.cut())
                .map(|cut| cut.at)
            else {
                continue;
            };
            if let Some(drain) = self.drain(index, DrainKind::FinalRetire, Some(at), at) {
                drains.push(drain);
            }
        }
        self.run_drains(drains, now)?;
        for &index in instances {
            if let Some(instance) = self.instances.get_mut(index) {
                instance.retired = true;
                instance.retiring = None;
                instance.gated = None;
            }
        }
        Ok(())
    }

    /// Fires flush ticks of instances at their flush times; see [`tick_drains`][Self::tick_drains].
    pub(super) fn fire_ticks(
        &mut self, ticks: Vec<(usize, AdmissionTime)>, now: AdmissionTime,
    ) -> Result<(), InterpretErrorKind> {
        let drains = self.tick_drains(ticks);
        self.run_drains(drains, now)
    }

    /// Returns the drains of flush ticks that fire at the given flush times, each clamped before its cut if its
    /// instance retires, and records each as its instance's last flush.
    pub(super) fn tick_drains(&mut self, ticks: Vec<(usize, AdmissionTime)>) -> Vec<Drain> {
        let mut drains = Vec::new();
        for (index, flush) in ticks {
            let in_publication_epoch = self.in_publication_epoch(index);
            let Some(instance) = self.instances.get_mut(index) else {
                continue;
            };
            let cut = instance.retiring.as_ref().and_then(|retiring| retiring.by.cut());
            let flush = retiring_flush_time(flush, cut, in_publication_epoch);
            instance.last_flush = Some(flush);
            if let Some(drain) = self.drain(index, DrainKind::Tick, Some(flush), flush) {
                drains.push(drain);
            }
        }
        drains
    }

    /// Returns the drain of one instance, under the generation the shared rules choose for it now, or `None` if no
    /// live generation binds it.
    pub(super) fn drain(
        &self, index: usize, kind: DrainKind, flush: Option<AdmissionTime>, stamp_at: AdmissionTime,
    ) -> Option<Drain> {
        let instance = self.instances.get(index)?;
        let schedule = self.schedule.as_ref()?;
        let bound = instance.root(index);
        let generation = stamping_generation(
            schedule,
            |generation| {
                self.programs
                    .get(&generation)
                    .is_some_and(|program| program.binding(bound).is_some())
            },
            |generation| instance.tracker.switched(generation),
            instance.retiring_refs(),
        )?;
        Some(Drain {
            instance: index,
            kind,
            flush,
            stamp_at,
            generation,
        })
    }

    /// Runs drains: those of each generation in one pass over its plan, the oldest generation first, each drain at
    /// its node's turn, and its output from there on.
    pub(super) fn run_drains(&mut self, drains: Vec<Drain>, now: AdmissionTime) -> Result<(), InterpretErrorKind> {
        let mut by_generation = BTreeMap::<GenerationId, Vec<Drain>>::new();
        for drain in drains {
            by_generation.entry(drain.generation.id()).or_default().push(drain);
        }
        for (generation, drains) in by_generation {
            let Some(mut program) = self.programs.remove(&generation) else {
                return Err(InterpretErrorKind::Internal {
                    message: format!("generation {} drains an instance but has no plan", generation),
                });
            };
            let result = self.drain_pass(&mut program, drains, now, None);
            self.programs.insert(generation, program);
            result?;
        }
        Ok(())
    }

    /// Runs one pass over `program` in which each drain runs at its node's turn; with `local`, every state owner of
    /// the program that is not isolated also ticks, or finishes under the policy given, at its turn.
    pub(super) fn drain_pass(
        &mut self, program: &mut Program, drains: Vec<Drain>, now: AdmissionTime, local: Option<LocalWork>,
    ) -> Result<(), InterpretErrorKind> {
        let mut by_node = BTreeMap::<usize, Vec<Drain>>::new();
        for drain in drains {
            let bound = self
                .instances
                .get(drain.instance)
                .map_or(drain.instance, |instance| instance.root(drain.instance));
            let Some(node) = program.binding(bound) else {
                return Err(InterpretErrorKind::Internal {
                    message: format!(
                        "generation {} drains instance {}, which it does not bind",
                        program.generation.id(),
                        drain.instance
                    ),
                });
            };
            by_node.entry(node).or_default().push(drain);
        }
        self.pass(program, Queue::new(), now, |run, program, node| {
            let mut queue = match local {
                Some(work) if program.nodes[node].is_local_state() || work.all_kernels => {
                    run.local_work(program, node, work, now)?
                }
                _ => Queue::new(),
            };
            for drain in by_node.remove(&node).unwrap_or_default() {
                queue.extend(run.run_drain(program, node, drain, now)?);
            }
            Ok(queue)
        })
    }

    /// Runs one drain of an isolated state owner's instance, at the instance's node in `program`, and returns the
    /// deliveries its output causes.
    pub(super) fn run_drain(
        &mut self, program: &Program, node: usize, drain: Drain, now: AdmissionTime,
    ) -> Result<Queue, InterpretErrorKind> {
        let id = program.nodes[node].id.clone();
        let gone = || InterpretErrorKind::Internal {
            message: format!("instance {} of `{}` is gone", drain.instance, id),
        };
        // The output clock moves only when the drain emits something: a drain that emits nothing, such as the final
        // retirement of a lineage a reset drain already emptied, stamps nothing.
        let key = self.instances.get(drain.instance).ok_or_else(gone)?.output_clock();
        let mut clock = *self
            .output_clocks
            .entry(key.clone())
            .or_insert_with(|| OutputClock::new(AdmissionPointId::new(u32::MAX)));
        let header = clock.stamp(drain.stamp_at, drain.generation.clone());
        let step = program.nodes[node].step();
        let mut call = KernelCall::new(&header, self.instant(now), port_count(program, node))
            .with_watermark(drain.flush)
            .with_param(step.as_deref());
        let kernel = &mut self.instances.get_mut(drain.instance).ok_or_else(gone)?.kernel;
        match drain.kind {
            DrainKind::Tick => call.repeat(&id, |turn| kernel.advance_time(turn))?,
            DrainKind::Reset => call.repeat(&id, |turn| kernel.control(turn, &mut ClockReset))?,
            DrainKind::FinalRetire => call.repeat(&id, |turn| kernel.finish(turn, DrainPolicy::Retire))?,
            DrainKind::Shutdown => call.repeat(&id, |turn| kernel.finish(turn, DrainPolicy::Shutdown))?,
        }
        // In candidate mode every record carries what its entry was admitted with, per bucket it holds.
        let admitted = self
            .instances
            .get(drain.instance)
            .and_then(|instance| instance.admitted.clone());
        let queue = self.kernel_output(program, node, call, &header, |metric| {
            let entry = admitted.as_ref()?.get(&ContextIdentity::of(metric))?;
            Some(Arc::new(entry.tag()))
        })?;
        if !queue.is_empty() {
            self.output_clocks.insert(key, clock);
        }
        Ok(queue)
    }

    /// Runs one flush tick for every state owner, in topological order.
    pub(super) fn advance(&mut self, clock: AdmissionTime) -> Result<(), InterpretErrorKind> {
        // The wall clock the ticks read may jump back as an admission clock's does: by more than the reset threshold,
        // it is a new timeline, which the lanes of a partitioned frame tick in, as an arrival replay states.
        let jump = Duration::from_nanos(self.clock.as_unix_nanos().saturating_sub(clock.as_unix_nanos()));
        if clock < self.clock && jump <= self.points.reset_threshold() {
            return Err(InterpretErrorKind::ClockWentBackward {
                previous: self.clock,
                clock,
            });
        }
        self.clock = clock;
        if self.active_id().is_none() {
            return Ok(());
        }

        if self.interpreter.heartbeats_at_ticks {
            let mut sources = BTreeMap::new();
            for program in self.programs.values() {
                for node in program.sources() {
                    if let Some(group) = program.points[node].first().copied().flatten() {
                        for (lane, point) in group.ids().enumerate() {
                            sources
                                .entry(point)
                                .or_insert_with(|| (program.nodes[node].id.clone(), lane as u16));
                        }
                    }
                }
            }
            for (point, (source, lane)) in sources {
                // An admission clock never moves back: a lane that stamped later in its clock's epoch already
                // heartbeats past the tick.
                if self.points.epoch(point) == self.points.clock_epoch(point)
                    && self.points.latest(point).is_some_and(|latest| latest > clock)
                {
                    continue;
                }
                self.admit(&source, lane, clock, &[])?;
            }
        }

        // Gated ticks whose grace ended fire at the watermark, before this tick.
        let mut expired = Vec::new();
        for index in self.instances.indices() {
            let one_clock = self.one_clock(index);
            let Some(instance) = self.instances.get_mut(index) else {
                continue;
            };
            if instance.gated.is_none_or(|(_, until)| clock < until) {
                continue;
            }
            instance.gated = None;
            if let Some(flush) = self.gate.on_grace(instance.watermark(one_clock), instance.last_flush) {
                expired.push((index, flush));
            }
        }
        self.fire_ticks(expired, clock)?;

        // The tick: each isolated instance by the gate, and every other state owner of the active plan.
        let mut fired = Vec::new();
        for index in self.instances.indices() {
            let one_clock = self.one_clock(index);
            let Some(instance) = self.instances.get_mut(index) else {
                continue;
            };
            // A tick still waiting at the gate is not run again: the earlier one fires first. An instance a reset
            // window holds fires no tick: an older epoch's drains when its epoch closes, and a newer epoch's has no
            // watermark until every upstream point switched to it.
            if !instance.ticks() || instance.gated.is_some() {
                continue;
            }
            match self
                .gate
                .due(true, instance.watermark(one_clock), clock, instance.last_flush)
            {
                TickDecision::Skip => {}
                TickDecision::Gate { second } => {
                    instance.gated = Some((second, clock.saturating_add(self.gate.grace())));
                }
                TickDecision::Fire { flush } => fired.push((index, flush)),
            }
        }
        let drains = self.tick_drains(fired);
        // A heartbeat may have finalized a cut, and started the installation waiting for it.
        let Some(active) = self.active_id() else {
            return Ok(());
        };
        self.local_and_drains(active, drains, clock, LocalWork::tick())
    }

    /// Runs drains under their generations, as [`run_drains`][Self::run_drains] does, together with local work for
    /// every state owner of the active plan that is not isolated, in the active generation's pass, which always runs.
    pub(super) fn local_and_drains(
        &mut self, active: GenerationId, drains: Vec<Drain>, now: AdmissionTime, work: LocalWork,
    ) -> Result<(), InterpretErrorKind> {
        let mut by_generation = BTreeMap::<GenerationId, Vec<Drain>>::new();
        by_generation.entry(active).or_default();
        for drain in drains {
            by_generation.entry(drain.generation.id()).or_default().push(drain);
        }
        for (generation, drains) in by_generation {
            let Some(mut program) = self.programs.remove(&generation) else {
                return Err(InterpretErrorKind::Internal {
                    message: format!("generation {} has no plan", generation),
                });
            };
            let local = (generation == active || work.all_kernels).then_some(work);
            let result = self.drain_pass(&mut program, drains, now, local);
            self.programs.insert(generation, program);
            result?;
        }
        Ok(())
    }

    /// Runs the work an event gives a node that is not an isolated state owner: a tick for a stateful kernel or an
    /// opaque host, or a finish for any kernel or host.
    pub(super) fn local_work(
        &mut self, program: &mut Program, node: usize, work: LocalWork, now: AdmissionTime,
    ) -> Result<Queue, InterpretErrorKind> {
        let watermark = self.watermark(program, node);
        with_exec(program, node, |program, exec| match (exec, work.policy) {
            (
                Exec::Kernel {
                    kernel: Some(kernel), ..
                },
                policy,
            ) => {
                let header = time_driven_header(program, watermark);
                let mut call =
                    KernelCall::new(&header, self.instant(now), port_count(program, node)).with_watermark(watermark);
                match policy {
                    None => call.repeat(&program.nodes[node].id, |turn| kernel.advance_time(turn))?,
                    Some(policy) => call.repeat(&program.nodes[node].id, |turn| kernel.finish(turn, policy))?,
                }
                self.kernel_output(program, node, call, &header, |_| None)
            }
            (Exec::Host(Some(adapter)), None) => {
                let mut output = HostOutput::new(port_count(program, node));
                adapter.tick(HostTick { now, watermark }, &mut output);
                self.readmit(program, node, output, now, true)
            }
            (Exec::Host(Some(adapter)), Some(policy)) => {
                let mut output = HostOutput::new(port_count(program, node));
                adapter.finish(HostFinish { now, policy }, &mut output);
                self.readmit(program, node, output, now, false)
            }
            _ => Ok(Queue::new()),
        })
    }

    /// Finishes every kernel and host under the shutdown policy, in topological order, and ends the input.
    pub(super) fn close(&mut self) -> Result<(), InterpretErrorKind> {
        self.closed = true;
        if let Some(queued) = self.queued.take() {
            self.record(queued.event, InstallOutcome::Waiting);
        }
        let Some(active) = self.active_id() else {
            return Ok(());
        };
        let clock = self.clock;
        // Every upstream point's last stamp has arrived, so an open reset window closes first, each older epoch at its
        // exact watermark so far.
        let open = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.one_clock && slot.window.is_open())
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in open {
            let Some(slot) = self.slots.get_mut(&key) else {
                continue;
            };
            let epochs = slot.window.close(&slot.tracker);
            self.close_window(&key, epochs, None, clock)?;
        }
        let mut drains = Vec::new();
        for index in self.instances.indices() {
            let one_clock = self.one_clock(index);
            let in_publication_epoch = self.in_publication_epoch(index);
            let Some(instance) = self.instances.get(index).filter(|instance| !instance.retired) else {
                continue;
            };
            // Every upstream point's data has arrived, so the state owner closes at the later of what it saw in its
            // epoch and the clock; a retiring lineage never past its cut.
            let cut = instance.retiring.as_ref().and_then(|retiring| retiring.by.cut());
            let latest = if one_clock {
                instance.tracker.max_latest_in(instance.epoch)
            } else {
                instance.tracker.max_latest()
            };
            let at = retiring_flush_time(shutdown_time(latest, clock), cut, in_publication_epoch);
            if let Some(drain) = self.drain(index, DrainKind::Shutdown, Some(at), at) {
                drains.push(drain);
            }
        }
        self.local_and_drains(active, drains, clock, LocalWork::finish(DrainPolicy::Shutdown))
    }
}
