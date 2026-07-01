use std::{
    process::Stdio,
    time::{Duration, Instant},
};

use airlock::docker;
use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecResults},
};
use futures::TryStreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::Action;
use crate::assertions::{AssertionContext, AssertionResult};

pub(super) struct TargetExecAction {
    command: Vec<String>,
    timeout: Duration,
}

impl TargetExecAction {
    pub(super) fn new(command: Vec<String>, timeout: Duration) -> Self {
        Self { command, timeout }
    }
}

#[async_trait::async_trait]
impl Action for TargetExecAction {
    fn name(&self) -> &'static str {
        "target_exec"
    }

    fn description(&self) -> String {
        format!("Run command in target environment: {}", self.command.join(" "))
    }

    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let result = if ctx.is_host_process {
            exec_on_host_with_timeout(
                &self.command,
                self.timeout,
                &ctx.cancel_token,
                &ctx.container_exit_token,
            )
            .await
        } else {
            exec_in_container_with_timeout(
                &ctx.container_name,
                &self.command,
                self.timeout,
                &ctx.cancel_token,
                &ctx.container_exit_token,
            )
            .await
        };

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

async fn exec_in_container_with_timeout(
    container_name: &str, command: &[String], timeout: Duration, cancel_token: &CancellationToken,
    exit_token: &CancellationToken,
) -> Result<String, GenericError> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;

    loop {
        if cancel_token.is_cancelled() || exit_token.is_cancelled() {
            return Err(generic_error!("Action cancelled because the target exited."));
        }
        if Instant::now() > deadline {
            return Err(match last_error {
                Some(error) => generic_error!(
                    "Timed out running command in container '{}'. Last attempt failed: {}.",
                    container_name,
                    error
                ),
                None => generic_error!(
                    "Timed out running command in container '{}'. No exec attempt completed before the deadline.",
                    container_name
                ),
            });
        }

        match exec_in_container_collect(container_name, command.to_vec()).await {
            Ok(output) => return Ok(output),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

async fn exec_on_host_with_timeout(
    command: &[String], timeout: Duration, cancel_token: &CancellationToken, exit_token: &CancellationToken,
) -> Result<String, GenericError> {
    if command.is_empty() {
        return Err(generic_error!("target_exec command must not be empty."));
    }

    tokio::select! {
        _ = cancel_token.cancelled() => Err(generic_error!("Action cancelled.")),
        _ = exit_token.cancelled() => Err(generic_error!("Action cancelled because the target exited.")),
        result = tokio::time::timeout(timeout, exec_on_host_collect(command.to_vec())) => match result {
            Ok(result) => result,
            Err(_) => Err(generic_error!("Timed out running host command '{}'.", command.join(" "))),
        }
    }
}

async fn exec_in_container_collect(container_name: &str, cmd: Vec<String>) -> Result<String, GenericError> {
    if cmd.is_empty() {
        return Err(generic_error!("target_exec command must not be empty."));
    }

    let docker = docker::connect().error_context("Failed to connect to Docker.")?;
    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions::<String> {
                cmd: Some(cmd),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .error_context("Failed to create exec.")?;
    let exec_id = exec.id.clone();
    let result = docker
        .start_exec(&exec_id, None)
        .await
        .error_context("Failed to start exec.")?;

    let mut output_text = String::new();
    if let StartExecResults::Attached { mut output, .. } = result {
        while let Some(chunk) = output.try_next().await.error_context("Failed to read exec output.")? {
            match chunk {
                LogOutput::StdOut { message } | LogOutput::StdErr { message } => {
                    output_text.push_str(&String::from_utf8_lossy(&message));
                }
                _ => {}
            }
        }
    }

    let inspect = docker
        .inspect_exec(&exec_id)
        .await
        .error_context("Failed to inspect exec.")?;
    if inspect.exit_code != Some(0) {
        return Err(generic_error!("exec exited {:?}: {}", inspect.exit_code, output_text));
    }

    Ok(output_text)
}

async fn exec_on_host_collect(cmd: Vec<String>) -> Result<String, GenericError> {
    let (program, args) = cmd
        .split_first()
        .ok_or_else(|| generic_error!("target_exec command must not be empty."))?;

    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_error_context(|| format!("Failed to run host command '{}'.", cmd.join(" ")))?;

    let mut output_text = String::new();
    output_text.push_str(&String::from_utf8_lossy(&output.stdout));
    output_text.push_str(&String::from_utf8_lossy(&output.stderr));

    if !output.status.success() {
        return Err(generic_error!("host command exited {}: {}", output.status, output_text));
    }

    Ok(output_text)
}

#[cfg(test)]
mod tests {
    use super::{exec_on_host_collect, TargetExecAction};
    use crate::actions::Action as _;

    #[test]
    fn description_includes_command() {
        let action = TargetExecAction::new(
            vec!["pwsh".to_string(), "-File".to_string(), "/send.ps1".to_string()],
            std::time::Duration::from_secs(5),
        );

        assert_eq!(
            action.description(),
            "Run command in target environment: pwsh -File /send.ps1"
        );
    }

    #[tokio::test]
    async fn host_exec_collects_stdout() {
        let output = exec_on_host_collect(vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf target-exec".to_string(),
        ])
        .await
        .expect("host command should succeed");

        assert_eq!(output, "target-exec");
    }
}
