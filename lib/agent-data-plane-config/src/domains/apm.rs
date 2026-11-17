//! APM domain: the experimental trace receiver proxy.
//!
//! With the proxy enabled, ADP takes over the trace-agent's receiver: it listens where the trace-agent normally listens
//! and forwards requests for endpoints it does not handle itself to the trace-agent, which the Datadog Agent moves to
//! [`proxy_destination`](Domain::proxy_destination). Trace *processing* (obfuscation, sampling, stats) is shared with
//! the OTLP path and lives in the [`traces`](super::traces) domain.
//!
//! The listen settings are the trace-agent's own `apm_config.*` keys, so ADP binds exactly the address tracers are
//! already configured to reach. The global `bind_host` also applies, and lives in the shared configuration.
//!
//! The domain also holds the trace-agent receiver settings that ADP doesn't apply itself but reports to tracers, such
//! as the connection limit and the resource limits, as the trace-agent reads them.

use serde::Serialize;

use crate::defaults::DEFAULT_APM_PROXY_DESTINATION;
use crate::ConfigValue;

/// Resolved APM configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
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
    ///
    /// The trace-agent also binds every interface when it runs in a container and neither `bind_host` nor
    /// `apm_config.apm_non_local_traffic` was set explicitly. That fallback isn't part of the configuration, so the
    /// provenance is kept to resolve the relay's listen host.
    pub non_local_traffic: ConfigValue<bool>,

    /// Where the relocated trace-agent listens, as a URL (`data_plane.experimental.apm.proxy_destination`).
    ///
    /// Either `http://host:port` or `unix:///path/to/socket`. Every request is forwarded
    /// here. Defaults to `http://127.0.0.1:8127`. (not in Datadog Agent config schema)
    ///
    /// Over a Unix domain socket, the trace-agent attributes a request that carries no container headers to the peer
    /// of the connection, which is ADP. That covers requests from TCP clients that send no container headers, and from
    /// Unix domain socket clients whose container ADP couldn't resolve. An `http://` destination leaves their origin
    /// unknown instead.
    pub proxy_destination: String,

    /// Maximum number of connections the trace-agent's receiver accepts (`apm_config.connection_limit`).
    ///
    /// `0` when the key is unset: the trace-agent reads the key only when it is set, so it ignores the schema
    /// default of 2000. A negative value is read as `0`, with a warning: the trace-agent applies no limit for either.
    pub connection_limit: u32,

    /// Timeout, in seconds, for a request to the trace-agent's receiver (`apm_config.receiver_timeout`).
    ///
    /// `0`, the default, leaves the trace-agent's own default of 5 seconds. A negative value is read as `0`, with a
    /// warning: the trace-agent uses its default for either.
    pub receiver_timeout: u32,

    /// Maximum size, in bytes, of a request body the trace-agent's receiver accepts (`apm_config.max_payload_size`).
    ///
    /// Defaults to 25 MiB. A negative value is a translation error.
    pub max_payload_size: u64,

    /// Memory, in bytes, the trace-agent aims to stay under (`apm_config.max_memory`).
    ///
    /// The provenance is kept because the trace-agent disables the limit in a container when the key is unset; see
    /// [`Domain::effective_max_memory`].
    pub max_memory: ConfigValue<f64>,

    /// CPU, in percent of a core, the trace-agent aims to stay under (`apm_config.max_cpu_percent`).
    ///
    /// The provenance is kept because the trace-agent disables the limit in a container when the key is unset; see
    /// [`Domain::effective_max_cpu`].
    pub max_cpu_percent: ConfigValue<f64>,

    /// Whether the trace-agent proxies tracer telemetry on `/telemetry/proxy/` (`apm_config.telemetry.enabled`).
    pub telemetry_enabled: bool,
}

impl Default for Domain {
    fn default() -> Self {
        Self {
            // Saluki-only settings retain these defaults when unset.
            proxy_destination: DEFAULT_APM_PROXY_DESTINATION.to_string(),
            // Witnessed settings are overwritten during translation.
            receiver_port: 0,
            receiver_socket: None,
            non_local_traffic: ConfigValue::defaulted(false),
            connection_limit: 0,
            receiver_timeout: 0,
            max_payload_size: 0,
            max_memory: ConfigValue::defaulted(0.0),
            max_cpu_percent: ConfigValue::defaulted(0.0),
            telemetry_enabled: false,
        }
    }
}

