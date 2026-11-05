//! Caller-owned container output streams and exit waits.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use bollard::{
    container::LogOutput,
    errors::Error,
    models::ContainerStateStatusEnum,
    query_parameters::{LogsOptions, WaitContainerOptions},
    Docker,
};
use futures::{stream::BoxStream, Stream, StreamExt as _};
use saluki_error::{generic_error, ErrorContext as _, GenericError};

use crate::{
    docker::redacted_docker_error,
    exec::{chunk, OutputChunk},
};

/// How a container's main process ended, as reported by Docker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerExit {
    /// The exit code; 128 + n for a signal death, as Docker reports it.
    pub code: i64,
    /// The error message from Docker for the exit, if it gave a nonempty one.
    pub error: Option<String>,
}

/// A container's stdout and stderr frames from its start, followed until the container stops.
///
/// The stream replays output even after exit. It also ends when the container is removed; use
/// [`Driver::wait_exit`](crate::driver::Driver::wait_exit) to observe the exit separately. Bytes pass through
/// unchanged. Dropping the stream cancels only the read, leaving the container running.
///
/// # Errors
///
/// Yields at most one error if Docker fails to supply logs or sends a stdin or console frame, then ends.
/// Errors name the container and omit log bytes.
pub struct LogStream {
    container: String,
    output: BoxStream<'static, Result<LogOutput, Error>>,
    done: bool,
}

impl fmt::Debug for LogStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogStream")
            .field("container", &self.container)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl Stream for LogStream {
    type Item = Result<OutputChunk, GenericError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let result = match this.output.as_mut().poll_next(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => None,
            Poll::Ready(Some(Ok(frame))) => Some(
                chunk(frame).with_error_context(|| format!("Invalid log output from container '{}'", this.container)),
            ),
            Poll::Ready(Some(Err(error))) => Some(Err(docker_error("read logs from", &this.container, error))),
        };
        if !matches!(result, Some(Ok(_))) {
            this.done = true;
        }
        Poll::Ready(result)
    }
}

pub(crate) async fn logs(docker: &Docker, container: &str) -> Result<LogStream, GenericError> {
    ensure_started(docker, container).await?;
    let output = docker.logs(
        container,
        Some(LogsOptions {
            follow: true,
            stdout: true,
            stderr: true,
            ..Default::default()
        }),
    );
    Ok(LogStream {
        container: container.to_string(),
        output: Box::pin(output),
        done: false,
    })
}

pub(crate) fn wait(
    docker: &Docker, container: &str,
) -> impl Future<Output = Result<ContainerExit, GenericError>> + Send + 'static {
    let docker = docker.clone();
    let container = container.to_string();
    async move {
        ensure_started(&docker, &container).await?;
        let mut stream = docker.wait_container(
            &container,
            Some(WaitContainerOptions {
                condition: "not-running".to_string(),
            }),
        );
        match stream.next().await {
            Some(Ok(response)) => Ok(ContainerExit {
                code: response.status_code,
                error: response
                    .error
                    .and_then(|error| error.message)
                    .filter(|message| !message.is_empty()),
            }),
            Some(Err(Error::DockerContainerWaitError { code, error })) => Ok(ContainerExit {
                code,
                error: (!error.is_empty()).then_some(error),
            }),
            Some(Err(error)) => Err(docker_error("wait for", &container, error)),
            None => Err(generic_error!(
                "Wait for container '{container}' ended without an exit status"
            )),
        }
    }
}

async fn ensure_started(docker: &Docker, container: &str) -> Result<(), GenericError> {
    let inspect = docker
        .inspect_container(container, None)
        .await
        .map_err(|error| docker_error("inspect", container, error))?;
    // Docker reports a fabricated exit (0 or a failed start's code) and empty logs for unstarted containers.
    match inspect.state.and_then(|state| state.status) {
        Some(ContainerStateStatusEnum::CREATED) => Err(generic_error!("Container '{container}' has not started")),
        Some(_) => Ok(()),
        None => Err(generic_error!("Container '{container}' reports no state")),
    }
}

