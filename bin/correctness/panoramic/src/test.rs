use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::reporter::TestResult;

pub(crate) struct RuntimeConfig {
    pub log_dir: Option<PathBuf>,
    pub mounts_dir: PathBuf,
}

const DEFAULT_TIMEOUT: Duration = Duration::from_mins(10);

#[derive(Debug, Default, Clone, Copy, Eq, Ord, PartialOrd, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TestSuite {
    #[default]
    Integration,
    Correctness,
}

#[derive(Debug, Clone)]
pub(crate) struct TestContext {
    /// Provides a teardown signal to a running test.
    ///
    /// Tests should watch this in a `tokio::select!` block so that they can tear down their resources when they have
    /// been canceled either by an event or because they have timed out. They will be given a short grace period after
    /// this fires to tear down resources.
    #[allow(dead_code)]
    cancel: CancellationToken,

    /// A directory, which has been created for them, into which tests may write their logs.
    #[allow(dead_code)]
    log_dir: PathBuf,

    /// A directory from which files should be mounted into one or more of the domain-specific containers used in this
    /// test.
    // TODO: this is an ugly hack introduced to support the PANORAMIC_DYNAMIC feature. Consider generalizing if needed.
    // For example: this could become runtime_config: HashMap<String, String> for shuttling domain specific items from
    // runtime to a test.
    mounts_dir: PathBuf,
}

impl TestContext {
    pub(crate) fn new(cancel: CancellationToken, log_dir: PathBuf, mounts_dir: PathBuf) -> Self {
        Self {
            cancel,
            log_dir,
            mounts_dir,
        }
    }

    pub(crate) fn cancel(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub(crate) fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub(crate) fn mounts_dir(&self) -> &Path {
        &self.mounts_dir
    }
}

#[async_trait]
pub(crate) trait Test: Send + Sync {
    /// The name of the test for reporting. Must be unique among all tests that are being executed.
    fn name(&self) -> String;

    /// The suite of tests that the test belongs to.
    fn suite(&self) -> TestSuite;

    /// A description of the test for reporting and documentation purposes.
    fn description(&self) -> Option<String>;

    /// How long the test should be allowed to run for before it is considered a failure.
    fn timeout(&self) -> Duration {
        DEFAULT_TIMEOUT
    }

    /// Returns the list of images that this test depends on as a map of `name -> image`.
    ///
    /// Panoramic depends on container images to be built and ready. Build processes need to be able to inspect these
    /// so we offer a command by which a build process can see these.
    fn images(&self) -> BTreeMap<&str, String>;

    /// Inject runtime configuration that is not known at discovery time.
    fn set_runtime_config(&mut self, config: RuntimeConfig);

    /// Run the test and return the `TestResult`. Note that we do not return an error here. It is expected that you
    /// should handle errors and turn them into a failed `TestResult` and try not to panic.
    async fn run(&self, tctx: TestContext) -> TestResult;
}
