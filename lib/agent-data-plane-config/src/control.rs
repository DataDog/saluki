//! Topology gates, orchestration decisions and application configuration.
//!
//! `ControlConfiguration` is read only by config-system and the topology builder, not by
//! components. It carries pipeline activation gates, topology-shaping decisions, listen addresses,
//! logging (read before topology exists), bootstrap IPC parameters, and process-lifecycle knobs.

use std::{path::PathBuf, time::Duration};

use serde::Serialize;

use crate::ConfigValue;

/// Topology gates and orchestration decisions. Most are static; `logging.level` is live.
///
/// The derived `Default` is all zeroes, empty, and `false`, and serves only as the starting point for translation. The
/// effective default of each field is the one translation resolves, noted per field below.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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
    pub api_listen_address: String,

    /// Address the mutually authenticated control API listens on. Every HTTP and gRPC client must
    /// present the exact configured Agent IPC certificate during the TLS handshake.
    pub secure_api_listen_address: String,

    /// Logging configuration, read before runtime authority exists.
    pub logging: Logging,

    /// Bootstrap IPC and remote-agent connection parameters.
    pub ipc: ControlIpc,

    /// Grace period the aggregator is given to flush before shutdown.
    pub aggregator_stop_timeout: Duration,

    /// Override for the topology shutdown grace period.
    ///
    /// Defaults to `None`. When absent, the topology timeout is the sum of
    /// `aggregator_stop_timeout` and `forwarder_stop_timeout`.
    pub stop_timeout: Option<Duration>,

    /// Process memory ceiling, in bytes, that bounds validation and the global limiter work against.
    ///
    /// Defaults to `None`. When absent, ADP reads the ceiling from the process cgroup, but only when `DOCKER_DD_AGENT`
    /// is set to a non-empty value. When neither source supplies a value, bounds validation is skipped and the global
    /// limiter never exerts backpressure, whatever `memory_mode` and `enable_global_limiter` say.
    ///
    /// `Some(0)` is a ceiling of zero bytes rather than "no ceiling": every component bound then exceeds it, which is
    /// fatal under [`MemoryMode::Strict`]. A ceiling above 2^53 bytes is rejected during startup.
    ///
    /// Set this to the memory the process is allowed to use, and leave it unset only where cgroup detection supplies
    /// that number.
    pub memory_limit: Option<u64>,

    /// Fraction of `memory_limit` held back as headroom for memory the component bounds do not account for.
    ///
    /// Defaults to [`DEFAULT_MEMORY_SLOP_FACTOR`](crate::defaults::DEFAULT_MEMORY_SLOP_FACTOR) (`0.25`), which
    /// validates bounds against 75% of `memory_limit`. Valid values run from `0.0` up to but excluding `1.0`, where
    /// `0.0` holds nothing back. A value outside that range, including `NaN`, fails startup once a memory ceiling
    /// resolves, and goes unused when none does.
    ///
    /// Raise this for a workload whose real usage overshoots its validated bounds; lower it to hand more of a tight
    /// ceiling to the components that do account for their usage.
    pub memory_slop_factor: f64,

    /// Whether the global memory limiter exerts backpressure as usage approaches the effective ceiling.
    ///
    /// Defaults to [`DEFAULT_ENABLE_GLOBAL_LIMITER`](crate::defaults::DEFAULT_ENABLE_GLOBAL_LIMITER) (`true`). When
    /// `false`, the limiter is a no-op: it throttles nothing, and only the components' own bounds hold memory usage
    /// down. Either way it does nothing unless a memory ceiling resolves and `memory_mode` is
    /// [`MemoryMode::Permissive`] or [`MemoryMode::Strict`], because no other case installs a limiter.
    ///
    /// Turn this off to attribute a throughput drop to memory backpressure, accepting that the process can then run
    /// past `memory_limit`.
    pub enable_global_limiter: bool,

    /// How the component memory bounds are reconciled with the effective memory ceiling during startup.
    ///
    /// Defaults to [`MemoryMode::Disabled`]. Validation runs only when a memory ceiling resolves; without one,
    /// `Permissive` and `Strict` log that validation was skipped and startup continues.
    ///
    /// Run `Permissive` first to learn whether a ceiling fits the topology, then move to `Strict` where the platform
    /// kills a process that exceeds its ceiling and refusing to start is the better failure.
    pub memory_mode: MemoryMode,
}

/// Memory bounds validation and limiter behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryMode {
    /// Bounds validation is skipped and no limiter is installed, whatever `enable_global_limiter` says.
    #[default]
    Disabled,

    /// Bounds that do not fit the ceiling are logged as a warning and startup continues.
    ///
    /// Memory limiting is best effort: the limiter is installed when a ceiling resolves and
    /// `enable_global_limiter` is `true`.
    Permissive,

    /// Bounds that do not fit the ceiling fail startup.
    ///
    /// The limiter is installed on the same terms as [`MemoryMode::Permissive`].
    Strict,
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
    ///
    /// A defaulted or explicitly empty path selects the platform-specific ADP log file path.
    pub file: ConfigValue<String>,

    /// Whether file logging is turned off entirely.
    pub disable_file_logging: bool,

    /// Number of rotated log files retained.
    ///
    /// Defaults to `1`. The file writer retains one rotated file when this is `0`. A negative value is
    /// rejected during translation.
    pub file_max_rolls: usize,

    /// Maximum size, in bytes, a log file reaches before it is rotated.
    ///
    /// When defaulted, the logging stack keeps its own 10 MiB threshold instead.
    pub file_max_size: ConfigValue<u64>,
}

/// IPC and remote-agent connection parameters, read once at bootstrap before runtime authority
/// exists and again from the authoritative configuration once it does.
///
/// The derived `Default` is all zeroes and serves only as the starting point for translation. The
/// effective default of each field is the one the Datadog schema declares, noted per field below.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ControlIpc {
    /// Path to the Agent authentication token file.
    ///
    /// ADP sends the file contents as a bearer token to the Core Agent. Override this path only when the Core Agent
    /// uses a non-default token path, and configure both processes to use the same token.
    ///
    /// Defaults to an empty path, which selects the platform-specific Agent authentication token path.
    pub auth_token_file_path: PathBuf,

    /// Path to the shared Agent IPC mTLS identity file.
    ///
    /// The PEM file contains the certificate and private key used by ADP and its IPC peers. Every peer must use the
    /// same identity because authentication requires an exact certificate match. Override this path only when the Core
    /// Agent uses a non-default identity path.
    ///
    /// Defaults to an empty path, which selects `ipc_cert.pem` beside the resolved authentication token path.
    pub ipc_cert_file_path: PathBuf,

    /// TCP port the command API listens on.
    ///
    /// Defaults to `5001`.
    pub cmd_port: u16,

    /// vsock address used for guest/host IPC.
    ///
    /// Defaults to empty, which reaches the Core Agent over TCP on localhost at `cmd_port`.
    pub vsock_addr: String,

    /// Maximum gRPC message size, in bytes, accepted over the remote-agent IPC channel.
    ///
    /// Defaults to `134217728` (128 MiB).
    pub grpc_max_message_size: usize,

    /// Timeout for establishing a connection to the container runtime interface.
    pub cri_connection_timeout: i64,

    /// Timeout for a single container runtime interface query.
    pub cri_query_timeout: i64,

    /// Byte budget for the remote-agent IPC string interner. (not in Datadog Agent config schema)
    pub remote_agent_string_interner_size_bytes: usize,
}
