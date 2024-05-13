use async_trait::async_trait;
use async_walkdir::{DirEntry, Filtering, WalkDir};
use futures::StreamExt as _;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{Source, SourceBuilder, SourceContext},
    prelude::ErasedError,
    topology::{
        shutdown::{DynamicShutdownCoordinator, DynamicShutdownHandle},
        OutputDefinition,
    },
};
use saluki_event::DataType;
use serde::Deserialize;
use snafu::Snafu;
use std::path::{Path, PathBuf};
use std::{collections::HashSet, io};
use tokio::{select, sync::mpsc};
use tracing::{debug, info}; // Import the Display trait

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
enum Error {
    #[snafu(display("Directory incorrect"))]
    DirectoryIncorrect { source: io::Error },
}

/// Checks source.
///
/// Scans a directory for check configurations and emits them as things to run.
#[derive(Deserialize)]
pub struct ChecksConfiguration {
    /// The size of the buffer used to receive messages into, in bytes.
    ///
    /// Payloads cannot exceed this size, or they will be truncated, leading to discarded messages.
    ///
    /// Defaults to 8192 bytes.
    #[serde(default = "default_check_config_dir")]
    check_config_dir: String,
}

fn default_check_config_dir() -> String {
    "./conf.d".to_string()
}

struct DirCheckListener {
    base_path: PathBuf,
    known_check_paths: Vec<PathBuf>,
    // These could all be oneshot channels I think
    // but maybe buffering is useful
    new_path_tx: mpsc::Sender<PathBuf>,
    deleted_path_tx: mpsc::Sender<PathBuf>,
    new_path_rx: Option<mpsc::Receiver<PathBuf>>,
    deleted_path_rx: Option<mpsc::Receiver<PathBuf>>,
}

struct DirCheckListenerContext {
    shutdown_handle: DynamicShutdownHandle,
    listener: DirCheckListener,
}

impl DirCheckListener {
    /// Constructs a new `Listener` that will monitor the specified path.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<DirCheckListener, Error> {
        let path_ref = path.as_ref();
        if !path_ref.exists() {
            return Err(Error::DirectoryIncorrect {
                source: io::Error::new(io::ErrorKind::NotFound, "Path does not exist"),
            });
        }
        if !path_ref.is_dir() {
            return Err(Error::DirectoryIncorrect {
                source: io::Error::new(io::ErrorKind::NotFound, "Path is not a directory"),
            });
        }

        let (new_paths_tx, new_paths_rx) = mpsc::channel(100);
        let (deleted_paths_tx, deleted_paths_rx) = mpsc::channel(100);
        Ok(DirCheckListener {
            base_path: path.as_ref().to_path_buf(),
            known_check_paths: Vec::new(),
            new_path_tx: new_paths_tx,
            deleted_path_tx: deleted_paths_tx,
            new_path_rx: Some(new_paths_rx),
            deleted_path_rx: Some(deleted_paths_rx),
        })
    }

    pub fn subscribe(&mut self) -> (mpsc::Receiver<PathBuf>, mpsc::Receiver<PathBuf>) {
        (self.new_path_rx.take().unwrap(), self.deleted_path_rx.take().unwrap())
    }

    async fn update_check_entities(&mut self) {
        let new_check_paths = self.get_check_entities().await;
        let current: HashSet<PathBuf> = HashSet::from_iter(self.known_check_paths.clone().into_iter());
        let new: HashSet<PathBuf> = HashSet::from_iter(new_check_paths.iter().cloned());

        for entity in new.difference(&current) {
            self.known_check_paths.push(entity.clone());
            // todo error handling
            self.new_path_tx.send(entity.clone()).await;
        }
        for entity in current.difference(&new) {
            self.known_check_paths.retain(|e| e != entity);
            // todo error handling
            self.deleted_path_tx.send(entity.clone()).await;
        }
    }

    /// Retrieves all check entities from the base path that match the required check formats.
    pub async fn get_check_entities(&self) -> Vec<PathBuf> {
        let entries = WalkDir::new(&self.base_path).filter(|entry| async move {
            if let Some(true) = entry.path().file_name().map(|f| f.to_string_lossy().starts_with('.')) {
                return Filtering::IgnoreDir;
            }
            if is_check_entity(&entry).await {
                Filtering::Continue
            } else {
                Filtering::Ignore
            }
        });

        entries
            .filter_map(|e| async move {
                match e {
                    Ok(entry) => Some(entry.path().to_path_buf()),
                    Err(e) => {
                        eprintln!("Error traversing files: {}", e);
                        None
                    }
                }
            })
            .collect()
            .await
    }
}

