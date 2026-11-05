//! Test execution shared by TUI and plain output.
//!
//! The runner sends progress and results to the run collector as events.

use std::num::NonZeroUsize;
use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::stream::{self, StreamExt as _};
use panoramic_core::deadline::{Budget, Deadline};
use panoramic_core::events::{EventSender, TestEvent};
use panoramic_core::ledger::RECLAIM_TIME;
use panoramic_core::lifecycle::{self, CaseControl, NativeInputs};
use panoramic_core::machine_output::RESULT_LOG_FILE_NAME;
use panoramic_core::participant::ParticipantType;
use panoramic_core::plan::CasePlan;
use panoramic_core::record::{ActiveStep, CaseRecord, StepStatus};
use panoramic_core::result::{
    ErrorKind, Issue, IssueKind, PhaseTiming, PhaseTracker, StopCause, StoppedBy, TestResult, TimeoutAttribution,
    UNKNOWN_PHASE,
};
use panoramic_core::tasks::TASK_STOP_TIME;
use saluki_error::{generic_error, GenericError};
use tokio::pin;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::config::{case_dir, needs_pod};
use crate::test::{Test, TestContext};

/// How long a cancelled or timed-out test has to return its result before the runner drops it.
const GRACE_TIME: Duration = Duration::from_secs(30);

pub(crate) struct RunArgs {
    /// The number of tests to run in parallel.
    parallelism: NonZeroUsize,

    /// Whether to stop execution at the first failure.
    fail_fast: bool,

    /// Reports run progress and results to the collector.
    event_sender: EventSender,

    /// Requests run cancellation.
    ///
    /// The consume loop cancels this on an interrupt. Active tests get `GRACE_TIME` to return.
    cancel_all: CancellationToken,
}

impl RunArgs {
    pub(crate) fn new(cancel_all: CancellationToken, event_sender: EventSender) -> Self {
        Self {
            parallelism: NonZeroUsize::MIN,
            fail_fast: false,
            event_sender,
            cancel_all,
        }
    }

    pub(crate) fn with_parallelism(mut self, parallelism: NonZeroUsize) -> Self {
        self.parallelism = parallelism;
        self
    }

    pub(crate) fn with_fail_fast(mut self, fail_fast: bool) -> Self {
        self.fail_fast = fail_fast;
        self
    }
}

/// Owns the set of registered tests and orchestrates their execution.
///
/// Handles parallelism, timeout enforcement, cancellation propagation, log directory creation,
/// and event dispatch. Case bodies run through the [`Test`] interface.
pub(crate) struct Runner {
    tests: Vec<BuiltCase>,
    log_base_dir: PathBuf,
    kind_ready: Option<crate::test::KindReadyReceiver>,
}

/// A compiled case paired with its executable body.
pub(crate) struct BuiltCase {
    /// Case identity, placement, and procedure.
    pub(crate) plan: CasePlan,
    /// Executes the case or reports why it cannot run.
    body: Box<dyn Test>,
    requires_kind: bool,
}

impl BuiltCase {
    /// Pairs a runnable body with its plan and cluster prerequisite.
    pub(crate) fn new(plan: CasePlan, body: Box<dyn Test>) -> Self {
        Self {
            requires_kind: needs_pod(&plan),
            plan,
            body,
        }
    }

    /// Reports an unavailable realization without waiting for cluster setup.
    pub(crate) fn unprovidable(plan: CasePlan, message: String) -> Self {
        Self {
            plan,
            body: Box::new(Unprovidable { message }),
            requires_kind: false,
        }
    }
}

/// Runs a case's participants from verified artifacts.
pub(crate) struct NativeTest {
    pub(crate) inputs: Arc<NativeInputs>,
    pub(crate) types: &'static [&'static dyn ParticipantType],
}

#[async_trait]
impl Test for NativeTest {
    async fn run(&self, plan: &CasePlan, tctx: TestContext) -> TestResult {
        let control = CaseControl {
            observations: tctx.observations.clone(),
            record: tctx.record.clone(),
            deadline: tctx.deadline,
            cancel: tctx.test_cancel_token(),
            interrupt: tctx.interrupt.clone(),
            phases: tctx.phases.clone(),
            ledger: tctx.ledger.clone(),
            tasks: tctx.tasks.clone(),
            log_dir: tctx.log_dir().to_owned(),
        };
        lifecycle::run(plan.participants(), self.types, &self.inputs, &control).await
    }
}

/// Reports a realization that the host cannot provide as a setup error.
struct Unprovidable {
    message: String,
}

#[async_trait]
impl Test for Unprovidable {
    async fn run(&self, _plan: &CasePlan, _tctx: TestContext) -> TestResult {
        TestResult::errored("", ErrorKind::Setup, &self.message, Duration::ZERO, vec![])
    }
}

impl Runner {
    pub(crate) fn new(log_base_dir: impl Into<PathBuf>) -> Self {
        Self {
            tests: Vec::new(),
            log_base_dir: log_base_dir.into(),
            kind_ready: None,
        }
    }

    pub(crate) fn with_kind_ready(mut self, rx: crate::test::KindReadyReceiver) -> Self {
        self.kind_ready = Some(rx);
        self
    }

    /// Register a test. Returns an error if the test name is a duplicate.
    pub(crate) fn register(&mut self, built: BuiltCase) -> Result<(), GenericError> {
        // Inefficient but it probably does not matter unless we have thousands of tests.
        if self
            .tests
            .iter()
            .any(|existing| existing.plan.case().name() == built.plan.case().name())
        {
            return Err(generic_error!(
                "A test named '{}' already exists in the registry",
                built.plan.case().name()
            ));
        }
        self.tests.push(built);
        Ok(())
    }

    /// Runs the registered tests, reporting progress and results as events.
    pub(crate) async fn run_tests(&self, args: RunArgs) {
        let RunArgs {
            parallelism,
            fail_fast,
            event_sender,
            cancel_all,
        } = args;

        let tests = &self.tests;

        let _ = event_sender.send(TestEvent::RunStarted {
            total_tests: tests.len(),
        });

        let parallelism = parallelism.get();
        let semaphore = Arc::new(Semaphore::new(parallelism));

        if fail_fast {
            self.run_fail_fast(tests, &semaphore, &event_sender, &cancel_all).await;
        } else {
            self.run_parallel(tests, parallelism, &semaphore, &event_sender, &cancel_all)
                .await;
        }
    }

    async fn run_fail_fast(
        &self, tests: &[BuiltCase], semaphore: &Semaphore, event_sender: &EventSender, cancel_all: &CancellationToken,
    ) {
        for built in tests {
            if cancel_all.is_cancelled() {
                break;
            }

            let kind_ready = if built.requires_kind {
                self.kind_ready.clone()
            } else {
                None
            };
            // Kind tests wait for cluster readiness before acquiring a concurrency slot.
            if let Some(mut rx) = kind_ready.clone() {
                loop {
                    if rx.borrow().is_some() {
                        break;
                    }
                    tokio::select! {
                        _ = cancel_all.cancelled() => break,
                        result = rx.changed() => { if result.is_err() { break; } }
                    }
                }
            }

            let _permit = semaphore.acquire().await.unwrap();
            let result = Self::run_one(built, event_sender, cancel_all, self.log_base_dir.clone(), kind_ready).await;

            if !result.outcome.is_passed() {
                break;
            }
        }
    }

    async fn run_parallel(
        &self, tests: &[BuiltCase], parallelism: usize, semaphore: &Arc<Semaphore>, event_sender: &EventSender,
        cancel_all: &CancellationToken,
    ) {
        let mut futures = stream::FuturesUnordered::new();

        for built in tests {
            let semaphore = semaphore.clone();
            let cancel = cancel_all.clone();
            let log_base_dir = self.log_base_dir.clone();
            let mut kind_ready = if built.requires_kind {
                self.kind_ready.clone()
            } else {
                None
            };
            futures.push(async move {
                if cancel.is_cancelled() {
                    return;
                }

                // Kind tests wait for cluster readiness before acquiring a concurrency slot
                // so they don't starve Docker tests while the cluster is being set up.
                if let Some(ref mut rx) = kind_ready {
                    loop {
                        if rx.borrow().is_some() {
                            break;
                        }
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            result = rx.changed() => { if result.is_err() { break; } }
                        }
                    }
                }

                if cancel.is_cancelled() {
                    return;
                }

                let _permit = semaphore.acquire().await.unwrap();

                if cancel.is_cancelled() {
                    return;
                }

                Self::run_one(built, event_sender, &cancel, log_base_dir, kind_ready).await;
            });

            while futures.len() >= parallelism {
                futures.next().await;
            }
        }

