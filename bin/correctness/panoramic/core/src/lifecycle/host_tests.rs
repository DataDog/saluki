use std::collections::BTreeSet;

use serde_json::json;
use tokio::time::{timeout, Instant as Clock};

use super::*;
use crate::{
    case::{
        fixtures::{FixtureConfig, FixtureTypes, STEPS},
        CaseLoader, Registries,
    },
    deadline::Budget,
    endpoint::{ConsumedEndpoint, Delivery, Endpoint, EndpointKind, Evidence, Port},
    layout::Os,
    ledger::random_id,
    participant::{EffectiveConfig, ExitChannel, OutputChannel, RenderedConfig},
    plan::{CasePlan, Planner},
    recipe::{HostProcess, HostRecipe, Recipe},
    result::{ReclaimResult, TestOutcome},
};

const SHELL: ArtifactRole = ArtifactRole {
    name: "host-lifecycle-shell",
    kind: ArtifactKind::Binary,
    local_default: "/bin/sh",
};
const BROKEN: ArtifactRole = ArtifactRole {
    name: "host-lifecycle-broken",
    kind: ArtifactKind::Binary,
    local_default: "",
};
const SPECS: &[ProcessSpec] = &[
    ProcessSpec {
        name: "main",
        exit: ExitChannel::Wait,
        output: OutputChannel::Pipes,
    },
    ProcessSpec {
        name: "worker",
        exit: ExitChannel::Wait,
        output: OutputChannel::Pipes,
    },
];
const BOUND: Duration = Duration::from_secs(20);
struct FixtureType;

impl ParticipantType for FixtureType {
    fn name(&self) -> &'static str {
        "host-fixture"
    }
    fn supports(&self) -> &'static [(Os, RealizationKind)] {
        &[
            (Os::Linux, RealizationKind::HostProcessGroup),
            (Os::MacOs, RealizationKind::HostProcessGroup),
        ]
    }
    fn artifacts(&self, _: Realization) -> &'static [ArtifactRole] {
        &[SHELL, BROKEN]
    }
    fn stop_grace(&self, _: Realization) -> Duration {
        Duration::from_millis(200)
    }
    fn recipe(&self, _: Realization, config: &RenderedConfig) -> Result<Recipe, String> {
        let mut processes = Vec::new();
        for (name, key) in [("main", "SCRIPT"), ("worker", "WORKER")] {
            processes.push(HostProcess::new(
                name,
                if name == "worker" && config.env.contains_key("BROKEN") {
                    BROKEN
                } else {
                    SHELL
                },
                vec![
                    "-c".into(),
                    config.env.get(key).cloned().unwrap_or_else(|| "exit 0".into()),
                    config.env["MARKER"].clone(),
                ],
            ));
        }
        let mut recipe = HostRecipe::new(processes);
        recipe.env = config.env.clone();
        Ok(Recipe::HostProcessGroup(recipe))
    }
    fn endpoints(&self, _: Realization, config: &EffectiveConfig) -> Result<BTreeMap<String, Endpoint>, String> {
        Ok(if config.env.contains_key("SERVE") {
            BTreeMap::from([(
                "ready".into(),
                Endpoint::tcp(Port::Fixed(9000)).with_ready(Evidence::output_matches("main", "^ready$").within(BOUND)),
            )])
        } else {
            BTreeMap::new()
        })
    }
    fn consumes(&self, _: Realization, _: &EffectiveConfig) -> BTreeMap<String, ConsumedEndpoint> {
        BTreeMap::from([(
            "upstream".into(),
            ConsumedEndpoint {
                accepts: vec![EndpointKind::Tcp],
                required: false,
                delivery: Delivery::Env {
                    keys: BTreeSet::from(["UPSTREAM".into()]),
                    value: |address| BTreeMap::from([("UPSTREAM".into(), address.url().into())]),
                },
            },
        )])
    }
    fn processes(&self) -> &'static [&'static str] {
        &["main", "worker"]
    }
    fn process_specs(&self, _: Realization) -> &'static [ProcessSpec] {
        SPECS
    }
}

