use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use saluki_error::GenericError;
use tokio::fs;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::time::{interval, Duration};
use tracing::{debug, warn};

use crate::autodiscovery::{AutoDiscoveryProvider, AutodiscoveryEvent, Config, EventType};

/// A local auto-discovery provider that uses the file system.
pub struct LocalAutoDiscoveryProvider {
    search_paths: Vec<PathBuf>,
    known_configs: HashSet<String>,
    event_senders: Arc<Mutex<Vec<mpsc::Sender<AutodiscoveryEvent>>>>,
    monitoring_active: bool,
}

impl LocalAutoDiscoveryProvider {
    /// Creates a new `LocalAutoDiscoveryProvider` that will monitor the specified paths.
    pub fn new<P: AsRef<std::path::Path>>(paths: Vec<P>) -> Self {
        let search_paths: Vec<PathBuf> = paths
            .iter()
            .filter_map(|p| {
                if !p.as_ref().exists() {
                    warn!("Skipping path '{}' as it does not exist", p.as_ref().display());
                    return None;
                }
                if !p.as_ref().is_dir() {
                    warn!("Skipping path '{}', it is not a directory.", p.as_ref().display());
                    return None;
                }
                Some(p.as_ref().to_path_buf())
            })
            .collect();

        Self {
            search_paths,
            known_configs: HashSet::new(),
            event_senders: Arc::new(Mutex::new(Vec::new())),
            monitoring_active: false,
        }
    }

    /// Starts a background task that periodically scans for configuration changes
    fn start_background_monitor(&mut self, interval_sec: u64) -> Result<(), GenericError> {
        if self.monitoring_active {
            return Ok(());
        }

        if self.event_senders.lock().unwrap().is_empty() {
            return Err(GenericError::msg("Cannot start monitoring without subscribers"));
        }

        let search_paths = self.search_paths.clone();
        let event_senders = self.event_senders.clone();

        let mut interval = interval(Duration::from_secs(interval_sec));

        tokio::spawn(async move {
            let mut known_configs = HashSet::new();

            loop {
                interval.tick().await;

                // Scan for configurations and emit events for changes
                if let Err(e) = scan_and_emit_events(&search_paths, &mut known_configs, &event_senders).await {
                    warn!("Error scanning for configurations: {}", e);
                }
            }
        });

        self.monitoring_active = true;
        Ok(())
    }
}

/// Parse a YAML file into a Config object
async fn parse_config_file(path: &PathBuf) -> Result<(String, Config), GenericError> {
    let content = fs::read_to_string(path).await?;

    // Build config ID from the file name
    let config_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| GenericError::msg("Invalid file name"))?
        .to_string();

    // Create a Config
    let config = Config {
        name: config_id.clone(),
        event_type: EventType::Schedule,
        init_config: content.as_bytes().to_vec(),
        instances: Vec::new(), // Would parse from YAML in a real implementation
        metric_config: Vec::new(),
        logs_config: Vec::new(),
        ad_identifiers: Vec::new(),
        provider: "local".to_string(),
        service_id: String::new(),
        tagger_entity: String::new(),
        cluster_check: false,
        node_name: String::new(),
        source: path.to_string_lossy().to_string(),
        ignore_autodiscovery_tags: false,
        metrics_excluded: false,
        logs_excluded: false,
    };

    Ok((config_id, config))
}

