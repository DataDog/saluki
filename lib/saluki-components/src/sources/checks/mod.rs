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
use tracing::{debug, info, error};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
enum Error {
    #[snafu(display("Directory incorrect"))]
    DirectoryIncorrect { source: io::Error },
    CantReadConfiguration { source: serde_yaml::Error },
    NoSourceAvailable { reason: String },
}

/// Checks source.
///
/// Scans a directory for check configurations and emits them as things to run.
#[derive(Deserialize)]
pub struct ChecksConfiguration {
    /// The directory containing the check configurations.
    #[serde(default = "default_check_config_dir")]
    check_config_dir: String,
}

fn default_check_config_dir() -> String {
    "./conf.d".to_string()
}

fn default_check_implementation_dir() -> String {
    "./checks.d".to_string()
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

// checks configuration

#[derive(Debug, Deserialize)]
struct CheckInstance {
    configuration: CheckInstanceConfiguration,
    source_code_relative_filepath: PathBuf,
}
#[derive(Debug, Deserialize)]
struct CheckInstanceConfiguration {
    min_collection_interval_ms: u32,
}

// YAML configuration

#[derive(Debug, Deserialize)]
struct YamlCheckConfiguration {
    instance: Vec<YamlCheckInstance>
}

#[derive(Debug, Deserialize)]
struct YamlCheckInstance {
    min_collection_interval: u32,
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
    }

    if file_type.is_dir() {
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
    /// Creates a new `ChecksConfiguration` from the given configuration.
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

    let stream_shutdown_coordinator = DynamicShutdownCoordinator::default();

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
                listener.update_check_entities().await;
            }
            Some(new_entity) = new_entities.recv() => {
                // TODO - try to start running this check
                // So what component will be responsible for "running" the check?
                // There needs to be a "check registry" component that finds the checks
                // in 'checks.d' as well.
                // This is almost identical to the 'DirCheckListener' in that it just looks
                // at a directory and finds anything ending in '.py'
                // This component doesn't exist yet
                debug!("Received new check entity, {}", new_entity.display());

                // read the YAML configuration

                let config = match read_check_configuration(new_entity.clone()).await {
                    Ok(config) => config,
                    Err(e) => {
                        error!("Can't read check configuration: {}", e);
                        continue;
                    }
                };

                // check that a python check is available and prepare a CheckInstance

                let _checks = match prepare_checks(new_entity, config).await {
                    Ok(checks) => checks,
                    Err(e) => {
                        error!("Can't read check source code: {}", e);
                        continue;
                    }
                };

                // send all checks instances for scheduling to the scheduler

                // TODO(remy): send_to_scheduler(checks).await;
            }
            Some(deleted_entity) = deleted_entities.recv() => {
                debug!("Received deleted check entity {}", deleted_entity.display());
                // TODO - try to stop running this check
                // (low priority)
            }
        }
    }

    stream_shutdown_coordinator.shutdown().await;

    info!("Check listener stopped.");
}

/// read_check_configuration receives a configuration file to open, reads it
/// and returns a CheckInstanceConfiguration if it's valid.
async fn read_check_configuration(relative_filepath: PathBuf) -> Result<Vec<CheckInstanceConfiguration>, Error> {
    let file = std::fs::File::open(relative_filepath).expect("Could not open file.");
    let mut checks_config = Vec::new();
    let read_yaml: YamlCheckConfiguration = match serde_yaml::from_reader(file) {
        Ok(read) => read,
        Err(e) => {
            debug!("can't read configuration: {}", e);
            return Err(Error::CantReadConfiguration{source: e});
        },
    };

    for instance in read_yaml.instance.into_iter() {
        checks_config.push(CheckInstanceConfiguration{
            min_collection_interval_ms: instance.min_collection_interval,
        });
    }

    return Ok(checks_config);
}

/// prepare_checks prepares the CheckInstances to be ready to be scheduled
/// in the scheduler.
async fn prepare_checks(relative_filepath: PathBuf, config: Vec<CheckInstanceConfiguration>) -> Result<Vec<CheckInstance>, Error> {
    debug!("reading the check implementation: {}", relative_filepath.display());
    let mut checks = Vec::new();

    let mut check_rel_filepath = relative_filepath.clone();
    check_rel_filepath.set_extension("py");

//    let filename = check_rel_filepath.file_name().unwrap(); // TODO(remy): what about None here?

//    if !check_rel_filepath.pop() {
//        return Err(Error::NoSourceAvailable{ reason: format!("Can't go to parent directory") });
//    }

//    check_rel_filepath.push(filename);

//    if !check_rel_filepath.exists() {
        // check in a checks.d subdir
//        check_rel_filepath.push("checks.d");
//        return Err(Error::NoSourceAvailable{ reason: format!("c") }); // TODO(remy): ship the rel filepath in the error
//    }

    // TODO(remy): look for `check_name.d` directory instead.

    for instance_config in config.into_iter() {
        debug!("created a check from {}, min_collection_interval: {}", check_rel_filepath.display(), instance_config.min_collection_interval_ms);
        checks.push(CheckInstance{
            configuration: instance_config,
            source_code_relative_filepath: relative_filepath.clone(),
        });
    }

    return Ok(checks);
}

