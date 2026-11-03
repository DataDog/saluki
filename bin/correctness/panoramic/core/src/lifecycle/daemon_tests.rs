use std::{collections::BTreeSet, future::Future, time::Duration};

use airlock::{
    docker::{image_id, Docker},
    driver::{Driver, DriverConfig},
};
use bollard::models::ContainerInspectResponse;
use tokio::time::{timeout, Instant as TokioInstant};

use super::*;
use crate::{
    backend::linux_container::daemon_tests::{courier_bytes, remaining_resources},
    case::{
        fixtures::{self, FixtureConfig, FixtureTypes},
        CaseLoader, Registries,
    },
    endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind, Port},
    layout::Realization,
    participant::{EffectiveConfig, RenderedConfig},
    plan::Planner,
    recipe::{ContainerRecipe, RecipeFile},
    result::ReclaimResult,
};

const IMAGE: ArtifactRole = ArtifactRole {
    name: "lifecycle-test-image",
    kind: ArtifactKind::Image { os: Os::Linux },
    local_default: "panoramic-lifecycle-test:absent",
};
struct Fixture;
impl ParticipantType for Fixture {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[(Os::Linux, RealizationKind::LinuxContainer)]
    }
    fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
        &[IMAGE, COURIER_LINUX_BINARY]
    }
    fn recipe(&self, _: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        Ok(Recipe::Container(ContainerRecipe {
            image: if config.env.contains_key("BAD_ROLE") {
                FIXUP_IMAGE
            } else if config.env.contains_key("BAD_KIND") {
                COURIER_LINUX_BINARY
            } else {
                IMAGE
            },
            entrypoint: Some(if config.env.contains_key("BAD_START") {
                vec!["/missing-program".into()]
            } else {
                vec!["sh".into(), "-c".into()]
            }),
            command: Some(vec![config
                .env
                .get("SCRIPT")
                .cloned()
                .unwrap_or_else(|| "exec sleep 60".into())]),
            env: config.env.clone(),
            files: config
                .files
                .iter()
                .map(|(name, contents)| RecipeFile {
                    path: format!("/case/{name}"),
                    mode: 0o600,
                    contents: contents.clone(),
                })
                .collect(),
            mounts: vec![],
        }))
    }
    fn config_files(&self) -> &'static [&'static str] {
        &["settings"]
    }
    fn endpoints(&self, _: Realization, _: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
        Ok(BTreeMap::from([("api".into(), Endpoint::http(Port::Fixed(8080)))]))
    }
    fn consumes(&self, _: Realization, _: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        BTreeMap::from([(
            "upstream".into(),
            ConsumedEndpoint {
                accepts: vec![EndpointKind::Http],
                required: false,
                delivery: Delivery::Env {
                    keys: BTreeSet::new(),
                    value: |_| BTreeMap::new(),
                },
            },
        )])
    }
    fn processes(&self) -> &'static [&'static str] {
        &[]
    }
}

struct Observed {
    docker: Docker,
    plan: CasePlan<FixtureConfig>,
    result: TestResult,
    image_id: String,
    courier: Vec<u8>,
}
impl Observed {
    async fn container(&self, name: &str) -> ContainerInspectResponse {
        self.docker
            .inspect_container(&format!("airlock-{}-{name}", self.plan.scope().unwrap().id()), None)
            .await
            .unwrap()
    }
    fn driver(&self, name: &str) -> Driver {
        Driver::from_config(
            self.plan.scope().unwrap().id(),
            DriverConfig::from_image(name, IMAGE.local_default.into()),
        )
        .unwrap()
    }
}

