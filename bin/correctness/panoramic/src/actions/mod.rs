use saluki_error::GenericError;

use crate::assertions::{AssertionContext, AssertionResult};
use crate::config::ActionConfig;

mod adp_cli;
mod core_agent_config_set;
mod target_exec;

const DEFAULT_CORE_AGENT_CONFIG_ENDPOINT_TEMPLATE: &str = "https://localhost:55001/agent/config/{key}";

/// Trait for integration-test actions.
#[async_trait::async_trait]
pub trait Action: Send + Sync {
    /// Returns the name of this action type.
    fn name(&self) -> &'static str;

    /// Returns a human-readable description of what this action does.
    fn description(&self) -> String;

    /// Executes the action and returns a result shaped like an assertion result.
    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult;
}

/// Creates an action from its configuration.
pub fn create_action(config: &ActionConfig) -> Result<Box<dyn Action>, GenericError> {
    match config {
        ActionConfig::AdpCli { args, timeout } => Ok(Box::new(adp_cli::AdpCliAction::new(args.clone(), timeout.0))),
        ActionConfig::CoreAgentConfigSet {
            key,
            value,
            endpoint,
            timeout,
        } => Ok(Box::new(core_agent_config_set::CoreAgentConfigSetAction::new(
            key.clone(),
            value.clone(),
            endpoint.clone(),
            timeout.0,
        ))),
        ActionConfig::TargetExec { command, timeout } => {
            Ok(Box::new(target_exec::TargetExecAction::new(command.clone(), timeout.0)))
        }
    }
}

/// Returns the default Core Agent runtime config endpoint template.
pub fn default_core_agent_config_endpoint_template() -> String {
    DEFAULT_CORE_AGENT_CONFIG_ENDPOINT_TEMPLATE.to_string()
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, RwLock},
        time::Duration,
    };

    use tokio_util::sync::CancellationToken;

    use super::create_action;
    use crate::{
        assertions::{AssertionContext, LogBuffer, TargetCommand},
        config::{ActionConfig, HumanDuration},
    };

    fn host_context(command_prefix: Vec<String>, env: HashMap<String, String>) -> AssertionContext {
        AssertionContext {
            log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
            container_exit_token: CancellationToken::new(),
            cancel_token: CancellationToken::new(),
            port_mappings: HashMap::new(),
            container_ip: None,
            target_os: None,
            container_name: "adp-cli-action-test".to_string(),
            is_host_process: true,
            host_process_exit_code: None,
            docker_container_exit_code: None,
            core_agent_auth_token_path: None,
            adp_cli_command: TargetCommand::new(command_prefix).with_host_env(env),
        }
    }

    #[tokio::test]
    async fn adp_cli_host_action_passes_prefix_args_yaml_args_and_environment_to_real_child() {
        let action = create_action(&ActionConfig::AdpCli {
            args: vec!["yaml arg; printf not-interpreted".to_string()],
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let ctx = host_context(
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "printf '%s|%s|%s' \"$0\" \"$1\" \"$ADP_CLI_TEST_ENV\"".to_string(),
                "prefix-arg".to_string(),
            ],
            HashMap::from([("ADP_CLI_TEST_ENV".to_string(), "environment-value".to_string())]),
        );

        let result = action.execute(&ctx).await;

        assert!(result.passed, "unexpected failure: {}", result.message);
        assert!(
            result
                .message
                .contains("prefix-arg|yaml arg; printf not-interpreted|environment-value"),
            "unexpected output: {}",
            result.message
        );
    }

    #[test]
    fn adp_cli_description_does_not_expose_secret_argument() {
        let secret = "dynamic-credential-description-secret";
        let action = create_action(&ActionConfig::AdpCli {
            args: vec![secret.to_string()],
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");

        let description = action.description();

        assert_eq!(description, "Run tested ADP CLI command");
        assert!(!description.contains(secret), "description exposed the secret argument");
    }

    #[tokio::test]
    async fn adp_cli_spawn_failure_does_not_expose_secret_argument() {
        let secret = "dynamic-credential-spawn-secret";
        let action = create_action(&ActionConfig::AdpCli {
            args: vec![secret.to_string()],
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let ctx = host_context(
            vec!["panoramic-adp-cli-program-that-does-not-exist".to_string()],
            HashMap::new(),
        );

        let result = action.execute(&ctx).await;

        assert!(!result.passed, "missing child unexpectedly passed");
        assert!(
            result.message.contains("Failed to run tested ADP CLI command."),
            "unexpected error: {}",
            result.message
        );
        assert!(
            !result.message.contains(secret),
            "spawn error exposed the secret argument"
        );
    }

    #[tokio::test]
    async fn adp_cli_timeout_does_not_expose_secret_argument() {
        let secret = "dynamic-credential-timeout-secret";
        let action = create_action(&ActionConfig::AdpCli {
            args: vec![secret.to_string()],
            timeout: HumanDuration(Duration::from_millis(50)),
        })
        .expect("action should be created");
        let ctx = host_context(
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "sleep 5".to_string(),
                "adp-cli-timeout".to_string(),
            ],
            HashMap::new(),
        );
        let started = std::time::Instant::now();

        let result = action.execute(&ctx).await;

        assert!(!result.passed, "slow child unexpectedly passed");
        assert!(
            result.message.contains("Timed out running tested ADP CLI command."),
            "unexpected error: {}",
            result.message
        );
        assert!(
            !result.message.contains(secret),
            "timeout error exposed the secret argument"
        );
        assert!(started.elapsed() < Duration::from_secs(2), "timeout was not bounded");
    }

    #[tokio::test]
    async fn adp_cli_host_action_fails_on_nonzero_without_describing_environment_values() {
        let action = create_action(&ActionConfig::AdpCli {
            args: Vec::new(),
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let secret = "must-not-appear-in-action-errors";
        let ctx = host_context(
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "printf child-failed >&2; exit 7".to_string(),
            ],
            HashMap::from([("ADP_CLI_TEST_SECRET".to_string(), secret.to_string())]),
        );

        let result = action.execute(&ctx).await;

        assert!(!result.passed, "nonzero child unexpectedly passed");
        assert!(
            result.message.contains("child-failed"),
            "unexpected error: {}",
            result.message
        );
        assert!(
            !result.message.contains(secret),
            "error exposed the supplied environment"
        );
        assert!(
            !action.description().contains(secret),
            "description exposed the supplied environment"
        );
    }
}
