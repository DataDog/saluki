//! Prepares native case scopes, creates and starts participants, then runs their procedure.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use bytes::Bytes;
use futures::future::join_all;
use tokio_util::sync::CancellationToken;

use crate::{
    artifact::{binary_identity, ArtifactKind, ArtifactRole, VerifiedArtifact},
    backend::{
        linux_container::{LinuxParticipant, LinuxScope},
        RunBackends, COURIER_LINUX_BINARY, FIXUP_IMAGE,
    },
    deadline::Deadline,
    handle::Primitives,
    layout::{Os, Realization, RealizationKind},
    ledger::ResourceLedger,
    observe::ParticipantObservation,
    participant::{check_process_specs, find_type, ParticipantType, ProcessSpec},
    plan::ParticipantPlan,
    recipe::Recipe,
    record::CaseRecord,
    result::{ErrorKind, PhaseTracker, TestResult},
    tasks::CaseTasks,
};

#[cfg(test)]
mod daemon_tests;
mod readiness;
mod render;
mod steps;

/// Verified image IDs and container-binary bytes shared by every native case in a run.
pub struct NativeInputs {
    backends: Arc<RunBackends>,
    images: BTreeMap<&'static str, String>,
    binaries: BTreeMap<&'static str, Bytes>,
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
            binaries: BTreeMap::new(),
        };
        for artifact in verified {
            match artifact.kind {
                ArtifactKind::Image { .. } => {
                    inputs.images.insert(artifact.role, artifact.identity.clone());
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
                ArtifactKind::Binary | ArtifactKind::Install { .. } => {}
            }
        }
        Ok(inputs)
    }

    fn image(&self, role: ArtifactRole) -> Result<&str, String> {
        self.images
            .get(role.name)
            .map(String::as_str)
            .ok_or_else(|| format!("native inputs lack verified image role '{}'", role.name))
    }
}

/// Cancellation, phase reporting and resource ownership supplied by the case runner.
pub struct CaseControl {
    /// Procedure states and verdict retained by the runner through cancellation.
    pub record: CaseRecord,
    /// The case-wide deadline used to bound each step.
    pub deadline: Deadline,
    /// Cancellation of this case, including its deadline and run interruption.
    pub cancel: CancellationToken,
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
    scope: LinuxScope,
    id: String,
    process_specs: BTreeMap<String, &'static [ProcessSpec]>,
}

/// A started participant and its live observations.
struct Started {
    participant: Arc<LinuxParticipant>,
    container_id: String,
    ip: Option<String>,
    observation: ParticipantObservation,
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
                Ok((participant, container_id)) => {
                    created.insert(name.clone(), container_id);
                    participants.insert(name.clone(), Arc::new(participant));
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
            start_level(
                level,
                &mut participants,
                &mut running,
                &created,
                &prepared.process_specs,
                control,
            )
            .await
        }
        .await;
        phase.finish();
        if let Err(error) = result {
            return fail(error);
        }
    }
    let phase = control.phases.enter("steps");
    steps::run(plan, &running, control).await;
    phase.finish();
    control.record.result("", started.elapsed(), control.phases.completed())
}

async fn prepare(
    plan: ParticipantPlan<'_>, types: &[&dyn ParticipantType], inputs: &NativeInputs, control: &CaseControl,
) -> Result<Prepared, String> {
    let file = plan.file();
    let layout = plan.layout();
    let unsupported = layout
        .iter()
        .filter(|(_, realization)| realization.os != Os::Linux || realization.kind != RealizationKind::LinuxContainer)
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
    let image = inputs.image(FIXUP_IMAGE)?;
    let courier = inputs.binaries.get(COURIER_LINUX_BINARY.name).ok_or_else(|| {
        format!(
            "native inputs lack verified binary role '{}'",
            COURIER_LINUX_BINARY.name
        )
    })?;
    let scope = plan.scope();
    let id = scope.id();
    let names = file.participants.keys().map(String::as_str).collect::<Vec<_>>();
    control.ledger.isolation_group_with_id(id.clone());
    let scope = control
        .ledger
        .create(
            &id,
            &control.cancel,
            inputs
                .backends
                .prepare_linux_scope(scope, &names, image, courier.clone()),
        )
        .await
        .ok_or_else(|| "cancelled while preparing the isolation scope".to_string())?
        .map_err(|error| format!("failed to prepare isolation scope '{id}': {error:#}"))?;
    Ok(Prepared {
        scope,
        id,
        process_specs,
    })
}

