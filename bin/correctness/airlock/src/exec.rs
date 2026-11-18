//! Commands run inside a running container.

use std::time::Duration;

use bollard::exec::{CreateExecOptions, StartExecResults};
use bytes::Bytes;
use futures::TryStreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt as _},
    time::{sleep, timeout_at, Instant},
};

use crate::container::{Container, Output};

/// How often to ask Docker whether an exec whose output has ended has exited.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A command to run inside a running container.
#[derive(Clone, Debug)]
pub struct ExecRequest {
    command: Vec<String>,
    env: Vec<String>,
    stdin: Option<Bytes>,
}

impl ExecRequest {
    /// Creates a request that runs `command` with the container's environment and no standard input.
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            env: Vec::new(),
            stdin: None,
        }
    }

    /// Adds an environment variable for this command.
    pub fn with_env_var(mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.env.push(format!("{}={}", key.as_ref(), value.as_ref()));
        self
    }

    /// Writes `stdin` to the command's standard input, which is then closed. The command may exit without reading it
    /// all.
    pub fn with_stdin(mut self, stdin: Bytes) -> Self {
        self.stdin = Some(stdin);
        self
    }
}

/// The result of a command that ran to completion inside a container.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecOutput {
    /// The command's exit code.
    pub exit_code: i64,
    /// Everything the command wrote to standard output.
    pub stdout: Vec<u8>,
    /// Everything the command wrote to standard error.
    pub stderr: Vec<u8>,
}

impl Container {
    /// Runs `command` in the running container and waits for it to exit.
    ///
    /// With `stdin`, the bytes are written to the command's standard input, which is then closed. The command may exit
    /// without reading them all. Without it, the command has no standard input. A nonzero exit code is a result, not an
    /// error.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker cannot run the command, if its output cannot be read, if Docker does not report
    /// its exit code, or if `deadline` passes before the command exits.
    pub async fn exec(
        &self, command: Vec<String>, stdin: Option<Bytes>, deadline: Instant,
    ) -> Result<ExecOutput, GenericError> {
        let mut request = ExecRequest::new(command);
        request.stdin = stdin;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit_code = self.exec_into(request, &mut stdout, &mut stderr, deadline).await?;
        Ok(ExecOutput {
            exit_code,
            stdout,
            stderr,
        })
    }

    /// Runs `request` in the running container, writes its output to `stdout` and `stderr` as it arrives, and returns its
    /// exit code once it exits.
    ///
    /// A nonzero exit code is a result, not an error.
    ///
    /// # Errors
    ///
    /// Returns an error if Docker cannot run the command, if its output cannot be read or written, if Docker does not
    /// report its exit code, or if `deadline` passes before the command exits.
    pub async fn exec_into<O, E>(
        &self, request: ExecRequest, stdout: &mut O, stderr: &mut E, deadline: Instant,
    ) -> Result<i64, GenericError>
    where
        O: AsyncWrite + Unpin + ?Sized,
        E: AsyncWrite + Unpin + ?Sized,
    {
        let described = format!("{:?}", request.command);
        timeout_at(deadline, self.exec_unbounded(request, stdout, stderr))
            .await
            .map_err(|_| {
                generic_error!(
                    "The deadline passed before {} in container '{}' exited.",
                    described,
                    self.name
                )
            })?
    }

    async fn exec_unbounded<O, E>(
        &self, request: ExecRequest, stdout: &mut O, stderr: &mut E,
    ) -> Result<i64, GenericError>
    where
        O: AsyncWrite + Unpin + ?Sized,
        E: AsyncWrite + Unpin + ?Sized,
    {
        let ExecRequest { command, env, stdin } = request;
        let options = CreateExecOptions {
            attach_stdin: Some(stdin.is_some()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            env: Some(env),
            cmd: Some(command.clone()),
            ..Default::default()
        };
        let exec = self
            .docker
            .create_exec(&self.name, options)
            .await
            .with_error_context(|| {
                format!(
                    "Failed to create an exec of {:?} in container '{}'.",
                    command, &self.name
                )
            })?;

        let StartExecResults::Attached { mut output, mut input } = self
            .docker
            .start_exec(&exec.id, None)
            .await
            .with_error_context(|| format!("Failed to start {:?} in container '{}'.", command, &self.name))?
        else {
            return Err(generic_error!("Docker started {:?} detached from its output.", command));
        };

        // Write and read at once, so a command that answers before reading all of its input cannot stall either side.
        // A command that exits without reading all of its input is not an error: the write fails with a broken pipe or
        // stalls, as Docker stops reading, and it ends when the command exits.
        let write = async {
            if let Some(stdin) = stdin {
                let written = async {
                    input.write_all(&stdin).await?;
                    input.shutdown().await
                };
                match written.await {
                    Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => return e,
                    _ => {}
                }
            }
            std::future::pending().await
        };
        let finish = async {
            while let Some(log) = output
                .try_next()
                .await
                .with_error_context(|| format!("Failed to read the output of {:?}.", command))?
            {
                let written = match Output::from_log(log) {
                    Some(Output::Stdout(bytes)) => stdout.write_all(&bytes).await,
                    Some(Output::Stderr(bytes)) => stderr.write_all(&bytes).await,
                    None => Ok(()),
                };
                written.with_error_context(|| format!("Failed to pass on the output of {:?}.", command))?;
            }
            let flushed = async {
                stdout.flush().await?;
                stderr.flush().await
            };
            flushed
                .await
                .with_error_context(|| format!("Failed to pass on the output of {:?}.", command))?;

            // Docker can still report the exec as running after its output ends.
            let inspect = loop {
                let inspect = self
                    .docker
                    .inspect_exec(&exec.id)
                    .await
                    .with_error_context(|| format!("Failed to inspect the exec of {:?}.", command))?;
                if inspect.running != Some(true) {
                    break inspect;
                }
                sleep(EXIT_POLL_INTERVAL).await;
            };
            inspect
                .exit_code
                .ok_or_else(|| generic_error!("Docker reported no exit code for {:?}.", command))
        };
        tokio::select! {
            error = write => {
                Err(error).with_error_context(|| format!("Failed to write the standard input of {:?}.", command))
            }
            exit_code = finish => exit_code,
        }
    }
}
