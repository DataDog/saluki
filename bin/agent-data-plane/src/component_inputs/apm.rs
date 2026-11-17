//! Inputs of the APM relay.

use std::sync::Arc;

use agent_data_plane_config::{domains::apm, shared::SharedConfiguration};
use saluki_common::sync::live::Live;
use saluki_components::relays::apm::{ApmRelayConfiguration, TracerFlareConfiguration};
use saluki_env::workload::CaptureEntityResolver;

use super::forwarder_configuration;

/// Builds the relay inputs with the effective listen host, the tracer container resolver, and the API key tracer flares
/// are sent with.
pub(crate) fn apm_relay_configuration(
    apm: &apm::Domain, shared: &SharedConfiguration, containerized: bool,
    capture_entity_resolver: Option<Arc<dyn CaptureEntityResolver + Send + Sync>>, api_key: Live<String>,
) -> ApmRelayConfiguration {
    let (bind_host, non_local_traffic) = apm.listen_host(&shared.bind_host, containerized);

    ApmRelayConfiguration {
        receiver_socket: apm.receiver_socket.clone().unwrap_or_default(),
        proxy_destination: apm.proxy_destination.clone(),
        capture_entity_resolver,
        bind_host,
        non_local_traffic,
        receiver_port: apm.receiver_port,
        tracer_flare: TracerFlareConfiguration {
            site: shared.endpoints.site.value.clone(),
            api_key,
            forwarder: forwarder_configuration(shared),
        },
    }
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::ConfigValue;
    use saluki_env::workload::EntityId;

    use super::*;

    struct NoContainerResolver;

    impl CaptureEntityResolver for NoContainerResolver {
        fn resolve_container_entity_for_live_pid(&self, _process_id: u32) -> Option<EntityId> {
            None
        }
    }

    #[test]
    fn apm_relay_configuration_comes_from_resolved_configuration() {
        let apm = apm::Domain {
            receiver_port: 9126,
            receiver_socket: Some("/tmp/receiver.sock".to_string()),
            proxy_destination: "unix:///tmp/destination.sock".to_string(),
            non_local_traffic: ConfigValue::explicit(true),
            // The relay doesn't consume the trace-agent's reported settings yet.
            ..Default::default()
        };
        let mut shared = SharedConfiguration {
            bind_host: ConfigValue::explicit(Some("192.0.2.1".to_string())),
            ..Default::default()
        };
        shared.endpoints.site = ConfigValue::explicit("datadoghq.eu".to_string());
        let resolver: Arc<dyn CaptureEntityResolver + Send + Sync> = Arc::new(NoContainerResolver);
        let config = apm_relay_configuration(
            &apm,
            &shared,
            false,
            Some(resolver.clone()),
            Live::new_fixed("api-key".to_string()),
        );

        assert_eq!(config.receiver_port, 9126);
        assert_eq!(config.receiver_socket, "/tmp/receiver.sock");
        assert_eq!(config.proxy_destination, "unix:///tmp/destination.sock");
        assert_eq!(config.bind_host.as_deref(), Some("192.0.2.1"));
        assert!(config.non_local_traffic);
        assert!(Arc::ptr_eq(config.capture_entity_resolver.as_ref().unwrap(), &resolver));
        assert_eq!(config.tracer_flare.site, "datadoghq.eu");
        assert_eq!(config.tracer_flare.api_key.as_str(), "api-key");
    }

    #[test]
    fn apm_relay_configuration_resolves_container_fallback_and_disabled_socket() {
        let apm = apm::Domain::default();
        let shared = SharedConfiguration {
            bind_host: ConfigValue::defaulted(Some("localhost".to_string())),
            ..Default::default()
        };
        for containerized in [false, true] {
            let config = apm_relay_configuration(&apm, &shared, containerized, None, Live::new_fixed(String::new()));
            assert_eq!(config.non_local_traffic, containerized);
            assert_eq!(config.bind_host, None);
            assert!(config.receiver_socket.is_empty());
            assert!(config.capture_entity_resolver.is_none());
        }
    }
}
