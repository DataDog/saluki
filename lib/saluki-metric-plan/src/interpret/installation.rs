//! Installations: how a candidate's state owners correspond to the active plan's, commits, cuts, cancellation, and
//! finalization.

use std::{collections::BTreeSet, sync::Arc};

use saluki_core::execution::{
    AdmissionPointId, AdmissionSchedule, AdmissionTime, CutBoundary, DrainPolicy, GenerationId, GenerationToken,
};

use super::{
    internal, lineage_key, local,
    program::{build_kernel, build_state_kernel},
    CutInFlight, Exec, InstallOutcome, InstallRejection, Instance, InterpretErrorKind, LocalWork, PrepareError,
    Program, Queue, Queued, Retiring, Run, Transition,
};
use crate::{
    extension::TransitionClass,
    ir::Plan,
    limits,
    runtime::state::rules::{cut_time, lcm_secs, WatermarkTracker},
    validate::Validator,
};

impl Run<'_, '_> {
    pub(super) fn reject(&mut self, position: usize, rejection: InstallRejection) {
        self.last_rejection = Some(rejection.to_string());
        self.record(position, InstallOutcome::Rejected(rejection));
    }

    /// Proposes a plan: it is validated and prepared at once, and then starts, or, while a cut is in flight, waits for
    /// it to finish, replacing the installation waiting before it.
    ///
    /// Validation and preparation, which depend on the plan alone, are decided here, when it is proposed, as the
    /// installer rejects an invalid artifact before it competes for installation: only a valid candidate waits, so an
    /// invalid one never replaces it. What depends on the plan active when it starts, its transition and the instances
    /// it does not carry over, is decided then.
    pub(super) fn install(&mut self, position: usize, plan: &Plan) -> Result<(), InterpretErrorKind> {
        let Some(program) = self.prepare_candidate(position, plan)? else {
            return Ok(());
        };
        if self.cut.is_some() {
            let waiting = Queued {
                event: position,
                program,
            };
            if let Some(replaced) = self.queued.replace(waiting) {
                self.record(replaced.event, InstallOutcome::Superseded { by: position });
            }
            return Ok(());
        }
        self.start_install(position, program)
    }

    /// Starts the installation of the waiting plan, if one waits.
    pub(super) fn start_queued(&mut self) -> Result<(), InterpretErrorKind> {
        match self.queued.take() {
            Some(queued) => self.start_install(queued.event, queued.program),
            None => Ok(()),
        }
    }

    /// Validates and prepares a proposed plan, or records its rejection and returns `None`.
    fn prepare_candidate(&mut self, position: usize, plan: &Plan) -> Result<Option<Program>, InterpretErrorKind> {
        let report = Validator::new(self.interpreter.registry, self.interpreter.catalog).validate(plan);
        if !report.is_valid() {
            self.reject(position, InstallRejection::Invalid { report });
            return Ok(None);
        }

        // Nothing is stamped with a generation until it is published, so the ID of a candidate that is rejected, or
        // replaced while it waits, is never seen: the next candidate takes it.
        let generation = GenerationId::new(self.generations + 1);
        let prepared = Program::prepare(
            plan,
            self.interpreter.registry,
            &report,
            self.interpreter.catalog,
            &mut self.points,
            position,
            Arc::new(GenerationToken::new(generation)),
        );
        match prepared {
            Ok(program) => Ok(Some(program)),
            Err(PrepareError::Rejected(rejection)) => {
                self.reject(position, rejection);
                Ok(None)
            }
            Err(PrepareError::Fatal(kind)) => Err(kind),
        }
    }

