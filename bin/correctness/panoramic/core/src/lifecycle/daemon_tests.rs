use std::{collections::BTreeSet, future::Future, ops::RangeInclusive, time::Duration};

use airlock::{
    docker::{image_id, Docker},
    driver::{Driver, DriverConfig},
};
use bollard::models::{ContainerInspectResponse, PortBinding};
use tokio::time::{timeout, Instant as TokioInstant};

use super::*;
use crate::{
    backend::linux_container::daemon_tests::{courier_bytes, host_ports, remaining_resources},
    case::{
        fixtures::{self, FixtureConfig, FixtureTypes},
        CaseLoader, Registries,
    },
    deadline::Budget,
    endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind, Evidence, Port},
    layout::Realization,
    participant::{EffectiveConfig, ExitChannel, OutputChannel, RenderedConfig},
    plan::{
        fixtures::{attach_endpoint_evidence, participant_evidence},
        CasePlan, Planner,
    },
    recipe::{ContainerRecipe, RecipeFile},
    record::StepStatus,
    result::{ReclaimResult, TestOutcome},
    tasks::TASK_STOP_TIME,
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
    fn stop_grace(&self, _: Realization) -> Duration {
        Duration::from_secs(1)
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
    fn endpoints(&self, _: Realization, config: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
        let mut endpoints = BTreeMap::from([("api".into(), Endpoint::http(Port::Fixed(8080)))]);
        attach_endpoint_evidence(config, &mut endpoints);
        Ok(endpoints)
    }
    fn readiness(&self, _realization: Realization, config: &EffectiveConfig) -> Vec<Evidence> {
        participant_evidence(config)
    }
    fn consumes(&self, _: Realization, _: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        BTreeMap::from([(
            "upstream".into(),
            ConsumedEndpoint {
                accepts: vec![EndpointKind::Http, EndpointKind::UnixDatagram],
                required: false,
                delivery: Delivery::Env {
                    keys: BTreeSet::new(),
                    value: |_| BTreeMap::new(),
                },
            },
        )])
    }
    fn processes(&self) -> &'static [&'static str] {
        &["main", "worker"]
    }
    fn process_specs(&self, _: Realization) -> &'static [ProcessSpec] {
        &[
            ProcessSpec {
                name: "main",
                exit: ExitChannel::Container,
                output: OutputChannel::Container,
            },
            ProcessSpec {
                name: "worker",
                exit: ExitChannel::Record { file: "worker" },
                output: OutputChannel::File {
                    file: "worker",
                    stream: crate::participant::OutputStream::Merged,
                },
            },
        ]
    }
}

