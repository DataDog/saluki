use std::time::{Duration, Instant};

use super::{
    target_exec::{execute_target_command, CommandDiagnostics},
    Action,
};
use crate::assertions::{AssertionContext, AssertionResult};

const CORE_AGENT_CLI_DIAGNOSTIC_LABEL: &str = "Core Agent CLI command";

pub(super) struct CoreAgentCliAction {
    args: Vec<String>,
    output_contains: Option<String>,
    timeout: Duration,
}

impl CoreAgentCliAction {
    pub(super) fn new(args: Vec<String>, output_contains: Option<String>, timeout: Duration) -> Self {
        Self {
            args,
            output_contains,
            timeout,
        }
    }
}

#[async_trait::async_trait]
impl Action for CoreAgentCliAction {
    fn name(&self) -> &'static str {
        "core_agent_cli"
    }

    fn description(&self) -> String {
        format!("Run {CORE_AGENT_CLI_DIAGNOSTIC_LABEL}")
    }

    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let command = ctx.core_agent_cli_command.with_args(&self.args);
        let diagnostics = CommandDiagnostics::redacted(CORE_AGENT_CLI_DIAGNOSTIC_LABEL);
        let result = execute_target_command(
            ctx,
            &command,
            &diagnostics,
            self.timeout,
            ctx.core_agent_cli_command.host_env(),
        )
        .await;

        match result {
            Ok(output)
                if self
                    .output_contains
                    .as_ref()
                    .is_some_and(|expected| !output.contains(expected)) =>
            {
                AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: if output.trim().is_empty() {
                        format!("{CORE_AGENT_CLI_DIAGNOSTIC_LABEL} output did not contain the configured value.")
                    } else {
                        format!(
                            "{CORE_AGENT_CLI_DIAGNOSTIC_LABEL} output did not contain the configured value. Output: {}",
                            output.trim()
                        )
                    },
                    duration: started.elapsed(),
                }
            }
            Ok(output) => AssertionResult {
                name: self.name().to_string(),
                passed: true,
                message: if output.trim().is_empty() {
                    self.description()
                } else {
                    format!("{} Output: {}", self.description(), output.trim())
                },
                duration: started.elapsed(),
            },
            Err(e) => AssertionResult {
                name: self.name().to_string(),
                passed: false,
                message: e.to_string(),
                duration: started.elapsed(),
            },
        }
    }
}
