//! The one lifecycle: what runs a case from its plan, whatever the case checks.
//!
//! 1. **Prepare.** Each realization backend the case uses prepares its part of the case's scope. Every resource is
//!    entered in the ledger before it is made.
//! 2. **Create.** Participants are created level by level in create order, each from its plan, with every reference
//!    whose value exists by then filled in. Nothing runs yet.
//! 3. **Start.** Participants start level by level in start order. A level starts once every endpoint its participants
//!    connect to shows its readiness evidence, and a gate fails as soon as its server is seen to exit. Only a consumer
//!    waits, so a participant nobody connects to just starts. Each participant's exit and output are observed from its
//!    start.
//! 4. **Procedure.** The steps run in order, and a `parallel` block runs its steps together. Each step has its own
//!    budget inside the case's deadline. The procedure stops at the first step that does not pass.
//! 5. **Observe.** Exited participants get a short grace to finish their output, then the files to collect are copied
//!    out. This runs after a failed create or start too, if anything started.
//!
//! Reclaim is the runner's: after the lifecycle returns, the ledger removes everything the case made, on every path.
//! The lifecycle reads only the plan. It calls no participant type, and only an operation a step calls runs participant
//! type code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::{join_all, try_join_all};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, timeout_at, Instant};
use tokio_util::sync::CancellationToken;

use super::container::{CaseContainers, ContainerBackend, RealizedContainer};
use super::deadline::{Budget, Deadline};
use super::handle::{Handle, HandleError};
use super::ledger::ResourceLedger;
use super::observe::{self, Collected, Observation, ProcessExit, Sink, CONTAINER_LOG};
use super::participant::Enclosure;
use super::plan::{CasePlan, PlannedStep};
use super::record::{CaseRecord, RunningStep, StepResult, StepStatus, Verdict};
use super::reference::Values;
use super::result::{ErrorKind, ParticipantRun, PhaseTracker, ProcessRun, TestResult};
use super::spec::{Evidence, OutputChannel, ProcessSpec};
use super::step::{ProcedureNode, StepContext};
use super::tasks::CaseTasks;
use super::wiring::Gate;

/// How long a consumer waits for the readiness evidence of an endpoint it connects to.
const GATE_TIME: Duration = Duration::from_secs(60);
/// How often a gate looks for its evidence again.
const GATE_POLL: Duration = Duration::from_millis(250);
/// How long one probe or request of a gate may take.
const GATE_ATTEMPT: Duration = Duration::from_secs(5);
/// How long a step may run past its deadline to report its own result.
const STEP_SLACK: Duration = Duration::from_secs(10);
/// How long the output of exited participants has to arrive after the procedure.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// How long observing after the procedure may take. The case's deadline may have passed by then.
const OBSERVE_TIME: Duration = Duration::from_secs(15);
/// How long a participant's exit is watched: longer than any case, because the runner stops the watch with the case's
/// other tasks.
const EXIT_WATCH_TIME: Duration = Duration::from_secs(24 * 60 * 60);

/// What the runner gives one case's run.
pub(crate) struct CaseRun<'a> {
    /// Realizes the case's Linux containers.
    pub(crate) containers: &'a ContainerBackend,
    pub(crate) ledger: &'a ResourceLedger,
    /// Where output capture and exit watches run. The runner stops them before reclaim.
    pub(crate) tasks: &'a CaseTasks,
    /// Fires when the runner stops the case.
    pub(crate) cancel: &'a CancellationToken,
    pub(crate) deadline: Deadline,
    /// Where each participant's output and collected files are written, under its name.
    pub(crate) log_dir: &'a Path,
    pub(crate) phases: &'a PhaseTracker,
    pub(crate) record: &'a CaseRecord,
}

