//! IPC configuration.

use std::{path::PathBuf, time::Duration};

use backon::{BackoffBuilder, ConstantBuilder};
use saluki_config_tools::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use tonic::transport::Uri;
#[cfg(not(target_os = "linux"))]
use tracing::warn;

use crate::platform::PlatformSettings;

fn default_agent_ipc_endpoint() -> Uri {
    Uri::from_static("https://127.0.0.1:5001")
}

const fn default_connect_retry_attempts() -> usize {
    10
}

const fn default_grpc_max_message_size() -> usize {
    128 * 1024 * 1024
}

const fn default_connect_retry_backoff() -> Duration {
    Duration::from_secs(2)
}

/// Datadog Agent IPC authentication configuration.
#[derive(Deserialize)]
#[serde(default)]
pub struct IpcAuthConfiguration {
    /// Path to the Agent authentication token file.
    ///
    /// The contents of the file are passed as a bearer token in RPC requests to the IPC endpoint.
    ///
    /// Defaults to `<conf dir>/auth_token`, where `<conf dir>` is the platform-specific directory containing the Agent
    /// configuration.
    auth_token_file_path: PathBuf,

    /// Path to the Agent IPC TLS certificate file.
    ///
    /// The file is expected to be PEM-encoded, containing both a certificate and private key. The certificate will be
    /// used to verify the TLS server certificate presented by the Agent, and the certificate and private key will be
    /// used together to provide client authentication _to_ the Agent.
    ///
    /// Defaults to `ipc_cert.pem` in the same directory as the Agent authentication token file. (for example, if
    /// `auth_token_file_path` is `/etc/datadog-agent/auth_token`, this will be `/etc/datadog-agent/ipc_cert.pem`.)
    ipc_cert_file_path: Option<PathBuf>,
}

impl IpcAuthConfiguration {
    // Creates a new `IpcAuthConfiguration` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the configuration is invalid, an error is returned.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        config
            .as_typed::<Self>()
            .error_context("Failed to parse Datadog Agent IPC authentication configuration.")
    }

    /// Gets the path to the Agent authentication token file from the configuration.
    pub fn auth_token_file_path(&self) -> PathBuf {
        if self.auth_token_file_path.as_os_str().is_empty() {
            return PlatformSettings::get_auth_token_path();
        }

        self.auth_token_file_path.clone()
    }

    /// Gets the IPC certificate file path from the configuration.
    pub fn ipc_cert_file_path(&self) -> PathBuf {
        // If the IPC cert file path is set explicitly, we always prefer that.
        if let Some(path) = self.ipc_cert_file_path.as_ref() {
            if !path.as_os_str().is_empty() {
                return path.clone();
            }
        }

        // Otherwise, we default to the same directory as the auth token file with the default certificate file name.
        let auth_token_dir = if self.auth_token_file_path.as_os_str().is_empty() {
            PlatformSettings::get_config_dir_path()
        } else {
            self.auth_token_file_path
                .parent()
                .unwrap_or(PlatformSettings::get_config_dir_path())
        };

        auth_token_dir.join(PlatformSettings::get_ipc_cert_filename())
    }
}

impl Default for IpcAuthConfiguration {
    fn default() -> Self {
        Self {
            auth_token_file_path: PlatformSettings::get_auth_token_path(),
            ipc_cert_file_path: None,
        }
    }
}

