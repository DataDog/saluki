//! Real product images exercised through the native runner.

use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use panoramic_core::{
    endpoint::EndpointOperation,
    probe::{AttemptLimits, POLL_INTERVAL},
    record::{AssertionStatus, StepReport},
    step::{ExecCommand, ExitObservation, Need, OutputEnd, Step, StepContext, StepFactory, StepFields},
};
use panoramic_participants::{AgentAdp, TOOLS_LINUX_IMAGE};
use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::test_support::run_native_case;

const BUDGET: Duration = Duration::from_secs(30);
const PACKET: &[u8] = b"panoramic.test:1|c";

#[derive(Debug)]
struct Forward;

#[async_trait]
impl Step for Forward {
    fn name(&self) -> &'static str {
        "forward"
    }

    fn needs(&self) -> Vec<Need> {
        vec![
            Need::Endpoint("first".into(), "dogstatsd_forward".into(), EndpointOperation::Send),
            Need::Operation("first".into(), "dump".into()),
            Need::Operation("second".into(), "dump".into()),
        ]
    }

    fn budget(&self) -> Duration {
        BUDGET
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let result = tokio::time::timeout(BUDGET, async {
            let mut intakes = Vec::new();
            for name in ["first", "second"] {
                let intake = cx
                    .handle(name)
                    .ok_or_else(|| format!("participant '{name}' has not started"))?;
                loop {
                    let remaining = BUDGET.saturating_sub(started.elapsed());
                    if intake
                        .op("dump", json!({"kind": "dogstatsd_forwarded"}), remaining)
                        .await
                        .is_ok()
                    {
                        break;
                    }
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                intakes.push(intake);
            }
            cx.handle("first")
                .ok_or("participant 'first' has not started")?
                .send(
                    "dogstatsd_forward",
                    PACKET,
                    &AttemptLimits::new(Duration::from_secs(10)),
                )
                .await
                .map_err(|error| error.to_string())?;
            loop {
                let mut packets = Vec::new();
                for intake in &intakes {
                    let remaining = BUDGET.saturating_sub(started.elapsed());
                    let dump = intake
                        .op("dump", json!({"kind": "dogstatsd_forwarded"}), remaining)
                        .await
                        .map_err(|error| error.to_string())?;
                    packets.push(serde_json::from_value::<Vec<Vec<u8>>>(dump).map_err(|error| error.to_string())?);
                }
                if !packets[1].is_empty() {
                    return Ok((
                        AssertionStatus::Failed,
                        format!("second intake received packets: {:?}", packets[1]),
                    ));
                }
                if packets[0] == [PACKET] {
                    return Ok((
                        AssertionStatus::Passed,
                        "only the first intake received the packet".into(),
                    ));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        })
        .await;
        let (status, message) = match result {
            Ok(Ok(result)) => result,
            Ok(Err(message)) => (AssertionStatus::Errored, message),
            Err(_) => (
                AssertionStatus::Failed,
                "intakes did not return dumps containing the expected packets".into(),
            ),
        };
        StepReport::new(started, status, message)
    }
}

struct ForwardFactory;

impl StepFactory for ForwardFactory {
    fn name(&self) -> &'static str {
        "forward"
    }

    fn build(&self, _: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(Forward))
    }
}

#[tokio::test]
#[ignore = "needs product images; run with make test-panoramic-products"]
async fn two_intakes_are_ready_independently_and_keep_their_own_forwarded_packets() {
    let _ = crate::default_crypto_provider().install_default();
    let bindings = product_bindings();
    let (_logs, report) = run_native_case(
        panoramic_participants::REGISTERED_TYPES,
        &[&ForwardFactory],
        json!({
            "timeout": "90s", "subject_os": ["linux"],
            "participants": {
                "first": {"type": "datadog-intake", "subject": true},
                "second": {"type": "datadog-intake"},
            },
            "procedure": [{"step": "forward"}],
        }),
        bindings,
        CancellationToken::new(),
    )
    .await;
    assert_eq!(report["outcome"], "passed", "{report:#}");
    assert_eq!(report["assertions"][0]["status"], "passed");
    let participants = report["participants"].as_array().unwrap();
    assert_eq!(participants.len(), 2);
    for (participant, name) in participants.iter().zip(["first", "second"]) {
        assert_eq!(participant["name"], name);
        assert_eq!(participant["type"], "datadog-intake");
        assert_eq!(participant["processes"].as_array().unwrap().len(), 1);
        let process = &participant["processes"][0];
        assert_eq!(process["name"], "intake");
        assert_eq!(process["exit"]["state"], "running");
        assert_eq!(process["exit"]["source"], "container");
    }
}

