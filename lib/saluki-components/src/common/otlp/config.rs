//! Shared OTLP receiver configuration.

use serde::Deserialize;

fn default_grpc_endpoint() -> String {
    "0.0.0.0:4317".to_string()
}

fn default_http_endpoint() -> String {
    "0.0.0.0:4318".to_string()
}

fn default_transport() -> String {
    "tcp".to_string()
}

fn default_max_recv_msg_size_mib() -> u64 {
    4
}

fn default_traces_internal_port() -> u16 {
    5003
}

/// Receiver configuration for OTLP endpoints.
///
/// This follows the Agent's `otlp_config.receiver` structure.
#[derive(Deserialize, Debug, Default)]
pub struct Receiver {
    /// Protocol-specific receiver configuration.
    #[serde(default)]
    pub protocols: Protocols,
}

/// Traces configuration for OTLP.
#[derive(Clone, Deserialize, Debug)]
pub struct Traces {
    /// The internal port where trace-agent's OTLP gRPC receiver listens.
    ///
    /// Defaults to 5003.
    #[serde(default = "default_traces_internal_port")]
    pub internal_port: u16,
}

impl Default for Traces {
    fn default() -> Self {
        Self {
            internal_port: default_traces_internal_port(),
        }
    }
}

/// Protocol configuration for OTLP receiver.
#[derive(Deserialize, Debug, Default)]
pub struct Protocols {
    /// gRPC protocol configuration.
    #[serde(default)]
    pub grpc: GrpcConfig,

    /// HTTP protocol configuration.
    #[serde(default)]
    pub http: HttpConfig,
}

/// gRPC receiver configuration.
#[derive(Deserialize, Debug)]
pub struct GrpcConfig {
    /// The gRPC endpoint to listen on for OTLP requests.
    ///
    /// Defaults to `0.0.0.0:4317`.
    #[serde(default = "default_grpc_endpoint")]
    pub endpoint: String,

    /// The transport protocol to use for the gRPC listener.
    ///
    /// Defaults to `tcp`.
    #[serde(default = "default_transport")]
    pub transport: String,

    /// Maximum size (in MiB) of a gRPC message that can be received.
    ///
    /// Defaults to 4 MiB.
    #[serde(default = "default_max_recv_msg_size_mib", rename = "max_recv_msg_size_mib")]
    pub max_recv_msg_size_mib: u64,
}

/// HTTP receiver configuration.
#[derive(Deserialize, Debug)]
pub struct HttpConfig {
    /// The HTTP endpoint to listen on for OTLP requests.
    ///
    /// Defaults to `0.0.0.0:4318`.
    #[serde(default = "default_http_endpoint")]
    pub endpoint: String,

    /// The transport protocol to use for the HTTP listener.
    ///
    /// Defaults to `tcp`.
    #[serde(default = "default_transport")]
    pub transport: String,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            endpoint: default_grpc_endpoint(),
            transport: default_transport(),
            max_recv_msg_size_mib: default_max_recv_msg_size_mib(),
        }
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            endpoint: default_http_endpoint(),
            transport: default_transport(),
        }
    }
}