/// Datadog Agent IPC client configuration.
#[derive(Deserialize)]
pub struct RemoteAgentClientConfiguration {
    /// Datadog Agent IPC endpoint to connect to.
    ///
    /// Caution/weird: This is configuration is only available on agent-data-plane, and would allow
    /// one to connect to an Agent at a URI other than localhost/127.0.0.1. However, the Datadog
    /// configuration schema doesn't account for this and instead provides `cmd_port`. Therefore,
    ///
    /// **CAUTION**: if `cmd_port` is set, then `ipc_endpoint` is ignored.
    ///
    /// Defaults to `https://127.0.0.1:5001`.
    #[serde(
        rename = "agent_ipc_endpoint",
        with = "http_serde_ext::uri",
        default = "default_agent_ipc_endpoint"
    )]
    ipc_endpoint: Uri,

    /// The port that will be used to connect to the Datadog Agent IPC on the local host.
    ///
    /// Takes precedence over `ipc_endpoint` if set.
    cmd_port: Option<u16>,

    /// Authentication configuration for the IPC endpoint.
    #[serde(flatten, default)]
    auth: IpcAuthConfiguration,

    /// Number of allowed retry attempts when initially connecting.
    ///
    /// Defaults to `10`.
    #[serde(default = "default_connect_retry_attempts")]
    connect_retry_attempts: usize,

    /// Amount of time to wait between connection attempts when initially connecting.
    ///
    /// Defaults to 2 seconds.
    #[serde(default = "default_connect_retry_backoff")]
    connect_retry_backoff: Duration,

    /// Maximum message size for gRPC messages.
    ///
    /// Defaults to `128 * 1024 * 1024` (128 MB).
    #[serde(
        rename = "agent_ipc_grpc_max_message_size",
        default = "default_grpc_max_message_size"
    )]
    grpc_max_message_size: usize,

    /// vsock address for connecting to the Agent IPC endpoint via AF_VSOCK.
    ///
    /// When set, the IPC client connects over a vsock socket using the resolved CID with the port
    /// taken from the configured endpoint. This mirrors the Datadog Agent's `vsock_addr`
    /// configuration, enabling communication from within a guest VM (for example, Nitro Enclaves)
    /// to an Agent process running on the host or hypervisor.
    ///
    /// Accepted values:
    /// - `host`: connect to the host (CID 2, `VMADDR_CID_HOST`)
    /// - `hypervisor`: connect to the hypervisor (CID 0, `VMADDR_CID_HYPERVISOR`)
    /// - `local`: connect to the local VM (CID 3, `VMADDR_CID_LOCAL`)
    ///
    /// Defaults to unset (TCP connection).
    #[cfg(target_os = "linux")]
    #[serde(default, deserialize_with = "deserialize_vsock_addr")]
    vsock_addr: Option<u32>,

    // Non-Linux: capture raw value solely to emit a warning when configured.
    #[cfg(not(target_os = "linux"))]
    #[serde(default)]
    vsock_addr: String,
}

#[cfg(target_os = "linux")]
fn deserialize_vsock_addr<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    match Option::<String>::deserialize(deserializer)?.as_deref() {
        None | Some("") => Ok(None),
        Some("host") => Ok(Some(2)),       // VMADDR_CID_HOST
        Some("hypervisor") => Ok(Some(0)), // VMADDR_CID_HYPERVISOR
        Some("local") => Ok(Some(3)),      // VMADDR_CID_LOCAL
        Some(other) => Err(D::Error::custom(format!(
            "invalid vsock address '{}'; expected one of: host, hypervisor, local",
            other
        ))),
    }
}

impl RemoteAgentClientConfiguration {
    /// Creates a new `RemoteAgentClientConfiguration` from the given configuration.
    ///
    /// ## Errors
    ///
    /// If the configuration is invalid, an error is returned.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        let this = config
            .as_typed::<Self>()
            .error_context("Failed to parse Datadog Agent IPC client configuration.")?;

        #[cfg(not(target_os = "linux"))]
        if !this.vsock_addr.is_empty() {
            warn!("`vsock_addr` is configured but vsock is only supported on Linux. Setting will be ignored.");
        }

        Ok(this)
    }

    /// Returns a reference to the authentication configuration for the Remote Agent client.
    pub fn auth(&self) -> &IpcAuthConfiguration {
        &self.auth
    }

    /// Returns the IPC endpoint URI.
    ///
    /// If `cmd_port` is set, the endpoint is built from it, ignoring `ipc_endpoint`.
    pub fn endpoint(&self) -> Result<Uri, GenericError> {
        if let Some(cmd_port) = self.cmd_port {
            format!("https://127.0.0.1:{}", cmd_port)
                .parse::<Uri>()
                .with_error_context(|| format!("failed to build URI from cmd_port {cmd_port}"))
        } else {
            Ok(self.ipc_endpoint.clone())
        }
    }

    /// Returns the maximum message size for gRPC.
    pub fn grpc_max_message_size(&self) -> usize {
        self.grpc_max_message_size
    }

    /// Returns the vsock address to use for connecting to the IPC endpoint, if configured.
    ///
    /// Combines the CID from `vsock_addr` with the port resolved from `endpoint()`. Returns
    /// an error if `vsock_addr` is set but the endpoint has no explicit port.
    ///
    /// # Errors
    ///
    /// If the configured endpoint has no explicit port.
    #[cfg(target_os = "linux")]
    pub fn vsock_addr(&self) -> Result<Option<tokio_vsock::VsockAddr>, GenericError> {
        let Some(cid) = self.vsock_addr else {
            return Ok(None);
        };
        let port = self
            .endpoint()?
            .port_u16()
            .map(u32::from)
            .ok_or_else(|| saluki_error::generic_error!("vsock requires an explicit port in the IPC endpoint"))?;
        Ok(Some(tokio_vsock::VsockAddr::new(cid, port)))
    }
}

