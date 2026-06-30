use std::time::{Duration, Instant};

use airlock::docker;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecResults},
};
use futures::TryStreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio_util::sync::CancellationToken;

use super::Action;
use crate::assertions::{AssertionContext, AssertionResult};

pub(super) struct DogStatsDNamedPipeSendAction {
    pipe_name: String,
    payload: String,
    timeout: Duration,
}

impl DogStatsDNamedPipeSendAction {
    pub(super) fn new(pipe_name: String, payload: String, timeout: Duration) -> Self {
        Self {
            pipe_name,
            payload,
            timeout,
        }
    }
}

#[async_trait::async_trait]
impl Action for DogStatsDNamedPipeSendAction {
    fn name(&self) -> &'static str {
        "dogstatsd_named_pipe_send"
    }

    fn description(&self) -> String {
        format!("Send DogStatsD payload to Windows named pipe '{}'.", self.pipe_name)
    }

    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let result = if ctx.is_host_process || !ctx.target_is_windows() {
            Err(generic_error!(
                "dogstatsd_named_pipe_send is only supported for Windows container runtimes."
            ))
        } else {
            send_payload_in_windows_container(
                &ctx.container_name,
                &self.pipe_name,
                &self.payload,
                self.timeout,
                &ctx.cancel_token,
                &ctx.container_exit_token,
            )
            .await
        };

        match result {
            Ok(()) => AssertionResult {
                name: self.name().to_string(),
                passed: true,
                message: self.description(),
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

async fn send_payload_in_windows_container(
    container_name: &str, pipe_name: &str, payload: &str, timeout: Duration, cancel_token: &CancellationToken,
    exit_token: &CancellationToken,
) -> Result<(), GenericError> {
    let command = build_windows_named_pipe_send_command(pipe_name, payload, timeout);
    let deadline = Instant::now() + timeout;
    let mut last_error = None;

    loop {
        if cancel_token.is_cancelled() || exit_token.is_cancelled() {
            return Err(generic_error!("Action cancelled because the target exited."));
        }
        if Instant::now() > deadline {
            return Err(match last_error {
                Some(error) => generic_error!(
                    "Timed out sending DogStatsD payload to named pipe '{}'. Last attempt failed: {}.",
                    pipe_name,
                    error
                ),
                None => generic_error!(
                    "Timed out sending DogStatsD payload to named pipe '{}'. No send attempt completed before the deadline.",
                    pipe_name
                ),
            });
        }

        match exec_collect(
            container_name,
            vec![
                "pwsh".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                command.clone(),
            ],
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

fn build_windows_named_pipe_send_command(pipe_name: &str, payload: &str, timeout: Duration) -> String {
    let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
    let payload_base64 = BASE64.encode(payload.as_bytes());
    format!(
        "$pipe = [System.IO.Pipes.NamedPipeClientStream]::new('.', '{}', [System.IO.Pipes.PipeDirection]::Out); \
         $pipe.Connect({}); \
         $payload = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{}')); \
         $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload); \
         $pipe.Write($bytes, 0, $bytes.Length); \
         $pipe.Flush(); \
         $pipe.Dispose();",
        powershell_single_quote(pipe_name),
        timeout_ms,
        payload_base64
    )
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

fn powershell_single_quote(value: &str) -> String {
    value.replace('`', "``").replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::build_windows_named_pipe_send_command;

    #[test]
    fn windows_named_pipe_send_command_uses_dot_server_and_writes_utf8_payload() {
        let command = build_windows_named_pipe_send_command(
            "datadog-dogstatsd",
            "integration.named_pipe.invalid\n",
            std::time::Duration::from_secs(5),
        );

        assert!(command.contains("NamedPipeClientStream"));
        assert!(command.contains("'.'"));
        assert!(command.contains("'datadog-dogstatsd'"));
        assert!(command.contains("FromBase64String('aW50ZWdyYXRpb24ubmFtZWRfcGlwZS5pbnZhbGlkCg==')"));
        assert!(command.contains("[System.Text.Encoding]::UTF8.GetBytes"));
        assert!(command.contains("$pipe.Connect(5000)"));
    }
}
