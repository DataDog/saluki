use std::path::{Path, PathBuf};

use async_trait::async_trait;
use panoramic_core::deadline::Deadline;
use panoramic_core::ledger::ResourceLedger;
use panoramic_core::lifecycle::Observations;
use panoramic_core::plan::CasePlan;
use panoramic_core::record::CaseRecord;
use panoramic_core::result::{PhaseTracker, TestResult};
use panoramic_core::tasks::CaseTasks;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Cluster setup result awaited by the runner before pod cases start.
///
/// Holds `None` until setup completes, then `Some(Ok(()))` or `Some(Err(message))`.
pub(crate) type KindReadyReceiver = watch::Receiver<Option<Result<(), String>>>;

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

    /// Signals interruption of the whole run, independently of the case deadline.
    pub(crate) interrupt: CancellationToken,

    /// Where a test marks the phase it is executing, so the runner can name it if a deadline fires.
    pub(crate) phases: PhaseTracker,

    /// Step states and verdict shared with the runner for reporting after cancellation.
    pub(crate) record: CaseRecord,

    /// Live participant observations retained when the runner drops the case body.
    pub(crate) observations: Observations,

    /// Test resources registered for cleanup by the runner.
    pub(crate) ledger: ResourceLedger,

    /// Explicitly tracked background work, given a bounded stop period before resource cleanup.
    pub(crate) tasks: CaseTasks,
}

impl TestContext {
    pub(crate) fn new(
        cancel: CancellationToken, interrupt: CancellationToken, deadline: Deadline, log_dir: PathBuf,
    ) -> Self {
        Self {
            tasks: CaseTasks::new(&cancel),
            test_cancel_token: cancel,
            deadline,
            log_dir,
            interrupt,
            phases: PhaseTracker::default(),
            record: CaseRecord::default(),
            observations: Observations::default(),
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
    /// Runs the test, reporting errors in its `TestResult`.
    ///
    /// Watch the context's cancellation token and return when it fires. Register resources in its ledger for cleanup
    /// after the test ends. Use its `tasks` for runner-managed background work.
    async fn run(&self, plan: &CasePlan, tctx: TestContext) -> TestResult;
}