        while futures.next().await.is_some() {}
    }

    async fn run_one(
        built: &BuiltCase, event_sender: &EventSender, cancel_all: &CancellationToken, log_base_dir: PathBuf,
        kind_ready: Option<crate::test::KindReadyReceiver>,
    ) -> TestResult {
        let plan = &built.plan;
        let case = plan.case();
        let name = case.name().to_string();
        let timeout = case.timeout();
        let case_path = case_dir(case);
        let started = Instant::now();

        // A test-specific token lets a timeout cancel this test without cancelling the run.
        let test_cancel = CancellationToken::new();

        // Create a directory for the test to write logs into and pass it into the test context.
        let log_dir = log_base_dir.join(case.suite()).join(case.name());
        let setup_failure = |message: String| {
            let mut result = TestResult::errored(name.clone(), ErrorKind::Setup, message, started.elapsed(), vec![]);
            result.case_path = Some(case_path.clone());
            result.layout = Some(plan.layout_entries());

            // Report setup failures through events so the collector accounts for the selected test.
            let _ = event_sender.send(TestEvent::TestStarted { name: name.clone() });
            let _ = event_sender.send(TestEvent::TestCompleted {
                result: Box::new(result.clone()),
                log_dir: log_dir.clone(),
            });
            result
        };
        if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
            return setup_failure(format!(
                "Test failed to start due to an error during setup. Error creating log directory at {}: {e}",
                log_dir.display()
            ));
        }
        let deadline = Deadline::after(Budget::Case, timeout);
        let tctx = TestContext::new(test_cancel.clone(), cancel_all.clone(), deadline, log_dir.clone());

        let _ = event_sender.send(TestEvent::TestStarted { name: name.clone() });

        let phases = tctx.phases.clone();
        let record = tctx.record.clone();
        let ledger = tctx.ledger.clone();
        let tasks = tctx.tasks.clone();
        let observations = tctx.observations.clone();

        let (mut result, observed) = {
            let run_fut = async {
                if built.requires_kind {
                    let phase = phases.enter("kind_setup");
                    let status = kind_ready.as_ref().and_then(|rx| rx.borrow().clone());
                    match status {
                        Some(Ok(())) => {
                            phase.finish();
                        }
                        status => {
                            let message = match status {
                                Some(Err(error)) => format!("Kind cluster setup failed: {error}"),
                                _ => "Kind cluster setup did not complete".to_string(),
                            };
                            return TestResult::errored(
                                "",
                                ErrorKind::Setup,
                                message,
                                started.elapsed(),
                                phase.finish_and_collect(),
                            );
                        }
                    }
                }
                built.body.run(plan, tctx).await
            };
            pin!(run_fut);

            // On timeout or run cancellation, cancel the test and allow GRACE_TIME for it to return.
            //
            // The case timer wins ties with the body so an expired deadline is reported, even after a verdict.
            // The body precedes `cancel_all` to preserve a result ready when external cancellation arrives.
            // Snapshot observations before cancellation closes their watch channels.
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline.at) => {
                    let stop = Stop::snapshot(StoppedBy::CaseDeadline, &phases, &record);
                    let observed = observations.snapshot();
                    test_cancel.cancel();
                    let returned = tokio::time::timeout(GRACE_TIME, &mut run_fut).await.ok();
                    (stop.settle(returned, &name, timeout, started.elapsed(), &phases, &record), Some(observed))
                }
                r = &mut run_fut => (r, None),
                _ = cancel_all.cancelled() => {
                    let stop = Stop::snapshot(StoppedBy::Signal, &phases, &record);
                    let observed = observations.snapshot();
                    test_cancel.cancel();
                    let returned = tokio::time::timeout(GRACE_TIME, &mut run_fut).await.ok();
                    (stop.settle(returned, &name, timeout, started.elapsed(), &phases, &record), Some(observed))
                }
            }
            // Drop the body before cleanup; its tracked background tasks are handled below.
        };

        result.participants = observed.unwrap_or_else(|| observations.snapshot());
        observations.record_collections(&mut result);

        // Cleanup has its own budget because the test token may already be cancelled. Give tracked tasks time to stop
        // and mark interrupted creations before reclaim reads the ledger.
        let reclaim_started = Instant::now();
        let deadline = tokio::time::Instant::now() + RECLAIM_TIME;
        let running = tasks.stop_all(tokio::time::Instant::now() + TASK_STOP_TIME).await;
        if running > 0 {
            warn!(
                test = %name,
                running,
                "Background tasks were still tracked after the {:?} stop budget expired.",
                TASK_STOP_TIME
            );
            result.issues.push(Issue {
                kind: IssueKind::TasksStillRunning,
                message: format!(
                    "{running} background task(s) still tracked after the {TASK_STOP_TIME:?} stop budget expired."
                ),
            });
        }
        let (reclaim, issues) = ledger.reclaim(deadline).await;
        if !reclaim.is_empty() {
            result.phase_timings.push(PhaseTiming {
                phase: "reclaim".to_string(),
                duration: reclaim_started.elapsed(),
            });
            result.duration = started.elapsed();
        }
        result.reclaim = reclaim;
        result.issues.extend(issues);

        // Stamp the case identity, layout and artifact provenance before reporting the result.
        result.name = case.name().to_string();
        result.case_path = Some(case_path.clone());
        result.layout = Some(plan.layout_entries());
        result.log_dir = Some(log_dir.clone());

        // Retention can merge sidecars or delete captures; report only artifacts that remain on disk.
        crate::traffic::finalize(&log_dir, result.outcome);
        result.collected.retain(|collected| collected.path.exists());
        write_result_log(&result, &log_dir);
        panoramic_core::machine_output::write_test_report(&result, &log_dir);

        let _ = event_sender.send(TestEvent::TestCompleted {
            result: Box::new(result.clone()),
            log_dir,
        });

        result
    }
}

/// Test state captured before cancellation can change step statuses or advance the phase.
struct Stop {
    by: StoppedBy,
    /// The phase the test was in.
    active_phase: Option<String>,
    /// Whether the test had reached its verdict.
    decided: bool,
    /// The procedure steps that were running.
    active: Vec<ActiveStep>,
}

impl Stop {
    fn snapshot(by: StoppedBy, phases: &PhaseTracker, record: &CaseRecord) -> Self {
        Self {
            by,
            active_phase: phases.active(),
            decided: record.decided(),
            active: record.running(),
        }
    }

    /// Uses the result returned during grace, falling back to the case record when grace expires.
    ///
    /// Preserves a verdict reached before cancellation, recording any expired deadline or cleanup grace as an issue.
    /// Otherwise, the stop determines the outcome: a timeout for the case deadline, an internal error for a signal.
    fn settle(
        self, returned: Option<TestResult>, name: &str, timeout: Duration, elapsed: Duration, phases: &PhaseTracker,
        record: &CaseRecord,
    ) -> TestResult {
        let grace_phase = returned
            .is_none()
            .then(|| phases.active().unwrap_or_else(|| UNKNOWN_PHASE.to_string()));
        let mut result = returned.unwrap_or_else(|| record.result(name, elapsed, phases.completed()));
        let grace_expired = grace_phase.as_ref().map(|phase| Issue {
            kind: IssueKind::CleanupGraceExpired,
            message: format!("cleanup grace {:?} expired in {}", GRACE_TIME, phase),
        });

        if self.decided {
            if self.by == StoppedBy::CaseDeadline {
                result.issues.push(Issue {
                    kind: IssueKind::PostVerdictDeadline,
                    message: format!(
                        "case budget {:?} expired in {} after the verdict",
                        timeout,
                        self.active_phase.as_deref().unwrap_or(UNKNOWN_PHASE)
                    ),
                });
            }
            result.issues.extend(grace_expired);
            return result;
        }

        result.cause = Some(StopCause {
            by: self.by,
            active: self.active,
        });
        match (self.by, grace_phase) {
            (StoppedBy::CaseDeadline, None) => result
                .with_harness_error(ErrorKind::Timeout, format!("Test timed out after {:?}.", timeout))
                .with_timeout_attribution(TimeoutAttribution::test_deadline(timeout, self.active_phase)),
            (StoppedBy::CaseDeadline, Some(grace_phase)) => result
                .with_harness_error(
                    ErrorKind::Timeout,
                    format!(
                        "Test timed out after {:?} and did not return within {:?}.",
                        timeout, GRACE_TIME
                    ),
                )
                .with_timeout_attribution(TimeoutAttribution::cleanup_grace(
                    GRACE_TIME,
                    timeout,
                    self.active_phase,
                    Some(grace_phase),
                )),
            (StoppedBy::Signal, _) => {
                result.issues.extend(grace_expired);
                result.with_harness_error(ErrorKind::Internal, "Test was cancelled.")
            }
        }
    }
}

fn write_result_log(result: &TestResult, dir: impl AsRef<Path>) {
    let dir = dir.as_ref();

    let path = dir.join(RESULT_LOG_FILE_NAME);
    let mut f = match std::fs::File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to create result log file.");
            return;
        }
    };

    let _ = writeln!(f, "{} {} ({:.2?})", result.label(), result.name, result.duration);
    for line in result.cause_lines() {
        let _ = writeln!(f, "{}", line);
    }

    let ran: Vec<_> = result
        .assertion_results
        .iter()
        .enumerate()
        .filter(|(_, step)| step.status != StepStatus::NotRun)
        .collect();
    if !ran.is_empty() {
        let _ = writeln!(f);
        let _ = writeln!(f, "Assertions:");
        for (i, assertion) in ran {
            let indicator = if assertion.status == StepStatus::Passed {
                "+"
            } else {
                "-"
            };
            let _ = writeln!(f, "  {} {} ({:.2?})", indicator, assertion.name, assertion.duration);
            let full_details = result.assertion_details.get(i).map(|d| d.as_slice()).unwrap_or(&[]);
            if !full_details.is_empty() {
                for line in full_details {
                    let _ = writeln!(f, "    {}", line);
                }
            } else {
                for line in assertion.message.lines() {
                    let _ = writeln!(f, "    {}", line);
                }
            }
        }
    }

    if !result.phase_timings.is_empty() {
        let _ = writeln!(f);
        let _ = writeln!(f, "Phase timings:");
        for phase in &result.phase_timings {
            let _ = writeln!(f, "  {} ({:.2?})", phase.phase, phase.duration);
        }
    }
}

