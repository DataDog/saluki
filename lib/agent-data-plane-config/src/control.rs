//! Topology gates, orchestration decisions and application configuration.
//!
//! `ControlConfiguration` is read only by config-system and the topology builder, not by
//! components. It carries pipeline activation gates, topology-shaping decisions, listen addresses,
//! logging (read before topology exists), bootstrap IPC parameters, and process-lifecycle knobs.

use std::time::Duration;

use saluki_io::net::ListenAddress;
use serde::Serialize;

use crate::defaults::FAKE_LISTEN_ADDRESS;

/// Topology gates and orchestration decisions. Static for the process lifetime.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ControlConfiguration {
    /// Master switch for the whole data plane; when false, no pipelines are built.
    pub enabled: bool,

    /// Whether the DogStatsD metrics pipeline is built.
    pub dogstatsd: bool,

    /// Whether the checks metrics pipeline is built. (not in Datadog Agent config schema)
    pub checks: bool,

    /// Whether the OTLP pipeline is built.
    pub otlp: bool,

    /// Whether standalone mode is active, running without a core Agent. (not in Datadog Agent
    /// config schema)
    pub standalone_mode: bool,

    /// Whether the process registers itself with the core Agent as a remote agent.
    pub remote_agent_enabled: bool,

    /// Whether to subscribe to core Agent configuration updates over the newer config-stream
    /// endpoint.
    pub use_new_config_stream_endpoint: bool,

    /// Address the unsecured control API listens on.
    pub api_listen_address: ListenAddress,

    /// Address the TLS-secured control API listens on.
    pub secure_api_listen_address: ListenAddress,

    /// Logging configuration, read before runtime authority exists.
    pub logging: Logging,

    /// Bootstrap IPC and remote-agent connection parameters.
    pub ipc: ControlIpc,

    /// Grace period, configured in seconds, the aggregator is given to flush before shutdown.
    pub aggregator_stop_timeout: Duration,

    /// Optional Saluki-only override for the topology shutdown grace period, in seconds.
    ///
    /// Defaults to `None`. When absent, the topology timeout is the sum of
    /// `aggregator_stop_timeout` and `forwarder_stop_timeout`.
    pub stop_timeout_seconds: Option<u64>,

    /// Process memory ceiling, in bytes. (not in Datadog Agent config schema)
    pub memory_limit: u64,

    /// Fraction of the memory limit held back as headroom during memory accounting. (not in Datadog
    /// Agent config schema)
    pub memory_slop_factor: f64,
}

impl Default for ControlConfiguration {
    // Translation overwrites the listen addresses before publishing the configuration. Use the
    // shared placeholder here rather than duplicating Datadog schema defaults.
    fn default() -> Self {
        Self {
            enabled: false,
            dogstatsd: false,
            checks: false,
            otlp: false,
            standalone_mode: false,
            remote_agent_enabled: false,
            use_new_config_stream_endpoint: false,
            api_listen_address: FAKE_LISTEN_ADDRESS,
            secure_api_listen_address: FAKE_LISTEN_ADDRESS,
            logging: Default::default(),
            ipc: Default::default(),
            aggregator_stop_timeout: Default::default(),
            stop_timeout_seconds: None,
            memory_limit: 0,
            memory_slop_factor: 0.0,
        }
    }
}

impl ControlConfiguration {
    /// Derived decision the topology builder reads. The outbound Datadog forwarder is needed only
    /// if some pipeline that emits to Datadog is enabled.
    pub fn requires_datadog_forwarder(&self) -> bool {
        self.dogstatsd || self.checks || self.otlp
    }
}

/// Logging configuration, read before runtime authority exists.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Logging {
    /// Minimum severity a record must reach to be emitted.
    pub level: String,

    /// Whether log timestamps are formatted as RFC 3339.
    pub format_rfc3339: bool,

    /// Whether log records are emitted as JSON.
    pub format_json: bool,

    /// Whether logs are written to the console.
    pub to_console: bool,

    /// Whether logs are forwarded to syslog.
    pub to_syslog: bool,

    /// Whether syslog messages use the RFC 5424 framing.
    pub syslog_rfc: bool,

    /// Destination URI for syslog forwarding.
    pub syslog_uri: String,

    /// Path of the log file.
    pub file: String,

    /// Whether file logging is turned off entirely.
    pub disable_file_logging: bool,

    /// Number of rotated log files retained.
    pub file_max_rolls: usize,

    /// Maximum size, in bytes, a log file reaches before it is rotated.
    pub file_max_size: u64,
}

/// Bootstrap IPC and remote-agent connection parameters, read before runtime authority exists.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ControlIpc {
    /// TCP port the command API listens on.
    pub cmd_port: u16,

    /// vsock address used for guest/host IPC.
    pub vsock_addr: String,

    /// Maximum gRPC message size, in bytes, accepted over the remote-agent IPC channel.
    pub grpc_max_message_size: i64,

    /// Timeout for establishing a connection to the container runtime interface.
    pub cri_connection_timeout: i64,

    /// Timeout for a single container runtime interface query.
    pub cri_query_timeout: i64,

    /// Byte budget for the remote-agent IPC string interner. (not in Datadog Agent config schema)
    pub remote_agent_string_interner_size_bytes: usize,
}
