use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use airlock::{
    config::{
        DatadogIntakeConfig as AirlockDatadogIntakeConfig, MillstoneConfig as AirlockMillstoneConfig,
        TargetConfig as AirlockTargetConfig,
    },
    driver::{ContainerOs, DriverConfig},
};
use async_trait::async_trait;
use saluki_error::{generic_error, GenericError};

use super::config::{Config, Runtime, TargetConfig};
use crate::reporter::TestResult;
use crate::test::{resolve_case_path, Test, TestContext, TestSuite};

// Correctness tests run two isolation groups (baseline + comparison), each with multiple
// containers, so they need more time than the default.
const CORRECTNESS_TIMEOUT: Duration = Duration::from_mins(20);

/// Container names a correctness test carries in [`Test::images`] and image overrides.
pub(crate) const BASELINE_IMAGE_NAME: &str = "baseline";
pub(crate) const COMPARISON_IMAGE_NAME: &str = "comparison";
pub(crate) const INTAKE_IMAGE_NAME: &str = "datadog-intake";
pub(crate) const MILLSTONE_IMAGE_NAME: &str = "millstone";

/// A named correctness case prepared from file data.
#[derive(Clone)]
pub(crate) struct CorrectnessTestCase {
    pub(crate) name: String,
    pub(crate) config: Config,
}

impl CorrectnessTestCase {
    pub(crate) fn new(name: String, config: Config) -> Self {
        Self { name, config }
    }

    /// Builds the load generator settings, resolving its case-relative configuration path.
    pub(crate) fn millstone_config(&self) -> AirlockMillstoneConfig {
        AirlockMillstoneConfig {
            image: self.config.millstone.image.clone(),
            binary_path: Some(self.config.millstone.binary_path.clone()),
            config_path: resolve_case_path(&self.config.loaded_from, &self.config.millstone.config_path),
        }
    }

    /// Builds the intake settings from the overridden file data.
    pub(crate) fn datadog_intake_config(&self) -> AirlockDatadogIntakeConfig {
        AirlockDatadogIntakeConfig {
            image: self.config.datadog_intake.image.clone(),
            binary_path: Some(self.config.datadog_intake.binary_path.clone()),
        }
    }

    /// Prepares Airlock target settings and resolves file mounts against the case directory.
    pub(crate) fn target_config(
        &self, target_config: &TargetConfig,
    ) -> Result<(AirlockTargetConfig, Vec<(PathBuf, PathBuf)>), GenericError> {
        let airlock_target_config = AirlockTargetConfig {
            image: target_config.image.clone(),
            entrypoint: target_config.entrypoint.clone(),
            command: target_config.command.clone(),
            additional_env_vars: env_assignments(&target_config.env),
            container_os: ContainerOs::Linux,
            host_cgroup_namespace: false,
        };

        let mut mounts = Vec::with_capacity(target_config.files.len());
        for file in &target_config.files {
            // Parse the two file paths -- host path and container path -- from the entry, and anchor
            // the host path at the case directory. The container path must be absolute.
            match file.split_once(':') {
                Some((host_path, container_path)) => {
                    let host_path = resolve_case_path(&self.config.loaded_from, host_path);
                    let container_path = Path::new(container_path);
                    if !container_path.is_absolute() {
                        return Err(generic_error!(
                            "Container path '{}' must be absolute.",
                            container_path.display()
                        ));
                    }

                    mounts.push((host_path, container_path.to_path_buf()));
                }
                None => {
                    return Err(generic_error!(
                        "Invalid file entry format (expected 'host_path:container_path', got '{}')",
                        file,
                    ))
                }
            };
        }

        Ok((airlock_target_config, mounts))
    }

    /// Builds a target driver from prepared settings and file mounts.
    pub(crate) async fn target_driver_config(
        &self, target_config: &TargetConfig,
    ) -> Result<DriverConfig, GenericError> {
        let (target, mounts) = self.target_config(target_config)?;
        let mut driver_config = DriverConfig::target("target", target).await?;
        for (host_path, container_path) in mounts {
            driver_config = driver_config.with_bind_mount(host_path, container_path);
        }
        Ok(driver_config)
    }
}

/// Converts environment data to ordered process-level assignments for Airlock.
pub(crate) fn env_assignments(env: &BTreeMap<String, String>) -> Vec<String> {
    env.iter().map(|(name, value)| format!("{}={}", name, value)).collect()
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

    fn case_path(&self) -> PathBuf {
        self.config.loaded_from.parent().unwrap_or(Path::new("")).to_path_buf()
    }

    fn timeout(&self) -> Duration {
        CORRECTNESS_TIMEOUT
    }

    fn images(&self) -> BTreeMap<&str, String> {
        let mut m = BTreeMap::new();
        m.insert(BASELINE_IMAGE_NAME, self.config.baseline.image.clone());
        m.insert(COMPARISON_IMAGE_NAME, self.config.comparison.image.clone());
        m.insert(INTAKE_IMAGE_NAME, self.config.datadog_intake.image.clone());
        m.insert(MILLSTONE_IMAGE_NAME, self.config.millstone.image.clone());
        m
    }

    fn runtime(&self) -> String {
        match self.config.runtime {
            Runtime::Docker => "docker".to_string(),
            Runtime::KubernetesInDocker => "kubernetes_in_docker".to_string(),
        }
    }

    async fn run(&self, tctx: TestContext) -> TestResult {
        crate::correctness::runner::run_correctness_test(self.clone(), tctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        serde_yaml::from_str(
            r#"
runtime: docker
analysis_mode: metrics
baseline: {image: "baseline:file"}
comparison: {image: "comparison:file"}
"#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn target_driver_rejects_malformed_mounts_and_relative_container_paths() {
        for (entry, expected) in [
            ("missing-separator", "Invalid file entry format"),
            ("host.yaml:relative/target.yaml", "must be absolute"),
        ] {
            let mut config = config();
            config.baseline.files.push(entry.to_string());
            let case = CorrectnessTestCase::new("mounts".to_string(), config);
            let error = case
                .target_driver_config(&case.config.baseline)
                .await
                .err()
                .expect("invalid mount");
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(case.config.baseline.files, [entry]);
        }
    }
}
