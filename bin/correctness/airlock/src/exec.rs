//! Structured and streaming command execution in running containers.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use bollard::{
    container::LogOutput,
    errors::Error,
    exec::{CreateExecOptions, StartExecOptions, StartExecResults},
    models::ExecInspectResponse,
    Docker,
};
use bytes::Bytes;
use futures::{future::BoxFuture, stream::BoxStream, Stream, StreamExt as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use tokio::io::AsyncWriteExt as _;
use tracing::debug;

use crate::docker::redacted_docker_error;

/// A command, additional environment variables, and optional stdin bytes.
pub struct ExecRequest {
    cmd: Vec<String>,
    env: Vec<(String, String)>,
    stdin: Option<Vec<u8>>,
}

impl ExecRequest {
    /// Creates a request for the command.
    pub fn new(cmd: Vec<String>) -> Self {
        Self {
            cmd,
            env: Vec::new(),
            stdin: None,
        }
    }

    /// Adds a variable to the container's environment, preserving insertion order.
    ///
    /// Keys must be nonempty and contain no `=`. Values may be empty or contain `=`.
    pub fn with_env_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Supplies stdin bytes and requests a write-side shutdown, including for an empty input.
    ///
    /// Windows engine named pipes only flush on shutdown; commands must use an explicit input length or framing.
    pub fn with_stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }
}

/// A command's complete output and exit code. Nonzero codes are data, including a missing program
/// (126 or 127, depending on the Docker release) and a killed command (137).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutput {
    /// Bytes written to standard output.
    pub stdout: Vec<u8>,
    /// Bytes written to standard error.
    pub stderr: Vec<u8>,
    /// The command's exit code as reported by Docker.
    pub exit_code: i64,
}

/// One Docker output frame, which may contain partial or multiple lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputChunk {
    /// Bytes written to standard output.
    Stdout(Bytes),
    /// Bytes written to standard error.
    Stderr(Bytes),
}

/// A command's output as it arrives, with stdin written while the stream is polled.
///
/// The stream ends when output closes, or after yielding one read or framing error. A background child holding
/// stdout or stderr open keeps the stream open. Dropping the stream stops reading and writing; the command keeps
/// running until it exits or the container stops. Callers can impose their own timeout.
pub struct ExecStream {
    docker: Docker,
    container: String,
    exec_id: String,
    output: BoxStream<'static, Result<LogOutput, Error>>,
    stdin_writer: Option<BoxFuture<'static, std::io::Result<()>>>,
    done: bool,
}

impl ExecStream {
    /// Inspects the command's exit code, including any nonzero code.
    ///
    /// Call this after the stream returns `None`. Earlier calls, including after a stream error, return an error
    /// immediately if the command is still running. Inspection neither waits for the command nor writes stdin.
    ///
    /// # Errors
    ///
    /// Returns an error if inspection fails, the command is still running, or Docker omits its state or exit code.
    pub fn exit_code(&self) -> impl Future<Output = Result<i64, GenericError>> + Send + 'static {
        // ExecStream isn't Sync, so a future borrowing it wouldn't be Send; clone what inspection needs.
        let docker = self.docker.clone();
        let container = self.container.clone();
        let exec_id = self.exec_id.clone();
        async move {
            let inspect = docker
                .inspect_exec(&exec_id)
                .await
                .map_err(|error| docker_error("inspect", &container, Some(&exec_id), error))?;
            exit_code(&container, &exec_id, &inspect)
        }
    }

    pub(crate) async fn collect_output(mut self) -> Result<ExecOutput, GenericError> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(chunk) = self.next().await {
            match chunk? {
                OutputChunk::Stdout(bytes) => stdout.extend_from_slice(&bytes),
                OutputChunk::Stderr(bytes) => stderr.extend_from_slice(&bytes),
            }
        }
        Ok(ExecOutput {
            stdout,
            stderr,
            exit_code: self.exit_code().await?,
        })
    }
}

