use std::{
    collections::HashMap,
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

pub(crate) enum CommandDiagnostics {
    FullCommand(String),
    Redacted(&'static str),
}

impl CommandDiagnostics {
    fn full(command: &[String]) -> Self {
        Self::FullCommand(command.join(" "))
    }

    fn validation_subject(&self) -> &str {
        match self {
            Self::FullCommand(_) => "target_exec command",
            Self::Redacted(label) => label,
        }
    }

    fn host_subject(&self) -> String {
        match self {
            Self::FullCommand(command) => format!("host command '{command}'"),
            Self::Redacted(label) => label.to_string(),
        }
    }

    fn container_subject(&self, container_name: &str) -> String {
        match self {
            Self::FullCommand(command) => format!("command in container '{container_name}': {command}"),
            Self::Redacted(label) => format!("{label} in container '{container_name}'"),
        }
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
        let diagnostics = CommandDiagnostics::full(&self.command);
        let result = execute_target_command(ctx, &self.command, &diagnostics, self.timeout, None).await;

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

pub(crate) async fn execute_target_command(
    ctx: &AssertionContext, command: &[String], diagnostics: &CommandDiagnostics, timeout: Duration,
    host_env: Option<&HashMap<String, String>>,
) -> Result<String, GenericError> {
    if ctx.is_host_process {
        match host_env {
            Some(host_env) => {
                exec_on_host_with_env_timeout(
                    command,
                    diagnostics,
                    host_env,
                    timeout,
                    &ctx.cancel_token,
                    &ctx.container_exit_token,
                )
                .await
            }
            None => {
                exec_on_host_with_timeout(
                    command,
                    diagnostics,
                    timeout,
                    &ctx.cancel_token,
                    &ctx.container_exit_token,
                )
                .await
            }
        }
    } else {
        exec_in_container_with_timeout(
            &ctx.container_name,
            command,
            diagnostics,
            timeout,
            &ctx.cancel_token,
            &ctx.container_exit_token,
        )
        .await
    }
}

async fn exec_in_container_with_timeout(
    container_name: &str, command: &[String], diagnostics: &CommandDiagnostics, timeout: Duration,
    cancel_token: &CancellationToken, exit_token: &CancellationToken,
) -> Result<String, GenericError> {
    if command.is_empty() {
        return Err(generic_error!(
            "{} must not be empty.",
            diagnostics.validation_subject()
        ));
    }

    tokio::select! {
        _ = cancel_token.cancelled() => Err(generic_error!("Action cancelled.")),
        _ = exit_token.cancelled() => Err(generic_error!("Action cancelled because the target exited.")),
        result = tokio::time::timeout(timeout, exec_in_container_collect(container_name, command.to_vec())) => match result {
            Ok(result) => result,
            Err(_) => Err(generic_error!("Timed out running {}.", diagnostics.container_subject(container_name))),
        }
    }
}

async fn exec_on_host_with_timeout(
    command: &[String], diagnostics: &CommandDiagnostics, timeout: Duration, cancel_token: &CancellationToken,
    exit_token: &CancellationToken,
) -> Result<String, GenericError> {
    if command.is_empty() {
        return Err(generic_error!(
            "{} must not be empty.",
            diagnostics.validation_subject()
        ));
    }

    tokio::select! {
        _ = cancel_token.cancelled() => Err(generic_error!("Action cancelled.")),
        _ = exit_token.cancelled() => Err(generic_error!("Action cancelled because the target exited.")),
        result = tokio::time::timeout(timeout, exec_on_host_collect(command.to_vec(), diagnostics)) => match result {
            Ok(result) => result,
            Err(_) => Err(generic_error!("Timed out running {}.", diagnostics.host_subject())),
        }
    }
}

async fn exec_on_host_with_env_timeout(
    command: &[String], diagnostics: &CommandDiagnostics, host_env: &HashMap<String, String>, timeout: Duration,
    cancel_token: &CancellationToken, exit_token: &CancellationToken,
) -> Result<String, GenericError> {
    if command.is_empty() {
        return Err(generic_error!(
            "{} must not be empty.",
            diagnostics.validation_subject()
        ));
    }

    tokio::select! {
        _ = cancel_token.cancelled() => Err(generic_error!("Action cancelled.")),
        _ = exit_token.cancelled() => Err(generic_error!("Action cancelled because the target exited.")),
        result = tokio::time::timeout(timeout, exec_on_host_collect_with_env(command.to_vec(), diagnostics, host_env)) => match result {
            Ok(result) => result,
            Err(_) => Err(generic_error!("Timed out running {}.", diagnostics.host_subject())),
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

async fn exec_on_host_collect(cmd: Vec<String>, diagnostics: &CommandDiagnostics) -> Result<String, GenericError> {
    let (program, args) = cmd
        .split_first()
        .ok_or_else(|| generic_error!("{} must not be empty.", diagnostics.validation_subject()))?;

    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_error_context(|| format!("Failed to run {}.", diagnostics.host_subject()))?;

    collect_host_output(output)
}

async fn exec_on_host_collect_with_env(
    cmd: Vec<String>, diagnostics: &CommandDiagnostics, host_env: &HashMap<String, String>,
) -> Result<String, GenericError> {
    let (program, args) = cmd
        .split_first()
        .ok_or_else(|| generic_error!("{} must not be empty.", diagnostics.validation_subject()))?;

    let output = Command::new(program)
        .args(args)
        .envs(host_env)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_error_context(|| format!("Failed to run {}.", diagnostics.host_subject()))?;

    collect_host_output(output)
}

fn collect_host_output(output: std::process::Output) -> Result<String, GenericError> {
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
    use super::{exec_on_host_collect, exec_on_host_with_timeout, CommandDiagnostics, TargetExecAction};
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
        let command = vec!["sh".to_string(), "-c".to_string(), "printf target-exec".to_string()];
        let diagnostics = CommandDiagnostics::full(&command);
        let output = exec_on_host_collect(command, &diagnostics)
            .await
            .expect("host command should succeed");

        assert_eq!(output, "target-exec");
    }

    #[tokio::test]
    async fn host_exec_reports_nonzero_exit_immediately() {
        let command = [
            "sh".to_string(),
            "-c".to_string(),
            "printf fail-message >&2; exit 7".to_string(),
        ];
        let diagnostics = CommandDiagnostics::full(&command);
        let started = std::time::Instant::now();
        let err = exec_on_host_with_timeout(
            &command,
            &diagnostics,
            std::time::Duration::from_secs(30),
            &tokio_util::sync::CancellationToken::new(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("host command should fail");

        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(err.to_string().contains("fail-message"));
    }

    fn slow_host_command() -> Vec<String> {
        #[cfg(windows)]
        {
            vec![
                "powershell".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                "Start-Sleep -Seconds 5".to_string(),
            ]
        }

        #[cfg(not(windows))]
        {
            vec!["sh".to_string(), "-c".to_string(), "sleep 5".to_string()]
        }
    }

    #[tokio::test]
    async fn host_exec_times_out() {
        let command = slow_host_command();
        let diagnostics = CommandDiagnostics::full(&command);
        let err = exec_on_host_with_timeout(
            &command,
            &diagnostics,
            std::time::Duration::from_millis(50),
            &tokio_util::sync::CancellationToken::new(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("host command should time out");

        assert!(
            err.to_string().contains("Timed out running host command"),
            "unexpected error: {err}"
        );
    }
}