impl Domain {
    /// Resolves the TCP listen host and whether to bind every interface.
    ///
    /// Explicit host or traffic settings apply as configured. Otherwise, containers bind every interface and
    /// non-containerized processes use the default loopback host.
    pub fn listen_host(&self, bind_host: &ConfigValue<Option<String>>, containerized: bool) -> (Option<String>, bool) {
        if bind_host.is_explicit() || self.non_local_traffic.is_explicit() {
            let host = if bind_host.is_explicit() {
                bind_host.value.clone()
            } else {
                None
            };
            (host, self.non_local_traffic.value)
        } else {
            (None, containerized)
        }
    }

    /// Resolves the memory, in bytes, the trace-agent aims to stay under, where `0` disables the limit.
    ///
    /// As in the trace-agent, a container leaves resource limits to its runtime: the limit is disabled there unless
    /// `apm_config.max_memory` was set explicitly.
    pub fn effective_max_memory(&self, containerized: bool) -> f64 {
        if containerized && !self.max_memory.is_explicit() {
            0.0
        } else {
            self.max_memory.value
        }
    }

    /// Resolves the CPU the trace-agent aims to stay under, as a fraction of a core, where `0` disables the limit.
    ///
    /// As in the trace-agent, a container leaves resource limits to its runtime: the limit is disabled there unless
    /// `apm_config.max_cpu_percent` was set explicitly.
    pub fn effective_max_cpu(&self, containerized: bool) -> f64 {
        if containerized && !self.max_cpu_percent.is_explicit() {
            0.0
        } else {
            self.max_cpu_percent.value / 100.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_host_follows_explicit_settings() {
        let host = Some("10.0.0.1".to_string());

        // An explicit `bind_host` applies, in a container or not.
        for containerized in [false, true] {
            assert_eq!(
                Domain {
                    non_local_traffic: ConfigValue::defaulted(false),
                    ..Default::default()
                }
                .listen_host(&ConfigValue::explicit(host.clone()), containerized),
                (host.clone(), false)
            );
        }

        // `apm_non_local_traffic` overrides it, even at its default value once `bind_host` is explicit, as in
        // Kubernetes where it defaults to `true`.
        assert_eq!(
            Domain {
                non_local_traffic: ConfigValue::defaulted(true),
                ..Default::default()
            }
            .listen_host(&ConfigValue::explicit(host.clone()), false),
            (host.clone(), true)
        );

        // An explicit `apm_non_local_traffic: false` keeps a container on the default host.
        assert_eq!(
            Domain {
                non_local_traffic: ConfigValue::explicit(false),
                ..Default::default()
            }
            .listen_host(&ConfigValue::defaulted(Some("localhost".to_string())), true),
            (None, false)
        );
    }

    #[test]
    fn resource_limits_are_disabled_in_a_container_unless_set() {
        let defaulted = Domain {
            max_memory: ConfigValue::defaulted(5e8),
            max_cpu_percent: ConfigValue::defaulted(50.0),
            ..Default::default()
        };
        assert_eq!(defaulted.effective_max_memory(false), 5e8);
        assert_eq!(defaulted.effective_max_cpu(false), 0.5);
        assert_eq!(defaulted.effective_max_memory(true), 0.0);
        assert_eq!(defaulted.effective_max_cpu(true), 0.0);

        let explicit = Domain {
            max_memory: ConfigValue::explicit(1e9),
            max_cpu_percent: ConfigValue::explicit(200.0),
            ..Default::default()
        };
        for containerized in [false, true] {
            assert_eq!(explicit.effective_max_memory(containerized), 1e9);
            assert_eq!(explicit.effective_max_cpu(containerized), 2.0);
        }
    }

    #[test]
    fn listen_host_binds_every_interface_in_a_container_when_unset() {
        let defaulted_host = ConfigValue::defaulted(Some("localhost".to_string()));

        assert_eq!(Domain::default().listen_host(&defaulted_host, true), (None, true));
        assert_eq!(Domain::default().listen_host(&defaulted_host, false), (None, false));
    }
}