/// Determines if a directory entry is a valid check entity based on defined patterns.
async fn is_check_entity(entry: &DirEntry) -> bool {
    let path = entry.path();
    let file_type = entry.file_type().await.expect("Couldn't get file type");
    if file_type.is_file() {
        // Matches `./mycheck.yaml`
        return path.extension().unwrap_or_default() == "yaml";
    } else if file_type.is_dir() {
        // Matches `./mycheck.d/conf.yaml`
        let conf_path = path.join("conf.yaml");
        return conf_path.exists()
            && path
                .file_name()
                .unwrap_or_default()
                .to_str()
                .unwrap_or("")
                .ends_with("check.d");
    }
    false
}

impl ChecksConfiguration {
    /// Creates a new `DogStatsDConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, ErasedError> {
        Ok(config.as_typed()?)
    }

    async fn build_listeners(&self) -> Result<Vec<DirCheckListener>, Error> {
        let mut listeners = Vec::new();

        let listener = DirCheckListener::from_path(&self.check_config_dir)?;

        listeners.push(listener);

        Ok(listeners)
    }
}

#[async_trait]
impl SourceBuilder for ChecksConfiguration {
    async fn build(&self) -> Result<Box<dyn Source + Send>, Box<dyn std::error::Error + Send + Sync>> {
        let listeners = self.build_listeners().await?;

        Ok(Box::new(Checks { listeners }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];

        OUTPUTS
    }
}

pub struct Checks {
    listeners: Vec<DirCheckListener>,
}

#[async_trait]
impl Source for Checks {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let global_shutdown = context
            .take_shutdown_handle()
            .expect("should never fail to take shutdown handle");

        let mut listener_shutdown_coordinator = DynamicShutdownCoordinator::default();

        // For each listener, spawn a dedicated task to run it.
        for listener in self.listeners {
            let listener_context = DirCheckListenerContext {
                shutdown_handle: listener_shutdown_coordinator.register(),
                listener,
            };

            tokio::spawn(process_listener(context.clone(), listener_context));
        }

        info!("Check source started.");

        // Wait for the global shutdown signal, then notify listeners to shutdown.
        global_shutdown.await;
        info!("Stopping Check source...");

        listener_shutdown_coordinator.shutdown().await;

        info!("Check source stopped.");

        Ok(())
    }
}

async fn process_listener(source_context: SourceContext, listener_context: DirCheckListenerContext) {
    let DirCheckListenerContext {
        shutdown_handle,
        mut listener,
    } = listener_context;
    tokio::pin!(shutdown_handle);

    let mut stream_shutdown_coordinator = DynamicShutdownCoordinator::default();

    info!("Check listener started.");
    // every 1 sec check for new check entities
    let (mut new_entities, mut deleted_entities) = listener.subscribe();
    loop {
        select! {
            _ = &mut shutdown_handle => {
                debug!("Received shutdown signal. Waiting for existing stream handlers to finish...");
                break;
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                debug!("Checking for new check entities...");
                listener.update_check_entities().await;
            }
            Some(new_entity) = new_entities.recv() => {
                debug!("Received new check entity {}", new_entity.display());
                // TODO - try to start running this check
            }
            Some(deleted_entity) = deleted_entities.recv() => {
                debug!("Received deleted check entity {}", deleted_entity.display());
            }
        }
    }

    stream_shutdown_coordinator.shutdown().await;

    info!("Check listener stopped.");
}