impl Stream for ExecStream {
    type Item = Result<OutputChunk, GenericError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }

        // Completing stdin before reading output can deadlock an echoing command; advance both on every poll.
        if let Some(writer) = &mut this.stdin_writer {
            if let Poll::Ready(result) = writer.as_mut().poll(cx) {
                if let Err(error) = result {
                    debug!(
                        container = this.container,
                        exec_id = this.exec_id,
                        kind = ?error.kind(),
                        "Exec stdin closed before writing completed"
                    );
                }
                this.stdin_writer = None;
            }
        }

        let result = match this.output.as_mut().poll_next(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => None,
            Poll::Ready(Some(Ok(frame))) => Some(chunk(frame).with_error_context(|| {
                format!(
                    "Invalid output from exec '{}' in container '{}'",
                    this.exec_id, this.container
                )
            })),
            Poll::Ready(Some(Err(error))) => Some(Err(docker_error(
                "read output from",
                &this.container,
                Some(&this.exec_id),
                error,
            ))),
        };
        if !matches!(result, Some(Ok(_))) {
            this.done = true;
            // A command that exits without reading stdin can leave the writer blocked indefinitely.
            this.stdin_writer = None;
        }
        Poll::Ready(result)
    }
}

pub(crate) async fn start(docker: &Docker, container: &str, request: ExecRequest) -> Result<ExecStream, GenericError> {
    let options =
        create_options(&request).with_error_context(|| format!("Invalid exec request for container '{container}'"))?;
    let exec = docker
        .create_exec(container, options)
        .await
        .map_err(|error| docker_error("create", container, None, error))?;
    let started = docker
        .start_exec(
            &exec.id,
            Some(StartExecOptions {
                detach: false,
                tty: false,
                ..Default::default()
            }),
        )
        .await
        .map_err(|error| docker_error("start", container, Some(&exec.id), error))?;
    let StartExecResults::Attached { output, mut input } = started else {
        return Err(generic_error!(
            "Exec '{}' in container '{container}' started without attached output",
            exec.id
        ));
    };
    let stdin_writer = request.stdin.map(|stdin| {
        Box::pin(async move {
            input.write_all(&stdin).await?;
            input.shutdown().await
        }) as BoxFuture<'static, std::io::Result<()>>
    });
    Ok(ExecStream {
        docker: docker.clone(),
        container: container.to_string(),
        exec_id: exec.id,
        output,
        stdin_writer,
        done: false,
    })
}

fn create_options(request: &ExecRequest) -> Result<CreateExecOptions<String>, GenericError> {
    if request.cmd.is_empty() {
        return Err(generic_error!("Exec command must be nonempty"));
    }
    if request.env.iter().any(|(key, _)| key.is_empty() || key.contains('=')) {
        return Err(generic_error!(
            "Exec environment keys must be nonempty and contain no '='"
        ));
    }
    Ok(CreateExecOptions {
        attach_stdin: Some(request.stdin.is_some()),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(false),
        cmd: Some(request.cmd.clone()),
        env: Some(
            request
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect(),
        ),
        ..Default::default()
    })
}

pub(crate) fn chunk(frame: LogOutput) -> Result<OutputChunk, GenericError> {
    match frame {
        LogOutput::StdOut { message } => Ok(OutputChunk::Stdout(message)),
        LogOutput::StdErr { message } => Ok(OutputChunk::Stderr(message)),
        LogOutput::StdIn { .. } => Err(generic_error!("Unexpected stdin frame in output")),
        LogOutput::Console { .. } => Err(generic_error!("Unexpected console frame in output")),
    }
}

fn exit_code(container: &str, exec_id: &str, inspect: &ExecInspectResponse) -> Result<i64, GenericError> {
    match inspect.running {
        Some(false) => inspect
            .exit_code
            .ok_or_else(|| generic_error!("Exec '{exec_id}' in container '{container}' has no exit code")),
        Some(true) => Err(generic_error!(
            "Exec '{exec_id}' in container '{container}' is still running"
        )),
        None => Err(generic_error!(
            "Exec '{exec_id}' in container '{container}' has no running state"
        )),
    }
}

