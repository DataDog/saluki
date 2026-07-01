use std::time::{Duration, Instant};

use airlock::docker;
use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecResults},
};
use futures::TryStreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio_util::sync::CancellationToken;

use super::Action;
use crate::assertions::{AssertionContext, AssertionResult};

pub(super) struct ContainerExecAction {
    command: Vec<String>,
    timeout: Duration,
}

impl ContainerExecAction {
    pub(super) fn new(command: Vec<String>, timeout: Duration) -> Self {
        Self { command, timeout }
    }
}

#[async_trait::async_trait]
impl Action for ContainerExecAction {
    fn name(&self) -> &'static str {
        "container_exec"
    }

    fn description(&self) -> String {
        format!("Run command in target container: {}", self.command.join(" "))
    }

    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let result = if ctx.is_host_process {
            Err(generic_error!(
                "container_exec is only supported for container runtimes."
            ))
        } else {
            exec_with_timeout(
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

async fn exec_with_timeout(
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

        match exec_collect(container_name, command.to_vec()).await {
            Ok(output) => return Ok(output),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

async fn exec_collect(container_name: &str, cmd: Vec<String>) -> Result<String, GenericError> {
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

#[cfg(test)]
mod tests {
    use super::ContainerExecAction;
    use crate::actions::Action as _;

    #[test]
    fn description_includes_command() {
        let action = ContainerExecAction::new(
            vec!["pwsh".to_string(), "-File".to_string(), "/send.ps1".to_string()],
            std::time::Duration::from_secs(5),
        );

        assert_eq!(
            action.description(),
            "Run command in target container: pwsh -File /send.ps1"
        );
    }
}
