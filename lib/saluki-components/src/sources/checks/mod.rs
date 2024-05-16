use async_trait::async_trait;
use async_walkdir::{DirEntry, Filtering, WalkDir};
use futures::StreamExt as _;
use saluki_config::GenericConfiguration;
use saluki_core::{
    components::{Source, SourceBuilder, SourceContext},
    topology::{
        shutdown::{
            ComponentShutdownCoordinator, ComponentShutdownHandle, DynamicShutdownCoordinator, DynamicShutdownHandle,
        },
        OutputDefinition,
    },
};
use saluki_error::GenericError;
use saluki_event::DataType;
use serde::Deserialize;
use snafu::Snafu;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    thread,
};
use std::{collections::HashSet, io};
use std::{fmt::Display, time::Duration};
use tokio::{select, sync::mpsc};
use tracing::{debug, error, info};

mod listener;
mod scheduler;

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
enum Error {
    #[snafu(display("Directory incorrect"))]
    DirectoryIncorrect {
        source: io::Error,
    },
    CantReadConfiguration {
        source: serde_yaml::Error,
    },
    NoSourceAvailable {
        reason: String,
    },
    Python {
        reason: String,
    },
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

#[derive(Debug, Clone)]
struct CheckRequest {
    name: Option<String>,
    instances: Vec<CheckInstanceConfiguration>,
    init_config: Option<serde_yaml::Value>,
    source: CheckSource,
}

impl Display for CheckRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Source: {} # Instances: {}", self.source, self.instances.len(),)?;
        if let Some(name) = &self.name {
            write!(f, " Name: {}", name)?
        }

        Ok(())
    }
}

impl CheckRequest {
    fn to_runnable_request(&self) -> Result<RunnableCheckRequest, Error> {
        let check_source_code = match &self.source {
            CheckSource::Yaml(path) => find_sibling_py_file(path)?,
        };
        Ok(RunnableCheckRequest {
            check_request: self.clone(),
            check_source_code,
        })
    }
}

struct RunnableCheckRequest {
    check_request: CheckRequest,
    check_source_code: PathBuf,
}

impl Display for RunnableCheckRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Request: {} CheckSource: {}",
            self.check_request,
            self.check_source_code.display()
        )
    }
}

#[derive(Debug, Deserialize)]
struct YamlCheckConfiguration {
    name: Option<String>,
    init_config: Option<serde_yaml::Value>,
    instances: Vec<YamlCheckInstance>,
}

#[derive(Debug, Deserialize)]
struct YamlCheckInstance {
    #[serde(default = "default_min_collection_interval_ms")]
    min_collection_interval: u32,
    // todo support arbitrary other fields
}

// checks configuration

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
struct CheckInstanceConfiguration {
    min_collection_interval_ms: u32,
}

fn default_min_collection_interval_ms() -> u32 {
    15_000
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
enum CheckSource {
    Yaml(PathBuf),
}

impl CheckSource {
    // todo refactor to async fs
    fn to_check_request(&self) -> Result<CheckRequest, Error> {
        match self {
            CheckSource::Yaml(path) => {
                let file = std::fs::File::open(path).expect("No error");
                let mut checks_config = Vec::new();
                let read_yaml: YamlCheckConfiguration = match serde_yaml::from_reader(file) {
                    Ok(read) => read,
                    Err(e) => {
                        debug!("can't read configuration at {}: {}", path.display(), e);
                        return Err(Error::CantReadConfiguration { source: e });
                    }
                };

                for instance in read_yaml.instances.into_iter() {
                    checks_config.push(CheckInstanceConfiguration {
                        min_collection_interval_ms: instance.min_collection_interval,
                    });
                }

                Ok(CheckRequest {
                    name: read_yaml.name,
                    instances: checks_config,
                    init_config: read_yaml.init_config,
                    source: self.clone(),
                })
            }
        }
    }
}

impl Display for CheckSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckSource::Yaml(path) => write!(f, "{}", path.display()),
        }
    }
}

impl ChecksConfiguration {
    /// Creates a new `ChecksConfiguration` from the given configuration.
    pub fn from_configuration(config: &GenericConfiguration) -> Result<Self, GenericError> {
        Ok(config.as_typed()?)
    }

    async fn build_listeners(&self) -> Result<Vec<listener::DirCheckRequestListener>, Error> {
        let mut listeners = Vec::new();

        let listener = listener::DirCheckRequestListener::from_path(&self.check_config_dir)?;

        listeners.push(listener);

        Ok(listeners)
    }
}

