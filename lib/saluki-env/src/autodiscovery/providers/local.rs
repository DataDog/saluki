use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use saluki_error::GenericError;
use serde::Deserialize;
use stringtheory::MetaString;
use tokio::fs;
use tokio::sync::broadcast::{self, Receiver, Sender};
use tokio::sync::OnceCell;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

use crate::autodiscovery::{AutodiscoveryEvent, AutodiscoveryProvider, Config};

const BG_MONITOR_INTERVAL: u64 = 30;

/// A local auto-discovery provider that uses the file system.
pub struct LocalAutodiscoveryProvider {
    search_paths: Vec<PathBuf>,
    sender: Sender<AutodiscoveryEvent>,
    listener_init: OnceCell<()>,
}

impl LocalAutodiscoveryProvider {
    /// Creates a new `LocalAutodiscoveryProvider` that will monitor the specified paths.
    pub fn new<P: AsRef<Path>>(paths: Vec<P>) -> Self {
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

        let (sender, _) = broadcast::channel::<AutodiscoveryEvent>(super::AD_STREAM_CAPACITY);

        Self {
            search_paths,
            sender,
            listener_init: OnceCell::new(),
        }
    }

    /// Starts a background task that periodically scans for configuration changes
    async fn start_background_monitor(&self, interval_sec: u64) {
        let mut interval = interval(Duration::from_secs(interval_sec));
        let sender = self.sender.clone();
        let search_paths = self.search_paths.clone();

        info!(
            "Scanning for local autodiscovery events every {} seconds.",
            interval_sec
        );

        tokio::spawn(async move {
            let mut known_configs = HashSet::new();

            loop {
                interval.tick().await;

                // Scan for configurations and emit events for changes
                if let Err(e) = scan_and_emit_events(&search_paths, &mut known_configs, &sender).await {
                    warn!("Error scanning for configurations: {}", e);
                }
            }
        });
    }
}

#[derive(Debug, Deserialize)]
struct CheckConfig {
    init_config: HashMap<String, serde_yaml::Value>,
    instances: Vec<HashMap<String, serde_yaml::Value>>,
}

/// Parse a YAML file into a Config object
async fn parse_config_file(path: &PathBuf) -> Result<(String, Config), GenericError> {
    let content = fs::read_to_string(path).await?;

    let check_config: CheckConfig = match serde_yaml::from_str(&content) {
        Ok(read) => read,
        Err(e) => {
            return Err(GenericError::from(e).context("Failed to decode yaml as check configuration."));
        }
    };

    let canonicalized_path = fs::canonicalize(&path).await?;

    // Build config ID from the file path
    let config_id = canonicalized_path.to_string_lossy().replace(['/', '\\'], "_");

    let instances: Vec<HashMap<MetaString, serde_yaml::Value>> = check_config
        .instances
        .into_iter()
        .map(|instance| {
            let mut result = HashMap::new();
            for (key, value) in instance {
                result.insert(key.into(), value);
            }
            result
        })
        .collect();

    let init_config = {
        let mut result = HashMap::new();
        for (key, value) in check_config.init_config {
            result.insert(key.into(), value);
        }
        result
    };

    // Create a Config
    let config = Config {
        name: MetaString::from(path.file_name().unwrap().to_string_lossy().to_string()),
        init_config,
        instances,
        metric_config: HashMap::new(),
        logs_config: HashMap::new(),
        ad_identifiers: Vec::new(),
        provider: MetaString::empty(),
        service_id: MetaString::empty(),
        tagger_entity: MetaString::empty(),
        cluster_check: false,
        node_name: MetaString::empty(),
        source: MetaString::from_static("local"),
        ignore_autodiscovery_tags: false,
        metrics_excluded: false,
        logs_excluded: false,
        advanced_ad_identifiers: Vec::new(),
    };

    Ok((config_id, config))
}

/// Scan and emit events based on configuration files in the directory
async fn scan_and_emit_events(
    paths: &[PathBuf], known_configs: &mut HashSet<String>, sender: &Sender<AutodiscoveryEvent>,
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

                                let event = AutodiscoveryEvent::Schedule { config };
                                let _ = sender.send(event);
                                known_configs.insert(config_id.clone());
                            } else {
                                // Config ID exists, but the content might have changed
                                debug!("Configuration updated: {}", config_id);

                                let event = AutodiscoveryEvent::Schedule { config };
                                let _ = sender.send(event);
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

        // Create an unschedule Config event
        let event = AutodiscoveryEvent::Unscheduled { config_id };
        let _ = sender.send(event);
    }

    Ok(())
}

#[async_trait]
impl AutodiscoveryProvider for LocalAutodiscoveryProvider {
    async fn subscribe(&self) -> Receiver<AutodiscoveryEvent> {
        self.listener_init
            .get_or_init(|| async {
                self.start_background_monitor(BG_MONITOR_INTERVAL).await;
            })
            .await;

        self.sender.subscribe()
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

        assert!(id.contains("saluki-env_src_autodiscovery_providers_test_data_test-config.yaml"));
        assert_eq!(config.name, "test-config.yaml");
        assert_eq!(config.init_config["service"], "test-service");
        assert_eq!(config.source, "local");
    }

    #[tokio::test]
    async fn test_scan_and_emit_events_new_config() {
        let dir = tempdir().unwrap();
        let _test_file = copy_test_file("config1.yaml", dir.path()).await;

        let mut known_configs = HashSet::new();
        let (sender, mut receiver) = broadcast::channel::<AutodiscoveryEvent>(10);

        scan_and_emit_events(&[dir.path().to_path_buf()], &mut known_configs, &sender)
            .await
            .unwrap();

        assert_eq!(known_configs.len(), 1);

        let event = receiver.try_recv().unwrap();
        assert!(matches!(event, AutodiscoveryEvent::Schedule { .. }));

        if let AutodiscoveryEvent::Schedule { config } = event {
            assert_eq!(config.name, "config1.yaml");
            assert_eq!(config.instances.len(), 1);
            assert_eq!(config.instances[0].len(), 3);
            assert_eq!(config.instances[0]["server"], "localhost");
            assert_eq!(config.instances[0]["port"], serde_yaml::Value::Number(8080.into()));
            assert_eq!(
                config.instances[0]["tags"],
                serde_yaml::Value::Sequence(vec![
                    serde_yaml::Value::String("test:true".to_string()),
                    serde_yaml::Value::String("env:test".to_string())
                ])
            );
        }
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_scan_and_emit_events_removed_config() {
        let dir = tempdir().unwrap();

        let mut known_configs = HashSet::new();
        known_configs.insert("removed-config".to_string());

        let (sender, mut receiver) = broadcast::channel::<AutodiscoveryEvent>(10);

        scan_and_emit_events(&[dir.path().to_path_buf()], &mut known_configs, &sender)
            .await
            .unwrap();

        assert_eq!(known_configs.len(), 0);

        let event = receiver.try_recv().unwrap();
        assert!(matches!(event, AutodiscoveryEvent::Unscheduled { config_id } if config_id == "removed-config"));

        assert!(receiver.try_recv().is_err());
    }
}