fn product_bindings() -> BTreeMap<String, String> {
    [
        (AgentAdp::LINUX_IMAGE, "PANORAMIC_AGENT_ADP_LINUX_IMAGE"),
        (TOOLS_LINUX_IMAGE, "PANORAMIC_TOOLS_LINUX_IMAGE"),
    ]
    .into_iter()
    .map(|(role, key)| {
        (
            role.name.into(),
            std::env::var(key).unwrap_or_else(|_| role.local_default.into()),
        )
    })
    .collect()
}

const AGENT_BUDGET: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug)]
struct Restart;

#[async_trait]
impl Step for Restart {
    fn name(&self) -> &'static str {
        "restart"
    }

    fn needs(&self) -> Vec<Need> {
        vec![
            Need::Process("adp".into(), "adp".into()),
            Need::Process("adp".into(), "agent".into()),
        ]
    }

    fn budget(&self) -> Duration {
        AGENT_BUDGET
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        let ready = regex::Regex::new(r#"listen_addr:"udp://[^"]+:58125" \| DogStatsD listener started\."#).unwrap();
        let outcome = tokio::time::timeout(AGENT_BUDGET, async {
            let handle = cx.handle("adp").ok_or("participant 'adp' has not started")?;
            loop {
                let batch = handle.output(Some("adp")).read();
                if let OutputEnd::Failed(reason) = batch.end {
                    return Err(reason);
                }
                let lines = batch.lines;
                if lines.iter().any(|line| ready.is_match(line)) {
                    break;
                }
                let exit = handle.exit("adp");
                if !matches!(exit, ExitObservation::Running) {
                    return Err(format!(
                        "ADP exited before its listener was ready: {exit:?}; output: {lines:?}"
                    ));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            let old_pid = handle
                .exec(
                    ExecCommand {
                        argv: ["s6-svstat", "-o", "pid", "/var/run/s6/services/data-plane"]
                            .map(String::from)
                            .to_vec(),
                        env: Vec::new(),
                        stdin: None,
                    },
                    Duration::from_secs(10),
                )
                .await
                .map_err(|error| error.to_string())?;
            if old_pid.exit_code != 0 {
                return Err(format!("cannot read ADP's supervised PID: {:?}", old_pid.stderr));
            }
            let killed = handle
                .exec(
                    ExecCommand {
                        argv: ["s6-svc", "-k", "/var/run/s6/services/data-plane"]
                            .map(String::from)
                            .to_vec(),
                        env: Vec::new(),
                        stdin: None,
                    },
                    Duration::from_secs(10),
                )
                .await
                .map_err(|error| error.to_string())?;
            if killed.exit_code != 0 {
                return Err(format!("s6-svc failed: {:?}", killed.stderr));
            }
            match handle.wait_for_exit("adp").await {
                ExitObservation::Exited(status) if status.code.is_none() && status.signal == Some(9) => {}
                other => return Err(format!("expected a recorded signal 9 death, got {other:?}")),
            }
            loop {
                let batch = handle.output(Some("adp")).read();
                if let OutputEnd::Failed(reason) = batch.end {
                    return Err(reason);
                }
                let lines = batch.lines;
                let restarted = lines.iter().filter(|line| ready.is_match(line)).count() >= 2;
                if restarted && lines.iter().any(|line| line.contains("restarting in 2 seconds")) {
                    break;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            let new_pid = handle
                .exec(
                    ExecCommand {
                        argv: ["s6-svstat", "-o", "pid", "/var/run/s6/services/data-plane"]
                            .map(String::from)
                            .to_vec(),
                        env: Vec::new(),
                        stdin: None,
                    },
                    Duration::from_secs(10),
                )
                .await
                .map_err(|error| error.to_string())?;
            if new_pid.exit_code != 0 || old_pid.stdout == new_pid.stdout || new_pid.stdout == b"-1\n" {
                return Err(format!(
                    "ADP did not restart with a new PID: {:?} -> {:?}",
                    old_pid.stdout, new_pid.stdout
                ));
            }
            if !matches!(handle.exit("agent"), ExitObservation::Running) {
                return Err("the Agent stopped during ADP's restart".into());
            }
            Ok("ADP's signal death was recorded and its supervisor restarted it".into())
        })
        .await;
        let (status, message) = match outcome {
            Ok(Ok(message)) => (AssertionStatus::Passed, message),
            Ok(Err(message)) => (AssertionStatus::Errored, message),
            Err(_) => (
                AssertionStatus::Failed,
                "product observation did not complete within two minutes".into(),
            ),
        };
        StepReport::new(started, status, message)
    }
}

impl StepFactory for Restart {
    fn name(&self) -> &'static str {
        Step::name(self)
    }
    fn build(&self, _: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(*self))
    }
}

fn connected_case(procedure: serde_json::Value) -> serde_json::Value {
    json!({
        "timeout": "180s", "subject_os": ["linux"],
        "participants": {
            "intake": {"type": "datadog-intake"},
            "adp": {"type": "agent-adp", "subject": true, "connect": {"intake": "intake.api"}, "config": {"env": {
                "DD_API_KEY": "00000000000000000000000000000000", "DD_HOSTNAME": "panoramic-product",
                "DD_DATA_PLANE_ENABLED": "true", "DD_DATA_PLANE_DOGSTATSD_ENABLED": "true",
                "DD_DOGSTATSD_NON_LOCAL_TRAFFIC": "true",
            }}},
        },
        "procedure": procedure,
    })
}

#[tokio::test]
#[ignore = "needs product images; run with make test-panoramic-products"]
async fn agent_adp_delivers_dogstatsd_metrics_to_its_connected_intake() {
    let _ = crate::default_crypto_provider().install_default();
    let case = connected_case(json!([
        {"step": "log_contains", "on": "adp", "pattern": r#"listen_addr:"udp://[^"]+" \| DogStatsD listener started\."#,
         "regex": true, "timeout": "60s"},
        {"step": "send", "on": "adp", "endpoint": "dogstatsd_udp", "payload": "panoramic.test:1|c", "timeout": "10s"},
        {"step": "intake_has_metric", "on": "intake", "name": "panoramic.test", "timeout": "90s"},
    ]));
    let (_logs, report) = run_native_case(
        panoramic_participants::REGISTERED_TYPES,
        panoramic_steps::REGISTERED_STEPS,
        case,
        product_bindings(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(report["outcome"], "passed", "{report:#}");
    let assertions = report["assertions"].as_array().unwrap();
    assert_eq!(assertions.len(), 3);
    assert!(assertions.iter().all(|step| step["status"] == "passed"), "{report:#}");
    let phases = report["phase_timings"].as_array().unwrap();
    let position = |name: &str| phases.iter().position(|phase| phase["phase"] == name).unwrap();
    assert!(position("start:intake") < position("start:adp"));
}

#[tokio::test]
#[ignore = "needs product images; run with make test-panoramic-products"]
async fn a_killed_adp_is_recorded_as_a_signal_death_and_restarted_by_its_supervisor() {
    let _ = crate::default_crypto_provider().install_default();
    let (_logs, report) = run_native_case(
        panoramic_participants::REGISTERED_TYPES,
        &[&Restart],
        connected_case(json!([{"step": "restart"}])),
        product_bindings(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(report["outcome"], "passed", "{report:#}");
    let adp = &report["participants"][0];
    assert_eq!(adp["name"], "adp");
    let processes = adp["processes"].as_array().unwrap();
    assert_eq!(processes[0]["name"], "adp");
    assert_eq!(
        processes[0]["exit"],
        json!({"state": "exited", "source": "record", "signal": 9})
    );
    assert_eq!(processes[1]["name"], "agent");
    assert_eq!(processes[1]["exit"], json!({"state": "running", "source": "record"}));
    let path = processes[0]["output"]["merged"].as_str().unwrap();
    assert!(path.ends_with("adp.log"), "{path}");
    let log = std::fs::read_to_string(path).unwrap();
    for text in [
        "DogStatsD listener started.",
        "starting agent-data-plane",
        "restarting in 2 seconds",
    ] {
        assert!(log.contains(text), "missing {text}: {log}");
    }
}

#[tokio::test]
#[ignore = "needs product images; run with make test-panoramic-products"]
async fn the_images_shipped_services_match_the_definition_sources_and_listener_defaults() {
    use airlock::{driver::DriverConfig, group::IsolationGroup};
    use panoramic_core::{
        layout::{Os, Realization, RealizationKind},
        participant::{CaseValues, ParticipantType},
    };

    let _ = crate::default_crypto_provider().install_default();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let docker = airlock::docker::connect().unwrap();
    let scope = format!("product-sources-{:x}", rand::random::<u64>());
    eprintln!("Product source test scope: {scope}");
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let image = product_bindings()[AgentAdp::LINUX_IMAGE.name].clone();
        let image = docker.inspect_image(&image).await?;
        let fixup = std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".into());
        let group = IsolationGroup::prepare(docker.clone(), scope.clone(), &["subject"], &fixup).await?;
        let driver = group.driver(DriverConfig::from_image("subject", image.id.clone().unwrap()))?;
        driver.create().await?;
        let mut comparisons = Vec::new();
        for (container, source) in [
            ("/etc/services.d/data-plane/run", "docker/s6-services/data-plane/run"),
            (
                "/etc/services.d/data-plane/finish",
                "docker/s6-services/data-plane/finish",
            ),
            ("/etc/services.d/agent/run", "docker/s6-services/agent/run"),
            (
                "/etc/services.d/agent/finish",
                "bin/correctness/panoramic/participants/src/agent_adp/assets/s6/agent-finish",
            ),
        ] {
            comparisons.push((
                container,
                driver.read_file(container).await?,
                std::fs::read(root.join(source))?,
            ));
        }
        Ok::<_, saluki_error::GenericError>((
            comparisons,
            image.config.and_then(|config| config.env).unwrap_or_default(),
        ))
    })
    .await;
    tokio::time::timeout(Duration::from_secs(30), IsolationGroup::remove(&docker, &scope))
        .await
        .unwrap()
        .unwrap();
    super::report_tests::assert_scope_removed(&scope).await;
    let (comparisons, env) = result.unwrap().unwrap();
    for (path, image, source) in comparisons {
        assert_eq!(image.as_deref(), Some(source.as_slice()), "{path}");
    }
    let mut keys = AgentAdp
        .default_env(
            Realization {
                os: Os::Linux,
                kind: RealizationKind::LinuxContainer,
            },
            &CaseValues::default(),
        )
        .into_keys()
        .collect::<Vec<_>>();
    keys.extend(
        [
            "DD_DATA_PLANE_ENABLED",
            "DD_DATA_PLANE_DOGSTATSD_ENABLED",
            "DD_USE_DOGSTATSD",
            "DD_DOGSTATSD_SOCKET",
            "DD_DOGSTATSD_STREAM_SOCKET",
            "DD_DOGSTATSD_NON_LOCAL_TRAFFIC",
            "DD_BIND_HOST",
            "DD_APM_RECEIVER_SOCKET",
        ]
        .map(String::from),
    );
    for entry in env {
        let key = entry.split_once('=').unwrap().0;
        let key = key.strip_prefix("ADP_").unwrap_or(key);
        assert!(
            !keys.iter().any(|read| read == key),
            "image supplies a listener setting the definition must account for: {entry}"
        );
    }
}

#[tokio::test]
#[ignore = "needs product images; run with make test-panoramic-products"]
async fn linux_logging_default_path_case_passes_natively() {
    let _ = crate::default_crypto_provider().install_default();
    let mut case: serde_json::Value = serde_saphyr::from_str(include_str!(
        "../../../../../test/integration/cases/adp-logging-default-path/config.yaml"
    ))
    .unwrap();
    case.as_object_mut().unwrap().remove("name");
    let (_logs, report) = run_native_case(
        panoramic_participants::REGISTERED_TYPES,
        panoramic_steps::REGISTERED_STEPS,
        case,
        product_bindings(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(report["outcome"], "passed", "{report:#}");
    let assertions = report["assertions"].as_array().unwrap();
    assert_eq!(assertions.len(), 2);
    assert!(
        assertions.iter().all(|assertion| assertion["status"] == "passed"),
        "{report:#}"
    );
    let participant = &report["participants"][0];
    assert_eq!(participant["name"], "adp");
    assert_eq!(participant["processes"][0]["name"], "adp");
    assert_eq!(participant["processes"][1]["name"], "agent");
}

fn bind_config(host: &str) -> serde_json::Value {
    json!({"env": {
        "DD_API_KEY": "00000000000000000000000000000000", "DD_HOSTNAME": "panoramic-bind",
        "DD_DATA_PLANE_ENABLED": "true", "DD_DATA_PLANE_STANDALONE_MODE": "false",
        "DD_DATA_PLANE_DOGSTATSD_ENABLED": "true", "DD_DOGSTATSD_NON_LOCAL_TRAFFIC": "false",
        "DD_BIND_HOST": host,
    }})
}

async fn assert_bind_case(config: serde_json::Value, listeners: &[(&str, u16)]) {
    let _ = crate::default_crypto_provider().install_default();
    let mut procedure = listeners
        .iter()
        .map(|(protocol, port)| {
            json!({
                "step": "log_contains", "on": "adp", "process": "adp",
                "pattern": format!("listen_addr:\"{protocol}://{{{{ adp.late.ip }}}}:{port}\""), "timeout": "60s",
            })
        })
        .collect::<Vec<_>>();
    procedure.push(json!({"parallel": [
        {"step": "process_stable_for", "on": "adp", "duration": "10s"},
        {"step": "log_not_contains", "on": "adp", "pattern": "panic|PANIC", "regex": true, "during": "10s"},
    ]}));
    let (_logs, report) = run_native_case(
        panoramic_participants::REGISTERED_TYPES,
        panoramic_steps::REGISTERED_STEPS,
        json!({
            "timeout": "180s", "subject_os": ["linux"],
            "participants": {"adp": {"type": "agent-adp", "subject": true, "config": config}},
            "procedure": procedure,
        }),
        product_bindings(),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(report["outcome"], "passed", "{report:#}");
    let assertions = report["assertions"].as_array().unwrap();
    assert_eq!(assertions.len(), listeners.len() + 2);
    assert!(assertions.iter().all(|step| step["status"] == "passed"), "{report:#}");
}

#[tokio::test]
#[ignore = "needs product images; run with make test-panoramic-products"]
async fn agent_adp_substitutes_its_own_ip_into_bind_host_before_adp_starts() {
    assert_bind_case(bind_config("{{ self.late.ip }}"), &[("udp", 58125)]).await;
}

#[tokio::test]
#[ignore = "needs product images; run with make test-panoramic-products"]
async fn agent_adp_maps_an_extra_host_to_its_own_ip_for_dogstatsd_and_apm_binds() {
    let mut config = bind_config("foo.local");
    config["params"] = json!({"extra_hosts": {"foo.local": "self"}});
    config["env"]["DD_DATA_PLANE_EXPERIMENTAL_APM_ENABLED"] = "true".into();
    config["env"]["DD_APM_NON_LOCAL_TRAFFIC"] = "false".into();
    config["env"]["DD_APM_ENABLED"] = "false".into();
    assert_bind_case(config, &[("udp", 58125), ("tcp", 58126)]).await;
}
