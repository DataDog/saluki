//! ADP's configuration, plugged into the shared remote agent runtime.
//!
//! These are thin wrappers: the runtime defines the traits, and ADP's configuration types live in other crates, so
//! the orphan rule requires a local type to implement one for the other.

use std::future::Future;
use std::io::Write as _;

use agent_data_plane_config::control::MemoryMode;
use agent_data_plane_config_system::{ConfigurationSystem, ConfigurationUpdates, LoadedConfiguration};
use bytesize::ByteSize;
use datadog_agent_commons::ipc::config::RemoteAgentClientConfiguration;
use datadog_agent_runtime::{
    control_plane::ConfigUpdatesRunner,
    env::{HostSettings, WorkloadSettings},
    facade::{AuthoritativeConfiguration, LocalConfiguration, RemoteAgentSpecification},
    logging::LoggingSettings,
};
use saluki_app::accounting::{MemoryBoundsConfiguration, MemoryMode as AppMemoryMode};
use saluki_config::dynamic::ConfigUpdate;
use saluki_error::GenericError;
use saluki_io::net::ListenAddress;
use tokio::sync::mpsc;

use crate::{
    config::{host_settings, remote_agent_client_configuration, workload_settings, DataPlaneConfiguration},
    internal::logging::get_logging_settings,
};

/// ADP as a remote agent.
pub enum AgentDataPlane {}

impl RemoteAgentSpecification for AgentDataPlane {
    type LocalConfiguration = AdpLocalConfig;
    type AuthoritativeConfiguration = AdpConfig;
    type ConfigurationUpdates = AdpConfigUpdates;
}

/// ADP's local configuration, loaded from the configuration file and the environment.
pub struct AdpLocalConfig(LoadedConfiguration);

impl AdpLocalConfig {
    /// Wraps the given local configuration.
    pub fn new(local: LoadedConfiguration) -> Self {
        Self(local)
    }
}

impl LocalConfiguration<AgentDataPlane> for AdpLocalConfig {
    fn is_standalone(&self) -> bool {
        DataPlaneConfiguration::from_configuration(self.0.local()).standalone_mode()
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        remote_agent_client_configuration(self.0.local())
    }

    fn advertised_secure_api_address(&self) -> Result<ListenAddress, GenericError> {
        DataPlaneConfiguration::from_configuration(self.0.local()).secure_api_listen_address()
    }

    async fn connect(
        self, config_stream: mpsc::Receiver<ConfigUpdate>,
    ) -> Result<(AdpConfig, AdpConfigUpdates), GenericError> {
        let (system, updates) = self.0.run(config_stream).await?;
        let config = AdpConfig {
            system,
            standalone: false,
        };
        Ok((config, AdpConfigUpdates(updates)))
    }

    async fn into_standalone(self) -> Result<AdpConfig, GenericError> {
        let system = self.0.standalone().await?;
        Ok(AdpConfig {
            system,
            standalone: true,
        })
    }
}

/// ADP's authoritative configuration.
pub struct AdpConfig {
    system: ConfigurationSystem,
    standalone: bool,
}

impl AdpConfig {
    /// Returns the configuration system.
    pub fn system(&self) -> &ConfigurationSystem {
        &self.system
    }
}

impl AuthoritativeConfiguration for AdpConfig {
    fn is_standalone(&self) -> bool {
        self.standalone
    }

    fn client_configuration(&self) -> Result<RemoteAgentClientConfiguration, GenericError> {
        remote_agent_client_configuration(&self.system.config())
    }

    fn logging(&self) -> LoggingSettings {
        get_logging_settings(&self.system.config().control.logging)
    }

    fn workload(&self) -> WorkloadSettings {
        workload_settings(&self.system.config())
    }

    fn host(&self) -> HostSettings {
        host_settings(&self.system.config())
    }

    fn remote_config_enabled(&self) -> bool {
        // https://github.com/DataDog/datadog-agent/blob/17ecddf4e3e/comp/trace/config/impl/setup.go#L114-L116
        self.system.config().domains.remote_configuration.enabled
    }