const TYPES: &[&dyn ParticipantType] = &[&FixtureType];
const REGISTRIES: Registries<'static> = Registries {
    participant_types: TYPES,
    steps: STEPS,
    typed_case_roles: &[],
};

fn plan(participants: serde_json::Value, procedure: serde_json::Value, run: &str) -> CasePlan<FixtureConfig> {
    let yaml = json!({"timeout": "30s", "subject_os": [Os::host().name()], "participants": participants, "procedure": procedure}).to_string();
    let case = CaseLoader::new(REGISTRIES, FixtureTypes, "host-fixture")
        .load_content(&yaml, "case", Path::new("/cases/host/config.yaml"))
        .unwrap()
        .remove(0);
    Planner::with_run(REGISTRIES, run)
        .compile(case, Os::host())
        .unwrap()
        .into_plan()
        .unwrap()
}

fn control(path: &Path) -> CaseControl {
    let cancel = CancellationToken::new();
    CaseControl {
        observations: Observations::default(),
        record: CaseRecord::default(),
        deadline: Deadline::after(Budget::Case, BOUND),
        tasks: CaseTasks::new(&cancel),
        cancel,
        phases: PhaseTracker::default(),
        ledger: ResourceLedger::default(),
        log_dir: path.into(),
    }
}

fn inputs(broken: &Path) -> NativeInputs {
    NativeInputs::new(
        Arc::default(),
        &[
            VerifiedArtifact {
                role: SHELL.name,
                kind: SHELL.kind,
                reference: "/bin/sh".into(),
                identity: "verified".into(),
            },
            VerifiedArtifact {
                role: BROKEN.name,
                kind: BROKEN.kind,
                reference: broken.to_string_lossy().into(),
                identity: "verified".into(),
            },
        ],
        Path::new("."),
    )
    .unwrap()
}

