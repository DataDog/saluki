//! Autodiscovery provider.
//!
//! This module provides the `Autodiscovery` trait, which deals with providing information about autodiscovery.

use std::collections::HashMap;
use std::hash::Hasher;

use async_trait::async_trait;
use datadog_protos::agent::{Config as ProtoConfig, ConfigEventType};
use fnv::FnvHasher;
use saluki_error::GenericError;
use stringtheory::MetaString;
use tokio::sync::broadcast::Receiver;
use twox_hash::XxHash64;

pub mod providers;

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
    pub name: MetaString,
    /// Kubernetes namespace
    pub namespace: MetaString,
}

/// Advanced autodiscovery identifier
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvancedADIdentifier {
    /// Kubernetes service
    pub kube_service: Option<KubeNamespacedName>,
    /// Kubernetes endpoints
    pub kube_endpoints: Option<KubeNamespacedName>,
}

/// Raw data trait for configuration data
pub trait RawData {
    /// Get the value of the data
    fn get_value(&self) -> &HashMap<MetaString, serde_yaml::Value>;

    /// Get a value from the data
    fn get(&self, key: &str) -> Option<&serde_yaml::Value> {
        self.get_value().get(key)
    }

    /// Convert the data to bytes
    fn to_bytes(&self) -> Result<Vec<u8>, GenericError> {
        let mut buffer = Vec::new();
        serde_yaml::to_writer(&mut buffer, &self.get_value())?;
        Ok(buffer)
    }
}

/// Generic map of key-value pairs
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Data {
    value: HashMap<MetaString, serde_yaml::Value>,
}

impl RawData for Data {
    fn get_value(&self) -> &HashMap<MetaString, serde_yaml::Value> {
        &self.value
    }
}

/// Configuration for a check instance
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Instance {
    /// Instance ID
    id: String,
    /// Instance value
    value: HashMap<MetaString, serde_yaml::Value>,
}

impl Instance {
    /// Get the instance ID
    pub fn id(&self) -> &String {
        &self.id
    }
}

impl RawData for Instance {
    fn get_value(&self) -> &HashMap<MetaString, serde_yaml::Value> {
        &self.value
    }
}

/// Configuration for a check
#[derive(Debug, Clone)]
pub struct Config {
    /// Configuration name/identifier
    pub name: MetaString,
    /// Raw configuration data
    pub init_config: Data,
    /// Instance configurations
    pub instances: Vec<Data>,
    /// Metric configuration
    pub metric_config: Data,
    /// Logs configuration
    pub logs_config: Data,
    /// Auto-discovery identifiers
    pub ad_identifiers: Vec<MetaString>,
    /// Advanced auto-discovery identifiers
    pub advanced_ad_identifiers: Vec<AdvancedADIdentifier>,
    /// Provider that discovered this config
    pub provider: MetaString,
    /// Service ID
    pub service_id: MetaString,
    /// Tagger entity
    pub tagger_entity: MetaString,
    /// Whether this is a cluster check
    pub cluster_check: bool,
    /// Node name
    pub node_name: MetaString,
    /// Source of the configuration
    pub source: MetaString,
    /// Whether to ignore autodiscovery tags
    pub ignore_autodiscovery_tags: bool,
    /// Whether metrics are excluded
    pub metrics_excluded: bool,
    /// Whether logs are excluded
    pub logs_excluded: bool,
}

/// Check configuration
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckConfig {
    /// Check name
    pub name: MetaString,
    /// Init configuration data
    pub init_config: Data,
    /// Instance configurations
    pub instances: Vec<Instance>,
    /// Source of the configuration
    pub source: MetaString,
}

impl Config {
    /// Get the digest for the config
    pub fn digest(&self) -> u64 {
        let mut h = XxHash64::with_seed(0);

        h.write(self.name.as_bytes());
        for i in &self.instances {
            h.write(&i.to_bytes().unwrap_or_default());
        }
        h.write(&self.init_config.to_bytes().unwrap_or_default());
        for i in &self.ad_identifiers {
            h.write(i.as_bytes());
        }
        h.write(self.node_name.as_bytes());
        h.write(&self.logs_config.to_bytes().unwrap_or_default());
        h.write(self.service_id.as_bytes());
        h.write(if self.ignore_autodiscovery_tags {
            b"true"
        } else {
            b"false"
        });

        h.finish()
    }
}