/// Runs the case `plan` describes, with what `run` gives. The result says how each participant ran: its realization,
/// and, once it started, each process's exit and output; and the collected files the outcome keeps.
pub(crate) async fn run(plan: &CasePlan, run: &CaseRun<'_>) -> TestResult {
    let started = Instant::now();
    let mut case = Case {
        plan,
        run,
        values: Values::of_plan(&plan.participants),
        created: BTreeMap::new(),
        started: BTreeMap::new(),
    };
    match case.set_up().await {
        Ok(()) => case.procedure().await,
        Err(message) => {
            let result = TestResult::errored(
                &plan.case.name,
                ErrorKind::Setup,
                message,
                started.elapsed(),
                run.phases.completed(),
            );
            run.record.decide(Verdict::SetupFailed(Box::new(result)));
        }
    }
    let (mut processes, collected) = case.observe().await;
    let mut result = run
        .record
        .result(&plan.case.name, started.elapsed(), run.phases.completed());
    result.subject_os = Some(plan.subject_os);
    result.participants = plan
        .participants
        .iter()
        .map(|(name, planned)| {
            let participant = ParticipantRun {
                realization: planned.realization,
                processes: processes.remove(name).unwrap_or_default(),
            };
            (name.clone(), participant)
        })
        .collect();
    result.collected = observe::retain(collected, result.outcome.is_passed()).await;
    result
}

/// One case as it runs.
struct Case<'a> {
    plan: &'a CasePlan,
    run: &'a CaseRun<'a>,
    /// What fills references, as participants are created and start.
    values: Values,
    /// Participants created and not started yet.
    created: BTreeMap<String, RealizedContainer>,
    started: BTreeMap<String, Started>,
}

/// A started participant, and what observes it.
struct Started {
    realized: Arc<RealizedContainer>,
    observation: Observation,
    /// Where each process's output is captured on the runner host.
    outputs: BTreeMap<String, PathBuf>,
    /// The tasks that capture its output.
    capture: Vec<JoinHandle<Option<()>>>,
}

