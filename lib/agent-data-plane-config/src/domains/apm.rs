//! APM domain: the experimental trace receiver proxy.
//!
//! With the proxy enabled, ADP takes over the trace-agent's receiver: it listens where the trace-agent normally listens
//! and forwards every request to the trace-agent, which the Datadog Agent moves to
//! [`proxy_destination`](Domain::proxy_destination). Trace *processing* (obfuscation, sampling, stats) is shared with
//! the OTLP path and lives in the [`traces`](super::traces) domain.
//!
//! The listen settings are the trace-agent's own `apm_config.*` keys, so ADP binds exactly the address tracers are
//! already configured to reach. The global `bind_host` also applies, and lives in the shared configuration.

use serde::Serialize;

use crate::defaults::DEFAULT_APM_PROXY_DESTINATION;

/// Resolved APM configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Domain {
    /// TCP port the trace receiver listens on (`apm_config.receiver_port`).
    ///
    /// `0` disables the TCP listener.
    pub receiver_port: u16,

    /// Unix domain socket path the trace receiver listens on (`apm_config.receiver_socket`).
    ///
    /// `None` disables the Unix domain socket listener. Defaults to `/var/run/datadog/apm.socket` on Linux and AIX,
    /// matching the trace-agent, and to disabled elsewhere.
    pub receiver_socket: Option<String>,

    /// Whether the TCP listener accepts traffic from other hosts (`apm_config.apm_non_local_traffic`).
    ///
    /// When `true`, the listener binds every interface, overriding `bind_host`, as the trace-agent does.
    pub non_local_traffic: bool,

    /// Where the relocated trace-agent listens, as a URL (`data_plane.experimental.apm.proxy_destination`).
    ///
    /// Either `http://host:port` or `unix:///path/to/socket`. Every request ADP does not handle itself is forwarded
    /// here. Defaults to `http://127.0.0.1:8127`. (not in Datadog Agent config schema)
    pub proxy_destination: String,

    /// Maximum accepted `/v1.0/traces` request body size, in bytes.
    ///
    /// Requests whose body exceeds this size are rejected with `413 Payload Too Large` before any decoding is attempted.
    ///
    /// Defaults to `26214400` (25MiB).
    pub max_payload_size: usize,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            // Saluki-only settings retain these defaults when unset.
            proxy_destination: DEFAULT_APM_PROXY_DESTINATION.to_string(),
            // Witnessed settings are overwritten during translation.
            receiver_port: 0,
            receiver_socket: None,
            non_local_traffic: false,
            max_payload_size: 26214400,
        }
    }
}