/// Scan and emit events based on configuration files in the directory
async fn scan_and_emit_events(
    paths: &[PathBuf], known_configs: &mut HashSet<String>,
    event_senders: &Arc<Mutex<Vec<mpsc::Sender<AutodiscoveryEvent>>>>,
) -> Result<(), GenericError> {
    let mut found_configs = HashSet::new();

    for path in paths {
        let mut entries = fs::read_dir(path).await?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();

            // Only process YAML files
            if let Some(ext) = path.extension() {
                if ext == "yaml" || ext == "yml" {
                    // Process the file if it's a valid configuration
                    match parse_config_file(&path).await {
                        Ok((config_id, config)) => {
                            found_configs.insert(config_id.clone());

                            // Check if this is a new or updated configuration
                            if !known_configs.contains(&config_id) {
                                debug!("New configuration found: {}", config_id);

                                let event = AutodiscoveryEvent { config };
                                broadcast_event(event_senders, event).await;
                                known_configs.insert(config_id.clone());
                            } else {
                                // Config ID exists, but the content might have changed
                                debug!("Configuration updated: {}", config_id);

                                let event = AutodiscoveryEvent { config };
                                broadcast_event(event_senders, event).await;
                            }
                        }
                        Err(e) => {
                            warn!("Failed to parse config file {}: {}", path.display(), e);
                        }
                    }
                }
            }
        }
    }

    // Clean up removed configurations
    let to_remove: Vec<String> = known_configs
        .iter()
        .filter(|config_id| !found_configs.contains(*config_id))
        .cloned()
        .collect();

    for config_id in to_remove {
        debug!("Configuration removed: {}", config_id);
        known_configs.remove(&config_id);

        // Create an unschedule config
        let config = Config {
            name: config_id,
            event_type: EventType::Unschedule,
            init_config: Vec::new(),
            instances: Vec::new(),
            metric_config: Vec::new(),
            logs_config: Vec::new(),
            ad_identifiers: Vec::new(),
            provider: "local".to_string(),
            service_id: String::new(),
            tagger_entity: String::new(),
            cluster_check: false,
            node_name: String::new(),
            source: String::new(),
            ignore_autodiscovery_tags: false,
            metrics_excluded: false,
            logs_excluded: false,
        };

        let event = AutodiscoveryEvent { config };
        broadcast_event(event_senders, event).await;
    }

    Ok(())
}

/// Broadcasts an event to all subscribers, removing any that have closed their channels
async fn broadcast_event(senders: &Arc<Mutex<Vec<mpsc::Sender<AutodiscoveryEvent>>>>, event: AutodiscoveryEvent) {
    let mut to_remove = Vec::new();

    {
        let senders_guard = senders.lock().unwrap();
        for (idx, sender) in senders_guard.iter().enumerate() {
            match sender.try_send(event.clone()) {
                Ok(_) => {}
                Err(TrySendError::Closed(_)) => {
                    // Channel is closed, mark for removal
                    to_remove.push(idx);
                }
                Err(TrySendError::Full(_)) => {
                    // Channel is full, this is a backpressure situation
                    warn!("Channel full - subscriber cannot keep up with autodiscovery events");
                }
            }
        }
    }

    // Remove closed senders if any
    if !to_remove.is_empty() {
        let removed_count = to_remove.len();
        let mut senders_guard = senders.lock().unwrap();
        // Remove from back to front to avoid index shifting issues
        to_remove.sort_unstable_by(|a, b| b.cmp(a));
        debug!("Removing {} closed autodiscovery subscribers", removed_count);

        for idx in to_remove {
            senders_guard.swap_remove(idx);
        }
    }
}

#[async_trait]
impl AutoDiscoveryProvider for LocalAutoDiscoveryProvider {
    type Error = GenericError;

