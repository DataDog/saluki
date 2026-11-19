//! `exec`: a command run inside a participant that is already running.
//!
//! The command is a list the case writes, or the name of one of the participant's own commands, such as `adp`.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::time::{timeout_at, Instant};

use super::{stopped, EXEC_SLACK};
use crate::core::case::Timeout;
use crate::core::handle::{Exited, HandleError};
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
        &["command", "args", "env"]
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step = fields.decode::<RunCommand>()?;
        match &step.command {
            CommandLine::Named(name) if name.is_empty() => {
                return Err("command names one of the participant's commands, and the name is empty".to_string())
            }
            CommandLine::Argv(argv) if argv.is_empty() => {
                return Err("command is a non-empty list: the program, then its arguments".to_string())
            }
            _ => {}
        }
        Ok(Box::new(step))
    }
}

/// What `exec` runs.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CommandLine {
    /// The name of one of the participant's commands.
    Named(String),
    /// A program and its arguments.
    Argv(Vec<Template>),
}

/// Passes once the command exits 0, within `timeout`, and, when `output_contains` is set, its standard output followed
/// by its standard error holds that text. `args` follow the command's own arguments. A command still running at the
/// deadline fails the step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunCommand {
    on: String,
    command: CommandLine,
    #[serde(default)]
    args: Vec<Template>,
    #[serde(default)]
    env: BTreeMap<String, Template>,
    output_contains: Option<String>,
    timeout: Timeout,
}

/// The last bytes of `output`, trimmed.
fn quoted(output: &[u8]) -> String {
    let tail = &output[output.len().saturating_sub(MAX_QUOTED_OUTPUT_BYTES)..];
    String::from_utf8_lossy(tail).trim().to_string()
}

/// `message`, then what the command wrote to each stream.
fn with_output(mut message: String, exited: &Exited) -> String {
    for (label, output) in [("standard output", &exited.stdout), ("standard error", &exited.stderr)] {
        let output = quoted(output);
        if !output.is_empty() {
            message.push_str(&format!("\n{}:\n{}", label, output));
        }
    }
    message
}

#[async_trait]
impl Step for RunCommand {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn needs(&self) -> Vec<Need> {
        vec![match &self.command {
            CommandLine::Named(name) => Need::Command(self.on.clone(), name.clone()),
            CommandLine::Argv(_) => Need::Participant(self.on.clone()),
        }]
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let ran = async {
            let handle = cx.handle(&self.on)?;
            let mut env = BTreeMap::new();
            for (key, value) in &self.env {
                env.insert(key.clone(), cx.fill(value, false)?);
            }
            let args = self
                .args
                .iter()
                .map(|argument| cx.fill(argument, false))
                .collect::<Result<Vec<_>, _>>()?;
            let limit = cx.deadline.saturating_duration_since(Instant::now()) + EXEC_SLACK;
            let (command, ended) = match &self.command {
                CommandLine::Named(name) => {
                    let command = std::iter::once(name.as_str()).chain(args.iter().map(String::as_str));
                    let command = command.collect::<Vec<_>>().join(" ");
                    (
                        command,
                        timeout_at(cx.deadline, handle.command(name, &args, &env, limit)).await,
                    )
                }
                CommandLine::Argv(argv) => {
                    let mut argv = argv
                        .iter()
                        .map(|argument| cx.fill(argument, false))
                        .collect::<Result<Vec<_>, _>>()?;
                    argv.extend(args);
                    let command = argv.join(" ");
                    (command, timeout_at(cx.deadline, handle.exec(argv, env, limit)).await)
                }
            };
            let Ok(ended) = ended else {
                let message = format!("'{}' did not exit within {}", command, self.timeout.text);
                return Ok(StepReport {
                    status: StepStatus::Failed,
                    message,
                });
            };
            let exited = ended?;
            if exited.code != 0 {
                let message = with_output(format!("'{}' exited with code {}.", command, exited.code), &exited);
                return Ok(StepReport {
                    status: StepStatus::Failed,
                    message,
                });
            }
            if let Some(text) = &self.output_contains {
                let output = [exited.stdout.as_slice(), exited.stderr.as_slice()].concat();
                if !String::from_utf8_lossy(&output).contains(text.as_str()) {
                    let message = format!(
                        "'{}' exited with code 0, and its output does not contain '{}'.",
                        command, text
                    );
                    return Ok(StepReport {
                        status: StepStatus::Failed,
                        message: with_output(message, &exited),
                    });
                }
            }
            let stdout = quoted(&exited.stdout);
            let message = if stdout.is_empty() {
                format!("'{}' exited with code 0.", command)
            } else {
                format!("'{}' exited with code 0 and wrote: {}", command, stdout)
            };
            Ok::<_, HandleError>(StepReport {
                status: StepStatus::Passed,
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