async fn with_case<F: Future<Output = ()> + Send + 'static>(
    participants: &str, check: impl FnOnce(Observed) -> F + Send + 'static,
) {
    let backends = Arc::new(RunBackends::default());
    let docker = airlock::docker::connect().expect("Docker daemon is unavailable; check DOCKER_HOST");
    let image = std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".into());
    let image_id = image_id(&docker, &image)
        .await
        .unwrap()
        .expect("PANORAMIC_TEST_IMAGE must already be local");
    let courier = courier_bytes();
    let files = tempfile::tempdir().unwrap();
    std::fs::write(files.path().join("regular.txt"), b"case file").unwrap();
    std::fs::write(files.path().join("template.txt"), "peer={{ peer.late.container_id }}").unwrap();
    std::fs::write(files.path().join("courier"), &courier).unwrap();
    let registries = Registries {
        participant_types: &[&Fixture],
        steps: fixtures::STEPS,
        typed_case_roles: &[],
    };
    let case = CaseLoader::new(registries, FixtureTypes, "fixture-participants")
        .load_content(
            &format!("timeout: 60s\nsubject_os: [linux]\nparticipants:\n{participants}\nprocedure: [{{step: noop}}]\n"),
            "case",
            &files.path().join("config.yaml"),
        )
        .unwrap()
        .remove(0);
    let plan = Planner::new(registries, Default::default())
        .unwrap()
        .compile(case, Os::Linux)
        .unwrap()
        .into_plan()
        .unwrap();
    let id = plan.scope().unwrap().id();
    eprintln!("Native daemon test scope: {id}");
    // Only the verified image ID resolves to a local image.
    let verified = vec![
        VerifiedArtifact {
            role: IMAGE.name,
            reference: IMAGE.local_default.into(),
            identity: image_id.clone(),
            kind: IMAGE.kind,
        },
        VerifiedArtifact {
            role: FIXUP_IMAGE.name,
            reference: image,
            identity: image_id.clone(),
            kind: FIXUP_IMAGE.kind,
        },
        VerifiedArtifact {
            role: COURIER_LINUX_BINARY.name,
            reference: "courier".into(),
            identity: binary_identity(courier.as_slice()).unwrap(),
            kind: COURIER_LINUX_BINARY.kind,
        },
    ];
    let inputs = NativeInputs::new(backends, &verified, files.path()).unwrap();
    let ledger = ResourceLedger::default();
    let control = CaseControl {
        cancel: CancellationToken::new(),
        phases: PhaseTracker::default(),
        ledger: ledger.clone(),
    };
    let test_docker = docker.clone();
    let outcome = tokio::spawn(async move {
        timeout(Duration::from_secs(60), async {
            let result = run(&plan, &[&Fixture], &inputs, &control).await;
            check(Observed {
                docker: test_docker,
                plan,
                result,
                image_id,
                courier,
            })
            .await;
        })
        .await
        .expect("native scenario exceeded one minute");
    })
    .await;
    let (reclaimed, issues) = ledger.reclaim(TokioInstant::now() + Duration::from_secs(30)).await;
    let remaining = timeout(Duration::from_secs(30), remaining_resources(&docker, &id))
        .await
        .unwrap()
        .unwrap();
    assert!(remaining.is_empty(), "{id}: {remaining:?}");
    assert!(issues.is_empty(), "{id}: {issues:?}");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].resource, format!("isolation_group:{id}"));
    assert_eq!(reclaimed[0].result, ReclaimResult::Reclaimed);
    if let Err(error) = outcome {
        std::panic::resume_unwind(error.into_panic());
    }
}

const CONNECTED: &str = "
  consumer:
    type: fixture
    subject: true
    connect: {upstream: peer.api}
    config: {env: {PEER: '{{ peer.late.container_id }}'}}
  peer: {type: fixture}
  sibling: {type: fixture}
";

const FILES: &str = "
  consumer:
    type: fixture
    subject: true
    config:
      files:
        settings: {from: template.txt, template: true}
  peer: {type: fixture, config: {files: {settings: regular.txt}}}
";

const UNDECLARED_ROLE: &str = "
  subject: {type: fixture, subject: true, config: {env: {BAD_ROLE: '1'}}}
";

const NON_IMAGE_ROLE: &str = "
  subject: {type: fixture, subject: true, config: {env: {BAD_KIND: '1'}}}
";

const FAILED_START: &str = "
  first: {type: fixture, subject: true, config: {env: {BAD_START: '1'}}}
  second: {type: fixture, config: {env: {BAD_START: '1'}}}
