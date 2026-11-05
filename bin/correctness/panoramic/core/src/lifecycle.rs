//! Prepares native case scopes, creates and starts participants, then runs their procedure.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures::future::join_all;
use tokio_util::sync::CancellationToken;

use crate::{
    artifact::{binary_identity, ArtifactKind, ArtifactRole, VerifiedArtifact},
    backend::{self, runtime, LateFact, ParticipantRequest, RunBackends, RuntimeParticipant, ScopeRuntime},
    deadline::Deadline,
    handle::Primitives,
    layout::{Realization, RealizationKind},
    ledger::ResourceLedger,
    observe::{
        collect::{self, CollectionReport},
        ParticipantObservation,
    },
    participant::{check_process_specs, find_type, ParticipantType, ProcessSpec},
    plan::ParticipantPlan,
    recipe::Recipe,
    record::CaseRecord,
    result::{ErrorKind, ParticipantReport, PhaseTracker, TestResult},
    tasks::CaseTasks,
};

#[cfg(test)]
mod daemon_tests;
#[cfg(all(test, unix))]
mod host_tests;
mod readiness;
mod render;
mod steps;

/// Maximum time to finish capturing output from participants whose exit was observed.
const OBSERVE_TIME: Duration = Duration::from_secs(5);

/// Participant identities and live observations retained after the case body returns or is dropped.
#[derive(Clone, Default)]
pub struct Observations {
    participants: Arc<Mutex<BTreeMap<String, ObservedParticipant>>>,
    collections: Arc<Mutex<CollectionReport>>,
}

impl std::fmt::Debug for Observations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observations")
            .field("participants", &self.snapshot())
            .finish()
    }
}

struct ObservedParticipant {
    report: ParticipantReport,
    specs: &'static [ProcessSpec],
    observation: Option<Arc<ParticipantObservation>>,
}

impl Observations {
    /// Adds completed collections and collection issues to the runner's final result.
    /// Call once after the case body has returned or been dropped.
    pub fn record_collections(&self, result: &mut TestResult) {
        let report = self.collections.lock().unwrap();
        result.collected.clone_from(&report.collected);
        result.issues.extend(report.issues.iter().cloned());
    }

    fn register(&self, name: &str, participant_type: &str, realization: Realization, specs: &'static [ProcessSpec]) {
        self.participants.lock().unwrap().insert(
            name.into(),
            ObservedParticipant {
                report: ParticipantReport {
                    name: name.into(),
                    participant_type: participant_type.into(),
                    os: realization.os,
                    realization: realization.kind,
                    processes: Vec::new(),
                },
                specs,
                observation: None,
            },
        );
    }

    fn started(&self, name: &str, observation: Arc<ParticipantObservation>) {
        self.participants
            .lock()
            .unwrap()
            .get_mut(name)
            .expect("started participants were registered before creation")
            .observation = Some(observation);
    }

    /// Takes a report snapshot in participant-name order, before the runner stops observation tasks.
    pub fn snapshot(&self) -> Vec<ParticipantReport> {
        self.participants
            .lock()
            .unwrap()
            .values()
            .map(|participant| {
                let mut report = participant.report.clone();
                if let Some(observation) = &participant.observation {
                    report.processes = observation.snapshot(participant.specs);
                }
                report
            })
            .collect()
    }
}

/// Verified image IDs, executable paths and container-binary bytes shared by native cases in a run.
pub struct NativeInputs {
    pub(crate) backends: Arc<RunBackends>,
    images: BTreeMap<&'static str, String>,
    image_references: BTreeMap<&'static str, String>,
    binaries: BTreeMap<&'static str, Bytes>,
    programs: BTreeMap<&'static str, PathBuf>,
}

impl NativeInputs {
    /// Loads container binaries once, resolving their references against `base` and rechecking their digests.
    ///
    /// # Errors
    ///
    /// Returns an error when a container binary cannot be read or has changed since verification.
    pub fn new(backends: Arc<RunBackends>, verified: &[VerifiedArtifact], base: &Path) -> Result<Self, String> {
        let mut inputs = Self {
            backends,
            images: BTreeMap::new(),
            image_references: BTreeMap::new(),
            binaries: BTreeMap::new(),
            programs: BTreeMap::new(),
        };
        for artifact in verified {
            match artifact.kind {
                ArtifactKind::Image { .. } => {
                    inputs.images.insert(artifact.role, artifact.identity.clone());
                    inputs
                        .image_references
                        .insert(artifact.role, artifact.reference.clone());
                }
                ArtifactKind::ContainerBinary { .. } => {
                    let path = base.join(&artifact.reference);
                    let bytes =
                        std::fs::read(&path).map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
                    let identity = binary_identity(bytes.as_slice())
                        .map_err(|error| format!("cannot hash '{}': {error}", path.display()))?;
                    if identity != artifact.identity {
                        return Err(format!("'{}' changed after verification", path.display()));
                    }
                    inputs.binaries.insert(artifact.role, bytes.into());
                }
                ArtifactKind::Binary | ArtifactKind::Install { .. } => {
                    inputs.programs.insert(artifact.role, base.join(&artifact.reference));
                }
            }
        }
        Ok(inputs)
    }

    pub(crate) fn image(&self, role: ArtifactRole) -> Result<&str, String> {
        self.images
            .get(role.name)
            .map(String::as_str)
            .ok_or_else(|| format!("native inputs lack verified image role '{}'", role.name))
    }

    /// Returns the verified image reference used when loading images into a cluster.
    ///
    /// # Errors
    ///
    /// Returns an error when the role has no verified image binding.
    pub fn image_reference(&self, role: ArtifactRole) -> Result<&str, String> {
        self.image_references
            .get(role.name)
            .map(String::as_str)
            .ok_or_else(|| format!("native inputs lack verified image role '{}'", role.name))
    }

    pub(crate) fn container_binary(&self, role: ArtifactRole) -> Result<&Bytes, String> {
        self.binaries
            .get(role.name)
            .ok_or_else(|| format!("native inputs lack verified binary role '{}'", role.name))
    }

    #[cfg(unix)]
    pub(crate) fn program(&self, role: ArtifactRole) -> Result<PathBuf, String> {
        let main_binary = match role.kind {
            ArtifactKind::Binary => None,
            ArtifactKind::Install { main_binary } => Some(main_binary),
            _ => return Err(format!("role '{}' is not a host binary or install", role.name)),
        };
        let root = self
            .programs
            .get(role.name)
            .ok_or_else(|| format!("native inputs lack verified {} role '{}'", role.kind.name(), role.name))?;
        let path = main_binary.map_or_else(|| root.clone(), |main| root.join(main));
        std::path::absolute(&path).map_err(|error| format!("cannot resolve program '{}': {error}", path.display()))
    }

