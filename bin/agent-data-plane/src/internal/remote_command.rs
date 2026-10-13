//! Remote-command service implementation.
//!
//! This module owns the command-provider gRPC protocol and its asynchronous output bridge. Keeping it separate from
//! `remote_agent` leaves that module focused on registration and the shared remote-agent lifecycle.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use agent_data_plane_config::SalukiConfiguration;
use async_trait::async_trait;
use datadog_agent_commons::ipc::session::SessionIdHandle;
use datadog_protos::agent::command::v1::{
    execute_command_response::Frame as ExecuteCommandFrame, remote_command_provider_server::RemoteCommandProvider,
    CommandProvider, ExecuteCommandRequest, ExecuteCommandResponse, ListCommandsRequest, ListCommandsResponse,
};
use futures::Stream;
use prost_types::Struct;
use saluki_error::{generic_error, GenericError};
use tokio::{
    sync::mpsc,
    time::{interval_at, Instant, MissedTickBehavior},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::Status;

use crate::cli::{
    dogstatsd::{parse_remote_dogstatsd_command, run_dogstatsd_command, REMOTE_DOGSTATSD_COMMANDS},
    remote::CommandOutput,
};

const SESSION_ID_METADATA_KEY: &str = "session_id";

/// The Core Agent CLI connection to its local gRPC proxy retains the grpc-go 4 MiB default receive limit.
///
/// Core Agent PR 55260 increases the Remote Agent Registry client's receive limit to 64 MiB and configures the proxy
/// server's send limit from `agent_ipc.grpc_max_message_size` (128 MiB by default). The final Core Agent CLI hop does
/// not override grpc-go's default, so remote providers must keep each response below 4 MiB. Three MiB leaves one MiB
/// for the protobuf response envelope and gRPC framing.
const CORE_AGENT_CLI_GRPC_MAX_INBOUND_BYTES: usize = 4 * 1024 * 1024;
const MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES: usize = 3 * 1024 * 1024;
const _: () = assert!(MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES < CORE_AGENT_CLI_GRPC_MAX_INBOUND_BYTES);

/// Maximum time stdout report data can remain buffered before it is sent to the command stream.
const REMOTE_COMMAND_OUTPUT_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
/// Limits full-payload output chunks awaiting framing.
const COMMAND_OUTPUT_EVENT_CAPACITY: usize = 2;
/// Limits full-payload framed responses awaiting tonic's stream consumer.
const COMMAND_RESPONSE_CAPACITY: usize = 4;

fn dogstatsd_command_provider() -> CommandProvider {
    CommandProvider {
        name: "dogstatsd".to_string(),
        description: "Inspect DogStatsD pipeline status".to_string(),
        commands: REMOTE_DOGSTATSD_COMMANDS
            .iter()
            .map(|descriptor| descriptor.to_rcp_command())
            .collect(),
    }
}

pub(crate) struct RemoteCommandProviderImpl {
    session_id: SessionIdHandle,
    current_config: Arc<arc_swap::ArcSwap<SalukiConfiguration>>,
}

impl RemoteCommandProviderImpl {
    pub(crate) fn new(
        session_id: SessionIdHandle, current_config: Arc<arc_swap::ArcSwap<SalukiConfiguration>>,
    ) -> Self {
        Self {
            session_id,
            current_config,
        }
    }

    fn response_with_session_id<T>(&self, value: T) -> Result<tonic::Response<T>, Status> {
        let session_id = self
            .session_id
            .get()
            .ok_or(Status::failed_precondition(
                "session ID not set; must be registered with Core Agent",
            ))?
            .to_grpc_header_value();
        let mut response = tonic::Response::new(value);
        response.metadata_mut().append(SESSION_ID_METADATA_KEY, session_id);
        Ok(response)
    }
}

enum CommandOutputEvent {
    Stdout(String),
    FlushStdout,
}

/// Asynchronously accepts UTF-8 stdout report data for the stream framing worker.
///
/// Individual writes are split before they enter the bounded queue. A full queue applies asynchronous backpressure to
/// the command rather than blocking a Tokio worker or accumulating an unbounded command transcript.
struct RemoteCommandOutput {
    sender: mpsc::Sender<CommandOutputEvent>,
}

impl RemoteCommandOutput {
    fn new(sender: mpsc::Sender<CommandOutputEvent>) -> Self {
        Self { sender }
    }

    async fn send(&self, event: CommandOutputEvent) -> std::io::Result<()> {
        self.sender.send(event).await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "remote command output stream closed before command output was delivered",
            )
        })
    }
}

#[async_trait]
impl CommandOutput for RemoteCommandOutput {
    async fn write_progress(&mut self, message: &str) -> std::io::Result<()> {
        self.write_stdout(&format!("{message}\n")).await
    }