";

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_native_case_creates_its_participants_in_create_order_starts_them_and_reclaim_removes_its_scope() {
    with_case(CONNECTED, |view| async move {
        assert_eq!(view.result.error.as_ref().unwrap().kind, ErrorKind::Evaluation);
        assert_eq!(
            view.result
                .phase_timings
                .iter()
                .map(|p| p.phase.as_str())
                .collect::<Vec<_>>(),
            [
                "prepare",
                "create:peer",
                "create:sibling",
                "create:consumer",
                "start:peer,sibling",
                "start:consumer"
            ]
        );
        for name in ["consumer", "peer", "sibling"] {
            let container = view.container(name).await;
            assert_eq!(container.state.unwrap().running, Some(true));
            assert_eq!(container.image.as_deref(), Some(view.image_id.as_str()));
            assert_eq!(container.config.unwrap().image.as_deref(), Some(view.image_id.as_str()));
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_participant_receives_the_container_id_of_a_participant_created_before_it() {
    with_case(CONNECTED, |view| async move {
        assert_eq!(view.result.error.as_ref().unwrap().kind, ErrorKind::Evaluation);
        let peer = view.container("peer").await.id.unwrap();
        let env = view.container("consumer").await.config.unwrap().env.unwrap();
        assert!(env.contains(&format!("PEER={peer}")), "{env:?}");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn each_participant_holds_the_verified_courier_and_its_rendered_case_files() {
    with_case(FILES, |view| async move {
        assert_eq!(view.result.error.as_ref().unwrap().kind, ErrorKind::Evaluation);
        let peer = view.container("peer").await.id.unwrap();
        for (name, contents) in [("consumer", format!("peer={peer}")), ("peer", "case file".into())] {
            let driver = view.driver(name);
            assert_eq!(
                driver.read_file("/panoramic/courier").await.unwrap().as_deref(),
                Some(view.courier.as_slice())
            );
            assert_eq!(
                driver.read_file("/case/settings").await.unwrap().as_deref(),
                Some(contents.as_bytes())
            );
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_recipe_naming_a_role_its_type_does_not_declare_fails_creation_naming_the_role() {
    with_case(UNDECLARED_ROLE, |view| async move {
        let error = view.result.error.unwrap();
        assert_eq!(error.kind, ErrorKind::Setup);
        assert_eq!(
            error.message,
            "recipe of participant 'subject' names role 'fixup-image', which type 'fixture' \
             does not declare for linux-container on linux",
        );
        assert_eq!(view.result.phase_timings.last().unwrap().phase, "create:subject");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_participant_that_exits_immediately_still_starts() {
    with_case(
        "  subject: {type: fixture, subject: true, config: {env: {SCRIPT: 'exit 7'}}}",
        |view| async move {
            assert_eq!(view.result.error.as_ref().unwrap().kind, ErrorKind::Evaluation);
            assert_eq!(view.result.phase_timings.last().unwrap().phase, "start:subject");
            assert!(!view
                .container("subject")
                .await
                .state
                .unwrap()
                .started_at
                .unwrap()
                .starts_with("0001"));
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_participant_that_fails_to_start_is_a_setup_error_in_its_start_phase() {
    with_case(FAILED_START, |view| async move {
        let error = view.result.error.unwrap();
        assert_eq!(error.kind, ErrorKind::Setup);
        for name in ["first", "second"] {
            assert!(error.message.contains(&format!("participant '{name}'")), "{error:?}");
        }
        assert_eq!(view.result.phase_timings.last().unwrap().phase, "start:first,second");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_recipe_naming_a_declared_binary_as_its_image_fails_creation_naming_the_role() {
    with_case(NON_IMAGE_ROLE, |view| async move {
        let error = view.result.error.unwrap();
        assert_eq!(error.kind, ErrorKind::Setup);
        assert_eq!(
            error.message,
            "recipe of participant 'subject' names role 'courier-linux-binary', which is not a Linux image",
        );
        assert_eq!(view.result.phase_timings.last().unwrap().phase, "create:subject");
    })
    .await;
}