    fn require_backends(&self, kinds: impl IntoIterator<Item = RealizationKind>) -> Result<(), String> {
        for kind in kinds {
            for role in backend::artifacts(kind) {
                let present = match role.kind {
                    ArtifactKind::Image { .. } => self.images.contains_key(role.name),
                    ArtifactKind::ContainerBinary { .. } => self.binaries.contains_key(role.name),
                    ArtifactKind::Binary | ArtifactKind::Install { .. } => self.programs.contains_key(role.name),
                };
                if !present {
                    return Err(format!(
                        "native inputs lack verified {} role '{}'",
                        role.kind.name(),
                        role.name
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Cancellation, phase reporting and resource ownership supplied by the case runner.
pub struct CaseControl {
    /// Participant observations retained by the runner through cancellation.
    pub observations: Observations,
    /// Procedure states and verdict retained by the runner through cancellation.
    pub record: CaseRecord,
    /// The case-wide deadline used to bound each step.
    pub deadline: Deadline,
    /// Cancellation of this case, including its deadline and run interruption.
    pub cancel: CancellationToken,
    /// Run interruption, independent of the case deadline so timeout artifacts can still be collected.
    pub interrupt: CancellationToken,
    /// The active phase and completed timings reported by the runner.
    pub phases: PhaseTracker,
    /// Resources reclaimed by the runner after the case body finishes or is dropped.
    pub ledger: ResourceLedger,
    /// Background tasks stopped by the runner before reclaim.
    pub tasks: CaseTasks,
    /// Case artifacts, with one output directory per participant.
    pub log_dir: PathBuf,
}

/// The registered scope used to create every participant in a case.
struct Prepared {
    scopes: BTreeMap<RealizationKind, Box<dyn ScopeRuntime>>,
    id: String,
    process_specs: BTreeMap<String, &'static [ProcessSpec]>,
}

struct Created {
    participant: Arc<dyn RuntimeParticipant>,
    late: BTreeMap<LateFact, String>,
}

/// A started participant and its live observations.
struct Started {
    participant: Arc<dyn RuntimeParticipant>,
    late: BTreeMap<LateFact, String>,
    observation: Arc<ParticipantObservation>,
}

/// Creates and starts a native participant case, then runs its procedure.
///
/// The runner owns deadlines and reclaim. Setup failures return directly; procedure outcomes follow the case record.
pub async fn run(
    plan: ParticipantPlan<'_>, types: &[&dyn ParticipantType], inputs: &NativeInputs, control: &CaseControl,
) -> TestResult {
    let started = Instant::now();
    let fail = |message| {
        TestResult::errored(
            "",
            ErrorKind::Setup,
            message,
            started.elapsed(),
            control.phases.completed(),
        )
    };
    let phase = control.phases.enter("prepare");
    let prepared = prepare(plan, types, inputs, control).await;
    phase.finish();
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return fail(error),
    };
    let mut created = BTreeMap::new();
    let mut participants = BTreeMap::new();
    let mut running = BTreeMap::new();
    for level in plan.create_order() {
        for name in level {
            let phase = control.phases.enter(format!("create:{name}"));
            let creation = create(plan, types, inputs, control, &prepared, name, &created).await;
            phase.finish();
            match creation {
                Ok(participant) => {
                    if let Some(id) = participant.late.get(&LateFact::ContainerId) {
                        created.insert(name.clone(), id.clone());
                    }
                    participants.insert(name.clone(), participant);
                }
                Err(error) => return fail(error),
            }
        }
    }
    for level in plan.start_order() {
        let phase = control.phases.enter(format!("start:{}", level.join(",")));
        let result = async {
            readiness::wait_for_level(
                plan,
                level,
                |name| {
                    let started: &Started = &running[name];
                    (started.participant.as_ref() as &dyn Primitives, &started.observation)
                },
                &control.cancel,
            )
            .await?;
            start_level(level, &mut participants, &mut running, control).await
        }
        .await;
        phase.finish();
        if let Err(error) = result {
            observe_and_collect(plan, types, &running, control).await;
            return fail(error);
        }
    }
    let phase = control.phases.enter("steps");
    steps::run(plan, &running, control).await;
    phase.finish();
    observe_and_collect(plan, types, &running, control).await;
    control.record.result("", started.elapsed(), control.phases.completed())
}

async fn observe_and_collect(
    plan: ParticipantPlan<'_>, types: &[&dyn ParticipantType], running: &BTreeMap<String, Started>,
    control: &CaseControl,
) {
    observe(control, async {
        for (name, started) in running {
            let participant_type = find_type(types, &plan.file().participants[name].participant_type)
                .expect("started participants have registered types");
            let files =
                participant_type.collect(plan.layout().get(name).expect("started participants have realizations"));
            if files.is_empty() {
                continue;
            }
            collect::collect(
                name,
                started.participant.as_ref(),
                files,
                &control.log_dir,
                &control.observations.collections,
                &control.interrupt,
            )
            .await;
        }
    })
    .await;
}

async fn observe(control: &CaseControl, collect: impl std::future::Future<Output = ()>) {
    let observations: Vec<_> = control
        .observations
        .participants
        .lock()
        .unwrap()
        .values()
        .filter_map(|participant| participant.observation.clone())
        .collect();
    if observations.is_empty() {
        return;
    }
    let phase = control.phases.enter("observe");
    let finish = join_all(observations.iter().map(|observation| observation.finish_output()));
    let _ = control
        .cancel
        .run_until_cancelled(tokio::time::timeout(OBSERVE_TIME, finish))
        .await;
    collect.await;
    phase.finish();
}

async fn prepare(
    plan: ParticipantPlan<'_>, types: &[&dyn ParticipantType], inputs: &NativeInputs, control: &CaseControl,
) -> Result<Prepared, String> {
    let file = plan.file();
    let layout = plan.layout();
    let unsupported = layout
        .iter()
        .filter(|(_, realization)| {
            runtime::implemented(realization.kind).is_none_or(|oses| !oses.contains(&realization.os))
        })
        .map(|(name, realization)| {
            format!(
                "panoramic cannot create {} participants natively: '{name}' on {}",
                realization.kind.name(),
                realization.os.name(),
            )
        })
        .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        return Err(unsupported.join("; "));
    }
    let mut process_specs = BTreeMap::new();
    for (name, participant) in &file.participants {
        let type_name = &participant.participant_type;
        let participant_type = find_type(types, type_name)
            .ok_or_else(|| format!("participant '{name}' has unregistered type '{type_name}'"))?;
        let realization = layout.get(name).expect("every participant has a realization");
        process_specs.insert(name.clone(), check_process_specs(name, participant_type, realization)?);
    }
    for name in file.participants.keys() {
        let path = control.log_dir.join(name);
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("cannot create the output directory '{}': {error}", path.display()))?;
    }
    let mut participants_by_kind = BTreeMap::<_, Vec<_>>::new();
    for (name, realization) in layout.iter() {
        participants_by_kind.entry(realization.kind).or_default().push(name);
    }
    inputs.require_backends(participants_by_kind.keys().copied())?;
    let mut scopes = BTreeMap::new();
    for (kind, participants) in participants_by_kind {
        scopes.insert(
            kind,
            runtime::prepare(kind, plan.scope(), &participants, inputs, control).await?,
        );
    }
    Ok(Prepared {
        scopes,
        id: plan.scope().id(),
        process_specs,
    })
}

async fn create(
    plan: ParticipantPlan<'_>, types: &[&dyn ParticipantType], inputs: &NativeInputs, control: &CaseControl,
    prepared: &Prepared, name: &str, created: &BTreeMap<String, String>,
) -> Result<Created, String> {
    let config = render::render(plan, name, created)?;
    let realization = plan
        .layout()
        .get(name)
        .expect("create order names laid-out participants");
    let file = plan.file();
    let type_name = &file.participants[name].participant_type;
    let participant_type = find_type(types, type_name)
        .ok_or_else(|| format!("participant '{name}' has unregistered type '{type_name}'"))?;
    let recipe = participant_type
        .recipe(realization, &config)
        .map_err(|error| format!("participant '{name}': {error}"))?;
    if let Recipe::Pod(pod) = &recipe {
        pod.check_late_files(&config)
            .map_err(|error| format!("participant '{name}': {error}"))?;
    }
    for role in recipe.roles() {
        if !participant_type.artifacts(realization).contains(&role) {
            return Err(undeclared_role(name, type_name, role, realization));
        }
    }
    let options = file.participants[name]
        .realization
        .as_ref()
        .map(|realization| &realization.options);
    let participant = prepared.scopes[&realization.kind].participant(ParticipantRequest {
        name,
        realization,
        recipe,
        specs: prepared.process_specs[name],
        runner_host_ports: plan.runner_host_ports(name),
        options: options.unwrap_or(&BTreeMap::new()),
        inputs,
    })?;
    control
        .ledger
        .participant(name, participant.removal(), participant_type.stop_grace(realization));
    control
        .observations
        .register(name, type_name, realization, prepared.process_specs[name]);
    let late = control
        .ledger
        .create_participant(name, &prepared.id, &control.cancel, participant.create())
        .await
        .ok_or_else(|| format!("cancelled while creating participant '{name}'"))?
        .map_err(|error| format!("{error:#}"))?;
    Ok(Created {
        participant,
        late: late.into_iter().collect(),
    })
}

async fn start_level(
    level: &[String], participants: &mut BTreeMap<String, Created>, running: &mut BTreeMap<String, Started>,
    control: &CaseControl,
) -> Result<(), String> {
    let results = join_all(level.iter().map(|name| {
        let Created { participant, mut late } = participants
            .remove(name)
            .expect("start order names created participants");
        async move {
            let result = control
                .cancel
                .run_until_cancelled(participant.start())
                .await
                .ok_or_else(|| format!("cancelled while starting participant '{name}'"))
                .and_then(|result| result.map_err(|error| format!("participant '{name}': {error:#}")));
            let result = match result {
                Ok(facts) => {
                    late.extend(facts);
                    let observation = participant
                        .clone()
                        .observe(&control.tasks, &control.log_dir.join(name))
                        .await;
                    let observation = Arc::new(observation);
                    control.observations.started(name, observation.clone());
                    Ok(Started {
                        participant,
                        late,
                        observation,
                    })
                }
                Err(error) => Err(error),
            };
            (name, result)
        }
    }))
    .await;
    let mut failures = Vec::new();
    for (name, result) in results {
        match result {
            Ok(started) => {
                running.insert(name.clone(), started);
            }
            Err(error) => failures.push(error),
        }
    }
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }
    let facts = participants
        .iter()
        .map(|(name, created)| (name.clone(), created.late.clone()))
        .chain(
            running
                .iter()
                .map(|(name, started)| (name.clone(), started.late.clone())),
        )
        .collect();
    for name in level {
        control
            .cancel
            .run_until_cancelled(running[name].participant.write_late_files(&facts))
            .await
            .ok_or_else(|| format!("cancelled while writing late files for participant '{name}'"))?
            .map_err(|error| format!("participant '{name}': {error:#}"))?;
    }
    Ok(())
}

fn undeclared_role(participant: &str, type_name: &str, role: ArtifactRole, realization: Realization) -> String {
    format!(
        "recipe of participant '{participant}' names role '{}', which type '{type_name}' \
         does not declare for {} on {}",
        role.name,
        realization.kind.name(),
        realization.os.name(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::{docker::test_util::FakeDocker, COURIER_LINUX_BINARY, COURIER_WINDOWS_BINARY, FIXUP_IMAGE},
        case::fixtures,
        deadline::Budget,
        layout::Os,
        participant::{ExitChannel, OutputChannel, OutputStream, RenderedConfig},
        plan::{CasePlan, Planner},
    };

    #[tokio::test(start_paused = true)]
    async fn observation_waits_only_for_exited_participants_and_obeys_its_bound_and_cancellation() {
        use tokio::{
            sync::watch,
            time::{sleep, Instant as Clock},
        };

        use crate::observe::{Capture, ParticipantExit, ProcessOutput};

        enum Completion {
            Finish,
            Bound,
            Cancel,
        }
        use Completion::{Bound, Cancel, Finish};

        for completion in [Finish, Bound, Cancel] {
            let dir = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let control = CaseControl {
                interrupt: CancellationToken::new(),
                observations: Observations::default(),
                record: CaseRecord::default(),
                deadline: Deadline::after(Budget::Case, Duration::from_secs(60)),
                phases: PhaseTracker::default(),
                ledger: ResourceLedger::default(),
                tasks: CaseTasks::new(&cancel),
                cancel: cancel.clone(),
                log_dir: dir.path().into(),
            };
            observe(&control, async {}).await;
            assert!(control.phases.completed().is_empty());
            let realization = Realization {
                os: Os::Linux,
                kind: RealizationKind::LinuxContainer,
            };
            let mut channels = Vec::new();
            for (name, exit) in [
                ("running", ParticipantExit::Running),
                (
                    "unknown",
                    ParticipantExit::Undetermined {
                        reason: "lost wait".into(),
                    },
                ),
                ("stopped", ParticipantExit::Exited { code: 0, error: None }),
            ] {
                control.observations.register(name, "fixture", realization, &[]);
                let (exit_tx, exit_rx) = watch::channel(exit);
                let (capture_tx, capture_rx) = watch::channel(Capture::Following);
                let (_container_tx, container_rx) = watch::channel(Capture::Complete);
                let mut observation =
                    ParticipantObservation::new(exit_rx, container_rx, Default::default(), Default::default());
                observation.outputs.insert(
                    "worker",
                    ProcessOutput {
                        capture: capture_rx,
                        paths: vec![],
                    },
                );
                control.observations.started(name, Arc::new(observation));
                channels.push((exit_tx, capture_tx));
            }
            let started = Clock::now();
            tokio::join!(observe(&control, async {}), async {
                match completion {
                    Finish => {
                        sleep(Duration::from_secs(2)).await;
                        channels[2].1.send_replace(Capture::Complete);
                    }
                    Bound => {}
                    Cancel => {
                        sleep(Duration::from_secs(1)).await;
                        cancel.cancel();
                    }
                }
            });
            let expected = match completion {
                Finish => Duration::from_secs(2),
                Bound => OBSERVE_TIME,
                Cancel => Duration::from_secs(1),
            };
            assert_eq!(started.elapsed(), expected);
            assert_eq!(
                control
                    .phases
                    .completed()
                    .iter()
                    .map(|phase| phase.phase.as_str())
                    .collect::<Vec<_>>(),
                ["observe"]
            );
        }
    }

    #[test]
    fn participant_identities_are_reported_from_registration_and_processes_from_startup() {
        use serde_json::json;
        use tokio::sync::watch;

        use crate::observe::{Capture, ParticipantExit, ProcessObservation, ProcessOutput};
        let observations = Observations::default();
        let realization = Realization {
            os: Os::Linux,
            kind: RealizationKind::LinuxContainer,
        };
        const SPECS: &[ProcessSpec] = &[ProcessSpec {
            name: "main",
            exit: ExitChannel::Container,
            output: OutputChannel::Container,
        }];
        observations.register("subject", "fixture", realization, SPECS);
        let identity = json!({"name": "subject", "type": "fixture", "os": "linux", "realization": "linux-container", "processes": []});
        assert_eq!(
            serde_json::to_value(observations.snapshot()).unwrap(),
            json!([identity])
        );
        let (_exit, exit_rx) = watch::channel(ParticipantExit::Running);
        let (_capture, capture_rx) = watch::channel(Capture::Following);
        let mut observation =
            ParticipantObservation::new(exit_rx.clone(), capture_rx.clone(), "stdout".into(), "stderr".into());
        observation
            .processes
            .insert("main", ProcessObservation::Container(exit_rx));
        observation.outputs.insert(
            "main",
            ProcessOutput {
                capture: capture_rx,
                paths: vec!["stdout".into(), "stderr".into()],
            },
        );
        observations.started("subject", Arc::new(observation));
        let mut expected = identity;
        expected["processes"] = json!([{"name": "main", "exit": {"state": "running", "source": "container"}, "output": {"stdout": "stdout", "stderr": "stderr"}}]);
        assert_eq!(
            serde_json::to_value(observations.snapshot()).unwrap(),
            json!([expected])
        );
    }

    #[test]
    fn the_default_recipe_names_the_type_and_realization() {
        let config = RenderedConfig {
            directory: "/participant".into(),
            env: BTreeMap::new(),
            params: BTreeMap::new(),
            files: BTreeMap::new(),
            late_files: Default::default(),
        };
        let error = fixtures::Server
            .recipe(
                crate::layout::Realization {
                    os: Os::Linux,
                    kind: RealizationKind::LinuxContainer,
                },
                &config,
            )
            .err()
            .unwrap();
        assert_eq!(
            error,
            "participant type 'server' has no recipe for linux-container on linux"
        );
    }

    #[test]
    fn native_inputs_reject_a_container_binary_whose_digest_differs_from_its_verified_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("courier"), b"verified").unwrap();
        let verified = [VerifiedArtifact {
            role: COURIER_LINUX_BINARY.name,
            reference: "courier".into(),
            identity: binary_identity(b"verified".as_slice()).unwrap(),
            kind: COURIER_LINUX_BINARY.kind,
        }];
        let inputs = NativeInputs::new(Arc::default(), &verified, dir.path()).unwrap();
        assert_eq!(inputs.binaries[COURIER_LINUX_BINARY.name].as_ref(), b"verified");
        std::fs::write(dir.path().join("courier"), b"changed").unwrap();
        assert_eq!(
            NativeInputs::new(Arc::default(), &verified, dir.path()).err().unwrap(),
            format!("'{}' changed after verification", dir.path().join("courier").display())
        );
    }

    #[tokio::test]
    async fn a_windows_participant_without_process_specs_is_a_setup_error_before_any_docker_call() {
        let api = FakeDocker::new(Some("linux"));
        let inputs = NativeInputs::new(Arc::new(RunBackends::with_docker(api.daemon())), &[], Path::new(".")).unwrap();
        let case = fixtures::LOADER
            .load_content(
                "cases",
                &fixtures::PARTICIPANT_FIELDS.replace("[linux]", "[windows]"),
                "case",
                Path::new("/cases/case/config.yaml"),
            )
            .unwrap()
            .remove(0);
        let plan = Planner::with_run(fixtures::REGISTRIES, "testcase")
            .compile(case, Os::Windows)
            .unwrap()
            .into_plan()
            .unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let control = CaseControl {
            interrupt: CancellationToken::new(),
            observations: Observations::default(),
            record: CaseRecord::default(),
            deadline: Deadline::after(Budget::Case, std::time::Duration::from_secs(60)),
            tasks: CaseTasks::new(&cancel),
            cancel,
            log_dir: log_dir.path().to_owned(),
            phases: PhaseTracker::default(),
            ledger: ResourceLedger::default(),
        };
        let result = run(plan.participants().unwrap(), fixtures::TYPES, &inputs, &control).await;
        let error = result.error.unwrap();
        assert_eq!(error.kind, ErrorKind::Setup);
        assert_eq!(
            error.message,
            "participant 'subject' of type 'server' as windows-container on windows declares no exit channel for process 'server'"
        );
        assert!(api.calls.lock().unwrap().is_empty());
        assert!(control.ledger.labels().is_empty());
    }

    #[tokio::test]
    async fn pod_participants_need_process_specs_before_registering_any_scope() {
        const TYPES: &[&dyn ParticipantType] = &[&fixtures::Server, &fixtures::PodServer];
        let registries = crate::case::Registries {
            participant_types: TYPES,
            ..fixtures::REGISTRIES
        };
        let loader = crate::case::CaseLoader::new(registries, fixtures::FixtureTypes);
        for (os, participants, expected) in [
            (
                Os::Linux,
                concat!(
                    "  first: {type: pod-server, subject: true, realization: {kind: kubernetes-pod}}\n",
                    "  second: {type: pod-server, realization: {kind: kubernetes-pod}}"
                ),
                concat!(
                    "participant 'first' of type 'pod-server' as kubernetes-pod on linux ",
                    "declares no exit channel for process 'server'"
                ),
            ),
            (
                Os::Linux,
                "  subject: {type: pod-server, subject: true, realization: {kind: kubernetes-pod}}",
                "participant 'subject' of type 'pod-server' as kubernetes-pod on linux declares no exit channel for process 'server'",
            ),
            (
                Os::Windows,
                "  subject: {type: server, subject: true}\n  other: {type: pod-server, os: linux, realization: {kind: kubernetes-pod}}",
                "participant 'other' of type 'pod-server' as kubernetes-pod on linux declares no exit channel for process 'server'",
            ),
        ] {
            let yaml = format!("timeout: 1s\nsubject_os: [{}]\nparticipants:\n{participants}\n{}\n", os.name(), fixtures::minimal_procedure!());
            let case = loader.load_content("cases", &yaml, "case", Path::new("/cases/case/config.yaml")).unwrap().remove(0);
            let plan = Planner::with_run(registries, "testcase").compile(case, os).unwrap().into_plan().unwrap();
            let api = FakeDocker::new(Some("linux"));
            let inputs = NativeInputs::new(Arc::new(RunBackends::with_docker(api.daemon())), &[], Path::new(".")).unwrap();
            let log_dir = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let control = CaseControl {
                interrupt: CancellationToken::new(),
                observations: Observations::default(),
                record: CaseRecord::default(),
                deadline: Deadline::after(Budget::Case, Duration::from_secs(60)),
                tasks: CaseTasks::new(&cancel),
                cancel,
                log_dir: log_dir.path().into(),
                phases: PhaseTracker::default(),
                ledger: ResourceLedger::default(),
            };
            let result = run(plan.participants().unwrap(), TYPES, &inputs, &control).await;
            let error = result.error.unwrap();
            assert_eq!(error.kind, ErrorKind::Setup);
            assert_eq!(error.message, expected);
            assert!(api.calls.lock().unwrap().is_empty());
            assert!(control.ledger.labels().is_empty());
            assert!(control.observations.snapshot().is_empty());
            assert_eq!(std::fs::read_dir(log_dir.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn creation_requires_deferred_template_files_to_be_placed_as_late_files() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::recipe::{ContainerRecipe, PodRecipe};

        struct PodFiles {
            late_path: Option<&'static str>,
            contents: Option<&'static [u8]>,
        }
        impl ParticipantType for PodFiles {
            fn name(&self) -> &'static str {
                "pod-files"
            }

            fn supports(&self) -> &'static [(Os, RealizationKind)] {
                &[(Os::Linux, RealizationKind::KubernetesPod)]
            }

            fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
                &[FIXUP_IMAGE]
            }

            fn recipe(&self, _: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
                let contents = self.contents.unwrap_or(&config.files["settings.yaml"]);
                let mut pod = PodRecipe::new(
                    "main",
                    ContainerRecipe::new(FIXUP_IMAGE)
                        .with_entrypoint(["/bin/sh"])
                        .with_command(["-c", "cat /etc/generated"])
                        .with_file("/etc/generated", 0o644, contents),
                );
                if let Some(path) = self.late_path {
                    pod = pod.with_late_file(path, "settings.yaml");
                }
                Ok(Recipe::Pod(pod))
            }

            fn config_files(&self) -> &'static [&'static str] {
                &["settings.yaml"]
            }

            fn processes(&self) -> &'static [&'static str] {
                &[]
            }
        }

        struct InspectPod {
            called: Arc<AtomicBool>,
            marked: bool,
            contents: &'static [u8],
        }
        impl ScopeRuntime for InspectPod {
            fn participant(&self, request: ParticipantRequest<'_>) -> Result<Arc<dyn RuntimeParticipant>, String> {
                self.called.store(true, Ordering::SeqCst);
                let Recipe::Pod(pod) = request.recipe else {
                    panic!("expected a pod recipe")
                };
                let file = &pod.containers[0].recipe.files[0];
                assert_eq!(file.path, "/etc/generated");
                assert_eq!(file.contents, self.contents);
                assert_eq!(pod.late_files.contains_key(&file.path), self.marked);
                Err("reached backend".into())
            }
        }

        // Unmarked deferred templates would reach the pod literally; create rejects them before contacting the backend.
        for (template, late_path, contents, accepted) in [
            (true, None, None, false),
            (true, Some("/etc/generated"), None, true),
            (false, None, None, true),
            (true, Some("/etc/other"), None, false),
            (true, Some("/etc/generated"), Some(b"different".as_slice()), true),
        ] {
            let participant_type = PodFiles { late_path, contents };
            let types: &[&dyn ParticipantType] = &[&participant_type];
            let registries = crate::case::Registries {
                participant_types: types,
                ..fixtures::REGISTRIES
            };
            let dir = tempfile::tempdir().unwrap();
            let source = fixtures::write_case(dir.path(), "case", "");
            std::fs::write(source.parent().unwrap().join("input.txt"), "id={{ self.late.pod_uid }}").unwrap();
            let yaml = format!(
                r#"
timeout: 1s
subject_os: [linux]
participants:
  subject:
    type: pod-files
    subject: true
    realization: {{kind: kubernetes-pod}}
    config:
      files:
        settings.yaml: {{from: input.txt, template: {template}}}
{}
"#,
                fixtures::minimal_procedure!()
            );
            let case = crate::case::CaseLoader::new(registries, fixtures::FixtureTypes)
                .load_content("cases", &yaml, "case", &source)
                .unwrap()
                .remove(0);
            let plan = Planner::with_run(registries, "testcase")
                .compile(case, Os::Linux)
                .unwrap()
                .into_plan()
                .unwrap();
            let called = Arc::new(AtomicBool::new(false));
            let prepared = Prepared {
                scopes: BTreeMap::from([(
                    RealizationKind::KubernetesPod,
                    Box::new(InspectPod {
                        called: called.clone(),
                        marked: late_path.is_some(),
                        contents: contents.unwrap_or(b"id={{ self.late.pod_uid }}"),
                    }) as Box<dyn ScopeRuntime>,
                )]),
                id: plan.participants().unwrap().scope().id(),
                process_specs: BTreeMap::from([("subject".into(), &[] as &'static [ProcessSpec])]),
            };
            let inputs = NativeInputs::new(Arc::default(), &[], dir.path()).unwrap();
            let cancel = CancellationToken::new();
            let control = CaseControl {
                observations: Observations::default(),
                record: CaseRecord::default(),
                deadline: Deadline::after(Budget::Case, Duration::from_secs(60)),
                tasks: CaseTasks::new(&cancel),
                cancel,
                interrupt: CancellationToken::new(),
                log_dir: dir.path().into(),
                phases: PhaseTracker::default(),
                ledger: ResourceLedger::default(),
            };
            let error = create(
                plan.participants().unwrap(),
                types,
                &inputs,
                &control,
                &prepared,
                "subject",
                &BTreeMap::new(),
            )
            .await
            .err()
            .unwrap();
            assert_eq!(called.load(Ordering::SeqCst), accepted);
            assert_eq!(
                error,
                if accepted {
                    "reached backend"
                } else if late_path.is_some() {
                    "participant 'subject': configuration file 'settings.yaml' names unplaced late recipe file '/etc/other'"
                } else {
                    "participant 'subject': configuration file 'settings.yaml' has deferred late references but is not placed as a late recipe file"
                }
            );
            assert!(control.ledger.labels().is_empty());
            assert!(control.observations.snapshot().is_empty());
        }
    }

    #[test]
    fn backend_inputs_are_checked_for_each_kind_including_after_a_kind_without_artifacts() {
        for other in [RealizationKind::HostProcessGroup, RealizationKind::KubernetesPod] {
            for kinds in [
                [other, RealizationKind::LinuxContainer],
                [RealizationKind::LinuxContainer, other],
            ] {
                let mut inputs = NativeInputs::new(Arc::default(), &[], Path::new(".")).unwrap();
                let missing_courier = "native inputs lack verified binary role 'courier-linux-binary'";
                assert_eq!(
                    inputs.require_backends([other]),
                    if other == RealizationKind::KubernetesPod {
                        Err(missing_courier.into())
                    } else {
                        Ok(())
                    }
                );
                let first_missing = if kinds[0] == RealizationKind::KubernetesPod {
                    missing_courier
                } else {
                    "native inputs lack verified image role 'fixup-image'"
                };
                assert_eq!(inputs.require_backends(kinds), Err(first_missing.into()));
                inputs.images.insert(FIXUP_IMAGE.name, "sha256:fixup".into());
                assert_eq!(
                    inputs.require_backends(kinds),
                    Err("native inputs lack verified binary role 'courier-linux-binary'".into())
                );
                inputs.binaries.insert(COURIER_LINUX_BINARY.name, Bytes::new());
                assert_eq!(inputs.require_backends(kinds), Ok(()));
            }
        }
    }

    #[test]
    fn windows_backend_inputs_require_the_verified_windows_courier() {
        let mut inputs = NativeInputs::new(Arc::default(), &[], Path::new(".")).unwrap();
        assert_eq!(
            inputs.require_backends([RealizationKind::WindowsContainer]),
            Err("native inputs lack verified binary role 'courier-windows-binary'".into())
        );
        inputs.binaries.insert(COURIER_WINDOWS_BINARY.name, Bytes::new());
        assert_eq!(inputs.require_backends([RealizationKind::WindowsContainer]), Ok(()));
    }

    #[tokio::test]
    async fn process_specs_must_cover_exactly_the_types_processes() {
        struct Specs(&'static [ProcessSpec]);
        impl ParticipantType for Specs {
            fn name(&self) -> &'static str {
                "server"
            }
            fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
                &[]
            }
            fn processes(&self) -> &'static [&'static str] {
                match self.0 {
                    [spec] if !["server", "worker"].contains(&spec.name) => std::slice::from_ref(&spec.name),
                    [ProcessSpec { name: "a", .. }, ProcessSpec { name: "a.stdout", .. }] => &["a", "a.stdout"],
                    _ => fixtures::Server.processes(),
                }
            }
            fn process_specs(&self, _: Realization) -> &'static [ProcessSpec] {
                self.0
            }
        }
        const SERVER: ProcessSpec = ProcessSpec {
            name: "server",
            exit: ExitChannel::Container,
            output: OutputChannel::Container,
        };
        const WORKER: ProcessSpec = ProcessSpec {
            name: "worker",
            exit: ExitChannel::Record { file: "worker" },
            output: OutputChannel::File {
                file: "output",
                stream: OutputStream::Merged,
            },
        };
        const CASES: &[(&[ProcessSpec], &str)] = &[
            (&[], "declares no exit channel for process 'server'"),
            (&[WORKER], "declares no exit channel for process 'server'"),
            (&[SERVER], "declares no exit channel for process 'worker'"),
            (
                &[
                    ProcessSpec {
                        exit: ExitChannel::Record { file: "x" },
                        ..SERVER
                    },
                    ProcessSpec {
                        exit: ExitChannel::Record { file: "x" },
                        ..WORKER
                    },
                ],
                "declares exit record file 'x' for processes 'server' and 'worker'; \
                 each process needs its own record file",
            ),
            (
                &[
                    SERVER,
                    WORKER,
                    ProcessSpec {
                        name: "extra",
                        exit: ExitChannel::Record { file: "extra" },
                        ..WORKER
                    },
                ],
                "declares an exit channel for process 'extra', which the type does not run",
            ),
            (
                &[SERVER, SERVER, WORKER],
                "declares two exit channels for process 'server'",
            ),
            (&[WORKER, WORKER], "declares no exit channel for process 'server'"),
            (
                &[
                    ProcessSpec {
                        exit: ExitChannel::Container,
                        ..WORKER
                    },
                    SERVER,
                ],
                "gives the container's exit to processes 'server' and 'worker'; only one process can own it",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec {
                        exit: ExitChannel::Record { file: "" },
                        ..WORKER
                    },
                ],
                "declares exit record file '' for process 'worker'; record files must be plain file names",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec {
                        exit: ExitChannel::Record { file: "." },
                        ..WORKER
                    },
                ],
                "declares exit record file '.' for process 'worker'; record files must be plain file names",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec {
                        exit: ExitChannel::Record { file: ".." },
                        ..WORKER
                    },
                ],
                "declares exit record file '..' for process 'worker'; record files must be plain file names",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec {
                        exit: ExitChannel::Record { file: "nested/file" },
                        ..WORKER
                    },
                ],
                "declares exit record file 'nested/file' for process 'worker'; record files must be plain file names",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec {
                        exit: ExitChannel::Record { file: "nested\\file" },
                        ..WORKER
                    },
                ],
                "declares exit record file 'nested\\file' for process 'worker'; record files must be plain file names",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec { output: OutputChannel::Container, ..WORKER },
                ],
                "gives the container's output to processes 'server' and 'worker'; only one process can own it",
            ),
            (
                &[
                    ProcessSpec {
                        output: OutputChannel::File { file: "output", stream: OutputStream::Stdout },
                        ..SERVER
                    },
                    WORKER,
                ],
                "declares output file 'output' for processes 'server' and 'worker'; each process needs its own output file",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec {
                        output: OutputChannel::File { file: "", stream: OutputStream::Merged },
                        ..WORKER
                    },
                ],
                "declares output file '' for process 'worker'; output files must be plain file names",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec {
                        output: OutputChannel::File { file: ".", stream: OutputStream::Merged },
                        ..WORKER
                    },
                ],
                "declares output file '.' for process 'worker'; output files must be plain file names",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec {
                        output: OutputChannel::File { file: "..", stream: OutputStream::Merged },
                        ..WORKER
                    },
                ],
                "declares output file '..' for process 'worker'; output files must be plain file names",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec {
                        output: OutputChannel::File { file: "nested/file", stream: OutputStream::Merged },
                        ..WORKER
                    },
                ],
                "declares output file 'nested/file' for process 'worker'; output files must be plain file names",
            ),
            (
                &[
                    SERVER,
                    ProcessSpec {
                        output: OutputChannel::File { file: "nested\\file", stream: OutputStream::Merged },
                        ..WORKER
                    },
                ],
                "declares output file 'nested\\file' for process 'worker'; output files must be plain file names",
            ),
            (
                &[ProcessSpec {
                    name: "container",
                    output: OutputChannel::File { file: "output", stream: OutputStream::Stdout },
                    ..SERVER
                }],
                "declares file output for process 'container' with stream Stdout; its host file would replace the container's own output at 'container.stdout.log'",
            ),
            (
                &[ProcessSpec {
                    name: "container",
                    output: OutputChannel::File { file: "output", stream: OutputStream::Stderr },
                    ..SERVER
                }],
                "declares file output for process 'container' with stream Stderr; its host file would replace the container's own output at 'container.stderr.log'",
            ),
            (
                &[
                    ProcessSpec {
                        name: "a",
                        output: OutputChannel::File { file: "a", stream: OutputStream::Stdout },
                        ..SERVER
                    },
                    ProcessSpec { name: "a.stdout", ..WORKER },
                ],
                "declares file output for process 'a.stdout' with stream Merged; its host file would replace output of process 'a' at 'a.stdout.log'",
            ),
            (
                &[ProcessSpec { name: "container.stdout", ..WORKER }],
                "declares file output for process 'container.stdout' with stream Merged; its host file would replace the container's own output at 'container.stdout.log'",
            ),
            (
                &[ProcessSpec { name: "container.stderr", ..WORKER }],
                "declares file output for process 'container.stderr' with stream Merged; its host file would replace the container's own output at 'container.stderr.log'",
            ),
            (
                &[ProcessSpec { name: "", ..WORKER }],
                "declares process ''; process names must be plain file names",
            ),
            (
                &[ProcessSpec { name: ".", ..WORKER }],
                "declares process '.'; process names must be plain file names",
            ),
            (
                &[ProcessSpec { name: "..", ..WORKER }],
                "declares process '..'; process names must be plain file names",
            ),
            (
                &[ProcessSpec { name: "../escape", ..WORKER }],
                "declares process '../escape'; process names must be plain file names",
            ),
            (
                &[ProcessSpec { name: "nested\\file", ..WORKER }],
                "declares process 'nested\\file'; process names must be plain file names",
            ),
            (
                &[ProcessSpec { name: "nested/file", ..SERVER }],
                "declares process 'nested/file'; process names must be plain file names",
            ),
        ];
        for (specs, message) in CASES {
            let api = FakeDocker::new(Some("linux"));
            let mut inputs =
                NativeInputs::new(Arc::new(RunBackends::with_docker(api.daemon())), &[], Path::new(".")).unwrap();
            inputs.images.insert(FIXUP_IMAGE.name, "sha256:fixup".into());
            inputs.binaries.insert(COURIER_LINUX_BINARY.name, Bytes::new());
            let log_dir = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let control = CaseControl {
                interrupt: CancellationToken::new(),
                observations: Observations::default(),
                record: CaseRecord::default(),
                deadline: Deadline::after(Budget::Case, std::time::Duration::from_secs(60)),
                tasks: CaseTasks::new(&cancel),
                cancel,
                phases: PhaseTracker::default(),
                ledger: ResourceLedger::default(),
                log_dir: log_dir.path().to_owned(),
            };
            let result = run(
                linux_plan().participants().unwrap(),
                &[&Specs(specs)],
                &inputs,
                &control,
            )
            .await;
            let error = result.error.unwrap();
            assert_eq!(error.kind, ErrorKind::Setup);
            assert_eq!(
                error.message,
                format!("participant 'subject' of type 'server' as linux-container on linux {message}")
            );
            assert_eq!(result.phase_timings.len(), 1);
            assert_eq!(result.phase_timings[0].phase, "prepare");
            assert!(api.calls.lock().unwrap().is_empty());
            assert!(control.ledger.labels().is_empty());
        }
    }

    #[tokio::test]
    async fn all_process_specs_are_checked_before_creating_output_directories() {
        const TYPES: &[&dyn ParticipantType] = &[&fixtures::Server, &fixtures::PodServer];
        let registries = crate::case::Registries {
            participant_types: TYPES,
            ..fixtures::REGISTRIES
        };
        let yaml = fixtures::PARTICIPANT_FIELDS.replace("participants:\n", "participants:\n  z: {type: pod-server}\n");
        let case = crate::case::CaseLoader::new(registries, fixtures::FixtureTypes)
            .load_content("cases", &yaml, "case", Path::new("/cases/case/config.yaml"))
            .unwrap()
            .remove(0);
        let plan = Planner::with_run(registries, "testcase")
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap();
        let api = FakeDocker::new(Some("linux"));
        let inputs = NativeInputs::new(Arc::new(RunBackends::with_docker(api.daemon())), &[], Path::new(".")).unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let control = CaseControl {
            interrupt: CancellationToken::new(),
            observations: Observations::default(),
            record: CaseRecord::default(),
            deadline: Deadline::after(Budget::Case, std::time::Duration::from_secs(60)),
            tasks: CaseTasks::new(&cancel),
            cancel,
            phases: PhaseTracker::default(),
            ledger: ResourceLedger::default(),
            log_dir: log_dir.path().to_owned(),
        };
        let result = run(plan.participants().unwrap(), TYPES, &inputs, &control).await;
        let error = result.error.unwrap();
        assert_eq!(error.kind, ErrorKind::Setup);
        assert_eq!(
            error.message,
            "participant 'z' of type 'pod-server' as linux-container on linux \
            declares no exit channel for process 'server'"
        );
        assert_eq!(std::fs::read_dir(log_dir.path()).unwrap().count(), 0);
        assert!(api.calls.lock().unwrap().is_empty());
        assert!(control.ledger.labels().is_empty());
    }

    #[tokio::test]
    async fn an_unwritable_output_directory_fails_preparation_before_any_docker_call() {
        let api = FakeDocker::new(Some("linux"));
        let mut inputs =
            NativeInputs::new(Arc::new(RunBackends::with_docker(api.daemon())), &[], Path::new(".")).unwrap();
        inputs.images.insert(FIXUP_IMAGE.name, "sha256:fixup".into());
        inputs.binaries.insert(COURIER_LINUX_BINARY.name, Bytes::new());
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::write(&log_dir, b"regular file").unwrap();
        let cancel = CancellationToken::new();
        let control = CaseControl {
            interrupt: CancellationToken::new(),
            observations: Observations::default(),
            record: CaseRecord::default(),
            deadline: Deadline::after(Budget::Case, std::time::Duration::from_secs(60)),
            tasks: CaseTasks::new(&cancel),
            cancel,
            phases: PhaseTracker::default(),
            ledger: ResourceLedger::default(),
            log_dir: log_dir.clone(),
        };
        let result = run(linux_plan().participants().unwrap(), fixtures::TYPES, &inputs, &control).await;
        let error = result.error.unwrap();
        assert_eq!(error.kind, ErrorKind::Setup);
        let path = log_dir.join("subject");
        let expected = std::fs::create_dir_all(&path).unwrap_err();
        assert_eq!(
            error.message,
            format!("cannot create the output directory '{}': {expected}", path.display())
        );
        assert_eq!(result.phase_timings.len(), 1);
        assert_eq!(result.phase_timings[0].phase, "prepare");
        assert!(api.calls.lock().unwrap().is_empty());
        assert!(control.ledger.labels().is_empty());
    }

    #[tokio::test]
    async fn missing_native_inputs_fail_preparation_without_registering_a_scope_or_contacting_docker() {
        for missing in [FIXUP_IMAGE, COURIER_LINUX_BINARY] {
            let api = FakeDocker::new(Some("linux"));
            let mut inputs =
                NativeInputs::new(Arc::new(RunBackends::with_docker(api.daemon())), &[], Path::new(".")).unwrap();
            if missing != FIXUP_IMAGE {
                inputs.images.insert(FIXUP_IMAGE.name, "sha256:fixup".into());
            }
            if missing != COURIER_LINUX_BINARY {
                inputs.binaries.insert(COURIER_LINUX_BINARY.name, Bytes::new());
            }
            let plan = linux_plan();
            let log_dir = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let control = CaseControl {
                interrupt: CancellationToken::new(),
                observations: Observations::default(),
                record: CaseRecord::default(),
                deadline: Deadline::after(Budget::Case, std::time::Duration::from_secs(60)),
                tasks: CaseTasks::new(&cancel),
                cancel,
                log_dir: log_dir.path().to_owned(),
                phases: PhaseTracker::default(),
                ledger: ResourceLedger::default(),
            };
            let result = run(plan.participants().unwrap(), fixtures::TYPES, &inputs, &control).await;
            let error = result.error.unwrap();
            assert_eq!(error.kind, ErrorKind::Setup);
            assert_eq!(
                error.message,
                format!(
                    "native inputs lack verified {} role '{}'",
                    missing.kind.name(),
                    missing.name
                ),
            );
            assert_eq!(result.phase_timings.len(), 1);
            assert_eq!(result.phase_timings[0].phase, "prepare");
            assert!(api.calls.lock().unwrap().is_empty());
            assert!(control.ledger.labels().is_empty());
        }
    }

    fn linux_plan() -> CasePlan<fixtures::FixtureConfig> {
        let case = fixtures::LOADER
            .load_content(
                "cases",
                fixtures::PARTICIPANT_FIELDS,
                "case",
                Path::new("/cases/case/config.yaml"),
            )
            .unwrap()
            .remove(0);
        Planner::with_run(fixtures::REGISTRIES, "testcase")
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap()
    }

    #[tokio::test]
    async fn a_failed_preparation_is_a_setup_error_and_the_plans_scope_stays_registered_for_reclaim() {
        let api = FakeDocker::new(Some("linux"));
        let mut inputs =
            NativeInputs::new(Arc::new(RunBackends::with_docker(api.daemon())), &[], Path::new(".")).unwrap();
        inputs.images.insert(FIXUP_IMAGE.name, "sha256:fixup".into());
        inputs.binaries.insert(COURIER_LINUX_BINARY.name, Bytes::new());
        let plan = linux_plan();
        let log_dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let control = CaseControl {
            interrupt: CancellationToken::new(),
            observations: Observations::default(),
            record: CaseRecord::default(),
            deadline: Deadline::after(Budget::Case, std::time::Duration::from_secs(60)),
            tasks: CaseTasks::new(&cancel),
            cancel,
            log_dir: log_dir.path().to_owned(),
            phases: PhaseTracker::default(),
            ledger: ResourceLedger::default(),
        };
        let result = run(plan.participants().unwrap(), fixtures::TYPES, &inputs, &control).await;
        assert_eq!(result.error.as_ref().unwrap().kind, ErrorKind::Setup);
        assert!(result
            .error
            .unwrap()
            .message
            .contains("this Docker connection cannot create containers"));
        assert_eq!(
            result
                .phase_timings
                .iter()
                .map(|phase| phase.phase.as_str())
                .collect::<Vec<_>>(),
            ["prepare"]
        );
        assert_eq!(control.ledger.labels(), ["isolation_group:testcase-0000"]);
        assert_eq!(*api.calls.lock().unwrap(), ["connect", "info"]);
    }
}
