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
    endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind, Evidence, Port},
    layout::Realization,
    participant::{EffectiveConfig, RenderedConfig},
    plan::{
        fixtures::{attach_endpoint_evidence, participant_evidence},
        Planner,
    },
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
    async fn assert_consumer_saw_marker(&self) {
        assert_eq!(
            self.result.error.as_ref().unwrap().kind,
            ErrorKind::Evaluation,
            "{:?}",
            self.result.error
        );
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
        assert_eq!(self.result.phase_timings.last().unwrap().phase, "start:consumer");
    }

    async fn assert_consumer_never_started(&self) {
        assert_eq!(self.result.error.as_ref().unwrap().kind, ErrorKind::Setup);
        assert_eq!(self.result.phase_timings.last().unwrap().phase, "start:consumer");
        let state = self.container("consumer").await.state.unwrap();
        assert_eq!(state.running, Some(false));
        assert!(state.started_at.unwrap().starts_with("0001"));
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
    with_case_on_ports(participants, 20000..=29999, check).await;
}

// Runs the case with runner-host ports drawn from `ports`. HTTP scenarios pass a port from `host_ports`,
// outside the production pool, so concurrent runs on one daemon do not collide.
async fn with_case_on_ports<F: Future<Output = ()> + Send + 'static>(
    participants: &str, ports: RangeInclusive<u16>, check: impl FnOnce(Observed) -> F + Send + 'static,
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
    with_case(&case, |view| async move {
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
    with_case_on_ports(&case, port..=port, |view| async move {
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
    with_case_on_ports(&case, port..=port, |view| async move {
        view.assert_consumer_never_started().await;
        let expected = "participant 'consumer' cannot start: http-ok on 'api' at '/ready' of participant 'server' \
                        was not ready within 2s: answered HTTP 503";
        assert_eq!(view.result.error.as_ref().unwrap().message, expected);
        let phase = view.result.phase_timings.last().unwrap();
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
        with_case(&case, |view| async move {
            view.assert_consumer_saw_marker().await;
        })
        .await;
    }
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with make test-panoramic-daemon"]
async fn a_server_that_exits_before_accepting_fails_its_consumers_start_without_waiting_out_its_bound() {
    let case = readiness_case("api", &[("READY_TCP", "api")], "exit 3");
    with_case(&case, |view| async move {
        view.assert_consumer_never_started().await;
        let message = &view.result.error.as_ref().unwrap().message;
        let expected = "participant 'consumer' cannot start: checking tcp-accepts on 'api' of participant 'server' \
                        failed: The courier probe to tcp://server:8080 failed: ";
        let failure = message.strip_prefix(expected).unwrap_or_else(|| panic!("{message}"));
        // Docker can refuse the exec or report exit 126 if the container exits after exec creation.
        let exited = "The courier never ran (exit 126); check that the binary is installed, executable \
                      and built for the container architecture";
        assert!(failure.starts_with("exec failed: ") || failure == exited, "{message}");
        let phase = view.result.phase_timings.last().unwrap();
        assert!(phase.duration < Duration::from_secs(10), "{phase:?}");
    })
    .await;
}
