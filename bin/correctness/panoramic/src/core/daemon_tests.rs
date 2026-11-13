//! Tests of the container backend and the lifecycle against a real Docker daemon. They are ignored by default; run them
//! by hand with `make test-container-daemon`, which builds the courier they copy into each container.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::time::Duration;

use airlock::image::{ensure_image, DAEMON_TEST_IMAGE};
use async_trait::async_trait;
use bollard::errors::Error;
use bollard::query_parameters::ListContainersOptionsBuilder;
use bollard::Docker;
use futures::FutureExt as _;
use serde::Deserialize;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::case::{self, CASE_FILE};
use super::container::{CaseContainers, ContainerBackend, LINUX};
use super::deadline::{Budget, Deadline};
use super::ledger::ResourceLedger;
use super::lifecycle::{self, CaseRun, Ran};
use super::observe::{ExitStatus, ProcessExit, CONTAINER_LOG};
use super::participant::Os;
use super::plan::{plan, CasePlan};
use super::record::{CaseRecord, StepStatus};
use super::reference::Values;
use super::registry::Registry;
use super::result::{ErrorKind, PhaseTracker, ReclaimEntry, ReclaimResult, TestOutcome};
use super::spec::Keep;
use super::step::{Need, Step, StepContext, StepFactory, StepFields, StepReport};
use super::tasks::{CaseTasks, TASK_STOP_TIME};

const BUDGET: Duration = Duration::from_secs(60);

fn deadline() -> Instant {
    Instant::now() + BUDGET
}

/// A planned case with its scope prepared.
struct Case {
    plan: CasePlan,
    containers: CaseContainers,
    cancel: CancellationToken,
}

/// Plans `config`, a case's `config.yaml` in which `{image}` stands for the test image, for Linux, with `registry`.
fn plan_linux(config: &str, registry: &Registry) -> CasePlan {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(CASE_FILE), config.replace("{image}", DAEMON_TEST_IMAGE)).unwrap();
    case::load(dir.path(), registry)
        .and_then(|case| plan(&case, Os::Linux, registry))
        .unwrap_or_else(|errors| panic!("the case should plan: {errors:?}"))
}

/// The Linux container backend, with the test image present and the courier `make build-courier` built.
async fn backend() -> (Docker, ContainerBackend) {
    let _ = crate::default_crypto_provider().install_default();
    let docker = airlock::docker::connect().expect("a Docker daemon should be reachable");
    ensure_image(&docker, DAEMON_TEST_IMAGE, deadline())
        .await
        .expect("the test image should be available");
    let courier = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join(LINUX.courier_binary());
    let courier = std::fs::read(&courier)
        .unwrap_or_else(|e| panic!("run `make build-courier` first: {}: {e}", courier.display()));
    let backend = ContainerBackend::new(docker.clone(), &LINUX, courier.into());
    (docker, backend)
}

/// Checks that the reclaim `entries` hold a case scope, and that nothing named after it is left.
async fn assert_scope_left_nothing(docker: &Docker, entries: &[ReclaimEntry]) {
    let scope = entries
        .iter()
        .find_map(|entry| entry.resource.strip_prefix("case_scope:"))
        .expect("the ledger should hold the case scope");
    assert_nothing_left(docker, scope).await;
}

/// Plans `config` for Linux, prepares its scope, runs `body`, and reclaims the scope, even when `body` panics.
///
/// Returns the reclaim entries.
async fn in_case<F, Fut>(config: &str, body: F) -> Vec<ReclaimEntry>
where
    F: FnOnce(Case) -> Fut,
    Fut: Future<Output = ()>,
{
    let plan = plan_linux(config, &Registry::default());
    let (docker, backend) = backend().await;
    let ledger = ResourceLedger::default();
    let outcome = AssertUnwindSafe(async {
        let cancel = CancellationToken::new();
        let containers = backend
            .prepare(&ledger, &cancel, deadline())
            .await
            .expect("the scope should be prepared");
        body(Case {
            plan,
            containers,
            cancel,
        })
        .await;
    })
    .catch_unwind()
    .await;
    let (entries, _) = ledger.reclaim(deadline()).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    assert_scope_left_nothing(&docker, &entries).await;
    entries
}

