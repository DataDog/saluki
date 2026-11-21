use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::core::backend::Backends;
use crate::core::deadline::Deadline;
use crate::core::ledger::ResourceLedger;
use crate::core::record::CaseRecord;
use crate::core::result::{PhaseTracker, TestResult};
use crate::core::tasks::CaseTasks;
use crate::participant_case::ParticipantCase;

/// Settings shared by every test in a run.
#[derive(Debug, Clone)]
pub(crate) struct RunnerSettings {
    /// Realizes the participants of participant cases, once the checks before the first case have passed.
    pub(crate) backends: Backends,
}

/// Carries information that a test needs at runtime such as the logging directory and the cancellation channel.
#[derive(Debug, Clone)]
pub(crate) struct TestContext {
    /// Signals a running test to stop.
    ///
    /// The runner cancels it on an interrupt or timeout. Tests should return their result within the grace period.
    test_cancel_token: CancellationToken,

    /// The case deadline; the runner cancels the test token when it passes.
    pub(crate) deadline: Deadline,

    /// A directory, which has been created for them, into which tests may write their logs.
    log_dir: PathBuf,

    /// Settings shared by every test in the run.
    pub(crate) settings: RunnerSettings,

    /// Where a test marks the phase it is executing, so the runner can name it if a deadline fires.
    pub(crate) phases: PhaseTracker,

    /// Step states and verdict shared with the runner for reporting after cancellation.
    pub(crate) record: CaseRecord,

    /// Test resources registered for cleanup by the runner.
    pub(crate) ledger: ResourceLedger,

    /// Explicitly tracked background work, given a bounded stop period before resource cleanup.
    pub(crate) tasks: CaseTasks,
}

impl TestContext {
    pub(crate) fn new(
        cancel: CancellationToken, deadline: Deadline, log_dir: PathBuf, settings: RunnerSettings,
    ) -> Self {
        Self {
            tasks: CaseTasks::new(&cancel),
            test_cancel_token: cancel,
            deadline,
            log_dir,
            settings,
            phases: PhaseTracker::default(),
            record: CaseRecord::default(),
            ledger: ResourceLedger::default(),
        }
    }

    pub(crate) fn test_cancel_token(&self) -> CancellationToken {
        self.test_cancel_token.clone()
    }

    pub(crate) fn log_dir(&self) -> &Path {
        &self.log_dir
    }
}

#[async_trait]
pub(crate) trait Test: Send + Sync {
    /// The name of the test for reporting. Must be unique among all tests that are being executed.
    fn name(&self) -> String;

    /// The suite the test belongs to: the name of the directory above its cases directory, such as `integration`. It
    /// names the test's log directory.
    fn suite(&self) -> String;

    /// A description of the test for reporting and documentation purposes.
    fn description(&self) -> Option<String>;

    /// The directory the test case was loaded from, for reporting.
    fn case_path(&self) -> PathBuf;

    /// How long the test should be allowed to run for before it's considered a failure.
    fn timeout(&self) -> Duration;

    /// The runtime name of the test: the name of the subject OS it runs on (`linux`, `mac` or `windows`), or
    /// `kubernetes_in_docker` for a test that runs pods.
    ///
    /// Used by the CI pipeline generator to select the appropriate job template.
    fn runtime(&self) -> String;

    /// Whether the test runs host processes. Their ports are fixed, so the runner runs one such test at a time, however
    /// many tests run in parallel.
    fn runs_host_processes(&self) -> bool {
        false
    }

    /// The test as a participant case: the checks before the first case read its plan, and `list` leaves it out on a
    /// subject OS it skips.
    fn participant_case(&self) -> Option<&ParticipantCase> {
        None
    }

    /// Runs the test, reporting errors in its `TestResult`.
    ///
    /// Watch the context's cancellation token and return when it fires. Register resources in its ledger for cleanup
    /// after the test ends. Use its `tasks` for runner-managed background work.
    async fn run(&self, tctx: TestContext) -> TestResult;
}