async fn create(
    plan: ParticipantPlan<'_>, types: &[&dyn ParticipantType], inputs: &NativeInputs, control: &CaseControl,
    prepared: &Prepared, name: &str, created: &BTreeMap<String, String>,
) -> Result<(LinuxParticipant, String), String> {
    let config = render::render(plan, name, created)?;
    let realization = plan
        .layout()
        .get(name)
        .expect("create order names laid-out participants");
    let file = plan.file();
    let type_name = &file.participants[name].participant_type;
    let participant_type = find_type(types, type_name)
        .ok_or_else(|| format!("participant '{name}' has unregistered type '{type_name}'"))?;
    let Recipe::Container(recipe) = participant_type
        .recipe(realization, &config)
        .map_err(|error| format!("participant '{name}': {error}"))?;
    if !participant_type.artifacts(realization).contains(&recipe.image) {
        return Err(undeclared_role(name, type_name, recipe.image, realization));
    }
    if !matches!(recipe.image.kind, ArtifactKind::Image { os: Os::Linux }) {
        return Err(format!(
            "recipe of participant '{name}' names role '{}', which is not a Linux image",
            recipe.image.name,
        ));
    }
    let image = inputs.image(recipe.image)?;
    let participant = prepared
        .scope
        .participant(name, image, &recipe, &plan.runner_host_ports(name))
        .map_err(|error| format!("participant '{name}': {error:#}"))?;
    let container_id = control
        .ledger
        .create(&prepared.id, &control.cancel, participant.create())
        .await
        .ok_or_else(|| format!("cancelled while creating participant '{name}'"))?
        .map_err(|error| format!("{error:#}"))?;
    Ok((participant, container_id))
}

async fn start_level(
    level: &[String], participants: &mut BTreeMap<String, Arc<LinuxParticipant>>,
    running: &mut BTreeMap<String, Started>, created: &BTreeMap<String, String>,
    process_specs: &BTreeMap<String, &'static [ProcessSpec]>, control: &CaseControl,
) -> Result<(), String> {
    let results = join_all(level.iter().map(|name| {
        let participant = participants
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
                Ok(ip) => {
                    let observation = participant
                        .observe(process_specs[name], &control.tasks, &control.log_dir.join(name))
                        .await;
                    Ok(Started {
                        container_id: created[name].clone(),
                        participant,
                        ip,
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
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
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
        backend::docker::test_util::FakeDocker,
        case::fixtures,
        deadline::Budget,
        participant::{ExitChannel, OutputChannel, OutputStream, RenderedConfig},
        plan::{CasePlan, Planner},
    };

    #[test]
    fn the_default_recipe_names_the_type_and_realization() {
        let config = RenderedConfig {
            env: BTreeMap::new(),
            params: BTreeMap::new(),
            files: BTreeMap::new(),
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
    async fn a_participant_on_a_realization_without_a_native_backend_is_a_setup_error_before_any_docker_call() {
        let api = FakeDocker::new(Some("linux"));
        let inputs = NativeInputs::new(Arc::new(RunBackends::with_docker(api.daemon())), &[], Path::new(".")).unwrap();
        let case = fixtures::LOADER
            .load_content(
                &fixtures::PARTICIPANT_FIELDS.replace("[linux]", "[macos]"),
                "case",
                Path::new("/cases/case/config.yaml"),
            )
            .unwrap()
            .remove(0);
        let plan = Planner::with_run(fixtures::REGISTRIES, "testcase")
            .compile(case, Os::MacOs)
            .unwrap()
            .into_plan()
            .unwrap();
        let log_dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let control = CaseControl {
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
            "panoramic cannot create host-process-group participants natively: 'subject' on macos"
        );
        assert!(api.calls.lock().unwrap().is_empty());
        assert!(control.ledger.labels().is_empty());
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
        let case = crate::case::CaseLoader::new(registries, fixtures::FixtureTypes, "fixture-participants")
            .load_content(&yaml, "case", Path::new("/cases/case/config.yaml"))
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