/// Scripted tests for exercising [`Runner`].
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;

    use panoramic_core::artifact::{ArtifactKind, ArtifactManifest, ArtifactRole};
    use panoramic_core::case::{CaseConfig, CaseLoader, Registries};
    use panoramic_core::layout::{Os, Realization, RealizationKind};
    use panoramic_core::plan::Planner;
    use panoramic_core::record::{AssertionResult, AssertionStatus};
    use panoramic_core::{
        case::{ProcedureNode, StepNode},
        procedure::{Member, Procedure},
        record::{StepReport, Verdict},
        step::{
            test_support::{run_step_reaching, run_step_with, FakeParticipant},
            StepFactory, StepFields,
        },
    };
    use tokio::sync::{mpsc, Notify};

    use super::*;

    pub(crate) struct PodTest;

    impl ParticipantType for PodTest {
        fn name(&self) -> &'static str {
            "pod-test"
        }

        fn supports(&self) -> &'static [(Os, RealizationKind)] {
            &[
                (Os::Linux, RealizationKind::KubernetesPod),
                (Os::Linux, RealizationKind::LinuxContainer),
            ]
        }

        fn artifacts(&self, realization: Realization) -> &'static [ArtifactRole] {
            if realization.kind == RealizationKind::KubernetesPod {
                &[
                    ArtifactRole {
                        name: "pod-image",
                        kind: ArtifactKind::Image { os: Os::Linux },
                        local_default: "pod:default",
                    },
                    ArtifactRole {
                        name: "pod-binary",
                        kind: ArtifactKind::Binary,
                        local_default: "pod-helper",
                    },
                ]
            } else {
                &[ArtifactRole {
                    name: "container-image",
                    kind: ArtifactKind::Image { os: Os::Linux },
                    local_default: "container:default",
                }]
            }
        }

        fn processes(&self) -> &'static [&'static str] {
            &[]
        }
    }

    pub(crate) fn native_pod_plan() -> CasePlan {
        let registries = Registries {
            participant_types: &[&PodTest],
            ..crate::REGISTRIES
        };
        let case = CaseLoader::new(registries)
            .load_content(
                "integration",
                r#"
timeout: 1m
subject_os: [linux]
participants:
  pod: {type: pod-test, subject: true, realization: {kind: kubernetes-pod}}
  peer: {type: pod-test}
  container: {type: pod-test, realization: {kind: linux-container}}
procedure: [{step: file_contains, on: pod, path: /ready, pattern: ready, timeout: 1s}]
"#,
                "native-pod",
                Path::new("/cases/native-pod/config.yaml"),
            )
            .unwrap()
            .remove(0);
        let manifest = ArtifactManifest::new(
            "test",
            BTreeMap::from([
                ("pod-image".into(), "pod:bound".into()),
                ("container-image".into(), "container:bound".into()),
            ]),
        )
        .unwrap();
        Planner::new(registries, manifest)
            .unwrap()
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap()
    }

    /// Runs a native case through artifact verification, checks its report and confirms scope removal.
    pub(crate) async fn run_native_case(
        participant_types: &'static [&'static dyn ParticipantType], steps: &[&dyn panoramic_core::step::StepFactory],
        case_json: serde_json::Value, mut bindings: BTreeMap<String, String>, cancel: CancellationToken,
    ) -> (tempfile::TempDir, serde_json::Value) {
        use clap::Parser as _;
        use panoramic_core::{
            artifact::{ArtifactKind, ArtifactManifest},
            backend::{self, RunBackends, FIXUP_IMAGE},
            case::CaseLoader,
            layout::RealizationKind,
            plan::Planner,
        };
        use serde_json::{json, Value};

        use super::report_tests::assert_scope_removed;
        use crate::{
            cli::{Cli, Command},
            console::Console,
            run_command,
        };

        let registries = panoramic_core::case::Registries {
            participant_types,
            steps,
        };
        let logs = tempfile::tempdir().unwrap();
        let participants = case_json["participants"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let case = CaseLoader::new(registries)
            .load_content(
                "cases",
                &case_json.to_string(),
                "native",
                &logs.path().join("cases/native/config.yaml"),
            )
            .unwrap()
            .remove(0);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
        let courier = backend::artifacts(RealizationKind::LinuxContainer)
            .iter()
            .find(|role| matches!(role.kind, ArtifactKind::ContainerBinary { .. }))
            .unwrap();
        let image = std::env::var("PANORAMIC_TEST_IMAGE").unwrap_or_else(|_| "alpine:latest".into());
        bindings.entry(FIXUP_IMAGE.name.into()).or_insert(image);
        bindings
            .entry(courier.name.into())
            .or_insert_with(|| root.join(courier.local_default).to_str().unwrap().into());
        let manifest = ArtifactManifest::new("runner tests", bindings).unwrap();
        let path = logs.path().join("artifacts.toml");
        std::fs::write(&path, manifest.render()).unwrap();
        let plan = Planner::new(registries, ArtifactManifest::load(&path).unwrap())
            .unwrap()
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap();
        let scope = panoramic_core::plan::test_support::scope_id(&plan);
        eprintln!("Native runner test scope: {scope}");
        let limit = plan.case().timeout() + Duration::from_secs(30);
        let Command::Run(cmd) = Cli::try_parse_from([
            "panoramic",
            "run",
            "--subject-os",
            "linux",
            "--no-tui",
            "--artifacts",
            path.to_str().unwrap(),
        ])
        .unwrap()
        .command
        else {
            panic!("expected run")
        };
        let (verified, native) = run_command::prepare_artifacts(
            Arc::new(RunBackends::default()),
            &cmd,
            &Console { use_tui: false },
            Os::Linux,
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        let verified = serde_json::to_value(verified.unwrap()).unwrap();
        let verified = verified.as_array().unwrap();
        assert_eq!(verified.len(), plan.artifacts().len());
        for binding in plan.artifacts() {
            assert!(verified.iter().any(|artifact| artifact["role"] == binding.role.name
                && artifact["identity"]
                    .as_str()
                    .is_some_and(|identity| !identity.is_empty())));
        }
        let mut runner = Runner::new(logs.path());
        runner
            .register(BuiltCase::new(
                plan,
                Box::new(NativeTest {
                    inputs: Arc::new(native.unwrap()),
                    types: participant_types,
                }),
            ))
            .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(limit, runner.run_tests(RunArgs::new(cancel, tx)))
            .await
            .expect("the runner must return after its cleanup budgets");
        let mut completed = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let TestEvent::TestCompleted { result, .. } = event {
                completed.push(result);
            }
        }
        assert_scope_removed(&scope).await;
        assert_eq!(completed.len(), 1);
        let result = &completed[0];
        let report: Value =
            serde_json::from_slice(&std::fs::read(result.log_dir.as_ref().unwrap().join("result.json")).unwrap())
                .unwrap();
        assert_eq!(report["schema_version"], 3);
        assert_eq!(
            report["participants"],
            serde_json::to_value(&result.participants).unwrap()
        );
        assert_eq!(report["issues"], json!([]), "{report:#}");
        let reclaim = report["reclaim"].as_array().unwrap();
        assert_eq!(reclaim.len(), participants.len() + 1, "{report:#}");
        // These fixtures create in name order; reclaim runs in reverse create order.
        for (entry, name) in reclaim.iter().zip(participants.iter().rev()) {
            assert_eq!(
                *entry,
                json!({"resource": format!("participant:{name}"), "participant": name, "result": "reclaimed"})
            );
        }
        assert_eq!(
            reclaim[participants.len()],
            json!({"resource": format!("isolation_group:{scope}"), "result": "reclaimed"})
        );
        (logs, report)
    }

    /// What a [`ScriptedTest`] does once it has entered its phase.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Behavior {
        /// Passes one assertion.
        Pass,
        /// Fails one assertion.
        Fail,
        /// Panics, taking the runner task down with it.
        Panic,
        /// Waits for its cancellation token, then returns only if `finish_on_cancel` is set.
        WaitForCancel { finish_on_cancel: bool },
        /// Sleeps until the case deadline, then passes.
        ReturnAtDeadline,
        /// Runs `procedure:` entries on the host against a `ready` log line and reports the step results.
        Procedure(&'static str),
        /// Runs [`Behavior::Procedure`], finishes its phase, then waits for cancellation in `then_phase`.
        ///
        /// Returns the recorded result if `returns_on_cancel` is set; otherwise stays pending.
        ProcedureThenWait {
            steps: &'static str,
            then_phase: &'static str,
            returns_on_cancel: bool,
        },
        /// Returns a setup error on cancellation. With `recorded`, records the verdict before waiting;
        /// otherwise simulates a setup wait that fails due to cancellation.
        SetupError { recorded: bool },
    }

    /// A test that enters a phase and then behaves as scripted.
    #[derive(Clone)]
    pub(crate) struct ScriptedTest {
        pub(crate) name: String,
        pub(crate) timeout: Duration,
        pub(crate) phase: &'static str,
        pub(crate) behavior: Behavior,
        /// Notified once the test has entered its phase.
        pub(crate) entered: Option<Arc<Notify>>,
        /// Host port for procedure HTTP requests.
        pub(crate) target_port: Option<u16>,
        /// A directory the test creates and registers before its behavior runs.
        pub(crate) registers: Option<PathBuf>,
        /// Spawns a pending task and records its drop.
        pub(crate) spawns: Option<TaskProbe>,
    }

    /// Observes task shutdown for a [`ScriptedTest`].
    #[derive(Debug, Default, Clone)]
    pub(crate) struct TaskProbe(Arc<std::sync::Mutex<Option<Dropped>>>);

    /// What a [`TaskProbe`] saw when its task was dropped.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) struct Dropped {
        /// Elapsed time since the task's drop guard was created.
        pub(crate) elapsed: Duration,
        /// Whether the test's registered directory still existed.
        pub(crate) directory_existed: bool,
    }

    impl TaskProbe {
        pub(crate) fn dropped(&self) -> Option<Dropped> {
            *self.0.lock().unwrap()
        }
    }

    struct ProbeGuard {
        probe: TaskProbe,
        started: tokio::time::Instant,
        directory: Option<PathBuf>,
    }

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            *self.probe.0.lock().unwrap() = Some(Dropped {
                elapsed: self.started.elapsed(),
                directory_existed: self.directory.as_ref().is_some_and(|dir| dir.exists()),
            });
        }
    }

    impl ScriptedTest {
        /// Creates a test with a timeout long enough for cancellation tests to interrupt it first.
        pub(crate) fn new(name: &str, behavior: Behavior) -> Self {
            Self {
                name: name.to_string(),
                timeout: Duration::from_secs(60),
                phase: "assertions",
                behavior,
                entered: None,
                target_port: None,
                registers: None,
                spawns: None,
            }
        }

        /// Plans a Linux integration case with this test's name and timeout, sourced from `/cases/<name>/config.yaml`.
        pub(crate) fn plan(&self) -> CasePlan {
            let case = crate::CASE_LOADER
                .load_content("cases",
                    &format!(
                        "name: {}\ntimeout: {}ms\nsubject_os: [linux]\nparticipants: {{adp: {{type: agent-adp, subject: true}}}}\nprocedure: [{{step: log_contains, on: adp, pattern: ready, timeout: 5s}}]\n",
                        self.name,
                        self.timeout.as_millis(),
                    ),
                    &self.name,
                    &PathBuf::from("/cases").join(&self.name).join("config.yaml"),
                )
                .expect("the scripted case should load")
                .remove(0);
            plan_for_linux(case)
        }
    }

    /// Compiles a Linux-enabled test case with local artifact defaults.
    pub(crate) fn plan_for_linux(case: CaseConfig) -> CasePlan {
        crate::config::tests::planner()
            .compile(case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap()
    }

    #[async_trait::async_trait]
    impl Test for ScriptedTest {
        async fn run(&self, _plan: &CasePlan, tctx: TestContext) -> TestResult {
            if let Some(dir) = &self.registers {
                std::fs::create_dir_all(dir).expect("the registered directory should be creatable");
                tctx.ledger.directory(dir.clone());
            }
            if let Some(probe) = &self.spawns {
                let guard = ProbeGuard {
                    probe: probe.clone(),
                    started: tokio::time::Instant::now(),
                    directory: self.registers.clone(),
                };
                tctx.tasks.spawn(async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                });
            }

            // Keep the phase active to model an interrupted phase; `ProcedureThenWait` finishes it before switching.
            let phase = tctx.phases.enter(self.phase);
            if let Some(entered) = &self.entered {
                entered.notify_one();
            }

            let assertion = |status| AssertionResult {
                name: "scripted".to_string(),
                status,
                message: String::new(),
                duration: Duration::from_millis(1),
            };
            match self.behavior {
                Behavior::Pass => TestResult::from_assertions(
                    &self.name,
                    Duration::from_millis(1),
                    vec![assertion(AssertionStatus::Passed)],
                    Vec::new(),
                ),
                Behavior::Fail => TestResult::from_assertions(
                    &self.name,
                    Duration::from_millis(1),
                    vec![assertion(AssertionStatus::Failed)],
                    Vec::new(),
                ),
                Behavior::Panic => panic!("scripted test '{}' panicked", self.name),
                Behavior::WaitForCancel { finish_on_cancel } => {
                    tctx.test_cancel_token().cancelled().await;
                    if !finish_on_cancel {
                        std::future::pending::<()>().await;
                    }
                    TestResult::from_assertions(&self.name, Duration::from_secs(1), Vec::new(), Vec::new())
                }
                Behavior::ReturnAtDeadline => {
                    tokio::time::sleep_until(tctx.deadline.at).await;
                    TestResult::from_assertions(
                        &self.name,
                        Duration::from_millis(1),
                        vec![assertion(AssertionStatus::Passed)],
                        Vec::new(),
                    )
                }
                Behavior::Procedure(steps) => {
                    let started = Instant::now();
                    self.run_procedure(steps, &tctx).await;
                    tctx.record.result(&self.name, started.elapsed(), Vec::new())
                }
                Behavior::ProcedureThenWait {
                    steps,
                    then_phase,
                    returns_on_cancel,
                } => {
                    let started = Instant::now();
                    self.run_procedure(steps, &tctx).await;
                    phase.finish();
                    let _then = tctx.phases.enter(then_phase);
                    tctx.test_cancel_token().cancelled().await;
                    if !returns_on_cancel {
                        std::future::pending::<()>().await;
                    }
                    tctx.record
                        .result(&self.name, started.elapsed(), tctx.phases.completed())
                }
                Behavior::SetupError { recorded } => {
                    let failed = |message| {
                        TestResult::errored(
                            &self.name,
                            ErrorKind::Setup,
                            message,
                            Duration::from_millis(1),
                            Vec::new(),
                        )
                    };
                    if recorded {
                        tctx.record.decide(Verdict::SetupFailed(Box::new(failed("boom"))));
                    }
                    tctx.test_cancel_token().cancelled().await;
                    failed(if recorded { "boom" } else { "Canceled while waiting." })
                }
            }
        }
    }

    impl ScriptedTest {
        /// Runs `steps` against a participant with a `ready` log line and records their results.
        async fn run_procedure(&self, steps: &str, tctx: &TestContext) {
            let fake = FakeParticipant::default();
            fake.push_line(None, "ready");
            let nodes: Vec<ProcedureNode> = serde_saphyr::from_str(steps).unwrap();
            let build = |node: StepNode| -> Result<Box<dyn Member + '_>, String> {
                let factory = panoramic_steps::REGISTERED_STEPS
                    .iter()
                    .copied()
                    .find(|factory| factory.name() == node.name)
                    .unwrap();
                let budget = factory.build(&node.fields)?.budget();
                Ok(Box::new(StepMember {
                    factory,
                    fields: node.fields,
                    budget,
                    fake: &fake,
                    port: self.target_port,
                }))
            };
            let procedure = Procedure::build(nodes.into_iter().map(|node| match node {
                ProcedureNode::Step(step) => ProcedureNode::Step(build(step)),
                ProcedureNode::Parallel(steps) => ProcedureNode::Parallel(steps.into_iter().map(build).collect()),
            }))
            .unwrap();
            procedure
                .run(tctx.deadline, &tctx.test_cancel_token(), &tctx.record)
                .await;
        }
    }

    struct StepMember<'a> {
        factory: &'static dyn StepFactory,
        fields: StepFields,
        budget: Duration,
        fake: &'a FakeParticipant,
        port: Option<u16>,
    }

    #[async_trait::async_trait]
    impl Member for StepMember<'_> {
        fn name(&self) -> &'static str {
            self.factory.name()
        }
        fn description(&self) -> String {
            self.factory.name().into()
        }
        fn budget(&self) -> Duration {
            self.budget
        }
        async fn run(&self) -> StepReport {
            match self.port {
                Some(port) => run_step_reaching(self.factory, &self.fields, self.fake, port).await,
                None => run_step_with(self.factory, &self.fields, Some(self.fake)).await,
            }
        }
    }

    /// Runs a test with a timeout to catch hangs.
    pub(crate) async fn run_case(
        built: &BuiltCase, log_base_dir: &Path, cancel_all: &CancellationToken, bound: Duration,
    ) -> TestResult {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::time::timeout(
            bound,
            Runner::run_one(built, &tx, cancel_all, log_base_dir.to_path_buf(), None),
        )
        .await
        .expect("the runner should finish the test within the bound")
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::layout::{LayoutEntry, Os, RealizationKind};
    use panoramic_core::record::StepStatus;
    use panoramic_core::result::test_support::{each_outcome, NOT_RUN_KIND};
    use panoramic_core::result::{ReclaimResult, TestOutcome, TimeoutDeadline};
    use tokio::sync::mpsc;

    use super::test_support::{plan_for_linux, run_case, Behavior, ScriptedTest, TaskProbe};
    use super::*;

    #[tokio::test]
    async fn the_runner_reports_a_native_case_through_the_lifecycle() {
        use panoramic_core::backend::RunBackends;

        use crate::config::tests::{plan_with_plain, Plain, PARTICIPANT_FIELDS};

        for fail_fast in [false, true] {
            let logs = tempfile::tempdir().unwrap();
            let plan = plan_with_plain(
                &PARTICIPANT_FIELDS
                    .replace("agent-adp", "plain")
                    .replace("[linux]", "[macos]"),
                "native",
                Os::MacOs,
            );
            let layout = plan.layout_entries();
            let inputs = NativeInputs::new(Arc::new(RunBackends::default()), &[], logs.path()).unwrap();
            let built = BuiltCase::new(
                plan,
                Box::new(NativeTest {
                    inputs: Arc::new(inputs),
                    types: &[&Plain],
                }),
            );
            let mut runner = Runner::new(logs.path());
            runner.register(built).unwrap();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let args = RunArgs::new(CancellationToken::new(), tx).with_fail_fast(fail_fast);
            tokio::time::timeout(BOUND, runner.run_tests(args)).await.unwrap();

            assert!(matches!(
                rx.try_recv().unwrap(),
                TestEvent::RunStarted { total_tests: 1 }
            ));
            assert!(matches!(rx.try_recv().unwrap(), TestEvent::TestStarted { name } if name == "native"));
            let TestEvent::TestCompleted { result, log_dir } = rx.try_recv().unwrap() else {
                panic!("native case should report completion");
            };
            assert_eq!(result.name, "native");
            assert_eq!(result.outcome, TestOutcome::Errored);
            let error = result.error.as_ref().unwrap();
            assert_eq!(error.kind, ErrorKind::Setup);
            assert_eq!(
                error.message,
                if Os::host() == Os::MacOs {
                    "participant 'adp': participant type 'plain' has no recipe for host-process-group on macos"
                } else {
                    "panoramic cannot create host-process-group participants natively: 'adp' on macos"
                }
            );
            if Os::host() == Os::MacOs {
                assert_eq!(phase_names(&result), ["prepare", "create:adp", "reclaim"]);
                assert_eq!(result.reclaim.len(), 1);
                let scope = &result.reclaim[0];
                assert_eq!(scope.result, ReclaimResult::Reclaimed);
                assert!(scope.participant.is_none());
                assert!(!Path::new(scope.resource.strip_prefix("directory:").unwrap()).exists());
            } else {
                assert_eq!(phase_names(&result), ["prepare"]);
                assert!(result.reclaim.is_empty());
            }
            assert!(result.issues.is_empty());
            assert_eq!(result.layout, Some(layout.clone()));
            assert_eq!(log_dir, logs.path().join("integration/native"));
            let report = written_report(&result);
            assert_eq!(report["name"], "native");
            assert_eq!(report["outcome"], "errored");
            assert_eq!(report["error"]["kind"], "setup");
            assert_eq!(report["error"]["message"], error.message);
            assert_eq!(report["phase_timings"][0]["phase"], "prepare");
            assert_eq!(report["reclaim"], serde_json::to_value(&result.reclaim).unwrap());
            assert_eq!(report["layout"], serde_json::to_value(layout).unwrap());
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn unprovidable_pods_report_without_waiting_for_kind_in_both_run_modes() {
        for fail_fast in [false, true] {
            let logs = tempfile::tempdir().unwrap();
            let (ready_tx, ready_rx) = tokio::sync::watch::channel(None);
            let mut runner = Runner::new(logs.path()).with_kind_ready(ready_rx);
            let built =
                BuiltCase::unprovidable(super::test_support::native_pod_plan(), "Docker context mismatch".into());
            runner.register(built).unwrap();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let args = RunArgs::new(CancellationToken::new(), tx).with_fail_fast(fail_fast);
            tokio::time::timeout(BOUND, runner.run_tests(args)).await.unwrap();
            assert!(ready_tx.borrow().is_none());
            let mut completed = false;
            while let Ok(event) = rx.try_recv() {
                if let TestEvent::TestCompleted { result, .. } = event {
                    assert_eq!(result.outcome, TestOutcome::Errored);
                    let error = result.error.unwrap();
                    assert_eq!(error.kind, ErrorKind::Setup);
                    assert_eq!(error.message, "Docker context mismatch");
                    assert!(result.reclaim.is_empty());
                    completed = true;
                }
            }
            assert!(completed);
        }
    }

    #[tokio::test]
    async fn native_pod_cases_report_failed_or_unfinished_kind_setup_without_creating_resources() {
        use panoramic_core::backend::RunBackends;

        use super::test_support::{native_pod_plan, PodTest};

        for fail_fast in [false, true] {
            for state in ["failed", "pending", "closed", "missing"] {
                let logs = tempfile::tempdir().unwrap();
                let inputs = NativeInputs::new(Arc::new(RunBackends::default()), &[], logs.path()).unwrap();
                let mut runner = Runner::new(logs.path());
                let (ready_tx, ready_rx) = tokio::sync::watch::channel(if state == "failed" {
                    Some(Err("cluster creation failed".to_string()))
                } else {
                    None
                });
                if state != "missing" {
                    runner = runner.with_kind_ready(ready_rx);
                }
                runner
                    .register(BuiltCase::new(
                        native_pod_plan(),
                        Box::new(NativeTest {
                            inputs: Arc::new(inputs),
                            types: &[&PodTest],
                        }),
                    ))
                    .unwrap();
                let (tx, mut rx) = mpsc::unbounded_channel();
                let args = RunArgs::new(CancellationToken::new(), tx).with_fail_fast(fail_fast);
                let task = tokio::spawn(async move { runner.run_tests(args).await });
                assert!(matches!(
                    rx.recv().await.unwrap(),
                    TestEvent::RunStarted { total_tests: 1 }
                ));
                if state == "pending" {
                    assert!(tokio::time::timeout(Duration::from_millis(25), rx.recv())
                        .await
                        .is_err());
                    assert!(!task.is_finished());
                    ready_tx.send(Some(Err("cluster creation failed".to_string()))).unwrap();
                }
                drop(ready_tx);
                tokio::time::timeout(BOUND, task).await.unwrap().unwrap();
                assert!(matches!(rx.try_recv().unwrap(), TestEvent::TestStarted { name } if name == "native-pod"));
                let TestEvent::TestCompleted { result, .. } = rx.try_recv().unwrap() else {
                    panic!("native pod case must report its setup failure");
                };
                assert_eq!(result.outcome, TestOutcome::Errored);
                let error = result.error.as_ref().unwrap();
                assert_eq!(error.kind, ErrorKind::Setup);
                assert_eq!(
                    error.message,
                    if matches!(state, "failed" | "pending") {
                        "Kind cluster setup failed: cluster creation failed"
                    } else {
                        "Kind cluster setup did not complete"
                    }
                );
                assert_eq!(phase_names(&result), ["kind_setup"]);
                assert!(result.reclaim.is_empty());
                assert!(result.participants.is_empty());
                assert!(result.issues.is_empty());
                let report = written_report(&result);
                assert_eq!(report["error"]["kind"], "setup");
                assert_eq!(report["error"]["message"], error.message);
                assert!(rx.try_recv().is_err());
            }
        }
    }

    fn pod_plan() -> CasePlan {
        struct Pod;

        impl ParticipantType for Pod {
            fn name(&self) -> &'static str {
                "pod"
            }
            fn supports(&self) -> &'static [(Os, RealizationKind)] {
                &[(Os::Linux, RealizationKind::KubernetesPod)]
            }
            fn artifacts(
                &self, _: panoramic_core::layout::Realization,
            ) -> &'static [panoramic_core::artifact::ArtifactRole] {
                &[]
            }
            fn processes(&self) -> &'static [&'static str] {
                &[]
            }
        }

        let registries = panoramic_core::case::Registries {
            participant_types: &[&Pod],
            ..crate::REGISTRIES
        };
        let pod_case = panoramic_core::case::CaseLoader::new(registries)
                .load_content(
                    "cases",
                    "timeout: 1m\nsubject_os: [linux]\nparticipants: {adp: {type: pod, subject: true, realization: {kind: kubernetes-pod}}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]",
                    "pod",
                    Path::new("/cases/pod/config.yaml"),
                )
                .unwrap()
                .remove(0);
        panoramic_core::plan::Planner::new(registries, Default::default())
            .unwrap()
            .compile(pod_case, Os::Linux)
            .unwrap()
            .into_plan()
            .unwrap()
    }

    #[tokio::test]
    async fn only_cases_that_need_a_kubernetes_pod_wait_for_the_kind_cluster() {
        for fail_fast in [false, true] {
            let logs = tempfile::tempdir().unwrap();
            let (ready_tx, ready_rx) = tokio::sync::watch::channel(None);
            let mut runner = Runner::new(logs.path()).with_kind_ready(ready_rx);
            let linux = ScriptedTest::new("linux", Behavior::Pass);
            let linux_plan = linux.plan();
            let pod_plan = pod_plan();
            for (plan, scripted) in [
                (linux_plan, linux),
                (pod_plan, ScriptedTest::new("pod", Behavior::Pass)),
            ] {
                runner.register(BuiltCase::new(plan, Box::new(scripted))).unwrap();
            }
            let (tx, mut rx) = mpsc::unbounded_channel();
            let args = RunArgs::new(CancellationToken::new(), tx)
                .with_parallelism(NonZeroUsize::new(2).unwrap())
                .with_fail_fast(fail_fast);
            let task = tokio::spawn(async move { runner.run_tests(args).await });
            loop {
                let event = tokio::time::timeout(BOUND, rx.recv()).await.unwrap().unwrap();
                match event {
                    TestEvent::TestStarted { name } => assert_eq!(name, "linux"),
                    TestEvent::TestCompleted { result, .. } => {
                        assert_eq!(result.name, "linux");
                        assert_eq!(result.outcome, TestOutcome::Passed);
                        break;
                    }
                    _ => {}
                }
            }
            assert!(tokio::time::timeout(Duration::from_millis(25), rx.recv())
                .await
                .is_err());
            assert!(!task.is_finished());
            ready_tx.send(Some(Ok(()))).unwrap();
            tokio::time::timeout(BOUND, task).await.unwrap().unwrap();
            assert!(matches!(rx.try_recv().unwrap(), TestEvent::TestStarted { name } if name == "pod"));
            let TestEvent::TestCompleted { result, .. } = rx.try_recv().unwrap() else {
                panic!("pod completion should follow readiness");
            };
            assert_eq!(result.name, "pod");
            assert_eq!(result.outcome, TestOutcome::Passed);
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn a_failed_or_incomplete_kind_setup_is_reported_without_running_the_pod_body() {
        struct MustNotRun;
        #[async_trait]
        impl Test for MustNotRun {
            async fn run(&self, _: &CasePlan, _: TestContext) -> TestResult {
                panic!("a pod body must not run before successful cluster setup");
            }
        }

        // Waiting for a channel update alone used to lose cluster setup errors and run the body anyway.
        for fail_fast in [false, true] {
            for (ready, message) in [
                (Some(Err("boom".to_string())), "Kind cluster setup failed: boom"),
                (None, "Kind cluster setup did not complete"),
            ] {
                let logs = tempfile::tempdir().unwrap();
                let (ready_tx, ready_rx) = tokio::sync::watch::channel(ready);
                drop(ready_tx);
                let mut runner = Runner::new(logs.path()).with_kind_ready(ready_rx);
                let plan = pod_plan();
                let layout = plan.layout_entries();
                runner.register(BuiltCase::new(plan, Box::new(MustNotRun))).unwrap();
                let (tx, mut rx) = mpsc::unbounded_channel();
                let args = RunArgs::new(CancellationToken::new(), tx).with_fail_fast(fail_fast);
                tokio::time::timeout(BOUND, runner.run_tests(args)).await.unwrap();
                assert!(matches!(
                    rx.try_recv().unwrap(),
                    TestEvent::RunStarted { total_tests: 1 }
                ));
                assert!(matches!(rx.try_recv().unwrap(), TestEvent::TestStarted { name } if name == "pod"));
                let TestEvent::TestCompleted { result, log_dir } = rx.try_recv().unwrap() else {
                    panic!("cluster setup failure must complete the selected test");
                };
                assert_eq!(result.name, "pod");
                assert_eq!(result.outcome, TestOutcome::Errored);
                let error = result.error.as_ref().unwrap();
                assert_eq!(error.kind, ErrorKind::Setup);
                assert_eq!(error.message, message);
                assert_eq!(result.case_path.as_deref(), Some(Path::new("/cases/pod")));
                assert_eq!(result.layout, Some(layout));
                assert_eq!(log_dir, logs.path().join("cases/pod"));
                assert!(rx.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn the_body_receives_the_run_interrupt_separately_from_case_cancellation() {
        struct InterruptsRun(CancellationToken);
        #[async_trait]
        impl Test for InterruptsRun {
            async fn run(&self, plan: &CasePlan, context: TestContext) -> TestResult {
                assert!(!context.interrupt.is_cancelled());
                self.0.cancel();
                assert!(context.interrupt.is_cancelled());
                assert!(!context.test_cancel_token().is_cancelled());
                ScriptedTest::new("case", Behavior::Pass).run(plan, context).await
            }
        }
        let interrupt = CancellationToken::new();
        let built = BuiltCase::new(
            ScriptedTest::new("case", Behavior::Pass).plan(),
            Box::new(InterruptsRun(interrupt.clone())),
        );
        let logs = tempfile::tempdir().unwrap();
        let result = run_case(&built, logs.path(), &interrupt, BOUND).await;
        assert!(interrupt.is_cancelled());
        assert_eq!(result.outcome, TestOutcome::Passed);
    }

    /// A bound for real-time tests.
    const BOUND: Duration = Duration::from_secs(5);

    /// Bounds paused-time tests while allowing the runner's [`GRACE_TIME`] to expire.
    const PAUSED_BOUND: Duration = Duration::from_secs(120);

    /// An assertion that matches the log line [`Behavior::Procedure`] provides.
    const LOG_MATCHES: &str = "- step: log_contains\n  on: subject\n  pattern: ready\n  timeout: 5s\n";

    /// Runs a test within `bound`, using `log_base_dir` and run cancellation `cancel_all`. Discards events.
    async fn run_scripted_test(
        test: &ScriptedTest, log_base_dir: &Path, cancel_all: &CancellationToken, bound: Duration,
    ) -> TestResult {
        let built = BuiltCase::new(test.plan(), Box::new(test.clone()));
        run_case(&built, log_base_dir, cancel_all, bound).await
    }

    /// Returns a token that cancels the run after `after`.
    fn cancel_all_after(after: Duration) -> CancellationToken {
        let cancel_all = CancellationToken::new();
        let cancel = cancel_all.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            cancel.cancel();
        });
        cancel_all
    }

    /// Each recorded step's kind, parallel member, and status.
    fn statuses(result: &TestResult) -> Vec<(&str, Option<usize>, StepStatus)> {
        result
            .assertion_results
            .iter()
            .map(|step| (step.name.as_str(), step.member, step.status))
            .collect()
    }

    fn phase_names(result: &TestResult) -> Vec<&str> {
        result.phase_timings.iter().map(|t| t.phase.as_str()).collect()
    }

    /// Returns registered resource labels and their cleanup results.
    fn reclaimed(result: &TestResult) -> Vec<(&str, ReclaimResult)> {
        result
            .reclaim
            .iter()
            .map(|entry| (entry.resource.as_str(), entry.result))
            .collect()
    }

    /// Reads the test's saved `result.json`.
    fn written_report(result: &TestResult) -> serde_json::Value {
        let log_dir = result
            .log_dir
            .as_ref()
            .expect("the runner should record the log directory");
        let written = std::fs::read_to_string(log_dir.join("result.json")).expect("result.json should exist");
        serde_json::from_str(&written).expect("result.json should parse")
    }

    fn issue(kind: IssueKind, message: &str) -> Issue {
        Issue {
            kind,
            message: message.to_string(),
        }
    }

    /// Creates a test that times out in `phase` and returns on cancellation.
    fn timing_out_in(phase: &'static str) -> ScriptedTest {
        ScriptedTest {
            timeout: Duration::from_millis(50),
            phase,
            ..ScriptedTest::new("stuck", Behavior::WaitForCancel { finish_on_cancel: true })
        }
    }

    #[tokio::test]
    async fn run_one_stamps_the_plan_layout_on_the_result_and_its_result_json() {
        let test = ScriptedTest::new("case", Behavior::Pass);
        let plan = crate::config::tests::plan(crate::config::tests::PARTICIPANT_FIELDS, Os::Linux);
        let layout = Some(vec![LayoutEntry {
            participant: "adp".into(),
            participant_type: "agent-adp".into(),
            os: Os::Linux,
            realization: RealizationKind::LinuxContainer,
        }]);
        let logs = tempfile::tempdir().unwrap();
        let built = BuiltCase::new(plan, Box::new(test));
        let result = run_case(&built, logs.path(), &CancellationToken::new(), BOUND).await;
        assert_eq!(result.outcome, TestOutcome::Passed);
        assert_eq!(result.layout, layout);
        assert_eq!(
            written_report(&result).get("layout"),
            Some(&serde_json::to_value(layout).unwrap())
        );
    }

    #[tokio::test]
    async fn setup_failure_before_the_test_starts_is_reported_as_an_error() {
        // A regular file where the log base directory belongs, so the test's log directory cannot be
        // created. Consumers that build the suite result from events have to see this failure, or
        // the run reports no test at all and exits zero.
        let occupied_base_dir = tempfile::NamedTempFile::new().expect("temp file should be creatable");
        let (tx, mut rx) = mpsc::unbounded_channel();

        let test = ScriptedTest::new("case", Behavior::WaitForCancel { finish_on_cancel: true });
        let plan = crate::config::tests::plan(crate::config::tests::PARTICIPANT_FIELDS, Os::Linux);
        let built = BuiltCase::new(plan, Box::new(test));
        let result = Runner::run_one(
            &built,
            &tx,
            &CancellationToken::new(),
            occupied_base_dir.path().to_path_buf(),
            None,
        )
        .await;

        assert_eq!(result.outcome, TestOutcome::Errored);
        assert_eq!(result.error.as_ref().map(|e| e.kind), Some(ErrorKind::Setup));
        assert_eq!(result.case_path, Some(PathBuf::from("/test/integration/cases/case")));
        assert_eq!(result.layout, Some(built.plan.layout_entries()));

        let mut reported = None;
        while let Ok(event) = rx.try_recv() {
            if let TestEvent::TestCompleted { result, .. } = event {
                reported = Some(result);
            }
        }

        let reported = reported.expect("a setup failure should be reported as a completed test");
        assert_eq!(reported.name, "case");
        assert_eq!(reported.layout, Some(built.plan.layout_entries()));
        assert_eq!(reported.outcome, TestOutcome::Errored);
        assert_eq!(reported.error.as_ref().map(|e| e.kind), Some(ErrorKind::Setup));
    }

    #[tokio::test]
    async fn reported_identity_comes_from_the_loaded_case() {
        let case = crate::CASE_LOADER
            .load_content(
                "correctness",
                crate::config::tests::PARTICIPANT_FIELDS,
                "reported",
                Path::new("/test/correctness/cases/reported/config.yaml"),
            )
            .unwrap()
            .remove(0);
        let plan = plan_for_linux(case);
        let test = ScriptedTest::new("body", Behavior::Pass);
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let built = BuiltCase::new(plan, Box::new(test));

        let result = tokio::time::timeout(
            BOUND,
            Runner::run_one(
                &built,
                &tx,
                &CancellationToken::new(),
                log_base_dir.path().to_path_buf(),
                None,
            ),
        )
        .await
        .expect("the runner should finish the test within the bound");

        let log_dir = log_base_dir.path().join("correctness/reported");
        assert_eq!(result.outcome, TestOutcome::Passed);
        assert_eq!(result.name, "reported");
        assert_eq!(
            result.case_path.as_deref(),
            Some(Path::new("/test/correctness/cases/reported"))
        );
        assert_eq!(result.log_dir.as_ref(), Some(&log_dir));
        assert_eq!(written_report(&result)["name"], "reported");
        let result_log = std::fs::read_to_string(log_dir.join(RESULT_LOG_FILE_NAME)).unwrap();
        assert!(result_log.starts_with("PASS reported "), "{result_log}");
        assert!(matches!(rx.try_recv().unwrap(), TestEvent::TestStarted { name } if name == "reported"));
        let TestEvent::TestCompleted {
            result,
            log_dir: reported_dir,
        } = rx.try_recv().unwrap()
        else {
            panic!("the runner should report completion");
        };
        assert_eq!(result.name, "reported");
        assert_eq!(
            result.case_path.as_deref(),
            Some(Path::new("/test/correctness/cases/reported"))
        );
        assert_eq!(reported_dir, log_dir);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_case_deadline_comes_from_the_loaded_case() {
        let case = crate::CASE_LOADER
            .load_content("cases",
                "name: reported\ntimeout: 50ms\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
                "reported",
                Path::new("/cases/reported/config.yaml"),
            )
            .unwrap()
            .remove(0);
        let plan = plan_for_linux(case);
        let test = ScriptedTest::new("body", Behavior::WaitForCancel { finish_on_cancel: true });
        assert_eq!(test.timeout, Duration::from_secs(60));
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let (tx, _rx) = mpsc::unbounded_channel();
        let built = BuiltCase::new(plan, Box::new(test));

        let result = tokio::time::timeout(
            BOUND,
            Runner::run_one(
                &built,
                &tx,
                &CancellationToken::new(),
                log_base_dir.path().to_path_buf(),
                None,
            ),
        )
        .await
        .expect("the runner should enforce the loaded case's deadline");

        assert_eq!(result.outcome, TestOutcome::TimedOut);
        assert_eq!(result.name, "reported");
        assert_eq!(
            result.timeout,
            Some(TimeoutAttribution::test_deadline(
                Duration::from_millis(50),
                Some("assertions".to_string()),
            ))
        );
    }

    #[tokio::test]
    async fn test_deadline_timeout_names_the_phase_that_was_active() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let result = run_scripted_test(
            &timing_out_in("container_start"),
            log_base_dir.path(),
            &CancellationToken::new(),
            BOUND,
        )
        .await;

        let timeout = result.timeout.expect("a timed-out test should carry timeout detail");
        assert_eq!(result.outcome, TestOutcome::TimedOut);
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "container_start");
        assert_eq!(timeout.configured_ms, 50);
    }

    #[tokio::test]
    async fn result_log_labels_a_timeout_and_gives_its_cause() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let result = run_scripted_test(
            &timing_out_in("container_start"),
            log_base_dir.path(),
            &CancellationToken::new(),
            BOUND,
        )
        .await;

        let log_dir = result.log_dir.expect("the runner should record the log directory");
        let result_log = std::fs::read_to_string(log_dir.join(RESULT_LOG_FILE_NAME)).expect("result.log should exist");
        let mut lines = result_log.lines();
        let first = lines.next().expect("result.log should not be empty");
        assert!(first.starts_with("TIMEOUT stuck "), "first line was: {}", first);
        assert_eq!(lines.next(), Some("case budget 50ms expired in container_start"));
    }

    #[test]
    fn result_log_counts_the_steps_that_never_ran_instead_of_listing_them() {
        let log_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let [_, (failed, _), ..] = each_outcome();

        write_result_log(&failed, log_dir.path());

        let result_log =
            std::fs::read_to_string(log_dir.path().join(RESULT_LOG_FILE_NAME)).expect("result.log should exist");
        let assertions: Vec<&str> = result_log
            .lines()
            .skip_while(|line| *line != "Assertions:")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .filter(|line| !line.starts_with("    "))
            .collect();
        assert_eq!(assertions, ["  - log_contains (1.00ms)"], "{result_log}");
        assert!(!result_log.contains(NOT_RUN_KIND), "{result_log}");
        assert!(result_log.contains("\n1 step not run\n"), "{result_log}");
    }

    #[tokio::test]
    async fn a_case_deadline_interrupts_the_running_step_and_keeps_the_finished_ones() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            timeout: Duration::from_millis(300),
            ..ScriptedTest::new(
                "procedure",
                Behavior::Procedure(
                    "- step: log_contains\n  on: subject\n  pattern: ready\n  timeout: 5s\n\
                     - step: process_stable_for\n  on: subject\n  duration: 30s\n",
                ),
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "assertions");
        assert_eq!(
            statuses(&result),
            [
                ("log_contains", None, StepStatus::Passed),
                ("process_stable_for", None, StepStatus::Interrupted),
            ]
        );
        let interrupted = &result.assertion_results[1];
        assert!(
            interrupted.message.contains("of its 30s step budget"),
            "{}",
            interrupted.message
        );
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        assert_eq!(cause.by, StoppedBy::CaseDeadline);
        let active: Vec<_> = cause
            .active
            .iter()
            .map(|step| (step.step, step.member, step.kind.as_str(), step.budget_ms))
            .collect();
        assert_eq!(active, [(1, None, "process_stable_for", 30_000)]);
    }

    #[tokio::test]
    async fn a_step_that_exhausts_its_own_budget_fails_the_test() {
        // The retry sleep outlasts the step budget, proving the slack preserves the assertion's own timeout message.
        // The last attempt is a request error or a probe timeout, depending on how busy the machine is.
        panoramic_steps::compare::intake::test_server::install_crypto_provider();
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            target_port: Some(panoramic_steps::compare::intake::test_server::closed_port()),
            ..ScriptedTest::new(
            "procedure",
            Behavior::Procedure(
                "- step: http_check\n  on: subject\n  endpoint: http\n  path: /\n  status:\n    equal: 200\n  timeout: 200ms\n",
            ),
        )};

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), BOUND).await;

        assert_eq!(result.outcome, TestOutcome::Failed);
        assert!(result.timeout.is_none());
        assert_eq!(result.assertion_results.len(), 1);
        let message = &result.assertion_results[0].message;
        assert!(
            message.starts_with(
                "HTTP GET '/' at endpoint 'http' of participant 'subject' did not match within 200ms; last attempt: "
            ),
            "{message}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_request_cut_short_by_the_case_deadline_is_interrupted() {
        let target = panoramic_steps::compare::intake::test_server::MockIntake::unresponsive().await;
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            timeout: Duration::from_secs(1),
            target_port: Some(target.port()),
            ..ScriptedTest::new(
                "stalled",
                Behavior::Procedure(
                    "- step: http_check\n  on: subject\n  endpoint: http\n  path: /\n  status:\n    equal: 200\n  timeout: 30s\n",
                ),
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut, "{:?}", result.error);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        let active: Vec<_> = cause
            .active
            .iter()
            .map(|step| (step.step, step.member, step.kind.as_str()))
            .collect();
        assert_eq!(active, [(0, None, "http_check")]);
        assert_eq!(statuses(&result), [("http_check", None, StepStatus::Interrupted)]);
        assert_eq!(
            result.assertion_results[0].message,
            "Interrupted after 1.0s. The case left this step 1.0s of its 30s step budget."
        );
    }

    #[tokio::test]
    async fn the_case_deadline_wins_when_the_test_returns_at_the_same_instant() {
        // Unbiased polling lets the body win roughly a third of ties. Repetition makes a false pass unlikely.
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        for run in 0..50 {
            let test = ScriptedTest {
                timeout: Duration::from_millis(10),
                ..ScriptedTest::new("tie", Behavior::ReturnAtDeadline)
            };

            let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), BOUND).await;

            assert_eq!(
                result.outcome,
                TestOutcome::TimedOut,
                "run {run} reported the body's result"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_dropped_after_its_procedure_keeps_its_steps_and_where_grace_ran_out() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let steps = "- step: log_contains\n  on: subject\n  pattern: ready\n  timeout: 5s\n\
                     - step: log_contains\n  on: subject\n  pattern: ready\n  timeout: 5s\n\
                     - parallel:\n  - step: process_stable_for\n    on: subject\n    duration: 600s\n  \
                     - step: process_stable_for\n    on: subject\n    duration: 600s\n\
                     - step: log_contains\n  on: subject\n  pattern: ready\n  timeout: 5s\n";
        let test = ScriptedTest {
            timeout: Duration::from_secs(1),
            ..ScriptedTest::new(
                "dropped",
                Behavior::ProcedureThenWait {
                    steps,
                    then_phase: "cleanup",
                    returns_on_cancel: false,
                },
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::CleanupGrace);
        assert_eq!(timeout.active_phase, "assertions");
        assert_eq!(timeout.grace_phase.as_deref(), Some("cleanup"));
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        assert_eq!(cause.by, StoppedBy::CaseDeadline);
        let member = |member| ActiveStep {
            step: 2,
            member: Some(member),
            kind: "process_stable_for".to_string(),
            elapsed_ms: 1_000,
            budget_ms: 600_000,
        };
        assert_eq!(cause.active, [member(0), member(1)]);
        let expected_statuses = [
            ("log_contains", None, StepStatus::Passed),
            ("log_contains", None, StepStatus::Passed),
            ("process_stable_for", Some(0), StepStatus::Interrupted),
            ("process_stable_for", Some(1), StepStatus::Interrupted),
            ("log_contains", None, StepStatus::NotRun),
        ];
        assert_eq!(statuses(&result), expected_statuses);
        assert_eq!(phase_names(&result), ["assertions"]);

        let json = written_report(&result);
        assert_eq!(json["schema_version"], 3);
        let written_statuses: Vec<&str> = json["assertions"]
            .as_array()
            .expect("assertions is an array")
            .iter()
            .map(|step| step["status"].as_str().expect("status is a string"))
            .collect();
        assert_eq!(
            written_statuses,
            ["passed", "passed", "interrupted", "interrupted", "not_run"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resources_are_reclaimed_after_the_runner_drops_a_body_stuck_past_grace() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let registered = tempfile::tempdir().expect("temp dir should be creatable");
        let stuck = |name: &str, behavior| ScriptedTest {
            timeout: Duration::from_secs(1),
            registers: Some(registered.path().join(name)),
            ..ScriptedTest::new(name, behavior)
        };
        let decided = stuck(
            "decided",
            Behavior::ProcedureThenWait {
                steps: LOG_MATCHES,
                then_phase: "intake_capture",
                returns_on_cancel: false,
            },
        );
        let undecided = stuck(
            "undecided",
            Behavior::WaitForCancel {
                finish_on_cancel: false,
            },
        );

        let decided = run_scripted_test(&decided, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;
        let undecided =
            run_scripted_test(&undecided, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(decided.outcome, TestOutcome::Passed, "{:?}", decided.error);
        let issue_kinds: Vec<_> = decided.issues.iter().map(|issue| issue.kind).collect();
        assert_eq!(
            issue_kinds,
            [IssueKind::PostVerdictDeadline, IssueKind::CleanupGraceExpired]
        );
        assert_eq!(undecided.outcome, TestOutcome::TimedOut);
        let timeout = undecided
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::CleanupGrace);
        let error = undecided
            .error
            .as_ref()
            .expect("a timed-out test should carry its error");
        assert_eq!(error.message, "Test timed out after 1s and did not return within 30s.");
        for result in [&decided, &undecided] {
            let dir = registered.path().join(&result.name);
            let resource = format!("directory:{}", dir.display());
            assert_eq!(reclaimed(result), [(resource.as_str(), ReclaimResult::Reclaimed)]);
            assert!(!dir.exists(), "{} should be removed", dir.display());
            assert_eq!(phase_names(result).last(), Some(&"reclaim"), "{}", result.name);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_passing_case_keeps_its_verdict_and_stops_its_tasks_before_reclaim() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let registered = tempfile::tempdir().expect("temp dir should be creatable");
        let dir = registered.path().join("passing");
        let probe = TaskProbe::default();
        let test = ScriptedTest {
            registers: Some(dir.clone()),
            spawns: Some(probe.clone()),
            ..ScriptedTest::new("passing", Behavior::Pass)
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::Passed, "{:?}", result.error);
        assert_eq!(result.issues, []);
        let resource = format!("directory:{}", dir.display());
        assert_eq!(reclaimed(&result), [(resource.as_str(), ReclaimResult::Reclaimed)]);
        assert!(!dir.exists(), "{} should be removed", dir.display());
        let dropped = probe.dropped().expect("the runner should stop the test's task");
        assert!(dropped.directory_existed, "the task should stop before reclaim");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tasks_outliving_the_stop_budget_are_reported_without_changing_the_verdict() {
        struct Blocks {
            release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        }

        #[async_trait::async_trait]
        impl Test for Blocks {
            async fn run(&self, _plan: &CasePlan, tctx: TestContext) -> TestResult {
                let release = self.release.lock().unwrap().take().unwrap();
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                tctx.tasks.spawn(async move {
                    let _ = started_tx.send(());
                    // Synchronous work holds a worker beyond cooperative cancellation; dropping the sender releases it.
                    let _ = release.recv();
                });
                started_rx.await.unwrap();
                TestResult::from_assertions("blocked-task", Duration::ZERO, Vec::new(), Vec::new())
            }
        }

        let logs = tempfile::tempdir().unwrap();
        let (release, receiver) = std::sync::mpsc::channel();
        let built = BuiltCase::new(
            ScriptedTest::new("blocked-task", Behavior::Pass).plan(),
            Box::new(Blocks {
                release: std::sync::Mutex::new(Some(receiver)),
            }),
        );
        let result = run_case(&built, logs.path(), &CancellationToken::new(), Duration::from_secs(15)).await;
        drop(release);
        assert_eq!(result.outcome, TestOutcome::Passed);
        assert_eq!(
            result.issues,
            [issue(
                IssueKind::TasksStillRunning,
                "1 background task(s) still tracked after the 5s stop budget expired."
            )]
        );
        let report = written_report(&result);
        assert_eq!(report["outcome"], "passed");
        assert_eq!(
            report["issues"],
            serde_json::json!([{
                "kind": "tasks_still_running", "message": result.issues[0].message,
            }])
        );
        assert_eq!(IssueKind::TasksStillRunning.to_string(), "tasks_still_running");
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_case_stops_its_tasks_at_the_deadline() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let registered = tempfile::tempdir().expect("temp dir should be creatable");
        let dir = registered.path().join("stuck");
        let probe = TaskProbe::default();
        let test = ScriptedTest {
            timeout: Duration::from_secs(1),
            registers: Some(dir.clone()),
            spawns: Some(probe.clone()),
            ..ScriptedTest::new(
                "stuck",
                Behavior::WaitForCancel {
                    finish_on_cancel: false,
                },
            )
        };

        let result = run_scripted_test(&test, log_base_dir.path(), &CancellationToken::new(), PAUSED_BOUND).await;

        assert_eq!(result.outcome, TestOutcome::TimedOut);
        let timeout = result
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::CleanupGrace);
        let dropped = probe.dropped().expect("the case deadline should stop the test's task");
        assert_eq!(dropped.elapsed, Duration::from_secs(1));
        let resource = format!("directory:{}", dir.display());
        assert_eq!(reclaimed(&result), [(resource.as_str(), ReclaimResult::Reclaimed)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_verdict_reached_before_the_runner_stops_the_test_is_kept() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let passes_then_waits = |timeout, returns_on_cancel| ScriptedTest {
            timeout,
            ..ScriptedTest::new(
                "decided",
                Behavior::ProcedureThenWait {
                    steps: LOG_MATCHES,
                    then_phase: "intake_capture",
                    returns_on_cancel,
                },
            )
        };
        let deadline_issue = issue(
            IssueKind::PostVerdictDeadline,
            "case budget 1s expired in intake_capture after the verdict",
        );
        let grace_issue = issue(
            IssueKind::CleanupGraceExpired,
            "cleanup grace 30s expired in intake_capture",
        );
        let cases = [
            (
                "timer, returns",
                passes_then_waits(Duration::from_secs(1), true),
                CancellationToken::new(),
                vec![deadline_issue.clone()],
            ),
            (
                "timer, never returns",
                passes_then_waits(Duration::from_secs(1), false),
                CancellationToken::new(),
                vec![deadline_issue, grace_issue],
            ),
            (
                "signal, returns",
                passes_then_waits(Duration::from_secs(60), true),
                cancel_all_after(Duration::from_secs(1)),
                vec![],
            ),
        ];

        for (case, test, cancel_all, issues) in cases {
            let result = run_scripted_test(&test, log_base_dir.path(), &cancel_all, PAUSED_BOUND).await;

            assert_eq!(result.outcome, TestOutcome::Passed, "{case}: {:?}", result.error);
            assert!(result.cause.is_none(), "{case}: {:?}", result.cause);
            assert!(result.timeout.is_none(), "{case}: {:?}", result.timeout);
            assert_eq!(result.issues, issues, "{case}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_setup_error_is_kept_only_if_it_was_reached_before_the_deadline() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let setup_error = |phase, recorded| ScriptedTest {
            timeout: Duration::from_secs(1),
            phase,
            ..ScriptedTest::new("setup", Behavior::SetupError { recorded })
        };

        let recorded = run_scripted_test(
            &setup_error("cleanup", true),
            log_base_dir.path(),
            &CancellationToken::new(),
            PAUSED_BOUND,
        )
        .await;

        assert_eq!(recorded.outcome, TestOutcome::Errored);
        let error = recorded.error.as_ref().expect("a setup failure should carry its error");
        assert_eq!((error.kind, error.message.as_str()), (ErrorKind::Setup, "boom"));
        assert_eq!(
            recorded.issues,
            [issue(
                IssueKind::PostVerdictDeadline,
                "case budget 1s expired in cleanup after the verdict"
            )]
        );

        let unrecorded = run_scripted_test(
            &setup_error("intake_start", false),
            log_base_dir.path(),
            &CancellationToken::new(),
            PAUSED_BOUND,
        )
        .await;

        assert_eq!(unrecorded.outcome, TestOutcome::TimedOut, "{:?}", unrecorded.error);
        let timeout = unrecorded
            .timeout
            .as_ref()
            .expect("a timed-out test should carry timeout detail");
        assert_eq!(timeout.deadline, TimeoutDeadline::TestDeadline);
        assert_eq!(timeout.active_phase, "intake_start");
    }

    #[tokio::test(start_paused = true)]
    async fn a_signal_whose_grace_expires_stays_interrupted() {
        let log_base_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let test = ScriptedTest {
            timeout: Duration::from_secs(60),
            ..ScriptedTest::new(
                "interrupted",
                Behavior::ProcedureThenWait {
                    steps: "- step: process_stable_for\n  on: subject\n  duration: 600s\n",
                    then_phase: "cleanup",
                    returns_on_cancel: false,
                },
            )
        };

        let result = run_scripted_test(
            &test,
            log_base_dir.path(),
            &cancel_all_after(Duration::from_secs(1)),
            PAUSED_BOUND,
        )
        .await;

        assert_eq!(result.outcome, TestOutcome::Errored);
        assert_eq!(result.error.as_ref().map(|e| e.kind), Some(ErrorKind::Internal));
        assert_eq!(result.label(), "INTERRUPTED");
        let cause = result.cause.as_ref().expect("a stopped test should carry its cause");
        assert_eq!(cause.by, StoppedBy::Signal);
        let active: Vec<_> = cause
            .active
            .iter()
            .map(|step| (step.step, step.kind.as_str()))
            .collect();
        assert_eq!(active, [(0, "process_stable_for")]);
        assert_eq!(
            result.issues,
            [issue(
                IssueKind::CleanupGraceExpired,
                "cleanup grace 30s expired in cleanup"
            )]
        );
        assert!(result.timeout.is_none(), "{:?}", result.timeout);
        assert_eq!(phase_names(&result), ["assertions"]);
        assert_eq!(
            statuses(&result),
            [("process_stable_for", None, StepStatus::Interrupted)]
        );
    }
}

#[cfg(test)]
mod report_tests;

#[cfg(test)]
mod daemon_tests;

#[cfg(test)]
mod product_tests;