fn is_not_found(error: &Error) -> bool {
    matches!(error, Error::DockerResponseServerError { status_code: 404, .. })
}

/// Checks that no container, network or volume named after the scope `name` is left.
async fn assert_nothing_left(docker: &Docker, name: &str) {
    let filters: HashMap<&str, Vec<&str>> = HashMap::from([("name", vec![name])]);
    let options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    let containers = docker.list_containers(Some(options)).await.unwrap();
    assert!(containers.is_empty(), "{containers:?}");
    let network = docker.inspect_network(name, None).await;
    assert!(network.as_ref().is_err_and(is_not_found), "{network:?}");
    let volume = docker.inspect_volume(name).await;
    assert!(volume.as_ref().is_err_and(is_not_found), "{volume:?}");
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_failed_start_leaves_no_container_network_or_volume() {
    let config = r#"
timeout: 1m
subject_os: [linux]
participants:
  app:
    subject: true
    image: "{image}"
    command: [/no/such/program]
procedure: []
"#;
    let entries = in_case(config, |case| async move {
        let values = Values::of_plan(&case.plan.participants);
        let mut app = case
            .containers
            .create("app", &case.plan.participants["app"], &values, &case.cancel, deadline())
            .await
            .unwrap();
        app.start(&case.cancel, deadline())
            .await
            .expect_err("a missing program should fail the start");
    })
    .await;
    let results: Vec<ReclaimResult> = entries.iter().map(|entry| entry.result).collect();
    assert_eq!(results, [ReclaimResult::Reclaimed]);
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_cancelled_case_leaves_no_container_network_or_volume() {
    let config = r#"
timeout: 1m
subject_os: [linux]
participants:
  app:
    subject: true
    image: "{image}"
    command: [sleep, "600"]
  late:
    image: "{image}"
    command: [sleep, "600"]
procedure: []
"#;
    let entries = in_case(config, |case| async move {
        let values = Values::of_plan(&case.plan.participants);
        let create = |name: &'static str| {
            case.containers
                .create(name, &case.plan.participants[name], &values, &case.cancel, deadline())
        };
        let mut app = create("app").await.unwrap();
        app.start(&case.cancel, deadline()).await.unwrap();

        case.cancel.cancel();
        let error = create("late")
            .await
            .expect_err("a cancelled case should create nothing more");
        assert!(format!("{error:#}").contains("Cancelled"), "{error:#}");
    })
    .await;
    // The ledger marks the scope unsettled whenever cancellation stops a create, even one that never reached Docker,
    // as here, so the scope is unsettled though nothing is left.
    let results: Vec<ReclaimResult> = entries.iter().map(|entry| entry.result).collect();
    assert_eq!(results, [ReclaimResult::Unsettled]);
}

/// A step for the tests of the lifecycle: waits for a process to exit, checks its exit code, then waits for a line of
/// its output, which may arrive after the exit.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExitsWithLine {
    on: String,
    process: String,
    code: i64,
    line: String,
    /// Cancelled as the step starts, for a test that cancels its case in a step.
    #[serde(skip)]
    cancel: Option<CancellationToken>,
}

struct ExitsWithLineFactory {
    /// What each step it builds cancels as it starts, if anything.
    cancel: Option<CancellationToken>,
}

impl StepFactory for ExitsWithLineFactory {
    fn name(&self) -> &'static str {
        "exits_with_line"
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let mut step = fields.decode::<ExitsWithLine>()?;
        step.cancel = self.cancel.clone();
        Ok(Box::new(step))
    }
}