impl BackoffBuilder for &RemoteAgentClientConfiguration {
    type Backoff = <ConstantBuilder as BackoffBuilder>::Backoff;

    fn build(self) -> Self::Backoff {
        ConstantBuilder::default()
            .with_delay(self.connect_retry_backoff)
            .with_max_times(self.connect_retry_attempts)
            .build()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use saluki_config_tools::ConfigurationLoader;

    use super::RemoteAgentClientConfiguration;
    use crate::platform::PlatformSettings;

    async fn get_remote_agent_config(
        ipc_cert_file_path: Option<&Path>, auth_token_file_path: Option<&Path>,
    ) -> RemoteAgentClientConfiguration {
        // Set the values in the config map if provided.
        let mut values = serde_json::Map::new();
        if let Some(path) = ipc_cert_file_path {
            values.insert(
                "ipc_cert_file_path".to_string(),
                path.to_string_lossy().into_owned().into(),
            );
        }
        if let Some(path) = auth_token_file_path {
            values.insert(
                "auth_token_file_path".to_string(),
                path.to_string_lossy().into_owned().into(),
            );
        }

        let (base_config, _) =
            ConfigurationLoader::for_tests(Some(serde_json::Value::Object(values)), None, false).await;
        RemoteAgentClientConfiguration::from_configuration(&base_config).unwrap()
    }

    #[tokio::test]
    async fn ipc_cert_file_path_empty_config() {
        let default_auth_token_path = PlatformSettings::get_auth_token_path();

        // When the auth token file path _and_ IPC cert file path are both unset, we should default to looking for the
        // IPC cert in the same directory as the auth token.
        let config = get_remote_agent_config(None, None).await;
        assert_eq!(
            config.auth().ipc_cert_file_path().parent(),
            default_auth_token_path.as_path().parent()
        );
        assert_eq!(
            config.auth().ipc_cert_file_path().file_name().map(Path::new),
            Some(PlatformSettings::get_ipc_cert_filename())
        );
    }

    #[tokio::test]
    async fn ipc_cert_file_path_defaults() {
        let default_auth_token_path = PlatformSettings::get_auth_token_path();

        // When the IPC cert file path is not set, it should default to the same directory as the auth token file using
        // the default certificate file name.
        let config = get_remote_agent_config(None, Some(&default_auth_token_path)).await;
        assert_eq!(
            config.auth().ipc_cert_file_path().parent(),
            default_auth_token_path.as_path().parent()
        );
        assert_eq!(
            config.auth().ipc_cert_file_path().file_name().map(Path::new),
            Some(PlatformSettings::get_ipc_cert_filename())
        );
    }

    #[tokio::test]
    async fn ipc_cert_file_path_explicitly_set() {
        let default_auth_token_path = PlatformSettings::get_auth_token_path();
        let custom_ipc_cert_path = PathBuf::from("/tmp/custom_ipc_cert.pem");

        // When the IPC cert file path is explicitly set, it should be used.
        let config = get_remote_agent_config(Some(&custom_ipc_cert_path), Some(&default_auth_token_path)).await;
        assert_eq!(custom_ipc_cert_path, config.auth().ipc_cert_file_path());
    }

    #[tokio::test]
    async fn ipc_cert_file_path_custom_auth_token_path() {
        let custom_auth_token_path = PathBuf::from("/secret/auth_token");

        // When the IPC cert file path is not set, but there's a custom auth token path (explicitly set, different from the default),
        // we should still look in the same directory as the auth token file using the default certificate file name.
        let config = get_remote_agent_config(None, Some(&custom_auth_token_path)).await;
        assert_eq!(
            config.auth().ipc_cert_file_path().parent(),
            custom_auth_token_path.as_path().parent()
        );
        assert_eq!(
            config.auth().ipc_cert_file_path().file_name().map(Path::new),
            Some(PlatformSettings::get_ipc_cert_filename())
        );
    }

    #[tokio::test]
    async fn ipc_cert_file_path_invalid_auth_token_path() {
        let invalid_auth_token_path = PathBuf::from("/");

        // If the auth token file path is somehow unset or invalid (for example, no parent directory), we should use the same
        // logic but with the default Datadog Agent configuration directory.
        let config = get_remote_agent_config(None, Some(&invalid_auth_token_path)).await;
        assert_eq!(
            config.auth().ipc_cert_file_path().parent(),
            Some(PlatformSettings::get_config_dir_path())
        );
        assert_eq!(
            config.auth().ipc_cert_file_path().file_name().map(Path::new),
            Some(PlatformSettings::get_ipc_cert_filename())
        );
    }

    async fn config_from_values(values: serde_json::Map<String, serde_json::Value>) -> RemoteAgentClientConfiguration {
        let (base_config, _) =
            ConfigurationLoader::for_tests(Some(serde_json::Value::Object(values)), None, false).await;
        RemoteAgentClientConfiguration::from_configuration(&base_config).unwrap()
    }

    #[tokio::test]
    async fn endpoint_defaults_to_port_5001() {
        let config = config_from_values(serde_json::Map::new()).await;
        assert_eq!(config.endpoint().unwrap().to_string(), "https://127.0.0.1:5001/");
    }

    #[tokio::test]
    async fn endpoint_uses_cmd_port() {
        let mut values = serde_json::Map::new();
        values.insert("cmd_port".to_string(), 7777.into());
        let config = config_from_values(values).await;
        assert_eq!(config.endpoint().unwrap().to_string(), "https://127.0.0.1:7777/");
    }

    #[tokio::test]
    async fn cmd_port_takes_precedence_over_ipc_endpoint() {
        let mut values = serde_json::Map::new();
        values.insert("cmd_port".to_string(), 8888.into());
        values.insert("agent_ipc_endpoint".to_string(), "https://10.0.0.1:3333".into());
        let config = config_from_values(values).await;
        assert_eq!(config.endpoint().unwrap().to_string(), "https://127.0.0.1:8888/");
    }

    #[tokio::test]
    async fn ipc_endpoint_used_when_no_cmd_port() {
        let mut values = serde_json::Map::new();
        values.insert("agent_ipc_endpoint".to_string(), "https://10.0.0.1:3333".into());
        let config = config_from_values(values).await;
        assert_eq!(config.endpoint().unwrap().to_string(), "https://10.0.0.1:3333/");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn vsock_addr_valid_values() {
        // (vsock_addr input, expected CID — port always comes from cmd_port=5001)
        let cases: &[(&str, Option<u32>)] = &[
            ("", None),
            ("host", Some(2)),
            ("hypervisor", Some(0)),
            ("local", Some(3)),
        ];

        for (input, expected_cid) in cases {
            let mut values = serde_json::Map::new();
            values.insert("vsock_addr".to_string(), (*input).into());
            values.insert("cmd_port".to_string(), 5001u16.into());
            let config = config_from_values(values).await;
            let result = config
                .vsock_addr()
                .expect("vsock_addr() should not error with cmd_port set");
            assert_eq!(result.map(|a| a.cid()), *expected_cid, "input: {input:?}");
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn vsock_addr_invalid_values() {
        let cases = &["invalid", "2", "HOST", "host ", "vm0"];

        for input in cases {
            let mut values = serde_json::Map::new();
            values.insert("vsock_addr".to_string(), (*input).into());
            let (base_config, _) =
                ConfigurationLoader::for_tests(Some(serde_json::Value::Object(values)), None, false).await;
            assert!(
                RemoteAgentClientConfiguration::from_configuration(&base_config).is_err(),
                "expected error for input: {input:?}",
            );
        }
    }
}
