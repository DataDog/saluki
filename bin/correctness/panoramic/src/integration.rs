//! Prepared integration test cases and their execution settings.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;

use crate::{
    config::{
        target_image_for_runtime, AssertionStep, IntegrationConfig, DEFAULT_INTAKE_IMAGE, INTAKE_IMAGE_NAME,
        MAC_RUNTIME, TARGET_IMAGE_NAME,
    },
    image_override::ImageOverrides,
    reporter::TestResult,
    test::{Test, TestContext, TestSuite},
};

/// Image and runtime choices shared by integration cases in a run.
pub(crate) struct IntegrationSettings {
    runtime: String,
    target_image: Option<String>,
    intake_image: String,
}

impl IntegrationSettings {
    pub(crate) fn new(runtime: &str, overrides: &ImageOverrides<'_>) -> Self {
        Self {
            runtime: runtime.to_string(),
            target_image: target_image_for_runtime(runtime).map(|image| overrides.image(TARGET_IMAGE_NAME, image)),
            intake_image: overrides.image(INTAKE_IMAGE_NAME, DEFAULT_INTAKE_IMAGE),
        }
    }
}

/// An integration case with selected images and a separate execution procedure.
#[derive(Clone, Debug)]
pub(crate) struct IntegrationTestCase {
    pub(crate) config: IntegrationConfig,
    pub(crate) active_runtime: String,
    pub(crate) procedure: Vec<AssertionStep>,
    resolved_images: BTreeMap<String, String>,
}

impl IntegrationTestCase {
    pub(crate) fn new(config: IntegrationConfig, settings: &IntegrationSettings) -> Self {
        let mut resolved_images = BTreeMap::new();
        if let Some(image) = &settings.target_image {
            resolved_images.insert(TARGET_IMAGE_NAME.to_string(), image.clone());
        }
        if config.intake.enabled {
            resolved_images.insert(INTAKE_IMAGE_NAME.to_string(), settings.intake_image.clone());
        }
        Self {
            procedure: config.procedure.clone(),
            config,
            active_runtime: settings.runtime.clone(),
            resolved_images,
        }
    }

    /// Returns the image the named container runs, or `None` when this instance has no such
    /// container.
    pub(crate) fn image(&self, name: &str) -> Option<&str> {
        self.resolved_images.get(name).map(String::as_str)
    }

    /// Replaces `{{PANORAMIC_DYNAMIC_*}}` placeholders in all assertion steps.
    pub(crate) fn resolve_dynamic_vars(&mut self, vars: &HashMap<String, String>) {
        for step in &mut self.procedure {
            crate::dynamic_vars::resolve_step(step, vars);
        }
    }

    /// Returns any unresolved `{{PANORAMIC_DYNAMIC_*}}` placeholders across all assertion steps.
    pub(crate) fn unresolved_placeholders(&self) -> Vec<String> {
        self.procedure
            .iter()
            .flat_map(crate::dynamic_vars::unresolved_step)
            .collect()
    }

    /// Count total individual assertions across all steps.
    pub(crate) fn total_assertion_count(&self) -> usize {
        self.procedure
            .iter()
            .map(|step| match step {
                AssertionStep::Single(_) | AssertionStep::Action(_) => 1,
                AssertionStep::Parallel { parallel } => parallel.len(),
            })
            .sum()
    }
}

#[async_trait]
impl Test for IntegrationTestCase {
    fn name(&self) -> String {
        self.config.name.clone()
    }

    fn suite(&self) -> TestSuite {
        TestSuite::Integration
    }

    fn description(&self) -> Option<String> {
        self.config.description.clone()
    }

    fn case_path(&self) -> PathBuf {
        self.config.loaded_from.parent().unwrap_or(Path::new("")).to_path_buf()
    }

    fn timeout(&self) -> Duration {
        self.config.timeout.0
    }

    fn images(&self) -> BTreeMap<&str, String> {
        self.resolved_images
            .iter()
            .map(|(name, image)| (name.as_str(), image.clone()))
            .collect()
    }

    fn runtime(&self) -> String {
        self.active_runtime.clone()
    }