#[async_trait]
impl Step for ExitsWithLine {
    fn name(&self) -> &'static str {
        "exits_with_line"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Process(self.on.clone(), self.process.clone())]
    }

    fn budget(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        let report = |status, message: String| StepReport { status, message };
        let handle = match cx.handle(&self.on) {
            Ok(handle) => handle,
            Err(e) => return report(StepStatus::Errored, e.to_string()),
        };
        let limit = cx.deadline.saturating_duration_since(Instant::now());
        match handle.wait_for_exit(&self.process, limit).await {
            Ok(ProcessExit::Exited(status)) if status.code == self.code => {}
            Ok(ProcessExit::Undetermined(why)) => return report(StepStatus::Errored, why),
            Ok(exit) => return report(StepStatus::Failed, format!("{exit:?}")),
            Err(e) => return report(StepStatus::Errored, e.to_string()),
        }
        let mut output = match handle.output(&self.process) {
            Ok(output) => output,
            Err(e) => return report(StepStatus::Errored, e.to_string()),
        };
        let found = tokio::time::timeout_at(cx.deadline, output.wait_for(|lines| lines.contains(&self.line)))
            .await
            .is_ok_and(|found| found.is_ok());
        if found {
            report(StepStatus::Passed, String::new())
        } else {
            report(StepStatus::Failed, format!("{:?}", *output.borrow()))
        }
    }
}

/// A case the lifecycle ran, and what the runner made of it.
struct Lived {
    ran: Ran,
    entries: Vec<ReclaimEntry>,
    logs: tempfile::TempDir,
}

impl Lived {
    /// How long the phase `name` of the lifecycle took.
    fn phase(&self, name: &str) -> Duration {
        let timings = &self.ran.result.phase_timings;
        timings
            .iter()
            .find(|timing| timing.phase == name)
            .unwrap_or_else(|| panic!("no phase '{name}': {timings:?}"))
            .duration
    }
}