impl From<ProtoConfig> for Config {
    fn from(proto: ProtoConfig) -> Self {
        // Convert advanced AD identifiers from proto
        let advanced_ad_identifiers = proto
            .advanced_ad_identifiers
            .into_iter()
            .map(|adv_id| {
                let kube_service = adv_id.kube_service.map(|svc| KubeNamespacedName {
                    name: svc.name.into(),
                    namespace: svc.namespace.into(),
                });

                let kube_endpoints = adv_id.kube_endpoints.map(|endpoints| KubeNamespacedName {
                    name: endpoints.name.into(),
                    namespace: endpoints.namespace.into(),
                });

                AdvancedADIdentifier {
                    kube_service,
                    kube_endpoints,
                }
            })
            .collect();

        let init_config = bytes_to_data(proto.init_config).unwrap_or_default();
        let instances = proto
            .instances
            .into_iter()
            .map(|instance| bytes_to_data(instance).unwrap_or_default())
            .collect();

        Self {
            name: proto.name.into(),
            init_config,
            instances,
            metric_config: bytes_to_data(proto.metric_config).unwrap_or_default(),
            logs_config: bytes_to_data(proto.logs_config).unwrap_or_default(),
            ad_identifiers: proto.ad_identifiers.into_iter().map(MetaString::from).collect(),
            advanced_ad_identifiers,
            provider: proto.provider.into(),
            service_id: proto.service_id.into(),
            tagger_entity: proto.tagger_entity.into(),
            cluster_check: proto.cluster_check,
            node_name: proto.node_name.into(),
            source: proto.source.into(),
            ignore_autodiscovery_tags: proto.ignore_autodiscovery_tags,
            metrics_excluded: proto.metrics_excluded,
            logs_excluded: proto.logs_excluded,
        }
    }
}

fn bytes_to_data(bytes: Vec<u8>) -> Result<Data, GenericError> {
    let parse_bytes = String::from_utf8(bytes)?;

    let map: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(&parse_bytes)?;

    let mut result = HashMap::<MetaString, serde_yaml::Value>::new();

    for (key, value) in map {
        result.insert(key.into(), value);
    }

    Ok(Data { value: result })
}

fn instance_id(name: &str, instance: &Data, digest: u64, init_config: &Data) -> String {
    let mut h2 = FnvHasher::default();
    h2.write_u64(digest);
    h2.write(&instance.to_bytes().unwrap_or_default());
    h2.write(&init_config.to_bytes().unwrap_or_default());

    let instance_name = instance_name(instance);
    let hash2 = h2.finish();

    if !instance_name.is_empty() {
        format!("{}:{}:{:X}", name, instance_name, hash2)
    } else {
        format!("{}:{:X}", name, hash2)
    }
}

fn instance_name(instance: &Data) -> String {
    if let Some(name) = instance.get("name") {
        if let Some(value) = name.as_str() {
            return value.to_string();
        }
    }
    if let Some(namespace) = instance.get("namespace") {
        if let Some(value) = namespace.as_str() {
            return value.to_string();
        }
    }
    "".to_string()
}

impl From<Config> for CheckConfig {
    fn from(config: Config) -> Self {
        let digest = config.digest();

        CheckConfig {
            name: config.name.clone(),
            init_config: config.init_config.clone(),
            instances: config
                .instances
                .into_iter()
                .map(|instance_data| Instance {
                    id: instance_id(&config.name, &instance_data, digest, &config.init_config),
                    value: instance_data.value,
                })
                .collect(),
            source: config.source,
        }
    }
}

impl From<ProtoConfig> for AutodiscoveryEvent {
    fn from(proto: ProtoConfig) -> AutodiscoveryEvent {
        let event_type = EventType::from(proto.event_type);

        let config = Config::from(proto);

        if !config.instances.is_empty() && !config.cluster_check {
            let check_config = CheckConfig::from(config);

            if event_type == EventType::Schedule {
                return AutodiscoveryEvent::CheckSchedule { config: check_config };
            } else {
                return AutodiscoveryEvent::CheckUnscheduled { config: check_config };
            }
        }

        if event_type == EventType::Schedule {
            AutodiscoveryEvent::Schedule { config }
        } else {
            AutodiscoveryEvent::Unscheduled { config }
        }
    }
}

/// An autodiscovery event
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum AutodiscoveryEvent {
    /// Schedule a check configuration
    CheckSchedule {
        /// Configuration
        config: CheckConfig,
    },
    /// Unschedule a check configuration
    CheckUnscheduled {
        /// Configuration
        config: CheckConfig,
    },
    /// Schedule a generic configuration
    Schedule {
        /// Configuration
        config: Config,
    },
    /// Unscheduled a generic configuration
    Unscheduled {
        /// Configuration
        config: Config,
    },
}

/// Provides autodiscovery functionality.
///
/// This trait is used to discover and monitor configuration files for checks.
#[async_trait]
pub trait AutodiscoveryProvider {
    /// Subscribe to autodiscovery events.
    async fn subscribe(&self) -> Option<Receiver<AutodiscoveryEvent>>;
}

