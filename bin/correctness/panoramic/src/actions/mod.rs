use saluki_error::GenericError;

use crate::assertions::{AssertionContext, AssertionResult};
use crate::config::ActionConfig;

mod container_exec;
mod core_agent_config_set;

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
        ActionConfig::ContainerExec { command, timeout } => Ok(Box::new(container_exec::ContainerExecAction::new(
            command.clone(),
            timeout.0,
        ))),
    }
}

/// Returns the default Core Agent runtime config endpoint template.
pub fn default_core_agent_config_endpoint_template() -> String {
    DEFAULT_CORE_AGENT_CONFIG_ENDPOINT_TEMPLATE.to_string()
}