impl Case<'_> {
    /// Prepares the case's scope, then creates and starts its participants.
    async fn set_up(&mut self) -> Result<(), String> {
        let phases = self.run.phases;
        let phase = phases.enter("prepare");
        let prepared = self.prepare().await;
        phase.finish();
        let containers = prepared?;

        let phase = phases.enter("create");
        let created = self.create(&containers).await;
        phase.finish();
        created?;

        let phase = phases.enter("start");
        let started = self.start().await;
        phase.finish();
        started
    }

    async fn prepare(&self) -> Result<CaseContainers, String> {
        // TODO: prepare the scopes of the other enclosures with their backends.
        if let Some(enclosure) = self
            .plan
            .scope
            .keys()
            .find(|&&enclosure| enclosure != Enclosure::LinuxContainer)
        {
            return Err(format!("The case has a {}, and that backend is not built.", enclosure));
        }
        self.run
            .containers
            .prepare(self.run.ledger, self.run.cancel, self.run.deadline.at)
            .await
            .map_err(|e| format!("{:#}", e))
    }

    /// Creates the participants level by level in create order, and records each one's identity.
    async fn create(&mut self, containers: &CaseContainers) -> Result<(), String> {
        let plan = self.plan;
        for level in &plan.create_order {
            let created = join_all(level.iter().map(|name| {
                containers.create(
                    name,
                    &plan.participants[name],
                    &self.values,
                    self.run.cancel,
                    self.run.deadline.at,
                )
            }))
            .await;
            for (name, created) in level.iter().zip(created) {
                let created = created.map_err(|e| format!("Failed to create participant '{}': {:#}", name, e))?;
                self.values.created(name, created.container.id());
                self.created.insert(name.clone(), created);
            }
        }
        Ok(())
    }

    /// Starts the participants level by level in start order, each level once its gates hold, and observes each that
    /// starts.
    async fn start(&mut self) -> Result<(), String> {
        let plan = self.plan;
        for level in &plan.start_order {
            let gates = level.iter().flat_map(|consumer| {
                plan.gates
                    .get(consumer)
                    .into_iter()
                    .flatten()
                    .map(move |gate| (consumer, gate))
            });
            // The first gate to fail ends the level, so a server's exit is reported at once.
            try_join_all(gates.map(|(consumer, gate)| self.gate(consumer, gate))).await?;

            let mut starting: Vec<(&String, RealizedContainer)> = level
                .iter()
                .filter_map(|name| self.created.remove(name).map(|created| (name, created)))
                .collect();
            let run = self.run;
            let results = join_all(
                starting
                    .iter_mut()
                    .map(|(_, created)| created.start(run.cancel, run.deadline.at)),
            )
            .await;
            // Every participant that started is observed, even when another of its level failed.
            let mut failed = None;
            for ((name, realized), result) in starting.into_iter().zip(results) {
                let observed = match result {
                    Ok(()) => self.observe_started(name, realized).await,
                    Err(e) => Err(format!("Failed to start participant '{}': {:#}", name, e)),
                };
                if let Err(e) = observed {
                    failed.get_or_insert(e);
                }
            }
            if let Some(failed) = failed {
                return Err(failed);
            }
        }
        Ok(())
    }

    /// Waits until the endpoint `gate` names shows its evidence, for the consumer `consumer`, and fails as soon as the
    /// server is seen to exit.
    async fn gate(&self, consumer: &str, gate: &Gate) -> Result<(), String> {
        let (server, endpoint) = (gate.target.participant.as_str(), gate.target.endpoint.as_str());
        let waits = format!(
            "Participant '{}' waits until {}.{} {}",
            consumer, server, endpoint, gate.evidence
        );
        let Some(started) = self.started.get(server) else {
            return Err(format!("{}, and participant '{}' has not started.", waits, server));
        };
        let handle = self.handle(server, started);
        let pattern = match &gate.evidence {
            Evidence::Output { pattern, .. } => {
                let pattern = self.values.fill(server, pattern);
                Some(pattern.map_err(|e| format!("{}: {}", waits, e))?)
            }
            _ => None,
        };

        let deadline = Instant::now() + GATE_TIME;
        loop {
            // The server's exit fails the gate. Otherwise, what the evidence shows this time.
            let polled = async {
                if let Some(exited) = self.server_exit(server, started, &gate.evidence).await {
                    return Err(exited);
                }
                Ok(match &gate.evidence {
                    Evidence::None => Ok(()),
                    Evidence::Accepts | Evidence::Exists => handle.probe(endpoint, GATE_ATTEMPT).await,
                    Evidence::Http { path } => match handle.http_get(endpoint, path, GATE_ATTEMPT).await {
                        Ok(response) if response.status == 200 => Ok(()),
                        Ok(response) => Err(HandleError::Software(format!("it answered {}", response.status))),
                        Err(e) => Err(e),
                    },
                    Evidence::Output { process, .. } => {
                        let pattern = pattern.as_deref().unwrap_or_default();
                        match handle.output(process) {
                            Ok(output) if output.borrow().iter().any(|line| line.contains(pattern)) => Ok(()),
                            Ok(_) => Err(HandleError::Software("no line has it yet".to_string())),
                            Err(e) => Err(e),
                        }
                    }
                })
            };
            let shown = tokio::select! {
                polled = polled => polled.map_err(|exited| format!("{}, and {}", waits, exited))?,
                _ = self.run.cancel.cancelled() => return Err(format!("{}: cancelled.", waits)),
            };
            let not_yet = match shown {
                Ok(()) => return Ok(()),
                Err(HandleError::Harness(e)) => return Err(format!("{}: {}", waits, e)),
                Err(HandleError::Software(e)) => e,
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "{}, and it did not within {:?}. Last: {}",
                    waits, GATE_TIME, not_yet
                ));
            }
            tokio::select! {
                _ = sleep_until((Instant::now() + GATE_POLL).min(deadline)) => {}
                _ = started.observation.wait_for_exit() => {}
                _ = self.run.cancel.cancelled() => return Err(format!("{}: cancelled.", waits)),
            }
        }
    }

    /// Why the server `name` of a gate with `evidence` is seen to have exited, or `None` while it runs: its container
    /// exited, or the process whose output is the evidence did. A supervisor may restart that process in a running
    /// container, and its exit record keeps the first exit.
    async fn server_exit(&self, name: &str, started: &Started, evidence: &Evidence) -> Option<String> {
        match started.observation.exit() {
            Some(Ok(code)) => return Some(format!("'{}' exited with code {}.", name, code)),
            Some(Err(why)) => return Some(format!("'{}' stopped, and its exit could not be read: {}", name, why)),
            None => {}
        }
        let Evidence::Output { process, .. } = evidence else {
            return None;
        };
        match self.handle(name, started).exit(process, GATE_ATTEMPT).await {
            Ok(ProcessExit::Running) => None,
            Ok(ProcessExit::Exited(status)) => {
                Some(format!("process '{}' of '{}' exited with {}.", process, name, status))
            }
            Ok(ProcessExit::Undetermined(why)) => Some(format!(
                "the exit of process '{}' of '{}' cannot be determined: {}",
                process, name, why
            )),
            Err(e) => Some(e.to_string()),
        }
    }

    /// Records what is known of `name` once it has started, and observes its exit and the output of each process.
    async fn observe_started(&mut self, name: &str, realized: RealizedContainer) -> Result<(), String> {
        let plan = self.plan;
        let planned = &plan.participants[name];
        if let Some(ip) = &realized.ip {
            self.values.started(name, ip);
        }
        let observation = Observation::new(planned.processes.iter().map(ProcessSpec::name));
        let realized = Arc::new(realized);
        let (container, watched) = (realized.container.clone(), observation.clone());
        self.run.tasks.spawn(async move {
            let exit = container.wait(Instant::now() + EXIT_WATCH_TIME).await;
            watched.exited(exit.map_err(|e| format!("{:#}", e)));
        });
        let mut started = Started {
            realized,
            observation,
            outputs: BTreeMap::new(),
            capture: Vec::new(),
        };
        let captured = self.capture(name, &mut started).await;
        self.started.insert(name.to_string(), started);
        captured
    }

    /// Captures the output of each process of `name` under its log directory, and the container log when no process
    /// is observed through it.
    async fn capture(&self, name: &str, started: &mut Started) -> Result<(), String> {
        let planned = &self.plan.participants[name];
        let dir = self.run.log_dir.join(name);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("Failed to create '{}': {}", dir.display(), e))?;
        let facts = planned.realization.enclosure.facts();
        let limit = self.run.deadline.at.saturating_duration_since(Instant::now());
        let tasks = self.run.tasks;
        let mut through_container = false;
        for process in &planned.processes {
            let output = dir.join(format!("{}.log", process.name()));
            let sink = Sink::create(
                output.clone(),
                Some((started.observation.clone(), process.name().to_string())),
            )
            .await?;
            started.outputs.insert(process.name().to_string(), output);
            let task = match process.output() {
                OutputChannel::Container => {
                    through_container = true;
                    tasks.spawn(sink.capture(started.realized.container.logs()))
                }
                OutputChannel::File { file } => {
                    let (Some(courier), Some(log_dir)) = (facts.courier, facts.log_dir(name)) else {
                        return Err(format!(
                            "Participant '{}' is a {}, which has no courier or log directory.",
                            name, planned.realization.enclosure
                        ));
                    };
                    let path = format!("{}{}{}", log_dir, facts.separator, file);
                    let realized = started.realized.clone();
                    tasks.spawn(async move { sink.follow(&*realized, courier, &path, limit).await })
                }
            };
            started.capture.push(task);
        }
        if !through_container {
            let sink = Sink::create(dir.join(CONTAINER_LOG), None).await?;
            let task = tasks.spawn(sink.capture(started.realized.container.logs()));
            started.capture.push(task);
        }
        Ok(())
    }

    /// Runs the procedure, recording each step.
    async fn procedure(&self) {
        let phase = self.run.phases.enter("procedure");
        let handles: BTreeMap<String, Handle<'_>> = self
            .started
            .iter()
            .map(|(name, started)| (name.clone(), self.handle(name, started)))
            .collect();
        self.run_procedure(&handles).await;
        phase.finish();
    }

    async fn run_procedure(&self, handles: &BTreeMap<String, Handle<'_>>) {
        let record = self.run.record;
        // Each entry's steps, with the record slot, position and parallel member of each.
        let mut slot = 0;
        let mut entries = Vec::new();
        for (index, node) in self.plan.procedure.iter().enumerate() {
            let parallel = matches!(node, ProcedureNode::Parallel(_));
            let mut steps = Vec::new();
            for (member, step) in node.steps().iter().enumerate() {
                steps.push((slot, index, parallel.then_some(member), step));
                slot += 1;
            }
            entries.push(steps);
        }
        record.plan(
            entries
                .iter()
                .flatten()
                .map(|&(_, index, member, step)| StepResult {
                    step: index,
                    member,
                    name: step.name.clone(),
                    status: StepStatus::NotRun,
                    message: "Not run.".to_string(),
                    duration: Duration::ZERO,
                })
                .collect(),
        );

        for steps in entries {
            if self.run.cancel.is_cancelled() {
                return;
            }
            let statuses = join_all(
                steps
                    .into_iter()
                    .map(|(slot, index, member, step)| self.run_step(slot, index, member, step, handles)),
            )
            .await;
            if statuses.contains(&StepStatus::Interrupted) {
                return;
            }
            if statuses.iter().any(|&status| status != StepStatus::Passed) {
                break;
            }
        }
        record.decide(Verdict::Procedure);
    }

    /// Runs one step until it reports, the runner stops the case, or its deadline and the slack pass.
    async fn run_step(
        &self, slot: usize, index: usize, member: Option<usize>, planned: &PlannedStep,
        handles: &BTreeMap<String, Handle<'_>>,
    ) -> StepStatus {
        let step = &planned.step;
        let case = self.run.deadline;
        let remaining = case.at.saturating_duration_since(Instant::now());
        // On a tie the step's own budget is the one that runs out, as `Deadline::narrow` has it. A step that asks for
        // more than the case has left is recorded with what is left.
        let (budget, deadline) = if step.budget() <= remaining {
            (step.budget(), Deadline::after(Budget::Step, step.budget()))
        } else {
            (remaining, case)
        };
        let running = RunningStep {
            step: index,
            member,
            name: step.name(),
            started: Instant::now(),
            budget,
            deadline,
        };
        self.run.record.start(slot, running);

        let cx = StepContext::new(handles, &self.values, deadline.at);
        let cancel = self.run.cancel;
        let result = tokio::select! {
            biased;
            report = step.run(&cx) => if report.status != StepStatus::Passed && cancel.is_cancelled() {
                running.interrupted(Some(report.message))
            } else {
                running.result(report.status, report.message, running.started.elapsed())
            },
            _ = cancel.cancelled() => running.interrupted(None),
            _ = sleep_until(deadline.at + STEP_SLACK) => match deadline.budget {
                Budget::Step => running.overran(),
                Budget::Case => running.interrupted(None),
            },
        };
        let status = result.status;
        self.run.record.finish(slot, result);
        status
    }

    /// Gives exited participants a grace to finish their output, then reads each process's exit and copies out the
    /// files to collect.
    async fn observe(&mut self) -> (BTreeMap<String, BTreeMap<String, ProcessRun>>, Vec<Collected>) {
        let mut processes = BTreeMap::new();
        let mut collected = Vec::new();
        if self.started.is_empty() {
            return (processes, collected);
        }
        let phase = self.run.phases.enter("observe");
        let deadline = Instant::now() + OBSERVE_TIME;
        let ending = self
            .started
            .values_mut()
            .filter(|started| started.observation.exit().is_some())
            .flat_map(|started| started.capture.iter_mut());
        let _ = timeout_at(Instant::now() + OUTPUT_GRACE, join_all(ending)).await;

        for (name, started) in &self.started {
            let planned = &self.plan.participants[name];
            let handle = self.handle(name, started);
            let mut runs = BTreeMap::new();
            for (process, output) in &started.outputs {
                let limit = deadline.saturating_duration_since(Instant::now());
                let exit = handle
                    .exit(process, limit)
                    .await
                    .unwrap_or_else(|e| ProcessExit::Undetermined(e.to_string()));
                let output = output.clone();
                runs.insert(process.clone(), ProcessRun { exit, output });
            }
            processes.insert(name.clone(), runs);
            let dir = self.run.log_dir.join(name);
            collected.extend(observe::collect(name, &planned.collect, &*started.realized, &dir, deadline).await);
        }
        phase.finish();
        (processes, collected)
    }

    fn handle<'s>(&'s self, name: &'s str, started: &'s Started) -> Handle<'s> {
        Handle::new(
            name,
            &self.plan.participants[name],
            &*started.realized,
            &self.values,
            &started.observation,
        )
    }
}
