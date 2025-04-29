//! Autodiscovery provider.
//!
//! This module provides the `Autodiscovery` trait, which deals with providing information about autodiscovery.

pub mod providers;

use async_trait::async_trait;
use datadog_protos::agent::{Config as ProtoConfig, ConfigEventType};
use tokio::sync::broadcast::Receiver;

/// Configuration event type
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventType {
    /// Schedule a configuration
    Schedule,
    /// Unschedule a configuration
    Unschedule,
}

impl From<ConfigEventType> for EventType {
    fn from(event_type: ConfigEventType) -> Self {
        match event_type {
            ConfigEventType::Schedule => EventType::Schedule,
            ConfigEventType::Unschedule => EventType::Unschedule,
        }
    }
}

impl From<i32> for EventType {
    fn from(value: i32) -> Self {
        if value == ConfigEventType::Unschedule as i32 {
            EventType::Unschedule
        } else {
            // Default to Schedule for unknown values
            EventType::Schedule
        }
    }
}

/// Kubernetes namespaced name
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubeNamespacedName {
    /// Kubernetes resource name
    pub name: String,
    /// Kubernetes namespace
    pub namespace: String,
}

/// Advanced autodiscovery identifier
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvancedADIdentifier {
    /// Kubernetes service
    pub kube_service: Option<KubeNamespacedName>,
    /// Kubernetes endpoints
    pub kube_endpoints: Option<KubeNamespacedName>,
}

/// Configuration data
#[derive(Debug, Clone)]
pub struct Config {
    /// Configuration name/identifier
    pub name: String,
    /// Raw configuration data
    pub init_config: Vec<u8>,
    /// Instance configurations
    pub instances: Vec<Vec<u8>>,
    /// Metric configuration
    pub metric_config: Vec<u8>,
    /// Logs configuration
    pub logs_config: Vec<u8>,
    /// Auto-discovery identifiers
    pub ad_identifiers: Vec<String>,
    /// Advanced auto-discovery identifiers
    pub advanced_ad_identifiers: Vec<AdvancedADIdentifier>,
    /// Provider that discovered this config
    pub provider: String,
    /// Service ID
    pub service_id: String,
    /// Tagger entity
    pub tagger_entity: String,
    /// Whether this is a cluster check
    pub cluster_check: bool,
    /// Node name
    pub node_name: String,
    /// Source of the configuration
    pub source: String,
    /// Whether to ignore autodiscovery tags
    pub ignore_autodiscovery_tags: bool,
    /// Whether metrics are excluded
    pub metrics_excluded: bool,
    /// Whether logs are excluded
    pub logs_excluded: bool,
}

impl From<ProtoConfig> for Config {
    fn from(proto: ProtoConfig) -> Self {
        // Convert advanced AD identifiers from proto
        let advanced_ad_identifiers = proto
            .advanced_ad_identifiers
            .into_iter()
            .map(|adv_id| {
                let kube_service = adv_id.kube_service.map(|svc| KubeNamespacedName {
                    name: svc.name,
                    namespace: svc.namespace,
                });

                let kube_endpoints = adv_id.kube_endpoints.map(|endpoints| KubeNamespacedName {
                    name: endpoints.name,
                    namespace: endpoints.namespace,
                });

                AdvancedADIdentifier {
                    kube_service,
                    kube_endpoints,
                }
            })
            .collect();

        Self {
            name: proto.name,
            init_config: proto.init_config,
            instances: proto.instances,
            metric_config: proto.metric_config,
            logs_config: proto.logs_config,
            ad_identifiers: proto.ad_identifiers,
            advanced_ad_identifiers,
            provider: proto.provider,
            service_id: proto.service_id,
            tagger_entity: proto.tagger_entity,
            cluster_check: proto.cluster_check,
            node_name: proto.node_name,
            source: proto.source,
            ignore_autodiscovery_tags: proto.ignore_autodiscovery_tags,
            metrics_excluded: proto.metrics_excluded,
            logs_excluded: proto.logs_excluded,
        }
    }
}

impl From<ProtoConfig> for AutodiscoveryEvent {
    fn from(proto: ProtoConfig) -> AutodiscoveryEvent {
        let event_type = EventType::from(proto.event_type);

        if event_type == EventType::Schedule {
            AutodiscoveryEvent::Schedule {
                config: Config::from(proto),
            }
        } else {
            AutodiscoveryEvent::Unscheduled { config_id: proto.name }
        }
    }
}

/// An autodiscovery event
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum AutodiscoveryEvent {
    /// Schedule a configuration
    Schedule {
        /// Configuration
        config: Config,
    },
    /// Unschedule a configuration
    Unscheduled {
        /// Configuration ID
        config_id: String,
    },
}