fn docker_error(action: &str, container: &str, error: Error) -> GenericError {
    let context = format!("Failed to {action} container '{container}'");
    redacted_docker_error(context, "no such container", error)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use futures::future::BoxFuture;

    use super::*;
    use crate::{
        driver::{Driver, DriverConfig},
        fake_daemon::FakeDaemon,
    };

    #[tokio::test]
    async fn a_wait_reports_any_code_and_docker_error_message_as_data() {
        let daemon = FakeDaemon::start(|method, path| {
            if method == "GET" && path.ends_with("/json") {
                return (200, r#"{"State":{"Status":"exited"}}"#);
            }
            match (method, path.split('?').next().unwrap()) {
                ("POST", "/containers/clean/wait") => (200, r#"{"StatusCode":0}"#),
                ("POST", "/containers/message/wait") => (200, r#"{"StatusCode":0,"Error":{"Message":"boom"}}"#),
                ("POST", "/containers/no-message/wait") => (200, r#"{"StatusCode":3,"Error":{}}"#),
                ("POST", "/containers/empty-message/wait") => (200, r#"{"StatusCode":7,"Error":{"Message":""}}"#),
                ("POST", "/containers/failure/wait") => (200, r#"{"StatusCode":9,"Error":{"Message":"boom"}}"#),
                ("POST", "/containers/first/wait") => (200, "{\"StatusCode\":0}\n{\"StatusCode\":1}\n"),
                _ => (500, r#"{"message":"unexpected request"}"#),
            }
        })
        .await;
        for (container, code, error) in [
            ("clean", 0, None),
            ("message", 0, Some("boom")),
            ("no-message", 3, None),
            ("empty-message", 7, None),
            ("failure", 9, Some("boom")),
            ("first", 0, None),
        ] {
            assert_eq!(
                wait(&daemon.docker, container).await.unwrap(),
                ContainerExit {
                    code,
                    error: error.map(String::from),
                },
                "{container}"
            );
        }
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 12);
        for request in requests.iter().filter(|request| request.starts_with("POST")) {
            assert!(request.ends_with("/wait?condition=not-running"), "{request}");
        }
    }

    #[tokio::test]
    async fn a_wait_that_ends_without_a_status_is_an_error() {
        let daemon = FakeDaemon::start(|_, path| match path.split('?').next().unwrap() {
            "/containers/empty/wait" => (200, ""),
            "/containers/truncated/wait" => (200, "{\"StatusCo"),
            _ => (200, r#"{"State":{"Status":"exited"}}"#),
        })
        .await;
        let error = wait(&daemon.docker, "empty").await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "Wait for container 'empty' ended without an exit status"
        );
        let error = wait(&daemon.docker, "truncated").await.unwrap_err();
        assert!(
            error.to_string().contains("Failed to wait for container 'truncated'"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_container_that_has_not_started_is_neither_waited_on_nor_followed() {
        let daemon = FakeDaemon::start(|_, _| (200, r#"{"State":{"Status":"created"}}"#)).await;
        let driver = Driver::in_group(
            daemon.docker.clone(),
            "test".to_string(),
            DriverConfig::from_image("subject", "image".to_string()),
        );
        let error = driver.wait_exit().await.unwrap_err();
        assert_eq!(error.to_string(), "Container 'airlock-test-subject' has not started");
        let error = driver.logs().await.unwrap_err();
        assert_eq!(error.to_string(), "Container 'airlock-test-subject' has not started");
        assert_eq!(
            *daemon.requests.lock().unwrap(),
            [
                "GET /containers/airlock-test-subject/json",
                "GET /containers/airlock-test-subject/json"
            ]
        );
    }

    #[tokio::test]
    async fn waiting_sends_no_request_until_polled() {
        let daemon = FakeDaemon::start(|_, _| (500, r#"{"message":"unexpected request"}"#)).await;
        let driver = Driver::in_group(
            daemon.docker.clone(),
            "test".to_string(),
            DriverConfig::from_image("subject", "image".to_string()),
        );
        let future: BoxFuture<'static, _> = Box::pin(driver.wait_exit());
        drop(driver);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(daemon.requests.lock().unwrap().is_empty());
        drop(future);
    }

    #[tokio::test]
    async fn missing_state_and_inspection_failures_prevent_observation() {
        let daemon = FakeDaemon::start(|_, path| match path {
            "/containers/no-state/json" => (200, "{}"),
            "/containers/no-status/json" => (200, r#"{"State":{}}"#),
            "/containers/missing/json" => (404, r#"{"message":"secret"}"#),
            _ => (500, r#"{"message":"secret"}"#),
        })
        .await;
        for (container, reason) in [
            ("no-state", "reports no state"),
            ("no-status", "reports no state"),
            ("missing", "no such container"),
            ("broken", "Failed to inspect"),
        ] {
            for error in [
                wait(&daemon.docker, container).await.unwrap_err(),
                logs(&daemon.docker, container).await.unwrap_err(),
            ] {
                let diagnostic = format!("{error:#}");
                assert!(diagnostic.contains(container), "{diagnostic}");
                assert!(diagnostic.contains(reason), "{diagnostic}");
                assert!(!diagnostic.contains("secret"), "{diagnostic}");
            }
        }
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 8);
        assert!(
            requests.iter().all(|request| request.ends_with("/json")),
            "{requests:?}"
        );
    }

    #[tokio::test]
    async fn wait_and_log_request_failures_name_the_container_and_redact_payloads() {
        let daemon = FakeDaemon::start(|_, path| {
            if path.ends_with("/json") {
                (200, r#"{"State":{"Status":"running"}}"#)
            } else if path.contains("/missing/") {
                (404, r#"{"message":"secret"}"#)
            } else {
                (500, r#"{"message":"secret"}"#)
            }
        })
        .await;
        for (container, reason) in [("missing", "no such container"), ("broken", "Docker HTTP 500")] {
            let mut stream = logs(&daemon.docker, container).await.unwrap();
            for error in [
                wait(&daemon.docker, container).await.unwrap_err(),
                stream.next().await.unwrap().unwrap_err(),
            ] {
                let diagnostic = format!("{error:#}");
                assert!(diagnostic.contains(container), "{diagnostic}");
                assert!(diagnostic.contains(reason), "{diagnostic}");
                assert!(!diagnostic.contains("secret"), "{diagnostic}");
            }
            assert!(stream.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn logs_follow_both_streams_from_start_only_when_polled_and_outlive_the_driver() {
        let daemon = FakeDaemon::start(|_, path| {
            if path.ends_with("/json") {
                (200, r#"{"State":{"Status":"exited"}}"#)
            } else {
                (200, "")
            }
        })
        .await;
        let driver = Driver::in_group(
            daemon.docker.clone(),
            "test".to_string(),
            DriverConfig::from_image("subject", "image".to_string()),
        );
        let mut stream: BoxStream<'static, _> = Box::pin(driver.logs().await.unwrap());
        drop(driver);
        assert_eq!(
            *daemon.requests.lock().unwrap(),
            ["GET /containers/airlock-test-subject/json"]
        );
        assert!(stream.next().await.is_none());
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].starts_with("GET /containers/airlock-test-subject/logs?"),
            "{requests:?}"
        );
        for option in [
            "follow=true",
            "stdout=true",
            "stderr=true",
            "timestamps=false",
            "tail=all",
        ] {
            assert!(requests[1].contains(option), "{requests:?}");
        }
    }

    #[tokio::test]
    async fn log_frames_keep_raw_bytes_and_end_after_eof_or_one_error() {
        let bytes = Bytes::from_static(b"\0\xff\x1b[31mpartial\nline\n");
        for terminal in [
            None,
            Some(Ok(LogOutput::StdIn { message: bytes.clone() })),
            Some(Ok(LogOutput::Console { message: bytes.clone() })),
            Some(Err(Error::DockerStreamError {
                error: "secret".to_string(),
            })),
        ] {
            let has_error = terminal.is_some();
            let mut frames = vec![
                Ok(LogOutput::StdOut { message: bytes.clone() }),
                Ok(LogOutput::StdErr { message: bytes.clone() }),
            ];
            if let Some(frame) = terminal {
                frames.push(frame);
                frames.push(Ok(LogOutput::StdOut {
                    message: Bytes::from_static(b"after-error"),
                }));
            }
            let mut stream = LogStream {
                container: "subject".to_string(),
                output: Box::pin(futures::stream::iter(frames)),
                done: false,
            };
            assert_eq!(
                format!("{stream:?}"),
                "LogStream { container: \"subject\", done: false, .. }"
            );
            assert_eq!(
                stream.next().await.unwrap().unwrap(),
                OutputChunk::Stdout(bytes.clone())
            );
            assert_eq!(
                stream.next().await.unwrap().unwrap(),
                OutputChunk::Stderr(bytes.clone())
            );
            if has_error {
                let error = stream.next().await.unwrap().unwrap_err();
                let diagnostic = format!("{error:#}");
                assert!(diagnostic.contains("container 'subject'"), "{diagnostic}");
                assert!(!diagnostic.contains("secret"), "{diagnostic}");
                assert!(!diagnostic.contains("partial"), "{diagnostic}");
                assert!(!diagnostic.contains("exec"), "{diagnostic}");
            }
            assert!(stream.next().await.is_none());
            assert!(stream.next().await.is_none());
            assert_eq!(
                format!("{stream:?}"),
                "LogStream { container: \"subject\", done: true, .. }"
            );
        }
    }
}