    async fn write_stdout(&mut self, output: &str) -> std::io::Result<()> {
        for chunk in utf8_chunks(output) {
            self.send(CommandOutputEvent::Stdout(chunk.to_owned())).await?;
        }
        Ok(())
    }

    async fn flush_stdout(&mut self) -> std::io::Result<()> {
        self.send(CommandOutputEvent::FlushStdout).await
    }
}

/// Splits text into valid UTF-8 strings no wider than a protobuf payload frame.
fn utf8_chunks(output: &str) -> impl Iterator<Item = &str> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start == output.len() {
            return None;
        }

        let mut end = (start + MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES).min(output.len());
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = &output[start..end];
        start = end;
        Some(chunk)
    })
}

async fn send_frame(
    sender: &mpsc::Sender<Result<ExecuteCommandResponse, Status>>, cancellation: &CancellationToken,
    frame: ExecuteCommandFrame,
) -> bool {
    tokio::select! {
        _ = cancellation.cancelled() => false,
        result = sender.send(Ok(ExecuteCommandResponse { frame: Some(frame) })) => result.is_ok(),
    }
}

async fn flush_pending_stdout(
    pending: &mut String, sender: &mpsc::Sender<Result<ExecuteCommandResponse, Status>>,
    cancellation: &CancellationToken,
) -> bool {
    if pending.is_empty() {
        return true;
    }
    let output = std::mem::take(pending);
    send_frame(sender, cancellation, ExecuteCommandFrame::Stdout(output)).await
}

/// Frames stdout report data and delivers buffered text at least every 100 ms while output is pending.
async fn stream_command_output(
    mut receiver: mpsc::Receiver<CommandOutputEvent>, sender: mpsc::Sender<Result<ExecuteCommandResponse, Status>>,
    cancellation: CancellationToken,
) {
    let mut pending = String::new();
    let mut flush_timer = interval_at(
        Instant::now() + REMOTE_COMMAND_OUTPUT_FLUSH_INTERVAL,
        REMOTE_COMMAND_OUTPUT_FLUSH_INTERVAL,
    );
    flush_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return,
            _ = flush_timer.tick() => {
                if !flush_pending_stdout(&mut pending, &sender, &cancellation).await {
                    return;
                }
            }
            event = receiver.recv() => match event {
                Some(CommandOutputEvent::Stdout(text)) => {
                    if pending.len() + text.len() > MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES
                        && !flush_pending_stdout(&mut pending, &sender, &cancellation).await
                    {
                        return;
                    }
                    pending.push_str(&text);
                }
                Some(CommandOutputEvent::FlushStdout) => {
                    if !flush_pending_stdout(&mut pending, &sender, &cancellation).await {
                        return;
                    }
                }
                None => {
                    flush_pending_stdout(&mut pending, &sender, &cancellation).await;
                    return;
                }
            },
        }
    }
}

struct CancellableCommandStream {
    inner: ReceiverStream<Result<ExecuteCommandResponse, Status>>,
    cancellation: CancellationToken,
}

impl Stream for CancellableCommandStream {
    type Item = Result<ExecuteCommandResponse, Status>;

    fn poll_next(
        mut self: Pin<&mut Self>, context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(context)
    }
}

impl Drop for CancellableCommandStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[async_trait]
impl RemoteCommandProvider for RemoteCommandProviderImpl {
    type ExecuteCommandStream = Pin<Box<dyn Stream<Item = Result<ExecuteCommandResponse, Status>> + Send>>;

    async fn list_commands(
        &self, _request: tonic::Request<ListCommandsRequest>,
    ) -> Result<tonic::Response<ListCommandsResponse>, Status> {
        self.response_with_session_id(ListCommandsResponse {
            providers: vec![dogstatsd_command_provider()],
        })
    }

    async fn execute_command(
        &self, request: tonic::Request<ExecuteCommandRequest>,
    ) -> Result<tonic::Response<Self::ExecuteCommandStream>, Status> {
        let (sender, receiver) = mpsc::channel(COMMAND_RESPONSE_CAPACITY);
        let cancellation = CancellationToken::new();
        let response = self.response_with_session_id(Box::pin(CancellableCommandStream {
            inner: ReceiverStream::new(receiver),
            cancellation: cancellation.clone(),
        }) as Self::ExecuteCommandStream)?;
        let command = if request.get_ref().provider_name != "dogstatsd" {
            Err(generic_error!(
                "unknown remote command provider `{}`",
                request.get_ref().provider_name
            ))
        } else {
            parse_remote_dogstatsd_command(
                &request.get_ref().command_path,
                request.get_ref().arguments.as_ref().unwrap_or(&Struct::default()),
            )
        };

        let current_config = Arc::clone(&self.current_config);
        tokio::spawn(async move {
            let exit_code = match command {
                Ok(command) => {
                    let (output_sender, output_receiver) = mpsc::channel(COMMAND_OUTPUT_EVENT_CAPACITY);
                    let output_task = tokio::spawn(stream_command_output(
                        output_receiver,
                        sender.clone(),
                        cancellation.clone(),
                    ));
                    let mut output = RemoteCommandOutput::new(output_sender);
                    let result =
                        run_dogstatsd_command(&current_config.load_full(), command, &mut output, &cancellation).await;
                    drop(output);
                    let _ = output_task.await;

                    match result {
                        Ok(()) => 0,
                        Err(error) => {
                            let _ = send_error(&sender, &cancellation, &error).await;
                            1
                        }
                    }
                }
                Err(error) => {
                    let _ = send_error(&sender, &cancellation, &error).await;
                    1
                }
            };
            let _ = send_frame(&sender, &cancellation, ExecuteCommandFrame::ExitCode(exit_code)).await;
        });

        Ok(response)
    }
}

