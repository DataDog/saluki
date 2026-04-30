use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::config::IntegrationConfig;
use crate::correctness::config::Config as CorrectnessConfig;
use crate::reporter::TestResult;
use crate::test::{Test, TestSuite};

/// Wraps a YAML-discovered integration `TestCase` as a `Test`.
pub(crate) struct IntegrationTestCase {
    tc: IntegrationConfig,
    log_dir: Option<PathBuf>,
    mounts_dir: PathBuf,
    cancel_token: CancellationToken,
}

impl IntegrationTestCase {
    pub(crate) fn new(tc: IntegrationConfig, log_dir: Option<PathBuf>, mounts_dir: PathBuf) -> Self {
        Self {
            tc,
            log_dir,
            mounts_dir,
            cancel_token: CancellationToken::new(),
        }
    }
}

#[async_trait]
impl Test for IntegrationTestCase {
    fn name(&self) -> String {
        self.tc.name.clone()
    }

    fn suite(&self) -> TestSuite {
        TestSuite::Integration
    }

    fn description(&self) -> Option<String> {
        self.tc.description.clone()
    }

    // TODO: decide if we really want integration tests to have no timeout.
    fn timeout(&self) -> Duration {
        self.tc.timeout.0
    }

    fn log_dir(&self) -> PathBuf {
        self.log_dir
            .as_ref()
            .map(|d| d.join("integration").join(&self.tc.name))
            .unwrap_or_else(|| PathBuf::from("/tmp/panoramic/integration").join(&self.tc.name))
    }

    fn images(&self) -> BTreeMap<&str, String> {
        let mut m = BTreeMap::new();
        m.insert("container", self.tc.container.image.clone());
        m
    }

    async fn run(&self) -> TestResult {
        let mut runner =
            crate::runner::TestRunner::new(self.tc.clone(), self.mounts_dir.clone(), self.cancel_token.clone());
        if let Some(ref dir) = self.log_dir {
            runner = runner.with_log_dir(dir.join("integration"));
        }
        let mut result = runner.run().await;
        result.log_dir = Some(self.log_dir());
        result
    }

    async fn cancel(&self) {
        self.cancel_token.cancel();
    }
}

/// Wraps a YAML-discovered correctness config as a `Test`.
pub(crate) struct CorrectnessTestCase {
    name: String,
    config: CorrectnessConfig,
    log_dir: Option<PathBuf>,
    mounts_dir: PathBuf,
    cancel_token: CancellationToken,
}

impl CorrectnessTestCase {
    pub(crate) fn new(name: String, config: CorrectnessConfig, log_dir: Option<PathBuf>, mounts_dir: PathBuf) -> Self {
        Self {
            name,
            config,
            log_dir,
            mounts_dir,
            cancel_token: CancellationToken::new(),
        }
    }
}

#[async_trait]
impl Test for CorrectnessTestCase {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn suite(&self) -> TestSuite {
        TestSuite::Correctness
    }

    fn description(&self) -> Option<String> {
        None
    }

    fn log_dir(&self) -> PathBuf {
        self.log_dir
            .as_ref()
            .map(|d| d.join("correctness").join(&self.name))
            .unwrap_or_else(|| PathBuf::from("/tmp/panoramic/correctness").join(&self.name))
    }

    fn images(&self) -> BTreeMap<&str, String> {
        let mut m = BTreeMap::new();
        m.insert("baseline", self.config.baseline.image.clone());
        m.insert("comparison", self.config.comparison.image.clone());
        m.insert("datadog-intake", self.config.datadog_intake.image.clone());
        m.insert("millstone", self.config.millstone.image.clone());
        m
    }

    async fn run(&self) -> TestResult {
        crate::correctness::runner::run_correctness_test(
            self.name.clone(),
            self.config.clone(),
            Some(self.log_dir()),
            self.mounts_dir.clone(),
            self.cancel_token.clone(),
        )
        .await
    }

    async fn cancel(&self) {
        self.cancel_token.cancel();
    }
}