#[async_trait]
impl SourceBuilder for ChecksConfiguration {
    async fn build(&self) -> Result<Box<dyn Source + Send>, GenericError> {
        //let (check_run_tx, check_run_rx) = mpsc::channel(100);
        //let (check_stop_tx, check_stop_rx) = mpsc::channel(100);
        //let runner = CheckRunner::new(check_run_rx, check_stop_rx)?;
        let listeners = self.build_listeners().await?;

        Ok(Box::new(Checks {
            listeners,
            //check_run_tx,
            //check_stop_tx,
        }))
    }

    fn outputs(&self) -> &[OutputDefinition] {
        static OUTPUTS: &[OutputDefinition] = &[OutputDefinition::default_output(DataType::Metric)];

        OUTPUTS
    }
}

pub struct Checks {
    listeners: Vec<listener::DirCheckRequestListener>,
}

impl Checks {
    // consumes self
    async fn run_inner(self, context: SourceContext, global_shutdown: ComponentShutdownHandle) -> Result<(), ()> {
        let mut listener_shutdown_coordinator = DynamicShutdownCoordinator::default();

        let Checks { listeners } = self;

        let mut joinset = tokio::task::JoinSet::new();
        // Run the local task set.
        // For each listener, spawn a dedicated task to run it.
        for listener in listeners {
            let listener_context = listener::DirCheckListenerContext {
                shutdown_handle: listener_shutdown_coordinator.register(),
                listener,
            };

            joinset.spawn_local(process_listener(context.clone(), listener_context));
        }

        info!("Check source started.");

        select! {
            j = joinset.join_next() => {
                match j {
                    Some(Ok(_)) => {
                        debug!("Check source task set exited normally.");
                    }
                    Some(Err(e)) => {
                        error!("Check source task set exited unexpectedly: {:?}", e);
                    }
                    None => {
                        // set is empty, all good here
                    }
                }
            },
            _ = global_shutdown => {
                info!("Stopping Check source...");

                listener_shutdown_coordinator.shutdown().await;
            }
        }

        info!("Check source stopped.");

        Ok(())
    }
}

#[async_trait]
impl Source for Checks {
    async fn run(mut self: Box<Self>, mut context: SourceContext) -> Result<(), ()> {
        let global_shutdown = context
            .take_shutdown_handle()
            .expect("should never fail to take shutdown handle");
        thread::spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .thread_name("check-runner")
                .enable_all()
                .build()
                .expect("Can't build runtime");

            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async {
                select! {
                    inner_res = self.run_inner(context, global_shutdown) => {
                        info!("Got inner res: {inner_res:?}");
                    }
                }
            })
        })
        .join()
        .expect("Can't join");

        Ok(())
    }
}

async fn process_listener(source_context: SourceContext, listener_context: listener::DirCheckListenerContext) {
    let listener::DirCheckListenerContext {
        shutdown_handle,
        mut listener,
    } = listener_context;
    tokio::pin!(shutdown_handle);

    let stream_shutdown_coordinator = DynamicShutdownCoordinator::default();

    //let (schedule_check_tx, schedule_check_rx) = mpsc::channel::<RunnableCheckRequest>(100);
    //let (unschedule_check_tx, unschedule_check_rx) = mpsc::channel::<CheckRequest>(100);
    let mut scheduler = scheduler::CheckScheduler::new(); // todo add shutdown handle

    //tokio::task::spawn_local(scheduler.run());
    info!("Check listener started.");
    let (mut new_entities, mut deleted_entities) = listener.subscribe();
    loop {
        select! {
            _ = &mut shutdown_handle => {
                debug!("Received shutdown signal. Waiting for existing stream handlers to finish...");
                break;
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                match listener.update_check_entities().await {
                    Ok(_) => {}
                    Err(e) => {
                        error!("Error updating check entities: {}", e);
                    }
                }
            }
            Some(new_entity) = new_entities.recv() => {
                let check_request = match new_entity.to_runnable_request() {
                    Ok(check_request) => check_request,
                    Err(e) => {
                        error!("Can't convert check source to runnable request: {}", e);
                        continue;
                    }
                };

                info!("Running a check request: {check_request}");

                //schedule_check_tx.send(check_request).await.expect("Couldn't send");
                scheduler.run_check(check_request);
            }
            Some(deleted_entity) = deleted_entities.recv() => {
                //unschedule_check_tx.send(deleted_entity).await.expect("Couldn't send");
                scheduler.stop_check(deleted_entity);
            }
        }
    }

    stream_shutdown_coordinator.shutdown().await;

    info!("Check listener stopped.");
}

/// Given a yaml config, find the corresponding python source code
/// Currently only looks in the same directory, no support for `checks.d` or `mycheck.d` directories
fn find_sibling_py_file(check_yaml_path: &Path) -> Result<PathBuf, Error> {
    let mut check_rel_filepath = check_yaml_path.to_path_buf();
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
    Ok(check_rel_filepath)
}
