//! ADP's configuration, plugged into the shared remote agent runtime.
//!
//! These are thin wrappers: the runtime defines the traits, and ADP's configuration types live in other crates, so
//! the orphan rule requires a local type to implement one for the other.

use std::future::Future;

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
use crate::cli::dogstatsd::{parse_remote_dogstatsd_command, run_dogstatsd_command};

const SESSION_ID_METADATA_KEY: &str = "session_id";

fn dogstatsd_command_provider() -> CommandProvider {
    CommandProvider {
        name: "dogstatsd".to_string(),
        description: "Inspect DogStatsD pipeline status".to_string(),
        commands: vec![
            remote_command(
                "stats",
                "Print basic statistics about metrics received by the data plane.",
                vec![
                    command_parameter(
                        "duration-secs",
                        "d",
                        "Amount of time to collect statistics for, in seconds.",
                        ParameterType::TypeUint,
                        true,
                    ),
                    command_parameter(
                        "mode",
                        "m",
                        "Analysis mode: summary or cardinality.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "sort-dir",
                        "s",
                        "Sort direction: asc or desc.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "filter",
                        "f",
                        "Exclude metrics whose names do not contain this value.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "limit",
                        "l",
                        "Maximum number of metrics to display.",
                        ParameterType::TypeUint,
                        false,
                    ),
                ],
            ),
            remote_command(
                "capture",
                "Start a DogStatsD traffic capture.",
                vec![
                    command_parameter(
                        "duration",
                        "d",
                        "Capture duration in Go duration syntax.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "path",
                        "p",
                        "Directory in which to write the capture.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "compressed",
                        "z",
                        "Whether to zstd-compress the capture file.",
                        ParameterType::TypeBool,
                        false,
                    ),
                ],
            ),
            remote_command(
                "replay",
                "Replay DogStatsD traffic from a capture file.",
                vec![
                    command_parameter(
                        "file",
                        "f",
                        "Path to the .dog or .dog.zstd capture file to replay.",
                        ParameterType::TypeString,
                        true,
                    ),
                    command_parameter(
                        "loops",
                        "l",
                        "Number of replay iterations; 0 repeats until cancelled.",
                        ParameterType::TypeUint,
                        false,
                    ),
                ],
            ),
            remote_command(
                "top",
                "Display DogStatsD contexts with the highest cardinality.",
                vec![
                    command_parameter(
                        "path",
                        "p",
                        "Read a context dump artifact instead of requesting one.",
                        ParameterType::TypeString,
                        false,
                    ),
                    command_parameter(
                        "num-metrics",
                        "m",
                        "Maximum number of metrics to display.",
                        ParameterType::TypeUint,
                        false,
                    ),
                    command_parameter(
                        "num-tags",
                        "t",
                        "Maximum number of tags to display per metric.",
                        ParameterType::TypeUint,
                        false,
                    ),
                ],
            ),
            remote_command(
                "dump-contexts",
                "Write currently tracked DogStatsD contexts as JSON.",
                Vec::new(),
            ),
        ],
    }
}

fn remote_command(name: &str, helper: &str, parameters: Vec<CommandParameter>) -> RemoteCommand {
    RemoteCommand {
        name: name.to_string(),
        short_name: name.to_string(),
        helper: helper.to_string(),
        parameters,
        is_runnable: true,
        ..Default::default()
    }
}

fn command_parameter(
    name: &str, short_name: &str, helper: &str, parameter_type: ParameterType, required: bool,
) -> CommandParameter {
    CommandParameter {
        name: name.to_string(),
        short_name: short_name.to_string(),
        helper: helper.to_string(),
        r#type: parameter_type.into(),
        required,
        is_flag: true,
        is_persistent: false,
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

struct RemoteCommandOutput {
    sender: mpsc::Sender<Result<ExecuteCommandResponse, Status>>,
}

impl std::io::Write for RemoteCommandOutput {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let output = String::from_utf8_lossy(buffer).into_owned();
        self.sender
            .try_send(Ok(ExecuteCommandResponse {
                frame: Some(ExecuteCommandFrame::Stdout(output)),
            }))
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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
        let response =
            self.response_with_session_id(Box::pin(ReceiverStream::new(receiver)) as Self::ExecuteCommandStream)?;
        let cancellation = CancellationToken::new();
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
        let cancel_on_disconnect = cancellation.clone();
        let disconnect_sender = sender.clone();
        tokio::spawn(async move {
            disconnect_sender.closed().await;
            cancel_on_disconnect.cancel();
        });
        tokio::spawn(async move {
            let exit_code = match command {
                Ok(command) => {
                    let mut output = RemoteCommandOutput { sender: sender.clone() };
                    match run_dogstatsd_command(&current_config.load_full(), command, &mut output, &cancellation, true)
                        .await
                    {
                        Ok(()) => 0,
                        Err(error) => {
                            let _ = sender
                                .send(Ok(ExecuteCommandResponse {
                                    frame: Some(ExecuteCommandFrame::Stderr(format!("{error:#}\n"))),
                                }))
                                .await;
                            1
                        }
                    }
                }
                Err(error) => {
                    let _ = sender
                        .send(Ok(ExecuteCommandResponse {
                            frame: Some(ExecuteCommandFrame::Stderr(format!("{error:#}\n"))),
                        }))
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

        let stats = &provider.commands[0];
        assert!(stats.is_runnable);
        assert_eq!(stats.parameters[0].name, "duration-secs");
        assert_eq!(stats.parameters[0].short_name, "d");
        assert!(stats.parameters[0].required);
        assert!(stats.parameters[0].is_flag);
    }

}
