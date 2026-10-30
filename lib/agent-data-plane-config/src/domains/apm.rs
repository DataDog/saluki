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
use crate::ConfigValue;

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
    fn listen_host_binds_every_interface_in_a_container_when_unset() {
        let defaulted_host = ConfigValue::defaulted(Some("localhost".to_string()));

        assert_eq!(Domain::default().listen_host(&defaulted_host, true), (None, true));
        assert_eq!(Domain::default().listen_host(&defaulted_host, false), (None, false));
    }
}