#[tokio::test]
async fn host_groups_run_through_readiness_procedure_reports_and_reclaim() {
    let run_id = random_id();
    let marker = format!("panoramic-host-{run_id}-lifecycle");
    let plan = plan(
        json!({
            "server": {"type": "host-fixture", "config": {"env": {
                "MARKER": marker, "SERVE": "yes",
                "SCRIPT": "sleep 0.1; echo made > shared/ready; echo ready; while [ ! -f shared/finish ]; do sleep 0.01; done; echo server-end",
                "WORKER": "while [ ! -f shared/finish ]; do sleep 0.01; done; echo worker-end >&2",
            }}},
            "subject": {"type": "host-fixture", "subject": true, "realization": {"kind": "host-process-group"}, "connect": {"upstream": "server.ready"}, "config": {"env": {
                "MARKER": marker, "SERVER_SHARED": "{{ server.facts.shared_dir }}",
                "SCRIPT": "test -f \"$SERVER_SHARED/ready\" || exit 12; echo consumer; echo done > \"$SERVER_SHARED/finish\"",
            }}}
        }),
        json!([
            {"step": "probe", "on": "subject", "process": "main"},
            {"step": "probe", "on": "subject", "process": "worker"},
            {"step": "probe", "on": "server", "process": "main"},
            {"step": "probe", "on": "server", "process": "worker"},
        ]),
        &run_id,
    );
    let logs = tempfile::tempdir().unwrap();
    let control = control(logs.path());
    let result = timeout(
        BOUND,
        run(
            plan.participants().unwrap(),
            TYPES,
            &inputs(Path::new("/bin/sh")),
            &control,
        ),
    )
    .await
    .unwrap();
    let report = serde_json::to_value(control.observations.snapshot()).unwrap();
    assert_eq!(control.tasks.stop_all(Clock::now() + BOUND).await, 0);
    let (reclaimed, issues) = control.ledger.reclaim(Clock::now() + BOUND).await;
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(result.outcome, TestOutcome::Passed, "{result:?}");
    assert_eq!(
        result
            .phase_timings
            .iter()
            .map(|phase| phase.phase.as_str())
            .collect::<Vec<_>>(),
        [
            "prepare",
            "create:server",
            "create:subject",
            "start:server",
            "start:subject",
            "steps",
            "observe"
        ]
    );
    let participants = report.as_array().unwrap();
    assert_eq!(
        participants
            .iter()
            .map(|participant| participant["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["server", "subject"]
    );
    for participant in participants {
        assert_eq!(participant["realization"], "host-process-group");
        let processes = participant["processes"].as_array().unwrap();
        assert_eq!(
            processes
                .iter()
                .map(|process| process["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["main", "worker"]
        );
        for process in processes {
            assert_eq!(
                process["exit"],
                json!({"state": "exited", "source": "process", "code": 0})
            );
            let stdout = process["output"]["stdout"].as_str().unwrap();
            let stderr = process["output"]["stderr"].as_str().unwrap();
            assert!(Path::new(stdout).is_file());
            assert!(Path::new(stderr).is_file());
            assert_ne!(stdout, stderr);
        }
    }
    assert_eq!(
        std::fs::read(logs.path().join("server/main.stdout.log")).unwrap(),
        b"ready\nserver-end\n"
    );
    assert_eq!(
        std::fs::read(logs.path().join("server/worker.stderr.log")).unwrap(),
        b"worker-end\n"
    );
    assert_eq!(
        reclaimed
            .iter()
            .map(|entry| entry.resource.as_str())
            .collect::<Vec<_>>(),
        [
            "participant:subject",
            "participant:server",
            &format!("directory:{}", plan.scope().unwrap().state_dir(Os::host()))
        ]
    );
    assert!(reclaimed.iter().all(|entry| entry.result == ReclaimResult::Reclaimed));
    assert!(!Path::new(&plan.scope().unwrap().state_dir(Os::host()))
        .parent()
        .unwrap()
        .exists());
}

#[tokio::test]
async fn a_failed_host_start_reclaims_processes_spawned_before_the_failure() {
    use std::os::unix::fs::PermissionsExt as _;
    let logs = tempfile::tempdir().unwrap();
    let broken = logs.path().join("broken");
    std::fs::write(&broken, b"#!/panoramic-no-such-interpreter\n").unwrap();
    std::fs::set_permissions(&broken, std::fs::Permissions::from_mode(0o755)).unwrap();
    let run_id = random_id();
    let marker = format!("panoramic-host-{run_id}-spawn-failure");
    let plan = plan(
        json!({"subject": {"type": "host-fixture", "subject": true, "realization": {"kind": "host-process-group"}, "config": {"env": {
            "MARKER": marker, "BROKEN": "yes", "SCRIPT": "echo $$ > shared/pid; while :; do sleep 1; done",
        }}}}),
        json!([{"step": "noop"}]),
        &run_id,
    );
    let control = control(logs.path());
    let result = timeout(
        BOUND,
        run(plan.participants().unwrap(), TYPES, &inputs(&broken), &control),
    )
    .await
    .unwrap();
    let pidfile = PathBuf::from(plan.scope().unwrap().state_dir(Os::host())).join("subject/shared/pid");
    let pid: i32 = timeout(BOUND, async {
        loop {
            if let Ok(contents) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = contents.trim().parse() {
                    break pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(pid > 0);
    struct KillOnDrop(i32);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
    let _cleanup = KillOnDrop(pid);
    assert_eq!(unsafe { libc::kill(-pid, 0) }, 0);
    assert_eq!(control.tasks.stop_all(Clock::now() + BOUND).await, 0);
    let (entries, issues) = control.ledger.reclaim(Clock::now() + BOUND).await;
    let error = result.error.unwrap();
    assert_eq!(error.kind, ErrorKind::Setup);
    assert!(
        error.message.contains("failed to start process 'worker'"),
        "{}",
        error.message
    );
    assert_eq!(entries.len(), 2);
    assert!(
        entries.iter().all(|entry| entry.result == ReclaimResult::Reclaimed),
        "{entries:?}"
    );
    assert!(issues.is_empty(), "{issues:?}");
    timeout(BOUND, async {
        while unsafe { libc::kill(-pid, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!Path::new(&plan.scope().unwrap().state_dir(Os::host()))
        .parent()
        .unwrap()
        .exists());
}
