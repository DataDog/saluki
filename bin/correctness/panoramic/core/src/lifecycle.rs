//! Prepares native case scopes, creates participants from verified artifacts and starts their dependency levels.

use std::{collections::BTreeMap, path::Path, sync::Arc, time::Instant};

use bytes::Bytes;
use futures::future::join_all;
use tokio_util::sync::CancellationToken;

use crate::{
    artifact::{binary_identity, ArtifactKind, ArtifactRole, VerifiedArtifact},
    backend::{
        linux_container::{LinuxParticipant, LinuxScope},
        RunBackends, COURIER_LINUX_BINARY, FIXUP_IMAGE,
    },
    case::CaseSchema,
    layout::{Os, Realization, RealizationKind},
    ledger::ResourceLedger,
    participant::{find_type, ParticipantType},
    plan::CasePlan,
    recipe::Recipe,
    result::{ErrorKind, PhaseTracker, TestResult},
};

#[cfg(test)]
mod daemon_tests;
mod render;

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
    /// Cancellation of this case, including its deadline and run interruption.
    pub cancel: CancellationToken,
    /// The active phase and completed timings reported by the runner.
    pub phases: PhaseTracker,
    /// Resources reclaimed by the runner after the case body finishes or is dropped.
    pub ledger: ResourceLedger,
}

/// The registered scope used to create every participant in a case.
struct Prepared {
    scope: LinuxScope,
    id: String,
}

/// Prepares, creates and starts a native participant case using verified image IDs and container binaries.
///
/// The runner owns deadlines and reclaim. Setup failures include phase timings; successful startup returns an
/// evaluation error.
pub async fn run<C>(
    plan: &CasePlan<C>, types: &[&dyn ParticipantType], inputs: &NativeInputs, control: &CaseControl,
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
    let prepared = prepare(plan, inputs, control).await;
    phase.finish();
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return fail(error),
    };
    let mut created = BTreeMap::new();
    let mut participants = BTreeMap::new();
    for level in plan.create_order().expect("participant plans have create orders") {
        for name in level {
            let phase = control.phases.enter(format!("create:{name}"));
            let creation = create(plan, types, inputs, control, &prepared, name, &created).await;
            phase.finish();
            match creation {
                Ok((participant, container_id)) => {
                    created.insert(name.clone(), container_id);
                    participants.insert(name.clone(), (participant, None));
                }
                Err(error) => return fail(error),
            }
        }
    }
    for level in plan.start_order().expect("participant plans have start orders") {
        let phase = control.phases.enter(format!("start:{}", level.join(",")));
        let result = start_level(level, &mut participants, control).await;
        phase.finish();
        if let Err(error) = result {
            return fail(error);
        }
    }
    TestResult::errored(
        "",
        ErrorKind::Evaluation,
        "the procedure did not run: panoramic runs no steps of natively executed cases",
        started.elapsed(),
        control.phases.completed(),
    )
}

async fn prepare<C>(plan: &CasePlan<C>, inputs: &NativeInputs, control: &CaseControl) -> Result<Prepared, String> {
    let CaseSchema::Participants { file, .. } = plan.case().schema() else {
        return Err("native execution requires a participant case".into());
    };
    let layout = plan.layout().expect("participant plans have layouts");
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
    let image = inputs.image(FIXUP_IMAGE)?;
    let courier = inputs.binaries.get(COURIER_LINUX_BINARY.name).ok_or_else(|| {
        format!(
            "native inputs lack verified binary role '{}'",
            COURIER_LINUX_BINARY.name
        )
    })?;
    let scope = plan.scope().expect("participant plans have scopes");
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
    Ok(Prepared { scope, id })
}

async fn create<C>(
    plan: &CasePlan<C>, types: &[&dyn ParticipantType], inputs: &NativeInputs, control: &CaseControl,
    prepared: &Prepared, name: &str, created: &BTreeMap<String, String>,
) -> Result<(LinuxParticipant, String), String> {
    let config = render::render(plan, name, created)?;
    let realization = plan
        .layout()
        .expect("participant plans have layouts")
        .get(name)
        .expect("create order names laid-out participants");
    let CaseSchema::Participants { file, .. } = plan.case().schema() else {
        unreachable!("preparation validates the case schema");
    };
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
    level: &[String], participants: &mut BTreeMap<String, (LinuxParticipant, Option<String>)>, control: &CaseControl,
) -> Result<(), String> {
    let results = join_all(level.iter().map(|name| {
        let participant = &participants[name].0;
        async move {
            let result = control
                .cancel
                .run_until_cancelled(participant.start())
                .await
                .ok_or_else(|| format!("cancelled while starting participant '{name}'"))
                .and_then(|result| result.map_err(|error| format!("participant '{name}': {error:#}")));
            (name, result)
        }
    }))
    .await;
    let mut failures = Vec::new();
    for (name, result) in results {
        match result {
            Ok(ip) => participants.get_mut(name).unwrap().1 = ip,
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
    use crate::{backend::docker::test_util::FakeDocker, case::fixtures, participant::RenderedConfig, plan::Planner};

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
        let control = CaseControl {
            cancel: CancellationToken::new(),
            phases: PhaseTracker::default(),
            ledger: ResourceLedger::default(),
        };
        let result = run(&plan, fixtures::TYPES, &inputs, &control).await;
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
            let control = CaseControl {
                cancel: CancellationToken::new(),
                phases: PhaseTracker::default(),
                ledger: ResourceLedger::default(),
            };
            let result = run(&plan, fixtures::TYPES, &inputs, &control).await;
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
        let control = CaseControl {
            cancel: CancellationToken::new(),
            phases: PhaseTracker::default(),
            ledger: ResourceLedger::default(),
        };
        let result = run(&plan, fixtures::TYPES, &inputs, &control).await;
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
