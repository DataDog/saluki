use saluki_error::GenericError;

use crate::assertions::{AssertionContext, AssertionResult};
use crate::config::ActionConfig;

mod adp_cli;
mod core_agent_cli;
mod core_agent_config_set;
mod target_exec;

pub(crate) use target_exec::{execute_target_command, CommandDiagnostics};

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
        ActionConfig::CoreAgentCli {
            args,
            output_contains,
            timeout,
        } => Ok(Box::new(core_agent_cli::CoreAgentCliAction::new(
            args.clone(),
            output_contains.clone(),
            timeout.0,
        ))),
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
        let command = TargetCommand::new(command_prefix).with_host_env(env);
        host_context_with_commands(command.clone(), command)
    }

    fn host_context_with_commands(
        adp_cli_command: TargetCommand, core_agent_cli_command: TargetCommand,
    ) -> AssertionContext {
        AssertionContext {
            log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
            container_exit_token: CancellationToken::new(),
            cancel_token: CancellationToken::new(),
            port_mappings: HashMap::new(),
            container_ip: None,
            target_os: None,
            container_name: "cli-action-test".to_string(),
            is_host_process: true,
            host_process_exit_code: None,
            docker_container_exit_code: None,
            core_agent_auth_token_path: None,
            adp_cli_command,
            core_agent_cli_command,
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

    #[tokio::test]
    async fn core_agent_cli_host_action_passes_prefix_args_yaml_args_and_environment_to_real_child() {
        let expected_output = "prefix-arg|yaml arg; printf not-interpreted|environment-value";
        let action = create_action(&ActionConfig::CoreAgentCli {
            args: vec!["yaml arg; printf not-interpreted".to_string()],
            output_contains: Some(expected_output.to_string()),
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let core_agent_cli_command = TargetCommand::new(vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s|%s|%s' \"$0\" \"$1\" \"$CORE_AGENT_CLI_TEST_ENV\"".to_string(),
            "prefix-arg".to_string(),
        ])
        .with_host_env(HashMap::from([(
            "CORE_AGENT_CLI_TEST_ENV".to_string(),
            "environment-value".to_string(),
        )]));
        let ctx = host_context_with_commands(
            TargetCommand::new(vec!["panoramic-wrong-cli-program".to_string()]),
            core_agent_cli_command,
        );

        let result = action.execute(&ctx).await;

        assert!(result.passed, "unexpected failure: {}", result.message);
        assert!(
            result.message.contains(expected_output),
            "unexpected output: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn core_agent_cli_output_matcher_fails_without_exposing_expected_value() {
        let expected_secret = "matcher-secret-that-must-not-appear";
        let action = create_action(&ActionConfig::CoreAgentCli {
            args: Vec::new(),
            output_contains: Some(expected_secret.to_string()),
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let ctx = host_context(
            vec!["sh".to_string(), "-c".to_string(), "printf actual-output".to_string()],
            HashMap::new(),
        );

        let result = action.execute(&ctx).await;

        assert!(!result.passed, "non-matching output unexpectedly passed");
        assert!(result.message.contains("actual-output"), "captured output was lost");
        assert!(!result.message.contains(expected_secret), "matcher value was exposed");
    }

    #[tokio::test]
    async fn core_agent_cli_description_and_spawn_failure_use_safe_diagnostics() {
        let argument_secret = "dynamic-credential-argument-secret";
        let matcher_secret = "dynamic-credential-matcher-secret";
        let action = create_action(&ActionConfig::CoreAgentCli {
            args: vec![argument_secret.to_string()],
            output_contains: Some(matcher_secret.to_string()),
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let ctx = host_context(
            vec!["panoramic-core-agent-cli-program-that-does-not-exist".to_string()],
            HashMap::new(),
        );

        assert_eq!(action.description(), "Run Core Agent CLI command");
        let result = action.execute(&ctx).await;

        assert!(!result.passed, "missing child unexpectedly passed");
        assert!(
            result.message.contains("Failed to run Core Agent CLI command."),
            "unexpected error: {}",
            result.message
        );
        assert!(
            !result.message.contains(argument_secret),
            "spawn error exposed an argument"
        );
        assert!(
            !result.message.contains(matcher_secret),
            "spawn error exposed the matcher"
        );
    }

    #[tokio::test]
    async fn core_agent_cli_timeout_uses_safe_diagnostics() {
        let argument_secret = "dynamic-credential-timeout-argument-secret";
        let matcher_secret = "dynamic-credential-timeout-matcher-secret";
        let action = create_action(&ActionConfig::CoreAgentCli {
            args: vec![argument_secret.to_string()],
            output_contains: Some(matcher_secret.to_string()),
            timeout: HumanDuration(Duration::from_millis(50)),
        })
        .expect("action should be created");
        let ctx = host_context(
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "sleep 5".to_string(),
                "core-agent-cli-timeout".to_string(),
            ],
            HashMap::new(),
        );
        let started = std::time::Instant::now();

        let result = action.execute(&ctx).await;

        assert!(!result.passed, "slow child unexpectedly passed");
        assert!(
            result.message.contains("Timed out running Core Agent CLI command."),
            "unexpected error: {}",
            result.message
        );
        assert!(
            !result.message.contains(argument_secret),
            "timeout error exposed an argument"
        );
        assert!(
            !result.message.contains(matcher_secret),
            "timeout error exposed the matcher"
        );
        assert!(started.elapsed() < Duration::from_secs(2), "timeout was not bounded");
    }

    #[tokio::test]
    async fn core_agent_cli_nonzero_failure_preserves_output_without_describing_environment_values() {
        let action = create_action(&ActionConfig::CoreAgentCli {
            args: Vec::new(),
            output_contains: None,
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let environment_secret = "must-not-appear-in-core-agent-cli-errors";
        let ctx = host_context(
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "printf child-failed >&2; exit 7".to_string(),
            ],
            HashMap::from([("CORE_AGENT_CLI_TEST_SECRET".to_string(), environment_secret.to_string())]),
        );

        let result = action.execute(&ctx).await;

        assert!(!result.passed, "nonzero child unexpectedly passed");
        assert!(
            result.message.contains("child-failed"),
            "unexpected error: {}",
            result.message
        );
        assert!(
            !result.message.contains(environment_secret),
            "error exposed the environment"
        );
        assert!(
            !action.description().contains(environment_secret),
            "description exposed the environment"
        );
    }
}