/// Provides autodiscovery functionality.
///
/// This trait is used to discover and monitor configuration files for checks.
#[async_trait]
pub trait AutodiscoveryProvider {
    /// Subscribe for autodiscovery events.
    ///
    /// This method returns a channel where autodiscovery events will be sent.
    async fn subscribe(&self) -> Receiver<AutodiscoveryEvent>;
}

#[cfg(test)]
mod tests {
    use datadog_protos::agent::{AdvancedAdIdentifier, KubeNamespacedName};

    use super::*;

    #[test]
    fn test_event_type_from_config_event_type() {
        assert_eq!(EventType::from(ConfigEventType::Schedule), EventType::Schedule);
        assert_eq!(EventType::from(ConfigEventType::Unschedule), EventType::Unschedule);
    }

    #[test]
    fn test_event_type_from_i32() {
        // Known values
        assert_eq!(EventType::from(0), EventType::Schedule); // Schedule is 0
        assert_eq!(EventType::from(1), EventType::Unschedule); // Unschedule is 1

        // Unknown values should default to Schedule
        assert_eq!(EventType::from(2), EventType::Schedule);
        assert_eq!(EventType::from(-1), EventType::Schedule);
    }

    #[test]
    fn test_config_from_proto_config() {
        // Create a ProtoConfig with test values
        let mut proto_config = ProtoConfig {
            name: "test-config".to_string(),
            event_type: ConfigEventType::Schedule as i32,
            init_config: b"init-data".to_vec(),
            instances: vec![b"instance1".to_vec(), b"instance2".to_vec()],
            provider: "test-provider".to_string(),
            ad_identifiers: vec!["id1".to_string(), "id2".to_string()],
            cluster_check: true,
            metric_config: vec![],
            logs_config: vec![],
            advanced_ad_identifiers: vec![],
            service_id: "service-id".to_string(),
            tagger_entity: "tagger-entity".to_string(),
            node_name: "node-name".to_string(),
            source: "source".to_string(),
            ignore_autodiscovery_tags: false,
            metrics_excluded: false,
            logs_excluded: false,
        };

        let kube_svc = KubeNamespacedName {
            name: "nginx".to_string(),
            namespace: "default".to_string(),
        };

        let adv_id = AdvancedAdIdentifier {
            kube_service: Some(kube_svc),
            kube_endpoints: None,
        };

        proto_config.advanced_ad_identifiers = vec![adv_id];

        let config = Config::from(proto_config);

        assert_eq!(config.name, "test-config");
        assert_eq!(config.init_config, b"init-data");
        assert_eq!(config.instances, vec![b"instance1".to_vec(), b"instance2".to_vec()]);
        assert_eq!(config.provider, "test-provider");
        assert_eq!(config.ad_identifiers, vec!["id1".to_string(), "id2".to_string()]);
        assert!(config.cluster_check);

        assert_eq!(config.advanced_ad_identifiers.len(), 1);
        let adv_id = &config.advanced_ad_identifiers[0];
        assert!(adv_id.kube_endpoints.is_none());
        assert!(adv_id.kube_service.is_some());
        let svc = adv_id.kube_service.as_ref().unwrap();
        assert_eq!(svc.name, "nginx");
        assert_eq!(svc.namespace, "default");
    }

    #[test]
    fn test_autodiscovery_event_from_proto_config() {
        // Create a ProtoConfig with test values
        let mut proto_config = ProtoConfig {
            name: "test-config".to_string(),
            event_type: ConfigEventType::Schedule as i32,
            init_config: b"init-data".to_vec(),
            instances: vec![b"instance1".to_vec(), b"instance2".to_vec()],
            provider: "test-provider".to_string(),
            ad_identifiers: vec!["id1".to_string(), "id2".to_string()],
            cluster_check: true,
            metric_config: vec![],
            logs_config: vec![],
            advanced_ad_identifiers: vec![],
            service_id: "service-id".to_string(),
            tagger_entity: "tagger-entity".to_string(),
            node_name: "node-name".to_string(),
            source: "source".to_string(),
            ignore_autodiscovery_tags: false,
            metrics_excluded: false,
            logs_excluded: false,
        };

        let kube_svc = KubeNamespacedName {
            name: "nginx".to_string(),
            namespace: "default".to_string(),
        };

        let adv_id = AdvancedAdIdentifier {
            kube_service: Some(kube_svc),
            kube_endpoints: None,
        };

        proto_config.advanced_ad_identifiers = vec![adv_id];

        let event = AutodiscoveryEvent::from(proto_config.clone());

        match event {
            AutodiscoveryEvent::Schedule { config: _config } => {}
            _ => panic!("Expected an Schedule event"),
        }

        proto_config.event_type = ConfigEventType::Unschedule as i32;

        let event = AutodiscoveryEvent::from(proto_config);

        match event {
            AutodiscoveryEvent::Unscheduled { config_id: _config_id } => {}
            _ => panic!("Expected an Unscheduled event"),
        }
    }
}
