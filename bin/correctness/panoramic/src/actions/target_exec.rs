use std::{collections::HashMap, fmt, process::Stdio, time::Duration};

use airlock::docker;
use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecResults},
};
use futures::TryStreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::process::Command;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::Action;
use crate::assertions::{AssertionContext, AssertionResult, AssertionStatus};

pub(super) struct TargetExecAction {
    command: Vec<String>,
    timeout: Duration,
}

impl TargetExecAction {
    pub(super) fn new(command: Vec<String>, timeout: Duration) -> Self {
        Self { command, timeout }
    }
}

enum CommandSubject {
    FullCommand(String),
    Redacted(&'static str),
}

#[derive(Clone, Copy)]
enum ChildOutputPolicy {
    Include,
    Omit,
}

pub(crate) struct CommandDiagnostics {
    subject: CommandSubject,
    child_output: ChildOutputPolicy,
}

impl CommandDiagnostics {
    fn full(command: &[String]) -> Self {
        Self {
            subject: CommandSubject::FullCommand(command.join(" ")),
            child_output: ChildOutputPolicy::Include,
        }
    }

    pub(crate) fn redacted(label: &'static str) -> Self {
        Self {
            subject: CommandSubject::Redacted(label),
            child_output: ChildOutputPolicy::Include,
        }
    }

    pub(crate) fn redacted_without_child_output(label: &'static str) -> Self {
        Self {
            subject: CommandSubject::Redacted(label),
            child_output: ChildOutputPolicy::Omit,
        }
    }

    fn validation_subject(&self) -> &str {
        match &self.subject {
            CommandSubject::FullCommand(_) => "target_exec command",
            CommandSubject::Redacted(label) => label,
        }
    }

    fn host_subject(&self) -> String {
        match &self.subject {
            CommandSubject::FullCommand(command) => format!("host command '{command}'"),
            CommandSubject::Redacted(label) => label.to_string(),
        }
    }

    fn container_subject(&self, container_name: &str) -> String {
        match &self.subject {
            CommandSubject::FullCommand(command) => format!("command in container '{container_name}': {command}"),
            CommandSubject::Redacted(label) => format!("{label} in container '{container_name}'"),
        }
    }

    fn includes_child_output(&self) -> bool {
        matches!(self.child_output, ChildOutputPolicy::Include)
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
                status: AssertionStatus::Passed,
                message: if output.trim().is_empty() {
                    self.description()
                } else {
                    format!("{} Output: {}", self.description(), output.trim())
                },
                duration: started.elapsed(),
            },
            Err(e) => AssertionResult {
                name: self.name().to_string(),
                status: e.status(),
                message: e.to_string(),
                duration: started.elapsed(),
            },
        }
    }
}

/// Why a target command didn't succeed.
#[derive(Debug)]
pub(crate) enum CommandError {
    /// The command was empty, host process setup or output collection failed, or the Docker API failed.
    Harness(GenericError),
    /// The command exited nonzero, timed out, or was interrupted.
    Failed(GenericError),
}

impl CommandError {
    /// Returns `Errored` for [`CommandError::Harness`] and `Failed` otherwise.
    pub(crate) fn status(&self) -> AssertionStatus {
        match self {
            Self::Harness(_) => AssertionStatus::Errored,
            Self::Failed(_) => AssertionStatus::Failed,
        }
    }
}

/// Renders the whole error chain, so a step message keeps the cause.
impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Harness(error) | Self::Failed(error) => write!(f, "{error:#}"),
        }
    }
}