    fn memory_bounds(&self) -> MemoryBoundsConfiguration {
        let control = &self.system.config().control;
        MemoryBoundsConfiguration {
            memory_limit: control.memory_limit.map(ByteSize::b),
            memory_slop_factor: control.memory_slop_factor,
            enable_global_limiter: control.enable_global_limiter,
            memory_mode: match control.memory_mode {
                MemoryMode::Disabled => AppMemoryMode::Disabled,
                MemoryMode::Permissive => AppMemoryMode::Permissive,
                MemoryMode::Strict => AppMemoryMode::Strict,
            },
        }
    }
}

/// Applies the configuration updates that ADP's configuration system receives from the Datadog Agent.
pub struct AdpConfigUpdates(ConfigurationUpdates);

impl ConfigUpdatesRunner for AdpConfigUpdates {
    fn run(&self) -> impl Future<Output = Result<(), GenericError>> + Send + 'static {
        let updates = self.0.run();
        async move { updates.await.map_err(GenericError::from) }
    }
}

use std::{pin::Pin, sync::Arc};
use agent_data_plane_config::SalukiConfiguration;
use async_trait::async_trait;
use datadog_agent_commons::ipc::session::SessionIdHandle;
use datadog_protos::agent::command::v1::{
    execute_command_response::Frame as ExecuteCommandFrame,
    remote_command_provider_server::RemoteCommandProvider,
    Command as RemoteCommand, CommandParameter, CommandProvider, ExecuteCommandRequest, ExecuteCommandResponse,
    ListCommandsRequest, ListCommandsResponse, ParameterType,
};
use futures::Stream;
use prost_types::Struct;
use saluki_error::generic_error;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::Status;
use crate::cli::dogstatsd::{parse_remote_dogstatsd_command, run_dogstatsd_command, DogstatsdCommandOutput, RemoteArgumentType, RemoteDogstatsdCommandDescriptor, REMOTE_DOGSTATSD_COMMANDS};

const SESSION_ID_METADATA_KEY: &str = "session_id";

fn dogstatsd_command_provider() -> CommandProvider {
    CommandProvider {
        name: "dogstatsd".to_string(),
        description: "Inspect DogStatsD pipeline status".to_string(),
        commands: REMOTE_DOGSTATSD_COMMANDS.iter().map(remote_command).collect(),
    }
}

fn remote_command(descriptor: &RemoteDogstatsdCommandDescriptor) -> RemoteCommand {
    RemoteCommand {
        name: descriptor.name.to_string(),
        short_name: descriptor.name.to_string(),
        helper: descriptor.helper.to_string(),
        parameters: descriptor.parameters.iter().map(command_parameter).collect(),
        is_runnable: true,
        ..Default::default()
    }
}

fn command_parameter(descriptor: &crate::cli::dogstatsd::RemoteDogstatsdParameterDescriptor) -> CommandParameter {
    CommandParameter {
        name: descriptor.name.to_string(),
        short_name: descriptor.short_name.to_string(),
        helper: descriptor.helper.to_string(),
        r#type: remote_parameter_type(descriptor.argument_type).into(),
        required: descriptor.required,
        is_flag: true,
        is_persistent: false,
    }
}

const fn remote_parameter_type(argument_type: RemoteArgumentType) -> ParameterType {
    match argument_type {
        RemoteArgumentType::String => ParameterType::TypeString,
        RemoteArgumentType::Bool => ParameterType::TypeBool,
        RemoteArgumentType::Uint => ParameterType::TypeUint,
    }
}

pub(crate) struct RemoteCommandProviderImpl {
    session_id: SessionIdHandle,
    current_config: Arc<arc_swap::ArcSwap<SalukiConfiguration>>,
}

