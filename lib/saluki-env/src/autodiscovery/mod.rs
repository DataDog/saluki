//! AutoDiscovery provider.
//!
//! This module provides the `AutoDiscovery` trait, which deals with providing information about autodiscovery.

pub mod providers;

use async_trait::async_trait;
use datadog_protos::agent::{Config as ProtoConfig, ConfigEventType};
use tokio::sync::mpsc;

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

/// Configuration data
#[derive(Debug, Clone)]
pub struct Config {
    /// Configuration name/identifier
    pub name: String,
    /// Event type (schedule/unschedule)
    pub event_type: EventType,
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
        let event_type = EventType::from(proto.event_type);

        Self {
            name: proto.name,
            event_type,
            init_config: proto.init_config,
            instances: proto.instances,
            metric_config: proto.metric_config,
            logs_config: proto.logs_config,
            ad_identifiers: proto.ad_identifiers,
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

/// An autodiscovery event
#[derive(Debug, Clone)]
pub struct AutodiscoveryEvent {
    /// Configuration payload
    pub config: Config,
}

/// Provides autodiscovery functionality.
///
/// This trait is used to discover and monitor configuration files for checks.
#[async_trait]
pub trait AutoDiscoveryProvider {
    /// Errors produced by the provider.
    type Error;

    /// Starts listening for autodiscovery events.
    ///
    /// This method allows the caller to specify a channel where autodiscovery events should be sent.
    /// The provider will deliver all autodiscovery events to this channel.
    /// For most implementations, this will also trigger an initial discovery of configurations.
    ///
    /// # Arguments
    ///
    /// * `sender` - The channel where autodiscovery events should be sent
    ///
    /// # Returns
    ///
    /// Returns a Result with either success or an error. This doesn't mean events will start
    /// flowing immediately, just that monitoring has been set up successfully.
    async fn subscribe(&mut self, sender: mpsc::Sender<AutodiscoveryEvent>) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
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
        let mut proto_config = ProtoConfig::default();
        proto_config.name = "test-config".to_string();
        proto_config.event_type = ConfigEventType::Schedule as i32;
        proto_config.init_config = b"init-data".to_vec();
        proto_config.instances = vec![b"instance1".to_vec(), b"instance2".to_vec()];
        proto_config.provider = "test-provider".to_string();
        proto_config.ad_identifiers = vec!["id1".to_string(), "id2".to_string()];
        proto_config.cluster_check = true;

        // Convert to our Config
        let config = Config::from(proto_config);

        // Verify fields were copied correctly
        assert_eq!(config.name, "test-config");
        assert_eq!(config.event_type, EventType::Schedule);
        assert_eq!(config.init_config, b"init-data");
        assert_eq!(config.instances, vec![b"instance1".to_vec(), b"instance2".to_vec()]);
        assert_eq!(config.provider, "test-provider");
        assert_eq!(config.ad_identifiers, vec!["id1".to_string(), "id2".to_string()]);
        assert_eq!(config.cluster_check, true);
    }
}
