//! Commands run inside a running container.

use std::time::Duration;

use bollard::exec::{CreateExecOptions, StartExecResults};
use bytes::Bytes;
use futures::TryStreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::{
    io::AsyncWriteExt as _,
    time::{sleep, timeout_at, Instant},
};

use crate::container::{Container, Output};

/// How often to ask Docker whether an exec whose output has ended has exited.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

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
        let described = format!("{:?}", command);
        timeout_at(deadline, self.exec_unbounded(command, stdin))
            .await
            .map_err(|_| {
                generic_error!(
                    "The deadline passed before {} in container '{}' exited.",
                    described,
                    self.name
                )
            })?
    }

    async fn exec_unbounded(&self, command: Vec<String>, stdin: Option<Bytes>) -> Result<ExecOutput, GenericError> {
        let options = CreateExecOptions {
            attach_stdin: Some(stdin.is_some()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
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
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            while let Some(log) = output
                .try_next()
                .await
                .with_error_context(|| format!("Failed to read the output of {:?}.", command))?
            {
                match Output::from_log(log) {
                    Some(Output::Stdout(bytes)) => stdout.extend_from_slice(&bytes),
                    Some(Output::Stderr(bytes)) => stderr.extend_from_slice(&bytes),
                    None => {}
                }
            }

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
            let exit_code = inspect
                .exit_code
                .ok_or_else(|| generic_error!("Docker reported no exit code for {:?}.", command))?;

            Ok(ExecOutput {
                exit_code,
                stdout,
                stderr,
            })
        };
        tokio::select! {
            error = write => {
                Err(error).with_error_context(|| format!("Failed to write the standard input of {:?}.", command))
            }
            output = finish => output,
        }
    }
}
