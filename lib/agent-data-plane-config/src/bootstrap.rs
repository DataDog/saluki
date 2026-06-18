//! [`BootstrapConfiguration`]: the small typed slice read before runtime authority exists.
//!
//! `BootstrapConfiguration` is the pre-runtime view, drawn from two source languages. Its struct
//! definition IS the bootstrap allowlist: a key qualifies only if it is needed pre-stream, in one of
//! four categories (logging, early telemetry, local CLI decisions, Agent IPC). The bias is strongly
//! against adding fields. If a value can wait for the runtime
//! [`SalukiConfiguration`](crate::SalukiConfiguration), it must.
//!
//! The source-class split is structural: [`DatadogBootstrap`] is loaded only from local
//! `datadog.yaml` / `DD_*` (the Datadog loader, with aliases and remapper); [`SalukiBootstrap`] is
//! loaded only from `saluki.yaml` / `SALUKI_*`. A Saluki key therefore cannot accidentally read a
//! `DD_*` variable, because it is not part of the Datadog sub-read at all.

/// The pre-runtime typed bootstrap slice.
///
/// Read before runtime authority exists. Drawn from two source languages, split structurally into
/// [`DatadogBootstrap`] and [`SalukiBootstrap`].
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct BootstrapConfiguration {
    /// The Datadog bootstrap slice (local `datadog.yaml` / `DD_*` only).
    pub datadog: DatadogBootstrap,

    /// The Saluki bootstrap slice (`saluki.yaml` / `SALUKI_*` only).
    pub saluki: SalukiBootstrap,
}

/// The tiny pre-runtime Datadog slice: logging, early telemetry, and Agent IPC.
///
/// Loaded from local Datadog sources only. Lifecycle value drift is expected: a key such as
/// `log_level` is read here from local sources at bootstrap, and the same key at runtime comes from
/// the stream (authority); the runtime value wins once the stream connects.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct DatadogBootstrap {
    /// Early logging configuration (level, format, file, syslog).
    ///
    /// Flattened: Datadog source keys are flat (`log_level`, `log_format_json`, ...), not nested
    /// under a `logging` map, so the sub-struct's flat fields map directly. Defaulted so a source
    /// that sets none of these keys still parses.
    #[serde(flatten, default)]
    pub logging: LoggingBootstrap,

    /// Early telemetry configuration.
    #[serde(flatten, default)]
    pub telemetry: TelemetryBootstrap,

    /// Agent IPC connection parameters.
    #[serde(flatten, default)]
    pub agent_ipc: AgentIpcBootstrap,

    /// Local API/CLI decisions read before runtime authority exists.
    ///
    /// These come from nested `data_plane.*` keys, which the typed flatten path does not resolve
    /// reliably; the config-system fills this slice explicitly during local source loading. It is
    /// therefore `#[serde(skip)]` here.
    #[serde(skip)]
    pub local_api: LocalApiBootstrap,
}

impl DatadogBootstrap {
    /// Returns the early logging configuration.
    pub fn logging(&self) -> &LoggingBootstrap {
        &self.logging
    }

    /// Returns the early telemetry configuration.
    pub fn telemetry(&self) -> &TelemetryBootstrap {
        &self.telemetry
    }

    /// Returns the Agent IPC connection parameters.
    pub fn agent_ipc(&self) -> &AgentIpcBootstrap {
        &self.agent_ipc
    }

    /// Returns the local API/CLI decisions.
    pub fn local_api(&self) -> &LocalApiBootstrap {
        &self.local_api
    }
}

/// Local API/CLI decisions read at bootstrap.
///
/// These are local control decisions needed before (or independent of) runtime authority: the
/// privileged ("secure") API listen address the non-run CLI handlers connect to and that the
/// remote-agent registration reports as its gRPC callback endpoint, and the DogStatsD UDS path the
/// `dogstatsd replay` CLI sends to. They are local decisions, so they belong in the bootstrap
/// allowlist rather than waiting for the runtime [`SalukiConfiguration`](crate::SalukiConfiguration).
#[derive(Clone, Debug, Default)]
pub struct LocalApiBootstrap {
    /// The privileged ("secure") API listen address (`data_plane.secure_api_listen_address`).
    ///
    /// Defaults to unset; callers fall back to the historical `tcp://0.0.0.0:5101` default.
    pub secure_api_listen_address: Option<String>,

    /// The DogStatsD UDS socket path (`dogstatsd_socket`).
    ///
    /// Used only by the `dogstatsd replay` CLI on Linux. Defaults to unset.
    pub dogstatsd_socket: Option<String>,
}

/// Early logging configuration read at bootstrap.
///
/// Mirrors the keys consulted by the binary's `LoggingConfigurationTranslator`
/// (`bin/agent-data-plane/src/internal/logging.rs`).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct LoggingBootstrap {
    /// The log level / filter directives.
    pub log_level: Option<String>,

    /// Whether to emit logs as JSON.
    pub log_format_json: Option<bool>,

    /// Whether to use RFC3339 timestamps.
    pub log_format_rfc3339: Option<bool>,

    /// Whether to log to the console.
    pub log_to_console: Option<bool>,

    /// Whether to log to syslog.
    pub log_to_syslog: Option<bool>,

    /// Whether to use RFC-format syslog messages.
    pub syslog_rfc: Option<bool>,

    /// The syslog endpoint URI.
    pub syslog_uri: Option<String>,

    /// The maximum size of the active log file before rotation.
    pub log_file_max_size: Option<String>,

    /// The number of rotated log files to keep.
    pub log_file_max_rolls: Option<usize>,

    /// Whether to disable file logging entirely.
    pub disable_file_logging: Option<bool>,

    /// The per-subagent ADP log file path (`data_plane.log_file`).
    pub data_plane_log_file: Option<String>,
}

/// Early telemetry configuration read at bootstrap.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct TelemetryBootstrap {
    /// The internal-telemetry metrics verbosity level (`metrics_level`).
    pub metrics_level: Option<String>,
}

/// Agent IPC connection parameters read at bootstrap.
///
/// Mirrors the keys consulted by the binary's remote-agent IPC setup
/// (`bin/agent-data-plane/src/internal/remote_agent.rs` and
/// `lib/datadog-agent/commons/src/ipc/config.rs`).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct AgentIpcBootstrap {
    /// The local command port used to connect to the Agent IPC endpoint.
    ///
    /// Takes precedence over `agent_ipc_endpoint` when set.
    pub cmd_port: Option<u16>,

    /// The Agent IPC endpoint URI (`agent_ipc_endpoint`).
    pub agent_ipc_endpoint: Option<String>,

    /// The path to the Agent authentication token file (`auth_token_file_path`).
    pub auth_token_file_path: Option<String>,
}

/// The Saluki-schema-only bootstrap slice.
///
/// Loaded from `saluki.yaml` / `SALUKI_*` only. Intentionally minimal: there are currently no
/// Saluki-schema-only values that must be read before runtime authority exists. Add a field here
/// only if a Saluki-schema-only value genuinely cannot wait for the runtime
/// [`SalukiConfiguration`](crate::SalukiConfiguration).
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct SalukiBootstrap {}