    /// Starts the installation of a prepared candidate: classifies its transition from the active plan, builds what it
    /// does not carry over, and commits it at once or arms its cut.
    pub(super) fn start_install(&mut self, position: usize, mut program: Program) -> Result<(), InterpretErrorKind> {
        let transition = match self.classify(&program) {
            Ok(transition) => transition,
            Err(PrepareError::Rejected(rejection)) => {
                self.reject(position, rejection);
                return Ok(());
            }
            Err(PrepareError::Fatal(kind)) => return Err(kind),
        };
        let carried_new = transition
            .carried_local
            .iter()
            .map(|(_, new)| *new)
            .collect::<BTreeSet<_>>();
        let lineages = match self
            .instantiate(&mut program, &carried_new)
            .and_then(|()| self.new_lineages(&program, &transition))
        {
            Ok(lineages) => lineages,
            Err(PrepareError::Rejected(rejection)) => {
                self.reject(position, rejection);
                return Ok(());
            }
            Err(PrepareError::Fatal(kind)) => return Err(kind),
        };

        if transition.is_cut() {
            self.arm(position, program, transition, lineages)
        } else {
            self.commit(position, program, transition, lineages)
        }
    }

    /// Classifies how the state owners of a candidate program correspond to those of the active one.
    pub(super) fn classify(&self, new: &Program) -> Result<Transition, PrepareError> {
        let mut transition = Transition::default();
        let Some(old) = self.active_id().and_then(|id| self.programs.get(&id)) else {
            transition.added = (0..new.nodes.len())
                .filter(|&node| new.nodes[node].is_isolated())
                .collect();
            return Ok(transition);
        };
        transition.replaces = true;
        let unsupported = |capability: String, stage| {
            Err(PrepareError::Fatal(InterpretErrorKind::Unsupported {
                capability,
                stage,
            }))
        };

        for (to, node) in new.nodes.iter().enumerate() {
            let previous = old.index.get(&node.id).map(|&from| (from, &old.nodes[from]));
            if node.is_isolated() || previous.is_some_and(|(_, previous)| previous.is_isolated()) {
                match previous {
                    Some((_, previous)) if previous.is_isolated() && node.is_isolated() => {
                        let class = previous.op.transition_to(&node.op);
                        if class == TransitionClass::Reject {
                            return Err(PrepareError::Rejected(InstallRejection::Transition {
                                node: node.id.clone(),
                                class,
                            }));
                        }
                        let Some((index, instance)) = previous
                            .instance()
                            .and_then(|index| self.instances.get(index).map(|instance| (index, instance)))
                        else {
                            return Err(internal(&format!("state owner `{}` binds no instance", node.id)));
                        };
                        let key = lineage_key(&node.id, &node.op)?;
                        if class == TransitionClass::Retain && instance.key == key {
                            let upstream = instance.tracker.upstream().collect::<Vec<_>>();
                            if upstream != new.upstream[to] {
                                return unsupported(
                                    format!(
                                        "changing the admission points upstream of state owner `{}` while it keeps \
                                         its state",
                                        node.id
                                    ),
                                    10,
                                );
                            }
                            transition.retained.push((to, index));
                        } else {
                            transition.added.push(to);
                            transition.retiring.push(index);
                            transition.widths.extend(previous.bucket_width());
                            transition.widths.extend(node.bucket_width());
                        }
                    }
                    Some(_) => {
                        return unsupported(
                            format!(
                                "moving state owner `{}` into or out of an isolation fence at an installation",
                                node.id
                            ),
                            10,
                        )
                    }
                    None => {
                        transition.added.push(to);
                        transition.widths.extend(node.bucket_width());
                    }
                }
                continue;
            }
            match previous {
                Some((from, previous)) if previous.is_stateful() || node.is_stateful() => {
                    match previous.op.transition_to(&node.op) {
                        TransitionClass::Reject => {
                            return Err(PrepareError::Rejected(InstallRejection::Transition {
                                node: node.id.clone(),
                                class: TransitionClass::Reject,
                            }))
                        }
                        TransitionClass::Retain if previous.op.params() == node.op.params() => {
                            transition.carried_local.push((from, to));
                        }
                        TransitionClass::Retain | TransitionClass::Cut => {
                            return unsupported(format!("changing {} while it keeps its state", local(node)), 10)
                        }
                    }
                }
                Some(_) => {}
                None if node.is_stateful() => {
                    return unsupported(format!("adding {} at an installation", local(node)), 10);
                }
                None => {}
            }
        }
        for node in &old.nodes {
            if new.index.contains_key(&node.id) || !node.is_stateful() {
                continue;
            }
            match node.instance() {
                Some(index) => {
                    transition.retiring.push(index);
                    transition.widths.extend(node.bucket_width());
                }
                None => {
                    return unsupported(format!("removing {} at an installation", local(node)), 10);
                }
            }
        }
        Ok(transition)
    }

