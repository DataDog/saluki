//! `exec`: a command the case writes, run inside a participant that is already running.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::time::{timeout_at, Instant};

use super::{stopped, EXEC_SLACK};
use crate::core::case::Timeout;
use crate::core::handle::HandleError;
use crate::core::record::StepStatus;
use crate::core::step::{Need, Step, StepContext, StepFactory, StepFields, StepPurpose, StepReport};
use crate::core::template::Template;

/// The most of a command's standard output or error a message quotes: the last bytes of each.
const MAX_QUOTED_OUTPUT_BYTES: usize = 4096;

pub(super) struct ExecFactory;

impl StepFactory for ExecFactory {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn purpose(&self) -> StepPurpose {
        StepPurpose::Action
    }

    fn template_fields(&self) -> &'static [&'static str] {
        &["command", "env"]
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step = fields.decode::<RunCommand>()?;
        if step.command.is_empty() {
            return Err("command is a non-empty list: the program, then its arguments".to_string());
        }
        Ok(Box::new(step))
    }
}

/// Passes once `command` exits 0, within `timeout`. A command still running at the deadline fails the step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunCommand {
    on: String,
    command: Vec<Template>,
    #[serde(default)]
    env: BTreeMap<String, Template>,
    timeout: Timeout,
}

/// The last bytes of `output`, trimmed.
fn quoted(output: &[u8]) -> String {
    let tail = &output[output.len().saturating_sub(MAX_QUOTED_OUTPUT_BYTES)..];
    String::from_utf8_lossy(tail).trim().to_string()
}

#[async_trait]
impl Step for RunCommand {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Participant(self.on.clone())]
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let ran = async {
            let handle = cx.handle(&self.on)?;
            let argv = self
                .command
                .iter()
                .map(|argument| cx.fill(argument, false))
                .collect::<Result<Vec<_>, _>>()?;
            let mut env = BTreeMap::new();
            for (key, value) in &self.env {
                env.insert(key.clone(), cx.fill(value, false)?);
            }
            let command = argv.join(" ");
            let limit = cx.deadline.saturating_duration_since(Instant::now()) + EXEC_SLACK;
            let Ok(ended) = timeout_at(cx.deadline, handle.exec(argv, env, limit)).await else {
                let message = format!("'{}' did not exit within {}", command, self.timeout.text);
                return Ok(StepReport {
                    status: StepStatus::Failed,
                    message,
                });
            };
            let exited = ended?;
            if exited.code == 0 {
                let stdout = quoted(&exited.stdout);
                let message = if stdout.is_empty() {
                    format!("'{}' exited with code 0.", command)
                } else {
                    format!("'{}' exited with code 0 and wrote: {}", command, stdout)
                };
                return Ok(StepReport {
                    status: StepStatus::Passed,
                    message,
                });
            }
            let mut message = format!("'{}' exited with code {}.", command, exited.code);
            for (label, output) in [("standard output", &exited.stdout), ("standard error", &exited.stderr)] {
                let output = quoted(output);
                if !output.is_empty() {
                    message.push_str(&format!("\n{}:\n{}", label, output));
                }
            }
            Ok::<_, HandleError>(StepReport {
                status: StepStatus::Failed,
                message,
            })
        };
        ran.await.unwrap_or_else(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::super::scripted::{observation, run, Scripted};
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_command_that_exits_nonzero_fails_and_quotes_its_output() {
        let fields = "{on: app, command: [sh, -c, 'exit 3'], timeout: 5s}";
        let failing = Scripted {
            exit: Some((3, "boom")),
            ..Scripted::default()
        };
        let report = run(&ExecFactory, fields, &failing, &observation()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);
        assert!(report.message.contains("code 3"), "{}", report.message);
        assert!(report.message.contains("boom"), "{}", report.message);
    }
}