/// Runs `command` in the target environment within `timeout`, returning its combined stdout and stderr.
///
/// # Errors
///
/// Returns [`CommandError::Harness`] for an empty command, host process I/O errors, or Docker API failures.
/// Returns [`CommandError::Failed`] on nonzero exit, timeout, test cancellation, or target exit.
pub(crate) async fn execute_target_command(
    ctx: &AssertionContext, command: &[String], diagnostics: &CommandDiagnostics, timeout: Duration,
    host_env: Option<&HashMap<String, String>>,
) -> Result<String, CommandError> {
    if command.is_empty() {
        return Err(CommandError::Harness(generic_error!(
            "{} must not be empty.",
            diagnostics.validation_subject()
        )));
    }

    if ctx.is_host_process {
        exec_on_host_with_timeout(
            command,
            diagnostics,
            host_env,
            timeout,
            &ctx.cancel_token,
            &ctx.container_exit_token,
        )
        .await
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
) -> Result<String, CommandError> {
    tokio::select! {
        _ = cancel_token.cancelled() => Err(CommandError::Failed(generic_error!("Action cancelled."))),
        _ = exit_token.cancelled() => Err(CommandError::Failed(generic_error!("Action cancelled because the target exited."))),
        result = tokio::time::timeout(timeout, exec_in_container_collect(container_name, command, diagnostics)) => match result {
            Ok(result) => result,
            Err(_) => Err(CommandError::Failed(generic_error!("Timed out running {}.", diagnostics.container_subject(container_name)))),
        }
    }
}

async fn exec_on_host_with_timeout(
    command: &[String], diagnostics: &CommandDiagnostics, host_env: Option<&HashMap<String, String>>,
    timeout: Duration, cancel_token: &CancellationToken, exit_token: &CancellationToken,
) -> Result<String, CommandError> {
    tokio::select! {
        _ = cancel_token.cancelled() => Err(CommandError::Failed(generic_error!("Action cancelled."))),
        _ = exit_token.cancelled() => Err(CommandError::Failed(generic_error!("Action cancelled because the target exited."))),
        result = tokio::time::timeout(timeout, exec_on_host_collect(command, diagnostics, host_env)) => match result {
            Ok(result) => result,
            Err(_) => Err(CommandError::Failed(generic_error!("Timed out running {}.", diagnostics.host_subject()))),
        }
    }
}

async fn exec_in_container_collect(
    container_name: &str, command: &[String], diagnostics: &CommandDiagnostics,
) -> Result<String, CommandError> {
    // A redacted command's errors show only its subject.
    let redact = |error: GenericError| {
        if diagnostics.includes_child_output() {
            error
        } else {
            error.context(format!(
                "Failed to run {}.",
                diagnostics.container_subject(container_name)
            ))
        }
    };
    let output = docker_exec(container_name, command.to_vec())
        .await
        .map_err(|error| CommandError::Harness(redact(error)))?;

    let output_text = format!("{}{}", output.stdout, output.stderr);
    if !output.succeeded() {
        let error = if diagnostics.includes_child_output() {
            generic_error!("exec exited {:?}: {}", output.exit_code, output_text)
        } else {
            generic_error!(
                "{} exited {:?}.",
                diagnostics.container_subject(container_name),
                output.exit_code
            )
        };
        return Err(CommandError::Failed(redact(error)));
    }

    Ok(output_text)
}

async fn exec_on_host_collect(
    command: &[String], diagnostics: &CommandDiagnostics, host_env: Option<&HashMap<String, String>>,
) -> Result<String, CommandError> {
    let (program, args) = command.split_first().ok_or_else(|| {
        CommandError::Harness(generic_error!(
            "{} must not be empty.",
            diagnostics.validation_subject()
        ))
    })?;
    let mut child = Command::new(program);
    child.args(args);
    if let Some(host_env) = host_env {
        child.envs(host_env);
    }

    let output = child
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_error_context(|| format!("Failed to run {}.", diagnostics.host_subject()))
        .map_err(CommandError::Harness)?;

    collect_host_output(output, diagnostics).map_err(CommandError::Failed)
}

fn collect_host_output(output: std::process::Output, diagnostics: &CommandDiagnostics) -> Result<String, GenericError> {
    let mut output_text = String::new();
    output_text.push_str(&String::from_utf8_lossy(&output.stdout));
    output_text.push_str(&String::from_utf8_lossy(&output.stderr));

    if !output.status.success() {
        return if diagnostics.includes_child_output() {
            Err(generic_error!("host command exited {}: {}", output.status, output_text))
        } else {
            Err(generic_error!(
                "{} exited {}.",
                diagnostics.host_subject(),
                output.status
            ))
        };
    }

    Ok(output_text)
}

/// Output and exit status of a Docker exec.
pub(crate) struct ExecOutput {
    /// The command's exit code, if Docker reported one.
    pub(crate) exit_code: Option<i64>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl ExecOutput {
    /// Returns whether the command exited zero.
    pub(crate) fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Runs `cmd` in `container_name` and collects its output.
///
/// # Errors
///
/// Returns an error only when the Docker API fails. A command that runs and exits nonzero is `Ok`.
pub(crate) async fn docker_exec(container_name: &str, cmd: Vec<String>) -> Result<ExecOutput, GenericError> {
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
    let result = docker
        .start_exec(&exec.id, None)
        .await
        .error_context("Failed to start exec.")?;

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let StartExecResults::Attached { mut output, .. } = result {
        while let Some(chunk) = output.try_next().await.error_context("Failed to read exec output.")? {
            match chunk {
                LogOutput::StdOut { message } => stdout.push_str(&String::from_utf8_lossy(&message)),
                LogOutput::StdErr { message } => stderr.push_str(&String::from_utf8_lossy(&message)),
                _ => {}
            }
        }
    }

    let inspect = docker
        .inspect_exec(&exec.id)
        .await
        .error_context("Failed to inspect exec.")?;
    Ok(ExecOutput {
        exit_code: inspect.exit_code,
        stdout,
        stderr,
    })
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
    async fn redacted_without_child_output_suppresses_stdout_and_stderr() {
        let stdout_secret = "suppressed-stdout-secret";
        let stderr_secret = "suppressed-stderr-secret";
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("printf '{stdout_secret}'; printf '{stderr_secret}' >&2; exit 9"),
        ];
        let diagnostics = CommandDiagnostics::redacted_without_child_output("fixed safe command label");

        let error = exec_on_host_collect(&command, &diagnostics, None)
            .await
            .expect_err("nonzero command should fail");
        let message = error.to_string();

        assert!(message.contains("fixed safe command label"));
        assert!(!message.contains(stdout_secret), "diagnostic exposed stdout: {message}");
        assert!(!message.contains(stderr_secret), "diagnostic exposed stderr: {message}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_interrupts_an_already_started_host_child() {
        let marker_id = rand::random::<u64>();
        let started_marker = std::env::temp_dir().join(format!("panoramic-exec-{marker_id}-started"));
        let release_marker = std::env::temp_dir().join(format!("panoramic-exec-{marker_id}-release"));
        let delayed_marker = std::env::temp_dir().join(format!("panoramic-exec-{marker_id}-delayed"));
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf x > \"$0\"; while ! test -e \"$1\"; do sleep 1; done; printf x > \"$2\"".to_string(),
            started_marker.to_string_lossy().into_owned(),
            release_marker.to_string_lossy().into_owned(),
            delayed_marker.to_string_lossy().into_owned(),
        ];
        let diagnostics = CommandDiagnostics::full(&command);
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let exit_token = tokio_util::sync::CancellationToken::new();
        let run = exec_on_host_with_timeout(
            &command,
            &diagnostics,
            None,
            std::time::Duration::from_secs(60),
            &cancel_token,
            &exit_token,
        );
        tokio::pin!(run);

        let started: Result<(), String> = tokio::select! {
            _ = async {
                while !started_marker.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            } => Ok(()),
            result = &mut run => Err(format!("child exited before its marker: {result:?}")),
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => Err("child did not start within 30 seconds".to_string()),
        };
        cancel_token.cancel();
        started.expect("host child should start and remain running");

        let error = tokio::time::timeout(std::time::Duration::from_secs(2), &mut run)
            .await
            .expect("cancellation should interrupt the child within 2 seconds")
            .expect_err("cancelled host command should fail");
        assert!(error.to_string().contains("Action cancelled"));

        std::fs::write(&release_marker, b"x").expect("release marker should be written");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let delayed_write_occurred = delayed_marker.exists();
        let _ = std::fs::remove_file(started_marker);
        let _ = std::fs::remove_file(release_marker);
        let _ = std::fs::remove_file(delayed_marker);
        assert!(
            !delayed_write_occurred,
            "cancelled host child performed its delayed marker write"
        );
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
            None,
            std::time::Duration::from_secs(30),
            &tokio_util::sync::CancellationToken::new(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("host command should fail");

        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(err.to_string().contains("fail-message"));
    }

    #[tokio::test]
    async fn a_command_that_cannot_start_reports_why() {
        let command = ["panoramic-missing-program".to_string()];
        let diagnostics = CommandDiagnostics::full(&command);

        let error = exec_on_host_collect(&command, &diagnostics, None)
            .await
            .expect_err("a missing program should fail to start");

        let message = error.to_string();
        let (context, cause) = message
            .split_once(".: ")
            .expect("the message should keep the spawn error");
        assert_eq!(context, "Failed to run host command 'panoramic-missing-program'");
        assert!(!cause.is_empty(), "{message}");
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
            vec!["sleep".to_string(), "5".to_string()]
        }
    }

    #[tokio::test]
    async fn host_exec_times_out() {
        let command = slow_host_command();
        let diagnostics = CommandDiagnostics::full(&command);
        let err = exec_on_host_with_timeout(
            &command,
            &diagnostics,
            None,
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