struct Observed {
    docker: Docker,
    plan: CasePlan<FixtureConfig>,
    result: TestResult,
    record: CaseRecord,
    image_id: String,
    courier: Vec<u8>,
    log_dir: PathBuf,
}
impl Observed {
    async fn container(&self, name: &str) -> ContainerInspectResponse {
        self.docker
            .inspect_container(&format!("airlock-{}-{name}", self.plan.scope().unwrap().id()), None)
            .await
            .unwrap()
    }
    async fn assert_consumer_saw_marker(&self) {
        assert_eq!(self.result.outcome, TestOutcome::Passed, "{:?}", self.result.error);
        let consumer = self.driver("consumer");
        let saw = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(bytes) = consumer.read_file("/airlock/consumer/shared/saw").await.unwrap() {
                    if !bytes.is_empty() {
                        break bytes;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the consumer must record whether the server's marker existed at startup");
        assert_eq!(saw, b"yes\n");
        assert_eq!(self.result.phase_timings.last().unwrap().phase, "observe");
    }

    async fn assert_consumer_never_started(&self) {
        assert_eq!(self.result.error.as_ref().unwrap().kind, ErrorKind::Setup);
        assert_eq!(self.result.phase_timings.last().unwrap().phase, "observe");
        let state = self.container("consumer").await.state.unwrap();
        assert_eq!(state.running, Some(false));
        assert!(state.started_at.unwrap().starts_with("0001"));
    }

    async fn output(&self, name: &str, stream: &str, expected: &str) -> String {
        let path = self.log_dir.join(name).join(format!("container.{stream}.log"));
        timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(output) = std::fs::read_to_string(&path) {
                    if output.contains(expected) {
                        return output;
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{} must contain {expected:?}", path.display()))
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
    participants: &str, procedure: &str, check: impl FnOnce(Observed) -> F + Send + 'static,
) {
    with_case_on_ports(participants, procedure, 20000..=29999, check).await;
}

// Runs the case with runner-host ports drawn from `ports`. HTTP scenarios pass a port from `host_ports`,
// outside the production pool, so concurrent runs on one daemon do not collide.
async fn with_case_on_ports<F: Future<Output = ()> + Send + 'static>(
    participants: &str, procedure: &str, ports: RangeInclusive<u16>, check: impl FnOnce(Observed) -> F + Send + 'static,
) {
    with_case_control(participants, procedure, ports, None, check).await;
}

#[derive(Clone, Copy)]
enum Stop {
    Cancel,
    Deadline,
}

async fn with_case_control<F: Future<Output = ()> + Send + 'static>(
    participants: &str, procedure: &str, ports: RangeInclusive<u16>, stop: Option<Stop>,
    check: impl FnOnce(Observed) -> F + Send + 'static,
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
            &format!("timeout: 60s\nsubject_os: [linux]\nparticipants:\n{participants}\nprocedure: {procedure}\n"),
            "case",
            &files.path().join("config.yaml"),
        )
        .unwrap()
        .remove(0);
    let plan = Planner::new(registries, Default::default())
        .unwrap()
        .with_port_range(ports)
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
    let cancel = CancellationToken::new();
    let tasks = CaseTasks::new(&cancel);
    let logs = tempfile::tempdir().unwrap();
    let control = CaseControl {
        observations: Observations::default(),
        record: CaseRecord::default(),
        deadline: Deadline::after(
            Budget::Case,
            Duration::from_secs(if matches!(stop, Some(Stop::Deadline)) { 5 } else { 60 }),
        ),
        cancel,
        phases: PhaseTracker::default(),
        ledger: ledger.clone(),
        tasks: tasks.clone(),
        log_dir: logs.path().to_owned(),
    };
    let test_docker = docker.clone();
    let outcome = tokio::spawn(async move {
        timeout(Duration::from_secs(60), async {
            let run = run(plan.participants().unwrap(), &[&Fixture], &inputs, &control);
            let interrupt = async {
                if matches!(stop, Some(Stop::Cancel)) {
                    while control.record.running().is_empty() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    control.cancel.cancel();
                }
                std::future::pending::<()>().await;
            };
            let mut result = tokio::select! {
                result = run => result,
                () = interrupt => unreachable!("the interruption observer waits for the case"),
            };
            result.participants = control.observations.snapshot();
            let mut expected_resources = vec![format!("isolation_group:{}", plan.scope().unwrap().id())];
            for name in plan.create_order().unwrap().iter().flatten() {
                if result.participants.iter().any(|participant| &participant.name == name) {
                    expected_resources.push(format!("participant:{name}"));
                }
            }
            check(Observed {
                docker: test_docker,
                plan,
                result,
                record: control.record.clone(),
                image_id,
                courier,
                log_dir: control.log_dir.clone(),
            })
            .await;
            expected_resources
        })
        .await
        .expect("native scenario exceeded one minute")
    })
    .await;
    let remaining_tasks = tasks.stop_all(TokioInstant::now() + TASK_STOP_TIME).await;
    let registered = ledger.labels();
    let (reclaimed, issues) = ledger.reclaim(TokioInstant::now() + Duration::from_secs(30)).await;
    let remaining = timeout(Duration::from_secs(30), remaining_resources(&docker, &id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(remaining_tasks, 0, "{id}: observation tasks must stop before reclaim");
    assert!(remaining.is_empty(), "{id}: {remaining:?}");
    assert!(issues.is_empty(), "{id}: {issues:?}");
    assert_eq!(
        reclaimed.iter().map(|entry| &entry.resource).collect::<Vec<_>>(),
        registered.iter().rev().collect::<Vec<_>>()
    );
    assert_eq!(reclaimed.last().unwrap().resource, format!("isolation_group:{id}"));
    for entry in &reclaimed {
        assert_eq!(entry.result, ReclaimResult::Reclaimed);
        assert_eq!(
            entry.participant.as_deref(),
            entry.resource.strip_prefix("participant:")
        );
    }
    match outcome {
        Ok(expected_resources) => assert_eq!(registered, expected_resources),
        Err(error) => std::panic::resume_unwind(error.into_panic()),
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
    with_case(CONNECTED, "[{step: noop}]", |view| async move {
        assert_eq!(view.result.outcome, TestOutcome::Passed, "{:?}", view.result.error);
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
                "start:consumer",
                "steps",
                "observe"
            ]
        );
        assert_eq!(
            view.result
                .participants
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["consumer", "peer", "sibling"]
        );
        assert!(view.result.participants.iter().all(|p| p.processes.len() == 2));
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
    with_case(CONNECTED, "[{step: noop}]", |view| async move {
        assert_eq!(view.result.outcome, TestOutcome::Passed, "{:?}", view.result.error);
        let peer = view.container("peer").await.id.unwrap();
        let env = view.container("consumer").await.config.unwrap().env.unwrap();
        assert!(env.contains(&format!("PEER={peer}")), "{env:?}");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn each_participant_holds_the_verified_courier_and_its_rendered_case_files() {
    with_case(FILES, "[{step: noop}]", |view| async move {
        assert_eq!(view.result.outcome, TestOutcome::Passed, "{:?}", view.result.error);
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
    with_case(UNDECLARED_ROLE, "[{step: noop}]", |view| async move {
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
        "[{step: noop}]",
        |view| async move {
            assert_eq!(view.result.outcome, TestOutcome::Passed, "{:?}", view.result.error);
            assert_eq!(view.result.phase_timings.last().unwrap().phase, "observe");
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
    with_case(FAILED_START, "[{step: noop}]", |view| async move {
        let error = view.result.error.unwrap();
        assert_eq!(error.kind, ErrorKind::Setup);
        for name in ["first", "second"] {
            assert!(error.message.contains(&format!("participant '{name}'")), "{error:?}");
        }
        assert_eq!(view.result.phase_timings.last().unwrap().phase, "start:first,second");
        assert_eq!(
            view.result
                .participants
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(view.result.participants.iter().all(|p| p.processes.is_empty()));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_recipe_naming_a_declared_binary_as_its_image_fails_creation_naming_the_role() {
    with_case(NON_IMAGE_ROLE, "[{step: noop}]", |view| async move {
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

fn readiness_case(endpoint: &str, evidence: &[(&str, &str)], script: &str) -> String {
    let mut env = evidence.iter().copied().collect::<BTreeMap<_, _>>();
    env.insert("SCRIPT", script);
    let env = serde_json::to_string(&env).unwrap();
    format!(
        r#"
  consumer:
    type: fixture
    subject: true
    connect: {{upstream: server.{endpoint}}}
    config:
      env:
        SCRIPT: >-
          if [ -f /airlock/server/shared/marker ]; then echo yes; else echo no; fi
          > /airlock/consumer/shared/saw;
          exec sleep 60
  server:
    type: fixture
    config: {{env: {env}}}
"#
    )
}

fn http_script(status: u16, delay: bool) -> String {
    let start = if delay {
        "sleep 1; touch /airlock/server/shared/marker;"
    } else {
        ""
    };
    format!(
        r#"{start}
exec nc -lk -s 0.0.0.0 -p 8080 -e sh -c '
while IFS= read -r line; do
    [ "$line" = "$(printf "\r")" ] && break
done
printf "HTTP/1.1 {status} Status\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
'"#
    )
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_consumer_starts_only_after_its_servers_endpoint_accepts_tcp_connections() {
    let case = readiness_case(
        "api",
        &[("READY_TCP", "api")],
        "sleep 1; touch /airlock/server/shared/marker; exec nc -lk -p 8080 -e cat",
    );
    with_case(&case, "[{step: noop}]", |view| async move {
        view.assert_consumer_saw_marker().await;
        assert!(view.plan.runner_host_ports("server").is_empty());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_consumer_waits_for_its_servers_http_endpoint_to_answer_200_through_a_published_port() {
    let case = readiness_case("api", &[("READY_HTTP", "/ready")], &http_script(200, true));
    let port = host_ports(1)[0];
    with_case_on_ports(&case, "[{step: noop}]", port..=port, |view| async move {
        view.assert_consumer_saw_marker().await;
        let ports = view.plan.runner_host_ports("server");
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].container_port, 8080);
        let config = view.container("server").await.host_config.unwrap();
        assert_eq!(
            config.port_bindings,
            Some(std::collections::HashMap::from([(
                "8080/tcp".into(),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".into()),
                    host_port: Some(ports[0].host_port.to_string()),
                }])
            ),]))
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn an_endpoint_that_never_answers_200_fails_its_consumers_start_within_its_bound() {
    let case = readiness_case(
        "api",
        &[("READY_HTTP", "/ready"), ("READY_WITHIN", "2")],
        &http_script(503, false),
    );
    let port = host_ports(1)[0];
    with_case_on_ports(&case, "[{step: noop}]", port..=port, |view| async move {
        view.assert_consumer_never_started().await;
        let expected = "participant 'consumer' cannot start: http-ok on 'api' at '/ready' of participant 'server' \
                        was not ready within 2s: answered HTTP 503";
        assert_eq!(view.result.error.as_ref().unwrap().message, expected);
        let phase = view
            .result
            .phase_timings
            .iter()
            .find(|phase| phase.phase == "start:consumer")
            .unwrap();
        assert!(phase.duration >= Duration::from_secs(2), "{phase:?}");
        assert!(phase.duration < Duration::from_secs(5), "{phase:?}");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_consumer_waits_for_its_servers_socket_and_file() {
    for script in [
        "sleep 1; ln -sf /airlock/server/shared/log.sock /dev/log; syslogd -O -; \
         sleep 1; touch /airlock/server/shared/marker; touch /ready; exec sleep 60",
        "sleep 1; touch /ready; sleep 1; touch /airlock/server/shared/marker; \
         ln -sf /airlock/server/shared/log.sock /dev/log; exec syslogd -n -O -",
    ] {
        let case = readiness_case(
            "log",
            &[
                ("READY_SOCKET", "{{self.facts.shared_dir}}/log.sock"),
                ("READY_FILE", "/ready"),
            ],
            script,
        );
        with_case(&case, "[{step: noop}]", |view| async move {
            view.assert_consumer_saw_marker().await;
        })
        .await;
    }
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_server_that_exits_before_accepting_fails_its_consumers_start_without_waiting_out_its_bound() {
    let case = readiness_case("api", &[("READY_TCP", "api")], "exit 3");
    with_case(&case, "[{step: noop}]", |view| async move {
        view.assert_consumer_never_started().await;
        let message = &view.result.error.as_ref().unwrap().message;
        assert!(
            message.starts_with("participant 'consumer' cannot start: "),
            "{message}"
        );
        assert!(message.contains("participant 'server'"), "{message}");
        let phase = view
            .result
            .phase_timings
            .iter()
            .find(|phase| phase.phase == "start:consumer")
            .unwrap();
        assert!(phase.duration < Duration::from_secs(10), "{phase:?}");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_started_participants_output_reaches_its_log_files_while_it_runs() {
    with_case(
        "  subject: {type: fixture, subject: true, \
         config: {env: {SCRIPT: 'echo out-1; echo err-1 >&2; exec sleep 60'}}}",
        "[{step: noop}]",
        |view| async move {
            assert_eq!(view.result.outcome, TestOutcome::Passed, "{:?}", view.result.error);
            assert_eq!(view.output("subject", "stdout", "out-1").await, "out-1\n");
            assert_eq!(view.output("subject", "stderr", "err-1").await, "err-1\n");
            assert_eq!(view.container("subject").await.state.unwrap().running, Some(true));
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_consumer_waiting_for_an_http_endpoint_stops_when_its_server_exits() {
    let case = readiness_case("api", &[("READY_HTTP", "/ready")], "echo bye; exit 3");
    let port = host_ports(1)[0];
    with_case_on_ports(&case, "[{step: noop}]", port..=port, |view| async move {
        view.assert_consumer_never_started().await;
        let message = &view.result.error.as_ref().unwrap().message;
        assert!(
            message.contains("participant 'server' exited with code 3 before http-ok on 'api'"),
            "{message}"
        );
        let phase = view
            .result
            .phase_timings
            .iter()
            .find(|phase| phase.phase == "start:consumer")
            .unwrap();
        assert!(phase.duration < Duration::from_secs(10), "{phase:?}");
        assert_eq!(view.output("server", "stdout", "bye").await, "bye\n");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_started_participants_output_is_observed_even_when_its_sibling_fails_to_start() {
    with_case(
        concat!(
            "  first: {type: fixture, subject: true, config: {env: {BAD_START: '1'}}}\n",
            "  second: {type: fixture, config: {env: {SCRIPT: 'echo started; exec sleep 60'}}}",
        ),
        "[{step: noop}]",
        |view| async move {
            assert_eq!(view.result.error.as_ref().unwrap().kind, ErrorKind::Setup);
            assert!(view
                .result
                .error
                .as_ref()
                .unwrap()
                .message
                .contains("participant 'first'"));
            assert_eq!(view.output("second", "stdout", "started").await, "started\n");
            assert_eq!(view.container("second").await.state.unwrap().running, Some(true));
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_consumer_starts_only_after_its_servers_output_matches() {
    let case = readiness_case(
        "api",
        &[("READY_OUTPUT", "^listening$")],
        "sleep 1; touch /airlock/server/shared/marker; echo listening; exec sleep 60",
    );
    with_case(&case, "[{step: noop}]", |view| async move {
        view.assert_consumer_saw_marker().await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_passing_procedure_runs_after_start_and_passes_the_case() {
    with_case(
        "  subject: {type: fixture, subject: true, config: {env: {SCRIPT: 'echo marker; exec sleep 60'}}}",
        "[{step: marker, on: subject}, {parallel: [{step: noop}, {step: probe, on: subject, path: /panoramic/courier}]}]",
        |view| async move {
            assert_eq!(view.result.outcome, TestOutcome::Passed);
            assert!(view.record.decided());
            assert_eq!(view.result.phase_timings.iter().map(|phase| phase.phase.as_str()).collect::<Vec<_>>(),
                ["prepare", "create:subject", "start:subject", "steps", "observe"]);
            assert_eq!(view.result.assertion_results.iter().map(|step| (step.step, step.member, step.status)).collect::<Vec<_>>(),
                [(0, None, StepStatus::Passed), (1, Some(0), StepStatus::Passed), (1, Some(1), StepStatus::Passed)]);
        },
    ).await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_failing_step_fails_the_case_and_leaves_later_steps_not_run() {
    with_case(
        "  subject: {type: fixture, subject: true}",
        "[{step: probe, on: subject, path: /missing}, {step: noop}]",
        |view| async move {
            assert_eq!(view.result.outcome, TestOutcome::Failed);
            assert!(view.record.decided());
            assert_eq!(
                view.result
                    .assertion_results
                    .iter()
                    .map(|step| step.status)
                    .collect::<Vec<_>>(),
                [StepStatus::Failed, StepStatus::NotRun]
            );
            assert_eq!(view.result.assertion_results[0].message, "file '/missing' is missing");
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_step_reaches_its_servers_endpoint_through_the_planned_runner_host_route() {
    let participants = readiness_case("api", &[("READY_TCP", "api")], "exec nc -lk -p 8080 -e cat");
    with_case(
        &participants,
        "[{step: reach, endpoint: server.api}]",
        |view| async move {
            assert_eq!(
                view.result.outcome,
                TestOutcome::Passed,
                "{:?}",
                view.result.assertion_results
            );
            assert_eq!(view.result.assertion_results[0].name, "reach");
            assert_eq!(view.result.assertion_results[0].message, "endpoint is listening");
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_step_resolves_a_late_fact_in_its_path_when_it_runs() {
    with_case(
        "  subject: {type: fixture, subject: true, config: {env: {SCRIPT: 'touch /airlock/subject/shared/$(hostname -i); echo marker; exec sleep 60'}}}",
        "[{step: marker, on: subject}, {step: probe, on: subject, path: '{{subject.facts.shared_dir}}/{{subject.late.ip}}'}]",
        |view| async move {
            assert_eq!(view.result.outcome, TestOutcome::Passed, "{:?}", view.result.assertion_results);
            assert_eq!(view.result.assertion_results.len(), 2);
            assert!(view.result.assertion_results.iter().all(|step| step.status == StepStatus::Passed));
        },
    ).await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_step_waiting_for_a_process_without_an_exit_record_is_errored() {
    with_case(
        "  subject: {type: fixture, subject: true, config: {env: {SCRIPT: 'exit 0'}}}",
        "[{step: probe, on: subject, process: worker}, {step: noop}]",
        |view| async move {
            assert_eq!(view.result.outcome, TestOutcome::Errored);
            assert_eq!(view.result.error.as_ref().unwrap().kind, ErrorKind::Evaluation);
            assert_eq!(view.result.assertion_results[0].status, StepStatus::Errored);
            assert_eq!(
                view.result.assertion_results[0].message,
                "participant 'subject' stopped without an exit record for process 'worker'"
            );
            assert_eq!(view.result.assertion_results[1].status, StepStatus::NotRun);
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_step_reading_output_with_a_failed_capture_is_errored() {
    with_case(
        "  subject: {type: fixture, subject: true, config: {env: {SCRIPT: 'mkdir /airlock/subject/log/worker; exit 0'}}}",
        "[{step: marker, on: subject}]",
        |view| async move {
            assert_eq!(view.result.outcome, TestOutcome::Errored);
            assert_eq!(view.result.assertion_results[0].status, StepStatus::Errored);
            assert!(view.result.assertion_results[0].message.contains("worker"), "{:?}", view.result.assertion_results);
        },
    ).await;
}

async fn interrupted_procedure(stop: Stop) {
    with_case_control(
        "  subject: {type: fixture, subject: true}",
        "[{step: marker, on: subject}, {step: noop}]",
        20000..=29999,
        Some(stop),
        |view| async move {
            assert!(!view.record.decided());
            assert_eq!(
                view.result
                    .assertion_results
                    .iter()
                    .map(|step| step.status)
                    .collect::<Vec<_>>(),
                [StepStatus::Interrupted, StepStatus::NotRun]
            );
            assert_eq!(view.result.phase_timings.last().unwrap().phase, "observe");
        },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn cancelling_a_running_procedure_interrupts_its_step_and_leaves_the_case_undecided() {
    interrupted_procedure(Stop::Cancel).await;
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn the_case_deadline_interrupts_a_running_step() {
    interrupted_procedure(Stop::Deadline).await;
}
