use std::time::{Duration, Instant};

use super::{
    target_exec::{execute_target_command, CommandDiagnostics},
    Action,
};
use crate::assertions::{AssertionContext, AssertionResult, TargetCommand};

#[derive(Clone, Copy)]
pub(super) enum TargetCli {
    Adp,
    CoreAgent,
}

impl TargetCli {
    fn name(self) -> &'static str {
        match self {
            Self::Adp => "adp_cli",
            Self::CoreAgent => "core_agent_cli",
        }
    }

    fn diagnostic_label(self) -> &'static str {
        match self {
            Self::Adp => "tested ADP CLI command",
            Self::CoreAgent => "Core Agent CLI command",
        }
    }

    fn command(self, ctx: &AssertionContext) -> &TargetCommand {
        match self {
            Self::Adp => &ctx.adp_cli_command,
            Self::CoreAgent => &ctx.core_agent_cli_command,
        }
    }
}

pub(super) struct TargetCliAction {
    target: TargetCli,
    args: Vec<String>,
    output_contains: Option<String>,
    timeout: Duration,
}

impl TargetCliAction {
    pub(super) fn new(
        target: TargetCli, args: Vec<String>, output_contains: Option<String>, timeout: Duration,
    ) -> Self {
        Self {
            target,
            args,
            output_contains,
            timeout,
        }
    }
}

#[async_trait::async_trait]
impl Action for TargetCliAction {
    fn name(&self) -> &'static str {
        self.target.name()
    }

    fn description(&self) -> String {
        format!("Run {}", self.target.diagnostic_label())
    }

    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let target_command = self.target.command(ctx);
        let command = target_command.with_args(&self.args);
        let diagnostics = CommandDiagnostics::redacted(self.target.diagnostic_label());
        let result = execute_target_command(ctx, &command, &diagnostics, self.timeout, target_command.host_env()).await;

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
                        format!(
                            "{} output did not contain the configured value.",
                            self.target.diagnostic_label()
                        )
                    } else {
                        format!(
                            "{} output did not contain the configured value. Output: {}",
                            self.target.diagnostic_label(),
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