impl RemoteCommandProviderImpl {
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

const MAX_REMOTE_COMMAND_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
// Leave room for protobuf and gRPC framing below grpc-go's default 4 MiB inbound message limit.
const MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES: usize = 3 * 1024 * 1024;
const _: () = assert!(MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES < 4 * 1024 * 1024);

struct RemoteCommandOutput {
    bytes: Vec<u8>,
}

impl RemoteCommandOutput {
    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl std::io::Write for RemoteCommandOutput {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let new_len = self.bytes.len().saturating_add(buffer.len());
        if new_len > MAX_REMOTE_COMMAND_OUTPUT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "remote command output exceeds the 16 MiB limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl DogstatsdCommandOutput for RemoteCommandOutput {
    fn write_status(&mut self, message: &str) -> std::io::Result<()> {
        writeln!(self, "{message}")
    }

    fn report_writer(&mut self) -> &mut (dyn std::io::Write + Send) {
        self
    }
}

fn remote_command_output_chunks(output: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0;

    while start < output.len() {
        let mut end = (start + MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES).min(output.len());
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(output[start..end].to_owned());
        start = end;
    }

    chunks
}

async fn send_remote_command_output_chunks(
    sender: &mpsc::Sender<Result<ExecuteCommandResponse, Status>>, output: &str,
    frame: fn(String) -> ExecuteCommandFrame,
) {
    for output in remote_command_output_chunks(output) {
        let _ = sender
            .send(Ok(ExecuteCommandResponse {
                frame: Some(frame(output)),
            }))
            .await;
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
        let (sender, receiver) = mpsc::channel(128);
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
                    let mut output = RemoteCommandOutput { bytes: Vec::new() };
                    let result =
                        run_dogstatsd_command(&current_config.load_full(), command, &mut output, &cancellation).await;
                    let stdout = String::from_utf8_lossy(&output.into_bytes()).into_owned();
                    send_remote_command_output_chunks(&sender, &stdout, ExecuteCommandFrame::Stdout).await;
                    match result {
                        Ok(()) => 0,
                        Err(error) => {
                            send_remote_command_output_chunks(
                                &sender,
                                &format!("{error:#}\n"),
                                ExecuteCommandFrame::Stderr,
                            )
                            .await;
                            1
                        }
                    }
                }
                Err(error) => {
                    send_remote_command_output_chunks(&sender, &format!("{error:#}\n"), ExecuteCommandFrame::Stderr)
                        .await;
                    1
                }
            };
            let _ = sender
                .send(Ok(ExecuteCommandResponse {
                    frame: Some(ExecuteCommandFrame::ExitCode(exit_code)),
                }))
                .await;
        });

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            assert!(command.is_runnable);
            assert_eq!(command.name, descriptor.name);
            assert_eq!(command.short_name, descriptor.name);
            assert_eq!(command.helper, descriptor.helper);
            assert_eq!(command.parameters.len(), descriptor.parameters.len());
            for (parameter, descriptor) in command.parameters.iter().zip(descriptor.parameters) {
                assert_eq!(parameter.name, descriptor.name);
                assert_eq!(parameter.short_name, descriptor.short_name);
                assert_eq!(parameter.helper, descriptor.helper);
                assert_eq!(
                    ParameterType::try_from(parameter.r#type).expect("parameter type should be valid"),
                    remote_parameter_type(descriptor.argument_type)
                );
                assert_eq!(parameter.required, descriptor.required);
                assert!(parameter.is_flag);
                assert!(!parameter.is_persistent);
            }
        }
    }

    #[test]
    fn dropping_command_stream_cancels_execution() {
        let cancellation = CancellationToken::new();
        let (_sender, receiver) = mpsc::channel(1);
        drop(CancellableCommandStream {
            inner: ReceiverStream::new(receiver),
            cancellation: cancellation.clone(),
        });
        assert!(cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn completed_command_sender_ends_the_stream() {
        let (sender, receiver) = mpsc::channel(1);
        drop(sender);
        let mut stream = CancellableCommandStream {
            inner: ReceiverStream::new(receiver),
            cancellation: CancellationToken::new(),
        };
        assert!(futures::StreamExt::next(&mut stream).await.is_none());
    }

    #[tokio::test]
    async fn execute_command_invalid_provider_streams_error_and_exit_code_with_session_header() {
        let session_id = SessionIdHandle::empty();
        session_id.update(Some(
            SessionId::new("test-session-id").expect("session ID should be valid"),
        ));
        let service = RemoteCommandProviderImpl {
            session_id,
            current_config: Arc::new(arc_swap::ArcSwap::from_pointee(SalukiConfiguration::default())),
        };

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
                .get(SESSION_ID_METADATA_KEY)
                .expect("response should include session ID"),
            "test-session-id"
        );

        let mut stream = response.into_inner();
        let stderr = stream
            .next()
            .await
            .expect("stream should contain stderr frame")
            .expect("stderr frame should not be a transport error");
        assert!(matches!(
            stderr.frame,
            Some(ExecuteCommandFrame::Stderr(message)) if message.contains("unknown remote command provider `invalid-provider`")
        ));

        let exit_code = stream
            .next()
            .await
            .expect("stream should contain exit code frame")
            .expect("exit code frame should not be a transport error");
        assert!(matches!(exit_code.frame, Some(ExecuteCommandFrame::ExitCode(1))));
        assert!(stream.next().await.is_none(), "stream should end after the exit code");
    }

    #[tokio::test]
    async fn execute_command_chunks_large_stderr_frames() {
        let session_id = SessionIdHandle::empty();
        session_id.update(Some(
            SessionId::new("test-session-id").expect("session ID should be valid"),
        ));
        let service = RemoteCommandProviderImpl {
            session_id,
            current_config: Arc::new(arc_swap::ArcSwap::from_pointee(SalukiConfiguration::default())),
        };
        let provider_name = "x".repeat(MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES * 2 + 1);
        let expected_error = format!("unknown remote command provider `{provider_name}`\n");

        let response = service
            .execute_command(tonic::Request::new(ExecuteCommandRequest {
                provider_name,
                command_path: Vec::new(),
                arguments: None,
            }))
            .await
            .expect("invalid provider should return an execution stream");
        let mut stream = response.into_inner();
        let mut stderr = String::new();
        while let Some(frame) = stream.next().await {
            match frame.expect("frame should not be a transport error").frame {
                Some(ExecuteCommandFrame::Stderr(message)) => {
                    assert!(message.len() <= MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES);
                    stderr.push_str(&message);
                }
                Some(ExecuteCommandFrame::ExitCode(1)) => break,
                frame => panic!("unexpected frame: {frame:?}"),
            }
        }

        assert_eq!(stderr, expected_error);
    }

    #[tokio::test]
    async fn remote_command_chunk_sender_preserves_stdout_stderr_order() {
        let (sender, mut receiver) = mpsc::channel(2);

        send_remote_command_output_chunks(&sender, "stdout", ExecuteCommandFrame::Stdout).await;
        send_remote_command_output_chunks(&sender, "stderr", ExecuteCommandFrame::Stderr).await;

        assert!(matches!(
            receiver.recv().await.expect("stdout frame should be sent").expect("frame should be valid").frame,
            Some(ExecuteCommandFrame::Stdout(message)) if message == "stdout"
        ));
        assert!(matches!(
            receiver.recv().await.expect("stderr frame should be sent").expect("frame should be valid").frame,
            Some(ExecuteCommandFrame::Stderr(message)) if message == "stderr"
        ));
    }

    #[test]
    fn remote_command_output_writes_status_and_reports_to_stdout() {
        let mut output = RemoteCommandOutput { bytes: Vec::new() };

        DogstatsdCommandOutput::write_status(&mut output, "command status")
            .expect("status should be written to remote command output");
        std::io::Write::write_all(&mut output, b"command report")
            .expect("report should be written to remote command output");

        assert_eq!(output.into_bytes(), b"command status\ncommand report");
    }

    #[test]
    fn remote_command_output_rejects_data_beyond_its_limit() {
        let mut output = RemoteCommandOutput {
            bytes: vec![0; MAX_REMOTE_COMMAND_OUTPUT_BYTES],
        };
        assert!(std::io::Write::write(&mut output, b"x").is_err());
    }

    #[test]
    fn remote_command_output_chunks_stay_below_the_grpc_go_default_message_limit() {
        let output = "x".repeat(MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES * 2 + 1);

        let chunks = remote_command_output_chunks(&output);

        assert_eq!(chunks.len(), 3);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.len() <= MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES));
        assert_eq!(chunks.concat(), output);
    }

    #[test]
    fn remote_command_output_chunks_preserve_utf8_characters_at_chunk_boundaries() {
        let output = format!("{}💚", "x".repeat(MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES - 1));

        let chunks = remote_command_output_chunks(&output);

        assert_eq!(chunks.concat(), output);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.len() <= MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES));
    }

    #[test]
    fn remote_command_error_chunks_preserve_utf8_characters_at_chunk_boundaries() {
        let error = format!("{}💚\n", "x".repeat(MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES - 1));

        let chunks = remote_command_output_chunks(&error);

        assert_eq!(chunks.concat(), error);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.len() <= MAX_REMOTE_COMMAND_STDOUT_FRAME_BYTES));
    }

}