#[async_trait]
impl<T> AutodiscoveryProvider for Option<T>
where
    T: AutodiscoveryProvider + Sync,
{
    async fn subscribe(&self) -> Option<Receiver<AutodiscoveryEvent>> {
        match self.as_ref() {
            Some(provider) => provider.subscribe().await,
            None => None,
        }
    }
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
    fn test_check_config_instance_id() {
        let proto_config = ProtoConfig {
            name: "test-check".to_string(),
            event_type: ConfigEventType::Schedule as i32,
            init_config: b"key: value".to_vec(),
            instances: vec![b"name: test".to_vec(), b"another_key: another_value".to_vec()],
            provider: "test-provider".to_string(),
            ad_identifiers: vec!["id1".to_string(), "id2".to_string()],
            cluster_check: false,
            metric_config: b"metric_key: metric_value".to_vec(),
            logs_config: b"log_key: log_value".to_vec(),
            advanced_ad_identifiers: vec![],
            service_id: "service-id".to_string(),
            tagger_entity: "tagger-entity".to_string(),
            node_name: "node-name".to_string(),
            source: "source".to_string(),
            ignore_autodiscovery_tags: false,
            metrics_excluded: false,
            logs_excluded: false,
        };

        let config = Config::from(proto_config);

        let check_config = CheckConfig::from(config);

        let id1 = &check_config.instances[0].id;
        let id2 = &check_config.instances[1].id;

        assert_ne!(id1, id2);

        assert_eq!(id1, "test-check:test:369F074E36651C8");
        assert_eq!(id2, "test-check:8C83712B7A572843");
    }

    #[test]
    fn test_config_from_proto_config() {
        // Create a ProtoConfig with test values
        let mut proto_config = ProtoConfig {
            name: "test-config".to_string(),
            event_type: ConfigEventType::Schedule as i32,
            init_config: b"key: value".to_vec(),
            instances: vec![
                b"instance_key: instance_value".to_vec(),
                b"another_key: another_value".to_vec(),
            ],
            provider: "test-provider".to_string(),
            ad_identifiers: vec!["id1".to_string(), "id2".to_string()],
            cluster_check: true,
            metric_config: b"metric_key: metric_value".to_vec(),
            logs_config: b"log_key: log_value".to_vec(),
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
        assert_eq!(config.provider, "test-provider");
        assert_eq!(
            config.ad_identifiers,
            vec![MetaString::from_static("id1"), MetaString::from_static("id2")]
        );
        assert!(config.cluster_check);
        assert_eq!(config.service_id, "service-id");
        assert_eq!(config.tagger_entity, "tagger-entity");
        assert_eq!(config.node_name, "node-name");
        assert_eq!(config.source, "source");
        assert!(!config.ignore_autodiscovery_tags);
        assert!(!config.metrics_excluded);
        assert!(!config.logs_excluded);
        assert_eq!(
            config.init_config.get("key"),
            Some(&serde_yaml::Value::String("value".to_string()))
        );
        assert_eq!(config.instances.len(), 2);
        assert_eq!(
            config.instances[0].get("instance_key"),
            Some(&serde_yaml::Value::String("instance_value".to_string()))
        );
        assert_eq!(
            config.instances[1].get("another_key"),
            Some(&serde_yaml::Value::String("another_value".to_string()))
        );
        assert_eq!(
            config.metric_config.get("metric_key"),
            Some(&serde_yaml::Value::String("metric_value".to_string()))
        );
        assert_eq!(
            config.logs_config.get("log_key"),
            Some(&serde_yaml::Value::String("log_value".to_string()))
        );

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
            instances: vec![],
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

        let event = AutodiscoveryEvent::from(proto_config.clone());

        match event {
            AutodiscoveryEvent::Unscheduled { config } => {
                assert_eq!(config.name, "test-config");
            }
            _ => panic!("Expected an Unscheduled event"),
        }

        proto_config.instances = vec![b"instance1".to_vec(), b"instance2".to_vec()];
        proto_config.cluster_check = false;
        proto_config.event_type = ConfigEventType::Schedule as i32;

        let event = AutodiscoveryEvent::from(proto_config.clone());

        match event {
            AutodiscoveryEvent::CheckSchedule { config: _config } => {}
            _ => panic!("Expected an CheckSchedule event"),
        }

        proto_config.event_type = ConfigEventType::Unschedule as i32;

        let event = AutodiscoveryEvent::from(proto_config);

        match event {
            AutodiscoveryEvent::CheckUnscheduled { config: _config } => {}
            _ => panic!("Expected an CheckUnscheduled event"),
        }
    }
}
