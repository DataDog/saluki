//! IPC configuration.

use std::path::PathBuf;

use saluki_config::GenericConfiguration;
use saluki_error::{ErrorContext as _, GenericError};
use serde::Deserialize;
use tonic::transport::Uri;

use crate::platform::PlatformSettings;

/// Datadog Agent IPC bearer-token and exact shared-certificate mTLS configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct IpcAuthConfiguration {
    /// Path to the Agent authentication token file.
    ///
    /// The contents of the file are passed as a bearer token in RPC requests to the IPC endpoint.
    ///
    /// Defaults to `<conf dir>/auth_token`, where `<conf dir>` is the platform-specific directory containing the Agent
    /// configuration.
    auth_token_file_path: PathBuf,

    /// Path to the shared Agent IPC mTLS identity file.
    ///
    /// The PEM file contains one certificate and its private key. IPC peers require exact leaf-certificate DER equality
    /// rather than CA-based trust, and each peer proves possession of the corresponding private key during the TLS
    /// handshake. The same identity authenticates both the client and server, so a CA chain cannot broaden it.
    ///
    /// Defaults to `ipc_cert.pem` in the same directory as the Agent authentication token file. (for example, if
    /// `auth_token_file_path` is `/etc/datadog-agent/auth_token`, this will be `/etc/datadog-agent/ipc_cert.pem`.)
    ipc_cert_file_path: Option<PathBuf>,
}

impl IpcAuthConfiguration {
    /// Creates a new `IpcAuthConfiguration` from the given configuration.
    ///
    /// # Errors
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

    /// Gets the shared IPC mTLS identity file path from the configuration.
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
#[derive(Clone, Debug)]
pub struct RemoteAgentClientConfiguration {
    /// Core Agent CMD API port used for remote-agent gRPC IPC on localhost.
    pub cmd_port: u16,

    /// Authentication configuration for the IPC endpoint.
    pub auth: IpcAuthConfiguration,

    /// Maximum message size for gRPC messages.
    pub grpc_max_message_size: usize,

    /// Resolved CID for connecting to the Agent IPC endpoint via AF_VSOCK.
    #[cfg(target_os = "linux")]
    pub vsock_cid: Option<u32>,
}

impl RemoteAgentClientConfiguration {
    /// Returns the Core Agent CMD API gRPC endpoint URI.
    pub fn endpoint(&self) -> Uri {
        format!("https://127.0.0.1:{}", self.cmd_port)
            .parse()
            .expect("a URI built from a u16 port is valid")
    }

    /// Returns the vsock address to use for connecting to the IPC endpoint, if configured.
    #[cfg(target_os = "linux")]
    pub fn vsock_addr(&self) -> Option<tokio_vsock::VsockAddr> {
        self.vsock_cid
            .map(|cid| tokio_vsock::VsockAddr::new(cid, u32::from(self.cmd_port)))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use saluki_config::ConfigurationLoader;

    use super::{IpcAuthConfiguration, RemoteAgentClientConfiguration};
    use crate::platform::PlatformSettings;

    async fn get_auth_config(
        ipc_cert_file_path: Option<&Path>, auth_token_file_path: Option<&Path>,
    ) -> IpcAuthConfiguration {
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
        IpcAuthConfiguration::from_configuration(&base_config).unwrap()
    }

    #[tokio::test]
    async fn ipc_cert_file_path_empty_config() {
        let default_auth_token_path = PlatformSettings::get_auth_token_path();

        // When the auth token file path _and_ IPC cert file path are both unset, we should default to looking for the
        // IPC cert in the same directory as the auth token.
        let config = get_auth_config(None, None).await;
        assert_eq!(
            config.ipc_cert_file_path().parent(),
            default_auth_token_path.as_path().parent()
        );
        assert_eq!(
            config.ipc_cert_file_path().file_name().map(Path::new),
            Some(PlatformSettings::get_ipc_cert_filename())
        );
    }

    #[tokio::test]
    async fn ipc_cert_file_path_defaults() {
        let default_auth_token_path = PlatformSettings::get_auth_token_path();

        // When the IPC cert file path is not set, it should default to the same directory as the auth token file using
        // the default certificate file name.
        let config = get_auth_config(None, Some(&default_auth_token_path)).await;
        assert_eq!(
            config.ipc_cert_file_path().parent(),
            default_auth_token_path.as_path().parent()
        );
        assert_eq!(
            config.ipc_cert_file_path().file_name().map(Path::new),
            Some(PlatformSettings::get_ipc_cert_filename())
        );
    }

    #[tokio::test]
    async fn ipc_cert_file_path_explicitly_set() {
        let default_auth_token_path = PlatformSettings::get_auth_token_path();
        let custom_ipc_cert_path = PathBuf::from("/tmp/custom_ipc_cert.pem");

        // When the IPC cert file path is explicitly set, it should be used.
        let config = get_auth_config(Some(&custom_ipc_cert_path), Some(&default_auth_token_path)).await;
        assert_eq!(custom_ipc_cert_path, config.ipc_cert_file_path());
    }

    #[tokio::test]
    async fn ipc_cert_file_path_custom_auth_token_path() {
        let custom_auth_token_path = PathBuf::from("/secret/auth_token");

        // When the IPC cert file path is not set, but there's a custom auth token path (explicitly set, different from the default),
        // we should still look in the same directory as the auth token file using the default certificate file name.
        let config = get_auth_config(None, Some(&custom_auth_token_path)).await;
        assert_eq!(
            config.ipc_cert_file_path().parent(),
            custom_auth_token_path.as_path().parent()
        );
        assert_eq!(
            config.ipc_cert_file_path().file_name().map(Path::new),
            Some(PlatformSettings::get_ipc_cert_filename())
        );
    }

    #[tokio::test]
    async fn ipc_cert_file_path_invalid_auth_token_path() {
        let invalid_auth_token_path = PathBuf::from("/");

        // If the auth token file path is somehow unset or invalid (for example, no parent directory), we should use the same
        // logic but with the default Datadog Agent configuration directory.
        let config = get_auth_config(None, Some(&invalid_auth_token_path)).await;
        assert_eq!(
            config.ipc_cert_file_path().parent(),
            Some(PlatformSettings::get_config_dir_path())
        );
        assert_eq!(
            config.ipc_cert_file_path().file_name().map(Path::new),
            Some(PlatformSettings::get_ipc_cert_filename())
        );
    }

    fn remote_agent_config(cmd_port: u16) -> RemoteAgentClientConfiguration {
        RemoteAgentClientConfiguration {
            cmd_port,
            auth: IpcAuthConfiguration::default(),
            grpc_max_message_size: 128 * 1024 * 1024,
            #[cfg(target_os = "linux")]
            vsock_cid: None,
        }
    }

    #[test]
    fn endpoint_uses_cmd_port() {
        for (cmd_port, expected) in [(5001, "https://127.0.0.1:5001/"), (7777, "https://127.0.0.1:7777/")] {
            assert_eq!(remote_agent_config(cmd_port).endpoint().to_string(), expected);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn vsock_addr_uses_resolved_cid_and_cmd_port() {
        let mut config = remote_agent_config(5001);
        assert_eq!(config.vsock_addr(), None);

        config.vsock_cid = Some(2);
        let addr = config.vsock_addr().expect("vsock address should be configured");
        assert_eq!(addr.cid(), 2);
        assert_eq!(addr.port(), 5001);
    }
}