    async fn run(&self, tctx: TestContext) -> TestResult {
        match self.active_runtime.as_str() {
            MAC_RUNTIME => {
                let mut runner = crate::unix_runner::UnixIntegrationRunner::new(self.clone(), tctx);
                runner.run().await
            }
            // Container runtimes share the Docker runner.
            _ => {
                let mut runner = crate::runner::IntegrationRunner::new(self.clone(), tctx);
                runner.run().await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ActionConfig, AssertionConfig, LINUX_RUNTIME};

    #[test]
    fn dynamic_substitution_changes_execution_steps_not_file_data() {
        // Substitution formerly mutated IntegrationConfig::procedure. Reusing the loaded data
        // must not inherit values from another execution.
        let config: IntegrationConfig = serde_yaml::from_str(
            r#"
name: dynamic
timeout: 1s
procedure:
  - action: target_exec
    command: [echo, "{{PANORAMIC_DYNAMIC_VALUE}}"]
  - assertion: log_contains
    pattern: "{{PANORAMIC_DYNAMIC_VALUE}}"
    timeout: 1s
  - parallel:
      - assertion: log_contains
        pattern: "{{PANORAMIC_DYNAMIC_LATE}}"
        timeout: 1s
"#,
        )
        .unwrap();
        let settings = IntegrationSettings::new(LINUX_RUNTIME, &ImageOverrides::new(&[]));
        let mut case = IntegrationTestCase::new(config.clone(), &settings);
        let other = IntegrationTestCase::new(config, &settings);
        assert_eq!(case.total_assertion_count(), 3);
        case.resolve_dynamic_vars(&HashMap::from([("VALUE".to_string(), "resolved".to_string())]));
        assert_eq!(case.unresolved_placeholders(), ["{{PANORAMIC_DYNAMIC_LATE}}"]);
        assert_eq!(other.unresolved_placeholders().len(), 3);
        assert_eq!(
            case.config
                .procedure
                .iter()
                .flat_map(crate::dynamic_vars::unresolved_step)
                .count(),
            3
        );
        let AssertionStep::Action(ActionConfig::TargetExec { command, .. }) = &case.procedure[0] else {
            panic!("expected action")
        };
        assert_eq!(command, &["echo", "resolved"]);
        let AssertionStep::Single(AssertionConfig::LogContains { pattern, .. }) = &case.procedure[1] else {
            panic!("expected assertion")
        };
        assert_eq!(pattern, "resolved");
        case.resolve_dynamic_vars(&HashMap::from([("LATE".to_string(), "later".to_string())]));
        assert!(case.unresolved_placeholders().is_empty());
        assert_eq!(
            case.config
                .procedure
                .iter()
                .flat_map(crate::dynamic_vars::unresolved_step)
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn assertions_execute_the_prepared_procedure() {
        use std::sync::{Arc, RwLock};

        use tokio_util::sync::CancellationToken;

        use crate::assertions::{run_assertion_steps, AssertionContext, LogBuffer, TargetCommand};

        let config = serde_yaml::from_str(
            r#"
name: prepared
timeout: 1s
procedure:
  - assertion: log_contains
    pattern: "{{PANORAMIC_DYNAMIC_VALUE}}"
    timeout: 1s
"#,
        )
        .unwrap();
        let settings = IntegrationSettings::new(LINUX_RUNTIME, &ImageOverrides::new(&[]));
        let mut case = IntegrationTestCase::new(config, &settings);
        case.resolve_dynamic_vars(&HashMap::from([("VALUE".to_string(), "resolved".to_string())]));
        let ctx = AssertionContext {
            log_buffer: Arc::new(RwLock::new(LogBuffer {
                stdout: vec!["resolved".to_string()],
                stderr: vec![],
            })),
            container_exit_token: CancellationToken::new(),
            cancel_token: CancellationToken::new(),
            port_mappings: HashMap::new(),
            container_ip: None,
            target_os: None,
            container_name: String::new(),
            is_host_process: true,
            host_process_exit_code: None,
            docker_container_exit_code: None,
            intake_host_port: None,
            core_agent_auth_token_path: None,
            adp_cli_command: TargetCommand::new(Vec::new()),
            core_agent_cli_command: TargetCommand::new(Vec::new()),
        };
        ctx.container_exit_token.cancel();
        let results = run_assertion_steps(&case, &ctx).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "{}", results[0].message);
        assert_eq!(results[0].message, "Found pattern 'resolved' in logs.");
        assert_eq!(
            case.config
                .procedure
                .iter()
                .flat_map(crate::dynamic_vars::unresolved_step)
                .collect::<Vec<_>>(),
            ["{{PANORAMIC_DYNAMIC_VALUE}}"]
        );
    }

    #[test]
    fn execution_image_lookup_matches_reporting_and_only_enabled_containers_exist() {
        let entries = [
            "container=target:custom".parse().unwrap(),
            "intake=intake:custom".parse().unwrap(),
        ];
        let settings = IntegrationSettings::new(LINUX_RUNTIME, &ImageOverrides::new(&entries));
        for enabled in [false, true] {
            let config = serde_yaml::from_str(&format!(
                "name: images\ntimeout: 1s\nprocedure: []\nintake: {{enabled: {enabled}}}\n"
            ))
            .unwrap();
            let case = IntegrationTestCase::new(config, &settings);
            assert_eq!(case.image(TARGET_IMAGE_NAME), Some("target:custom"));
            assert_eq!(case.image(INTAKE_IMAGE_NAME), enabled.then_some("intake:custom"));
            assert_eq!(case.images().len(), if enabled { 2 } else { 1 });
            for (name, image) in case.images() {
                assert_eq!(case.image(name), Some(image.as_str()));
            }
        }
    }
}
