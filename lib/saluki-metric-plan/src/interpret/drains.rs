//! Time-driven work of isolated state owners: what a stamp makes due before its batch is admitted (reset drains,
//! final retirements at a cut, gated ticks), flush ticks, and the end of input.

use std::collections::{BTreeMap, BTreeSet};

use saluki_core::execution::{AdmissionPointId, AdmissionTime, BatchHeader, ClockReset, DrainPolicy, GenerationId};

use super::{
    port_count, program::Stamp, time_driven_header, with_exec, Drain, DrainKind, Exec, HostFinish, HostOutput,
    HostTick, InstallOutcome, InterpretErrorKind, KernelCall, LocalWork, Program, Queue, Run,
};
use crate::runtime::state::rules::{
    retiring_flush_time, shutdown_time, stamping_generation, Observation, OutputClock, TickDecision,
};

impl Run<'_, '_> {
    /// Lets every isolated state owner upstream of a stamp's admission point observe it, and runs what the stamp
    /// makes due before its batch is admitted, in order: the reset drains of a clock reset, the final retirement of
    /// lineages whose replacement every upstream point has now switched to, and the gated ticks the stamp releases.
    pub(super) fn before_admission(
        &mut self, header: &BatchHeader, stamp: &Stamp, now: AdmissionTime,
    ) -> Result<(), InterpretErrorKind> {
        let Some(point) = header.admission_point else {
            return Ok(());
        };
        if stamp.reset {
            self.check_reset_supported(point)?;
        }

        let mut resets = Vec::new();
        for index in self.instances.indices() {
            let Some(instance) = self.instances.get_mut(index) else {
                continue;
            };
            if !instance.observes(point) {
                continue;
            }
            // Whether the instance's timeline before this stamp is the cut's, before the stamp moves it.
            let in_publication_epoch = instance
                .retiring
                .as_ref()
                .and_then(|retiring| retiring.by.cut())
                .is_some_and(|cut| instance.tracker.in_publication_epoch(cut));
            let latest = instance.tracker.max_latest();
            match instance.tracker.observe(header) {
                Observation::Ok => {}
                Observation::Reset { pre, .. } => resets.push((index, pre, latest, in_publication_epoch)),
                Observation::Fault => {
                    return Err(InterpretErrorKind::Internal {
                        message: format!("a state owner rejected the stamp of its upstream point {}", point.get()),
                    })
                }
            }
        }

        // A reset closes everything each lineage fed by the point holds, in the timeline that ended.
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
            for key in clocks {
                if let Some(clock) = self.output_clocks.get_mut(&key) {
                    clock.reset();
                }
            }
            for index in self.instances.indices() {
                if let Some(instance) = self
                    .instances
                    .get_mut(index)
                    .filter(|instance| instance.observes(point))
                {
                    instance.last_flush = None;
                    instance.gated = None;
                }
            }
        }

        // Every upstream point of a retiring lineage has switched: no batch of the generation it served can still come,
        // so it makes its final flush at the cut before the batch that switched it is admitted.
        let crossing = self
            .instances
            .indices()
            .into_iter()
            .filter(|&index| {
                self.instances.get(index).is_some_and(|instance| {
                    !instance.retired
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
            let Some(instance) = self.instances.get_mut(index) else {
                continue;
            };
            let Some((second, _)) = instance.gated else { continue };
            if let Some(flush) = self
                .gate
                .on_watermark(second, instance.tracker.watermark(), instance.last_flush)
            {
                instance.gated = None;
                released.push((index, flush));
            }
        }
        self.fire_ticks(released, now)
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
            let Some(instance) = self.instances.get_mut(index) else {
                continue;
            };
            let cut = instance.retiring.as_ref().and_then(|retiring| retiring.by.cut());
            let in_publication_epoch = cut.is_some_and(|cut| instance.tracker.in_publication_epoch(cut));
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
        let generation = stamping_generation(
            schedule,
            |generation| {
                self.programs
                    .get(&generation)
                    .is_some_and(|program| program.binding(index).is_some())
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
            let Some(node) = program.binding(drain.instance) else {
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
        let queue = self.kernel_output(program, node, call, &header)?;
        if !queue.is_empty() {
            self.output_clocks.insert(key, clock);
        }
        Ok(queue)
    }

    /// Runs one flush tick for every state owner, in topological order.
    pub(super) fn advance(&mut self, clock: AdmissionTime) -> Result<(), InterpretErrorKind> {
        if clock < self.clock {
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
                    if let Some(point) = program.points[node].first().copied().flatten() {
                        sources.entry(point).or_insert_with(|| program.nodes[node].id.clone());
                    }
                }
            }
            for (point, source) in sources {
                // An admission clock never moves back: a source that stamped later already heartbeats past the tick.
                if self.points.latest(point).is_some_and(|latest| latest > clock) {
                    continue;
                }
                self.admit(&source, clock, &[])?;
            }
        }

        // Gated ticks whose grace ended fire at the watermark, before this tick.
        let mut expired = Vec::new();
        for index in self.instances.indices() {
            let Some(instance) = self.instances.get_mut(index) else {
                continue;
            };
            if instance.gated.is_none_or(|(_, until)| clock < until) {
                continue;
            }
            instance.gated = None;
            if let Some(flush) = self.gate.on_grace(instance.tracker.watermark(), instance.last_flush) {
                expired.push((index, flush));
            }
        }
        self.fire_ticks(expired, clock)?;

        // The tick: each isolated instance by the gate, and every other state owner of the active plan.
        let mut fired = Vec::new();
        for index in self.instances.indices() {
            let Some(instance) = self.instances.get_mut(index) else {
                continue;
            };
            // A tick still waiting at the gate is not run again: the earlier one fires first.
            if instance.retired || instance.gated.is_some() {
                continue;
            }
            match self
                .gate
                .due(true, instance.tracker.watermark(), clock, instance.last_flush)
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
                self.kernel_output(program, node, call, &header)
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
        let mut drains = Vec::new();
        for index in self.instances.indices() {
            let Some(instance) = self.instances.get(index).filter(|instance| !instance.retired) else {
                continue;
            };
            // Every upstream point's data has arrived, so the state owner closes at the later of what it saw and the
            // clock; a retiring lineage never past its cut.
            let cut = instance.retiring.as_ref().and_then(|retiring| retiring.by.cut());
            let in_publication_epoch = cut.is_some_and(|cut| instance.tracker.in_publication_epoch(cut));
            let at = retiring_flush_time(
                shutdown_time(instance.tracker.max_latest(), clock),
                cut,
                in_publication_epoch,
            );
            if let Some(drain) = self.drain(index, DrainKind::Shutdown, Some(at), at) {
                drains.push(drain);
            }
        }
        self.local_and_drains(active, drains, clock, LocalWork::finish(DrainPolicy::Shutdown))
    }
}