    async fn subscribe(&mut self, sender: mpsc::Sender<AutodiscoveryEvent>) -> Result<(), Self::Error> {
        {
            let mut senders = self.event_senders.lock().unwrap();

            // Check if this sender's address is already in our list
            let sender_addr = format!("{:p}", &sender);
            if senders.iter().any(|s| format!("{:p}", s) == sender_addr) {
                debug!("Attempt to subscribe with duplicate sender - ignoring");
                return Ok(());
            }

            senders.push(sender);
        }

        // Scan for initial configurations
        let mut local_known = self.known_configs.clone();
        scan_and_emit_events(&self.search_paths, &mut local_known, &self.event_senders).await?;
        self.known_configs = local_known;

        // Start background monitoring if not already active
        if !self.monitoring_active {
            self.start_background_monitor(30)?; // Check every 30 seconds
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;

    use super::*;

    // Get the path to the test_data directory
    fn test_data_path() -> PathBuf {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(manifest_dir)
            .join("src")
            .join("autodiscovery")
            .join("providers")
            .join("test_data")
    }

    // Copy a file from test_data to the temp directory
    async fn copy_test_file(source_name: &str, temp_dir: &Path) -> PathBuf {
        let source_path = test_data_path().join(source_name);
        let target_path = temp_dir.join(source_name);

        let content = fs::read_to_string(&source_path)
            .await
            .unwrap_or_else(|_| panic!("Failed to read test file: {:?}", source_path));

        let mut file = fs::File::create(&target_path).await.unwrap();
        file.write_all(content.as_bytes()).await.unwrap();

        target_path
    }

    #[tokio::test]
    async fn test_parse_config_file() {
        let test_file = test_data_path().join("test-config.yaml");

        let (id, config) = parse_config_file(&test_file).await.unwrap();

        assert_eq!(id, "test-config");
        assert_eq!(config.name, "test-config");
        assert_eq!(config.event_type, EventType::Schedule);
        assert!(!config.init_config.is_empty());
        assert_eq!(config.provider, "local");
        assert_eq!(config.source, test_file.to_string_lossy().to_string());
    }

    #[tokio::test]
    async fn test_scan_and_emit_events_new_config() {
        let dir = tempdir().unwrap();
        let _test_file = copy_test_file("config1.yaml", dir.path()).await;

        let mut known_configs = HashSet::new();
        let event_senders = Arc::new(Mutex::new(Vec::new()));

        let (tx, mut rx) = mpsc::channel(10);
        event_senders.lock().unwrap().push(tx);

        scan_and_emit_events(&[dir.path().to_path_buf()], &mut known_configs, &event_senders)
            .await
            .unwrap();

        assert_eq!(known_configs.len(), 1);
        assert!(known_configs.contains("config1"));

        let event = rx.try_recv().unwrap();
        assert_eq!(event.config.name, "config1");
        assert_eq!(event.config.event_type, EventType::Schedule);

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_scan_and_emit_events_removed_config() {
        let dir = tempdir().unwrap();

        let mut known_configs = HashSet::new();
        known_configs.insert("removed-config".to_string());

        let event_senders = Arc::new(Mutex::new(Vec::new()));

        let (tx, mut rx) = mpsc::channel(10);
        event_senders.lock().unwrap().push(tx);

        scan_and_emit_events(&[dir.path().to_path_buf()], &mut known_configs, &event_senders)
            .await
            .unwrap();

        assert_eq!(known_configs.len(), 0);

        let event = rx.try_recv().unwrap();
        assert_eq!(event.config.name, "removed-config");
        assert_eq!(event.config.event_type, EventType::Unschedule);

        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_broadcast_event_removes_closed_subscribers() {
        let event_senders = Arc::new(Mutex::new(Vec::new()));

        let (tx1, _rx1) = mpsc::channel::<AutodiscoveryEvent>(10);

        let (tx2, rx2) = mpsc::channel::<AutodiscoveryEvent>(10);
        drop(rx2);

        event_senders.lock().unwrap().push(tx1);
        event_senders.lock().unwrap().push(tx2);

        let config = Config {
            name: "test".to_string(),
            event_type: EventType::Schedule,
            init_config: Vec::new(),
            instances: Vec::new(),
            metric_config: Vec::new(),
            logs_config: Vec::new(),
            ad_identifiers: Vec::new(),
            provider: "test".to_string(),
            service_id: String::new(),
            tagger_entity: String::new(),
            cluster_check: false,
            node_name: String::new(),
            source: String::new(),
            ignore_autodiscovery_tags: false,
            metrics_excluded: false,
            logs_excluded: false,
        };
        let event = AutodiscoveryEvent { config };

        broadcast_event(&event_senders, event).await;

        assert_eq!(event_senders.lock().unwrap().len(), 1);
    }
}
