use std::time::{Duration, Instant};

use super::{
    target_exec::{execute_target_command, CommandDiagnostics},
    Action,
};
use crate::assertions::{AssertionContext, AssertionResult};

const ADP_CLI_DIAGNOSTIC_LABEL: &str = "tested ADP CLI command";

pub(super) struct AdpCliAction {
    args: Vec<String>,
    timeout: Duration,
}

impl AdpCliAction {
    pub(super) fn new(args: Vec<String>, timeout: Duration) -> Self {
        Self { args, timeout }
    }
}

#[async_trait::async_trait]
impl Action for AdpCliAction {
    fn name(&self) -> &'static str {
        "adp_cli"
    }

    fn description(&self) -> String {
        format!("Run {ADP_CLI_DIAGNOSTIC_LABEL}")
    }

    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let command = ctx.adp_cli_command.with_args(&self.args);
        let diagnostics = CommandDiagnostics::redacted(ADP_CLI_DIAGNOSTIC_LABEL);
        let result = execute_target_command(
            ctx,
            &command,
            &diagnostics,
            self.timeout,
            ctx.adp_cli_command.host_env(),
        )
        .await;

        match result {
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
