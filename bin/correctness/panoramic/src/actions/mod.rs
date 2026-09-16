use saluki_error::GenericError;

use crate::assertions::{AssertionContext, AssertionResult};
use crate::config::ActionConfig;

mod core_agent_config_set;
mod dogstatsd_replay;
mod dogstatsd_send;
mod target_cli;
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
        ActionConfig::AdpCli { args, timeout } => Ok(Box::new(target_cli::TargetCliAction::new(
            target_cli::TargetCli::Adp,
            args.clone(),
            None,
            timeout.0,
        ))),
        ActionConfig::DogstatsdReplay {
            sender,
            capture_duration,
            stats_duration_secs,
            expected_metrics,
            timeout,
        } => Ok(Box::new(dogstatsd_replay::DogstatsdReplayAction::new(
            sender.clone(),
            capture_duration.0,
            *stats_duration_secs,
            expected_metrics.clone(),
            timeout.0,
        ))),
        ActionConfig::CoreAgentCli {
            args,
            output_contains,
            timeout,
        } => Ok(Box::new(target_cli::TargetCliAction::new(
            target_cli::TargetCli::CoreAgent,
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
        ActionConfig::DogstatsdSend { payload, port, timeout } => Ok(Box::new(
            dogstatsd_send::DogstatsdSendAction::new(payload.clone(), *port, timeout.0),
        )),
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
            intake_host_port: None,
            core_agent_auth_token_path: None,
            adp_cli_command,
            core_agent_cli_command,
        }
    }

    #[tokio::test]
    async fn dogstatsd_send_fails_for_a_windows_target() {
        let action = create_action(&ActionConfig::DogstatsdSend {
            payload: "panoramic.test:1|c".to_string(),
            port: 8125,
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let mut ctx = host_context_with_commands(TargetCommand::new(Vec::new()), TargetCommand::new(Vec::new()));
        ctx.is_host_process = false;
        ctx.target_os = Some(airlock::driver::ContainerOs::Windows);
        // Windows runtimes map exposed ports to themselves, which is what makes a host-side send
        // look like it worked.
        ctx.port_mappings = HashMap::from([("8125/udp".to_string(), 8125)]);

        let result = action.execute(&ctx).await;

        assert!(!result.passed, "Windows target unexpectedly passed: {}", result.message);
        assert!(
            result.message.contains("Windows"),
            "unexpected message: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn adp_cli_passes_prefix_literal_arguments_and_host_environment_to_child() {
        let action = create_action(&ActionConfig::AdpCli {
            args: vec!["yaml arg; printf not-interpreted".to_string()],
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let adp_command = TargetCommand::new(vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s|%s|%s' \"$0\" \"$1\" \"$ADP_CLI_TEST_ENV\"".to_string(),
            "prefix-arg".to_string(),
        ])
        .with_host_env(HashMap::from([(
            "ADP_CLI_TEST_ENV".to_string(),
            "environment-value".to_string(),
        )]));
        let ctx = host_context_with_commands(adp_command, TargetCommand::new(Vec::new()));

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

    #[tokio::test]
    async fn core_agent_cli_selects_its_target_and_applies_redacted_output_matching() {
        let core_command = TargetCommand::new(vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s' \"$1\"".to_string(),
            "core-agent-prefix".to_string(),
        ]);
        let ctx = host_context_with_commands(
            TargetCommand::new(vec!["panoramic-wrong-cli-program".to_string()]),
            core_command,
        );
        let passing = create_action(&ActionConfig::CoreAgentCli {
            args: vec!["selected-output".to_string()],
            output_contains: Some("selected-output".to_string()),
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let matcher_secret = "matcher-secret-that-must-not-appear";
        let failing = create_action(&ActionConfig::CoreAgentCli {
            args: vec!["selected-output".to_string()],
            output_contains: Some(matcher_secret.to_string()),
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");

        let pass_result = passing.execute(&ctx).await;
        let fail_result = failing.execute(&ctx).await;

        assert!(pass_result.passed, "unexpected failure: {}", pass_result.message);
        assert!(!fail_result.passed, "non-matching output unexpectedly passed");
        assert!(fail_result.message.contains("selected-output"));
        assert!(!fail_result.message.contains(matcher_secret));
    }

    #[test]
    fn target_cli_descriptions_are_fixed_and_redacted() {
        let argument_secret = "argument-secret-that-must-not-appear";
        let matcher_secret = "matcher-secret-that-must-not-appear";
        let adp = create_action(&ActionConfig::AdpCli {
            args: vec![argument_secret.to_string()],
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");
        let core_agent = create_action(&ActionConfig::CoreAgentCli {
            args: vec![argument_secret.to_string()],
            output_contains: Some(matcher_secret.to_string()),
            timeout: HumanDuration(Duration::from_secs(5)),
        })
        .expect("action should be created");

        assert_eq!(adp.description(), "Run tested ADP CLI command");
        assert_eq!(core_agent.description(), "Run Core Agent CLI command");
        for description in [adp.description(), core_agent.description()] {
            assert!(!description.contains(argument_secret));
            assert!(!description.contains(matcher_secret));
        }
    }
}