async fn send_error(
    sender: &mpsc::Sender<Result<ExecuteCommandResponse, Status>>, cancellation: &CancellationToken,
    error: &GenericError,
) -> bool {
    for chunk in utf8_chunks(&format!("{error:#}\n")) {
        if !send_frame(sender, cancellation, ExecuteCommandFrame::Stderr(chunk.to_owned())).await {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use datadog_agent_commons::ipc::session::SessionId;
    use datadog_protos::agent::command::v1::execute_command_response::Frame as ExecuteCommandFrame;
    use futures::StreamExt as _;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::{
        dogstatsd_command_provider, send_error, send_frame, stream_command_output, utf8_chunks,
        CancellableCommandStream, CommandOutput, ExecuteCommandRequest, ExecuteCommandResponse, RemoteCommandOutput,
        RemoteCommandProvider, RemoteCommandProviderImpl, COMMAND_OUTPUT_EVENT_CAPACITY,
        CORE_AGENT_CLI_GRPC_MAX_INBOUND_BYTES, MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES,
    };
    use crate::cli::dogstatsd::REMOTE_DOGSTATSD_COMMANDS;

    #[test]
    fn dogstatsd_command_provider_describes_every_remote_command() {
        let provider = dogstatsd_command_provider();

        assert_eq!(provider.name, "dogstatsd");
        assert_eq!(provider.description, "Inspect DogStatsD pipeline status");
        assert_eq!(
            provider
                .commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["stats", "capture", "replay", "top", "dump-contexts"]
        );

        for (command, descriptor) in provider.commands.iter().zip(REMOTE_DOGSTATSD_COMMANDS) {
            assert_eq!(command, &descriptor.to_rcp_command());
        }
    }

    #[test]
    fn dropping_command_stream_cancels_execution() {
        let cancellation = CancellationToken::new();
        let (_sender, receiver) = mpsc::channel(1);
        drop(CancellableCommandStream {
            inner: tokio_stream::wrappers::ReceiverStream::new(receiver),
            cancellation: cancellation.clone(),
        });
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn buffered_remote_command_output_flushes_within_100_milliseconds() {
        let (output_sender, output_receiver) = mpsc::channel(COMMAND_OUTPUT_EVENT_CAPACITY);
        let (sender, mut receiver) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let worker = tokio::spawn(stream_command_output(output_receiver, sender, cancellation));
        let mut output = RemoteCommandOutput::new(output_sender);

        output
            .write_stdout("buffered output")
            .await
            .expect("output should queue");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(99)).await;
        assert!(
            receiver.try_recv().is_err(),
            "output must remain buffered before the deadline"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        let frame = receiver.recv().await.expect("output should flush at the deadline");
        assert!(
            matches!(frame.expect("frame should be valid").frame, Some(ExecuteCommandFrame::Stdout(message)) if message == "buffered output")
        );

        drop(output);
        worker.await.expect("worker should stop when output closes");
    }

    #[tokio::test]
    async fn remote_progress_is_delivered_as_stdout() {
        let (output_sender, output_receiver) = mpsc::channel(COMMAND_OUTPUT_EVENT_CAPACITY);
        let (sender, mut receiver) = mpsc::channel(1);
        let worker = tokio::spawn(stream_command_output(output_receiver, sender, CancellationToken::new()));
        let mut output = RemoteCommandOutput::new(output_sender);

        output
            .write_progress("replay warning")
            .await
            .expect("progress should queue");
        output.flush_stdout().await.expect("stdout flush should queue");
        drop(output);

        let frame = receiver.recv().await.expect("progress should be delivered");
        assert!(
            matches!(frame.expect("frame should be valid").frame, Some(ExecuteCommandFrame::Stdout(message)) if message == "replay warning\n")
        );
        worker.await.expect("worker should stop when output closes");
    }

    #[tokio::test]
    async fn remote_output_streams_more_than_the_old_aggregate_limit_in_bounded_frames() {
        let (output_sender, output_receiver) = mpsc::channel(COMMAND_OUTPUT_EVENT_CAPACITY);
        let (sender, mut receiver) = mpsc::channel(1);
        let worker = tokio::spawn(stream_command_output(output_receiver, sender, CancellationToken::new()));
        let expected = "x".repeat(16 * 1024 * 1024 + 1);
        let write_task = tokio::spawn(async move {
            let mut output = RemoteCommandOutput::new(output_sender);
            output
                .write_stdout(&expected)
                .await
                .expect("large output should stream under backpressure");
            output.flush_stdout().await.expect("flush should queue");
            expected
        });

        let mut actual = String::new();
        while let Some(frame) = receiver.recv().await {
            match frame.expect("frame should be valid").frame {
                Some(ExecuteCommandFrame::Stdout(message)) => {
                    assert!(message.len() <= MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES);
                    actual.push_str(&message);
                }
                frame => panic!("unexpected frame: {frame:?}"),
            }
        }
        worker.await.expect("worker should finish");
        assert_eq!(actual, write_task.await.expect("writer should finish"));
    }

    #[tokio::test]
    async fn stdout_is_delivered_before_stderr_and_exit_code() {
        let (output_sender, output_receiver) = mpsc::channel(COMMAND_OUTPUT_EVENT_CAPACITY);
        let (sender, mut receiver) = mpsc::channel(3);
        let cancellation = CancellationToken::new();
        let worker = tokio::spawn(stream_command_output(
            output_receiver,
            sender.clone(),
            cancellation.clone(),
        ));
        let mut output = RemoteCommandOutput::new(output_sender);
        output.write_stdout("stdout").await.expect("output should queue");
        output.flush_stdout().await.expect("flush should queue");
        drop(output);
        worker.await.expect("output worker should finish before error handling");

        let error = saluki_error::generic_error!("stderr");
        send_error(&sender, &cancellation, &error).await;
        send_frame(&sender, &cancellation, ExecuteCommandFrame::ExitCode(1)).await;
        drop(sender);

        let frames = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|frame| frame.expect("frame should be valid").frame)
            .collect::<Vec<_>>();
        assert!(
            matches!(frames.as_slice(), [Some(ExecuteCommandFrame::Stdout(stdout)), Some(ExecuteCommandFrame::Stderr(stderr)), Some(ExecuteCommandFrame::ExitCode(1))] if stdout == "stdout" && stderr == "stderr\n")
        );
    }

    #[test]
    fn remote_command_frames_are_utf8_safe_and_below_the_core_agent_cli_limit() {
        let output = format!("{}💚", "x".repeat(MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES - 1));
        let chunks = utf8_chunks(&output).collect::<Vec<_>>();

        assert_eq!(chunks.concat(), output);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.len() <= MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES));
        assert!(chunks
            .iter()
            .all(|chunk| chunk.len() < CORE_AGENT_CLI_GRPC_MAX_INBOUND_BYTES));
        assert!(chunks.iter().all(|chunk| {
            prost::Message::encoded_len(&ExecuteCommandResponse {
                frame: Some(ExecuteCommandFrame::Stdout((*chunk).to_owned())),
            }) < CORE_AGENT_CLI_GRPC_MAX_INBOUND_BYTES
        }));
    }

    #[tokio::test]
    async fn execute_command_invalid_provider_streams_error_and_exit_code_with_session_header() {
        let session_id = datadog_agent_commons::ipc::session::SessionIdHandle::empty();
        session_id.update(Some(
            SessionId::new("test-session-id").expect("session ID should be valid"),
        ));
        let service = RemoteCommandProviderImpl::new(
            session_id,
            Arc::new(arc_swap::ArcSwap::from_pointee(
                agent_data_plane_config::SalukiConfiguration::default(),
            )),
        );

        let response = service
            .execute_command(tonic::Request::new(ExecuteCommandRequest {
                provider_name: "invalid-provider".to_string(),
                command_path: Vec::new(),
                arguments: None,
            }))
            .await
            .expect("invalid provider should return an execution stream");
        assert_eq!(
            response
                .metadata()
                .get("session_id")
                .expect("response should include session ID"),
            "test-session-id"
        );

        let mut stream = response.into_inner();
        let stderr = stream
            .next()
            .await
            .expect("stream should contain stderr frame")
            .expect("stderr should be valid");
        assert!(
            matches!(stderr.frame, Some(ExecuteCommandFrame::Stderr(message)) if message.contains("unknown remote command provider `invalid-provider`"))
        );
        let exit_code = stream
            .next()
            .await
            .expect("stream should contain exit code frame")
            .expect("exit code should be valid");
        assert!(matches!(exit_code.frame, Some(ExecuteCommandFrame::ExitCode(1))));
        assert!(stream.next().await.is_none(), "stream should end after the exit code");
    }
}