    /// Builds the instances of the stateful kernels that are not isolated and the opaque hosts that do not carry over.
    pub(super) fn instantiate(&self, program: &mut Program, carried: &BTreeSet<usize>) -> Result<(), PrepareError> {
        for (position, node) in program.nodes.iter_mut().enumerate() {
            if carried.contains(&position) {
                continue;
            }
            match &mut node.exec {
                Exec::Kernel {
                    spec,
                    kernel: kernel @ None,
                } => *kernel = Some(build_kernel(&node.id, spec)?),
                Exec::Host(adapter @ None) => {
                    let built = match self.interpreter.hosts.get(&node.id) {
                        Some(factory) => factory(&node.op),
                        None => {
                            return Err(PrepareError::Fatal(InterpretErrorKind::MissingAdapter {
                                node: node.id.clone(),
                            }))
                        }
                    };
                    let built = built.map_err(|source| {
                        PrepareError::Rejected(InstallRejection::HostBuild {
                            node: node.id.clone(),
                            source,
                        })
                    })?;
                    *adapter = Some(built);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Builds the kernels of the lineages a candidate starts, each with the state budget of its node.
    pub(super) fn new_lineages(
        &self, program: &Program, transition: &Transition,
    ) -> Result<Vec<(usize, Instance)>, PrepareError> {
        transition
            .added
            .iter()
            .map(|&position| {
                let node = &program.nodes[position];
                let Exec::State { spec, .. } = &node.exec else {
                    return Err(internal(&format!("`{}` starts a lineage but is not isolated", node.id)));
                };
                let kernel = build_state_kernel(&node.id, spec, self.budgets.context_budget(&node.id))?;
                let key = lineage_key(&node.id, &node.op)?;
                let upstream = &program.upstream[position];
                let mut instance = Instance::new(key, kernel, upstream);
                // An isolation slot keeps one watermark over its upstream points, which a new lineage starts from: the
                // progress of any instance fed by the same points that still observes them.
                if let Some(tracker) = self.slot_tracker(upstream) {
                    instance.tracker = tracker.clone();
                }
                Ok((position, instance))
            })
            .collect()
    }

    /// Returns the progress of the admission points `upstream` as an isolation slot fed by them keeps it: the tracker
    /// of an instance fed by exactly those points that has not retired, if there is one.
    fn slot_tracker(&self, upstream: &[AdmissionPointId]) -> Option<&WatermarkTracker> {
        let upstream = upstream.iter().copied().collect::<BTreeSet<_>>();
        self.instances
            .indices()
            .into_iter()
            .filter_map(|index| self.instances.get(index))
            .find(|instance| !instance.retired && instance.tracker.upstream().collect::<BTreeSet<_>>() == upstream)
            .map(|instance| &instance.tracker)
    }

    /// Binds the candidate's isolated state owners: retained lineages to their instances, new ones to new instances.
    pub(super) fn bind(&mut self, program: &mut Program, transition: &Transition, lineages: Vec<(usize, Instance)>) {
        let bindings = transition
            .retained
            .iter()
            .copied()
            .chain(
                lineages
                    .into_iter()
                    .map(|(position, instance)| (position, self.instances.insert(instance))),
            )
            .collect::<Vec<_>>();
        for (position, index) in bindings {
            if let Exec::State { instance, .. } = &mut program.nodes[position].exec {
                *instance = Some(index);
            }
        }
    }

    /// Makes a candidate active at once: the old plan's kernels that are not carried over finish under the retire
    /// policy, in the old generation.
    pub(super) fn commit(
        &mut self, position: usize, mut program: Program, transition: Transition, lineages: Vec<(usize, Instance)>,
    ) -> Result<(), InterpretErrorKind> {
        if let Some(active) = self.active_id() {
            if let Some(mut old) = self.programs.remove(&active) {
                let carried_old = transition
                    .carried_local
                    .iter()
                    .map(|(old, _)| *old)
                    .collect::<BTreeSet<_>>();
                let result = self.finish_old(&mut old, &carried_old);
                for (from, to) in &transition.carried_local {
                    let exec = std::mem::replace(&mut old.nodes[*from].exec, Exec::Drop);
                    program.nodes[*to].exec = exec;
                }
                result?;
            }
        }
        self.bind(&mut program, &transition, lineages);
        let generation = program.generation.clone();
        self.publish(program);
        self.schedule = Some(AdmissionSchedule::Steady(generation.clone()));
        self.free_unbound();
        self.record(
            position,
            InstallOutcome::Active {
                generation: generation.id(),
            },
        );
        Ok(())
    }

    /// Finishes the kernels and hosts of a plan that retires, except the nodes in `keep`, in one pass in its
    /// generation, under the retire policy.
    pub(super) fn finish_old(&mut self, old: &mut Program, keep: &BTreeSet<usize>) -> Result<(), InterpretErrorKind> {
        let clock = self.clock;
        self.pass(old, Queue::new(), clock, |run, program, node| {
            if keep.contains(&node) || program.nodes[node].is_isolated() {
                return Ok(Queue::new());
            }
            run.local_work(program, node, LocalWork::finish(DrainPolicy::Retire), clock)
        })
    }

    /// Adds a program to the live plans, with its outputs.
    pub(super) fn publish(&mut self, program: Program) {
        self.generations = self.generations.max(program.generation.id().get());
        for output in &program.outputs {
            self.output.outputs.entry(output.clone()).or_default();
        }
        self.programs.insert(program.generation.id(), program);
    }

    /// Arms the cut that makes a candidate active: it records the cut's boundary on the candidate's generation,
    /// marks the lineages that end as retiring, and publishes a schedule that cuts over to it.
    pub(super) fn arm(
        &mut self, position: usize, mut program: Program, transition: Transition, lineages: Vec<(usize, Instance)>,
    ) -> Result<(), InterpretErrorKind> {
        let Some(old) = self.active_id().and_then(|id| self.programs.get(&id)) else {
            return Err(InterpretErrorKind::Internal {
                message: "a cut needs an active plan".to_string(),
            });
        };
        if let Some(node) = old
            .nodes
            .iter()
            .chain(&program.nodes)
            .find(|node| node.is_local_state())
        {
            return Err(InterpretErrorKind::Unsupported {
                capability: format!(
                    "a cut while `{}`, a state owner that runs in a host component, is in either plan,",
                    node.id
                ),
                stage: 10,
            });
        }
        let old = old.generation.clone();
        let lcm = match lcm_secs(&transition.widths) {
            Ok(lcm) => lcm,
            Err(source) => {
                self.reject(position, InstallRejection::CutWidths { source });
                return Ok(());
            }
        };

        // The admission points whose stamps decide which lineage data reaches: those upstream of the lineages that
        // start or end.
        let mut affected = BTreeSet::new();
        for &index in &transition.retiring {
            if let Some(instance) = self.instances.get(index) {
                affected.extend(instance.tracker.upstream());
            }
        }
        for (_, instance) in &lineages {
            affected.extend(instance.tracker.upstream());
        }
        let now = affected
            .iter()
            .filter_map(|&point| self.points.latest(point))
            .fold(self.clock, AdmissionTime::max);
        let at = cut_time(now, lcm, limits::values::CUT_MARGIN);
        let boundary = CutBoundary {
            at,
            replaces: old.id(),
            epochs: affected
                .iter()
                .map(|&point| (point, self.points.epoch(point)))
                .collect(),
        };
        let new = program.generation.clone();
        if new.set_cut(boundary).is_err() {
            return Err(InterpretErrorKind::Internal {
                message: format!("generation {} was armed twice", new.id()),
            });
        }

        for &index in &transition.retiring {
            if let Some(instance) = self.instances.get_mut(index) {
                instance.retiring = Some(Retiring {
                    owner: old.clone(),
                    by: new.clone(),
                });
            }
        }
        self.bind(&mut program, &transition, lineages);
        self.publish(program);
        self.cut = Some(CutInFlight {
            old: old.clone(),
            new: new.clone(),
            at,
            affected: affected.into_iter().collect(),
            epochs_at_publish: self
                .points
                .ids()
                .map(|point| (point, self.points.epoch(point)))
                .collect(),
            admitted_to_new: BTreeSet::new(),
        });
        self.schedule = Some(AdmissionSchedule::Cut {
            old,
            new: new.clone(),
            at,
        });
        self.record(
            position,
            InstallOutcome::Cut {
                generation: new.id(),
                at,
            },
        );
        let clock = self.clock;
        self.check_finalization(clock)
    }

    /// Cancels the cut in flight if no admission point stamped its new generation yet, and with it the installation
    /// waiting for the cut, if one waits.
    ///
    /// What asks the installer to cancel a cut always proposes its own installation next: a newer revision, the
    /// default program after a revocation. The waiting installation, a re-composition of the revision the cut installs
    /// or an older proposal, is superseded by it, and starting it against the old generation would bring the revision
    /// the cancel withdraws into effect after all. A refused cancel leaves it waiting, for the proposal that follows to
    /// replace.
    pub(super) fn cancel(&mut self, position: usize) -> Result<(), InterpretErrorKind> {
        if self.cut.as_ref().is_none_or(|cut| !cut.admitted_to_new.is_empty()) {
            self.record(position, InstallOutcome::CancelRefused);
            return Ok(());
        }
        let Some(cut) = self.cut.take() else {
            return Ok(());
        };
        // Nothing was stamped with the new generation, so its lineages hold nothing, and nothing of it was delivered.
        self.schedule = Some(AdmissionSchedule::Steady(cut.old.clone()));
        self.programs.remove(&cut.new.id());
        for index in self.instances.indices() {
            if let Some(instance) = self.instances.get_mut(index) {
                if instance
                    .retiring
                    .as_ref()
                    .is_some_and(|retiring| retiring.by.id() == cut.new.id())
                {
                    instance.retiring = None;
                }
            }
        }
        self.free_unbound();
        self.record(position, InstallOutcome::Cancelled);
        if let Some(replaced) = self.queued.take() {
            self.record(replaced.event, InstallOutcome::Superseded { by: position });
        }
        Ok(())
    }

    /// Finalizes the cut in flight once every affected admission point stamps only its new generation: the plan it
    /// replaced retires, and the installation waiting for it starts.
    pub(super) fn check_finalization(&mut self, now: AdmissionTime) -> Result<(), InterpretErrorKind> {
        let Some(cut) = &self.cut else {
            return Ok(());
        };
        let passed = cut.affected.iter().all(|&point| {
            self.points.epoch(point) != cut.epoch_at_publish(point)
                || self.points.latest(point).is_some_and(|latest| latest >= cut.at)
        });
        if !passed {
            return Ok(());
        }
        let Some(cut) = self.cut.take() else {
            return Ok(());
        };
        // Every retiring lineage has crossed: every point upstream of it is among the affected ones, and each stamped
        // the new generation, which the lineage saw. One that has not would lose its state, so it retires here.
        let behind = self
            .instances
            .indices()
            .into_iter()
            .filter(|&index| {
                self.instances
                    .get(index)
                    .is_some_and(|instance| !instance.retired && instance.retiring.is_some())
            })
            .collect::<Vec<_>>();
        debug_assert!(
            behind.is_empty(),
            "every retiring lineage crosses before its cut finalizes"
        );
        if !behind.is_empty() {
            self.retire(&behind, now)?;
        }
        self.schedule = Some(AdmissionSchedule::Steady(cut.new.clone()));
        if let Some(mut old) = self.programs.remove(&cut.old.id()) {
            self.finish_old(&mut old, &BTreeSet::new())?;
        }
        self.free_unbound();
        self.start_queued()
    }

    /// Drops every instance no live plan binds and no cut retires: its kernel, and with it its state budget.
    pub(super) fn free_unbound(&mut self) {
        for index in self.instances.indices() {
            let bound = self.programs.values().any(|program| program.binding(index).is_some());
            let retiring = self
                .instances
                .get(index)
                .is_some_and(|instance| instance.retiring.is_some());
            if !bound && !retiring {
                self.instances.remove(index);
            }
        }
    }
}
