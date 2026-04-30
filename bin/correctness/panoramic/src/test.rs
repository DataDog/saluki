use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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

    /// Where this test is writing its logs.
    fn log_dir(&self) -> PathBuf;

    /// Returns the list of images that this test depends on as a map of `name -> image`.
    ///
    /// Panoramic depends on container images to be built and ready. Build processes need to be able to inspect these
    /// so we offer a command by which a build process can see these.
    fn images(&self) -> BTreeMap<&str, String>;

    /// Inject runtime configuration that is not known at discovery time.
    fn set_runtime_config(&mut self, config: RuntimeConfig);

    /// Run the test and return the `TestResult`. Note that we do not return an error here. It is expected that you
    /// should handle errors and turn them into a failed `TestResult` and try not to panic.
    async fn run(&self) -> TestResult;

    /// Request cancellation and clean up any resources (containers, networks, volumes). Called by the registry when a
    /// test exceeds its timeout.
    async fn cancel(&self);
}