/// Plans `config` for Linux, with the test step registered, and runs it through the lifecycle as the runner does. The
/// test step cancels the case as it starts if `cancel_in_a_step` is set. Then stops the case's tasks, reclaims, and
/// checks that nothing of the case scope is left, even when the lifecycle panics.
async fn run_case(config: &str, cancel_in_a_step: bool) -> Lived {
    let cancel = CancellationToken::new();
    let mut registry = Registry::default();
    registry.add_step(Box::new(ExitsWithLineFactory {
        cancel: cancel_in_a_step.then(|| cancel.clone()),
    }));
    let plan = plan_linux(config, &registry);
    let (docker, containers) = backend().await;
    let logs = tempfile::tempdir().unwrap();
    let (ledger, record, phases) = (
        ResourceLedger::default(),
        CaseRecord::default(),
        PhaseTracker::default(),
    );
    let tasks = CaseTasks::new(&cancel);
    let run = CaseRun {
        containers: &containers,
        ledger: &ledger,
        tasks: &tasks,
        cancel: &cancel,
        deadline: Deadline::after(Budget::Case, BUDGET),
        log_dir: logs.path(),
        phases: &phases,
        record: &record,
    };
    let outcome = AssertUnwindSafe(lifecycle::run(&plan, &run)).catch_unwind().await;
    tasks.stop_all(Instant::now() + TASK_STOP_TIME).await;
    let (entries, _) = ledger.reclaim(deadline()).await;
    let ran = outcome.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    assert_scope_left_nothing(&docker, &entries).await;
    Lived { ran, entries, logs }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_case_runs_through_every_phase_of_the_lifecycle() {
    // The server listens a second after it starts, and the worker starts only once it accepts connections. The worker
    // reaches the server, writes its output and its exit record as a supervisor would, leaves two files to collect,
    // and exits right after its last line, which only the file then holds.
    let config = r#"
timeout: 1m
subject_os: [linux]
participants:
  server:
    subject: true
    image: "{image}"
    command: [sh, -c, "sleep 1; echo listening; exec nc -lk -p 8080 -e true"]
    endpoints:
      api: {tcp: 8080}
  worker:
    image: "{image}"
    command:
      - sh
      - -c
      - |
        nc -z "$SERVER" 8080 && echo reached the server >> "$PANORAMIC_LOG_DIR/job.log"
        echo kept > /tmp/kept
        echo dropped > /tmp/dropped
        echo "3 0 $(date +%s).5" >> "$PANORAMIC_EXIT_DIR/job"
        sleep 1
        echo done >> "$PANORAMIC_LOG_DIR/job.log"
    env:
      SERVER: "{{ self.connections.server.host }}"
    connect: {server: server.api}
    processes:
      - {name: job, exit: {record: job}, output: {file: job.log}}
    collect:
      - {path: /tmp/kept, name: kept, keep: always}
      - {path: /tmp/dropped, name: dropped, keep: on_failure}
procedure:
  - {step: exits_with_line, on: worker, process: job, code: 3, line: done}
"#;
    let lived = run_case(config, false).await;
    let result = &lived.ran.result;
    assert_eq!(result.outcome, TestOutcome::Passed, "{result:?}");
    let phases: Vec<&str> = result
        .phase_timings
        .iter()
        .map(|timing| timing.phase.as_str())
        .collect();
    assert_eq!(phases, ["prepare", "create", "start", "procedure", "observe"]);

    let processes = &lived.ran.processes;
    let exited = ProcessExit::Exited(ExitStatus { code: 3, signal: None });
    assert_eq!(processes["worker"]["job"].exit, exited);
    assert_eq!(processes["server"]["main"].exit, ProcessExit::Running);
    let job = read(&processes["worker"]["job"].output);
    assert!(job.contains("reached the server\ndone\n"), "{job}");
    assert!(read(&processes["server"]["main"].output).contains("listening"));
    assert!(lived.logs.path().join("worker").join(CONTAINER_LOG).exists());

    // The case passed, so only the file kept always is left.
    let collected: Vec<(&Path, Keep)> = lived
        .ran
        .collected
        .iter()
        .map(|file| (file.path.as_path(), file.keep))
        .collect();
    let kept = lived.logs.path().join("worker/collected/kept");
    assert!(
        matches!(collected[..], [(path, Keep::Always)] if path == kept),
        "{collected:?}"
    );
    assert_eq!(read(&kept), "kept\n");
    assert!(!lived.logs.path().join("worker/collected/dropped").exists());

    let results: Vec<ReclaimResult> = lived.entries.iter().map(|entry| entry.result).collect();
    assert_eq!(results, [ReclaimResult::Reclaimed]);
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_gate_fails_when_its_server_exits_and_the_failed_start_leaves_nothing_behind() {
    let config = r#"
timeout: 1m
subject_os: [linux]
participants:
  server:
    subject: true
    image: "{image}"
    command: [sh, -c, "echo going away; exit 4"]
    endpoints:
      api: {tcp: 8080}
  client:
    image: "{image}"
    command: [sleep, "600"]
    env:
      SERVER: "{{ self.connections.server.host }}"
    connect: {server: server.api}
procedure: []
"#;
    let lived = run_case(config, false).await;
    let result = &lived.ran.result;
    assert_eq!(result.outcome, TestOutcome::Errored, "{result:?}");
    let error = result.error.as_ref().expect("a failed start is a harness error");
    assert_eq!(error.kind, ErrorKind::Setup);
    assert!(
        error.message.contains("'server' exited with code 4"),
        "{}",
        error.message
    );
    // Well inside the gate's own minute.
    assert!(lived.phase("start") < Duration::from_secs(30), "{result:?}");

    // The server started, so its output was captured, and the client never started.
    let processes = &lived.ran.processes;
    assert!(read(&processes["server"]["main"].output).contains("going away"));
    assert!(!processes.contains_key("client"));
    let results: Vec<ReclaimResult> = lived.entries.iter().map(|entry| entry.result).collect();
    assert_eq!(results, [ReclaimResult::Reclaimed]);
}

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn a_case_cancelled_in_a_step_leaves_nothing_behind() {
    let config = r#"
timeout: 1m
subject_os: [linux]
participants:
  app:
    subject: true
    image: "{image}"
    command: [sleep, "600"]
procedure:
  - {step: exits_with_line, on: app, process: main, code: 0, line: never}
"#;
    let lived = run_case(config, true).await;
    let steps: Vec<StepStatus> = lived
        .ran
        .result
        .assertion_results
        .iter()
        .map(|step| step.status)
        .collect();
    assert_eq!(steps, [StepStatus::Interrupted]);
    // Well inside the step's own budget.
    assert!(
        lived.phase("procedure") < Duration::from_secs(10),
        "{:?}",
        lived.ran.result
    );
    let results: Vec<ReclaimResult> = lived.entries.iter().map(|entry| entry.result).collect();
    assert_eq!(results, [ReclaimResult::Reclaimed]);
}