fn docker_error(action: &str, container: &str, exec_id: Option<&str>, error: Error) -> GenericError {
    let exec = exec_id.map_or_else(|| "exec".to_string(), |id| format!("exec '{id}'"));
    let context = format!("Failed to {action} {exec} in container '{container}'");
    match error {
        Error::DockerResponseServerError { status_code: 409, .. } => {
            generic_error!("{context}: container is not running (Docker HTTP 409)")
        }
        error => redacted_docker_error(context, "no such container or exec", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_options_attach_stdin_only_when_the_request_has_stdin() {
        for stdin in [None, Some(Vec::new()), Some(b"payload".to_vec())] {
            let mut request = ExecRequest::new(vec!["cat".to_string()])
                .with_env_var("Z", "first")
                .with_env_var("A", "second");
            if let Some(bytes) = stdin.clone() {
                request = request.with_stdin(bytes);
            }
            let options = create_options(&request).unwrap();
            assert_eq!(options.attach_stdin, Some(stdin.is_some()));
            assert_eq!(options.attach_stdout, Some(true));
            assert_eq!(options.attach_stderr, Some(true));
            assert_eq!(options.tty, Some(false));
            assert_eq!(options.cmd, Some(vec!["cat".to_string()]));
            assert_eq!(options.env, Some(vec!["Z=first".to_string(), "A=second".to_string()]));
            assert_eq!(options.user, None);
            assert_eq!(options.working_dir, None);
        }
    }

    #[test]
    fn a_request_with_an_empty_command_is_rejected() {
        assert!(create_options(&ExecRequest::new(Vec::new())).is_err());
    }

    #[test]
    fn an_env_key_that_is_empty_or_contains_equals_is_rejected() {
        for key in ["", "A=B"] {
            let request = ExecRequest::new(vec!["true".to_string()]).with_env_var(key, "secret");
            let error = create_options(&request).unwrap_err();
            assert!(!format!("{error:#}").contains("secret"));
        }
        let request = ExecRequest::new(vec!["true".to_string()])
            .with_env_var("A", "b=c")
            .with_env_var("B", "");
        assert_eq!(
            create_options(&request).unwrap().env,
            Some(vec!["A=b=c".to_string(), "B=".to_string()])
        );
    }

    #[test]
    fn output_frames_become_tagged_chunks_and_stdin_or_console_frames_are_errors() {
        let message = Bytes::from_static(b"\0\xffpartial\nline\n");
        assert_eq!(
            chunk(LogOutput::StdOut {
                message: message.clone()
            })
            .unwrap(),
            OutputChunk::Stdout(message.clone())
        );
        assert_eq!(
            chunk(LogOutput::StdErr {
                message: message.clone()
            })
            .unwrap(),
            OutputChunk::Stderr(message.clone())
        );
        assert!(chunk(LogOutput::StdIn {
            message: message.clone()
        })
        .is_err());
        assert!(chunk(LogOutput::Console { message }).is_err());
    }

    #[test]
    fn an_exec_reports_its_exit_code_only_once_it_has_stopped() {
        for running in [Some(false), Some(true), None] {
            for code in [Some(0), Some(3), Some(127), Some(137), None] {
                let inspect = ExecInspectResponse {
                    running,
                    exit_code: code,
                    ..Default::default()
                };
                let result = exit_code("container", "exec-id", &inspect);
                match (running, code) {
                    (Some(false), Some(code)) => assert_eq!(result.unwrap(), code),
                    _ => {
                        let error = result.unwrap_err().to_string();
                        assert!(error.contains("container"), "{error}");
                        assert!(error.contains("exec-id"), "{error}");
                    }
                }
            }
        }
    }

    #[test]
    fn docker_response_errors_explain_missing_or_stopped_targets_and_redact_the_body() {
        for (status_code, reason) in [
            (404, "no such container or exec (Docker HTTP 404)"),
            (409, "container is not running (Docker HTTP 409)"),
            (500, "Docker HTTP 500"),
        ] {
            let error = docker_error(
                "start",
                "container",
                Some("exec-id"),
                Error::DockerResponseServerError {
                    status_code,
                    message: "secret".to_string(),
                },
            );
            assert_eq!(
                error.to_string(),
                format!("Failed to start exec 'exec-id' in container 'container': {reason}")
            );
            assert!(!format!("{error:?}").contains("secret"));
            assert!(!format!("{error:#}").contains("secret"));
        }
    }

    #[test]
    fn transport_errors_keep_their_sources() {
        for source in [
            Error::IOError {
                err: std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused"),
            },
            Error::RequestTimeoutError,
        ] {
            let detail = source.to_string();
            let error = docker_error("inspect", "container", Some("exec-id"), source);
            assert_eq!(
                error.to_string(),
                "Failed to inspect exec 'exec-id' in container 'container'"
            );
            assert_eq!(error.downcast_ref::<Error>().unwrap().to_string(), detail);
            assert!(format!("{error:#}").contains(&detail));
        }
    }

    #[tokio::test]
    async fn json_and_stream_errors_redact_their_payloads() {
        let daemon = crate::fake_daemon::FakeDaemon::start(|_, _| {
            (200, r#"{"Running":"secret","ProcessConfig":{"arguments":["secret"]}}"#)
        })
        .await;
        let json_error = daemon.docker.inspect_exec("exec-id").await.unwrap_err();
        assert!(matches!(json_error, Error::JsonDataError { .. }), "{json_error}");
        for (source, reason) in [
            (json_error, "Docker returned invalid JSON data"),
            (
                Error::DockerStreamError {
                    error: "secret".to_string(),
                },
                "Docker output stream failed",
            ),
        ] {
            let error = docker_error("inspect", "container", Some("exec-id"), source);
            assert_eq!(
                error.to_string(),
                format!("Failed to inspect exec 'exec-id' in container 'container': {reason}")
            );
            assert!(!format!("{error:?}").contains("secret"));
            assert!(!format!("{error:#}").contains("secret"));
        }
    }

    #[tokio::test]
    async fn a_stream_ends_after_eof_or_an_error_and_drops_its_stdin_writer() {
        let docker = Docker::connect_with_http("http://127.0.0.1:1", 1, bollard::API_DEFAULT_VERSION).unwrap();
        for mut frames in [
            vec![],
            vec![Ok(LogOutput::Console {
                message: Bytes::from_static(b"secret"),
            })],
            vec![Err(Error::DockerResponseServerError {
                status_code: 500,
                message: "secret".to_string(),
            })],
        ] {
            let has_error = !frames.is_empty();
            if has_error {
                frames.push(Ok(LogOutput::StdOut {
                    message: Bytes::from_static(b"after-error"),
                }));
            }
            let mut stream = ExecStream {
                docker: docker.clone(),
                container: "container".to_string(),
                exec_id: "exec-id".to_string(),
                output: Box::pin(futures::stream::iter(frames)),
                stdin_writer: Some(Box::pin(futures::future::pending())),
                done: false,
            };
            let result = stream.next().await;
            if has_error {
                let error = result.unwrap().unwrap_err();
                let diagnostic = format!("{error:?}");
                assert!(diagnostic.contains("container"), "{diagnostic}");
                assert!(diagnostic.contains("exec-id"), "{diagnostic}");
                assert!(!diagnostic.contains("secret"), "{diagnostic}");
            } else {
                assert!(result.is_none());
            }
            assert!(stream.stdin_writer.is_none());
            assert!(stream.next().await.is_none());
            assert!(stream.next().await.is_none());
        }
    }
}
