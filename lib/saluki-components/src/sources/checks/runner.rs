use std::{collections::HashMap, path::PathBuf, time::Duration};

use saluki_core::topology::shutdown::DynamicShutdownHandle;
use snafu::Snafu;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::info;

use super::{CheckInstanceConfiguration, CheckRequest, RunnableCheckRequest};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum Error {}

pub struct CheckRunner {
    new_checks_rx: mpsc::Receiver<RunnableCheckRequest>,
    deleted_checks_rx: mpsc::Receiver<CheckRequest>,
    // key here is the configuration that spawned this check
    running: HashMap<PathBuf, Vec<JoinHandle<()>>>,
}

impl CheckRunner {
    pub fn new(
        new_checks_rx: mpsc::Receiver<RunnableCheckRequest>, deleted_checks_rx: mpsc::Receiver<CheckRequest>,
    ) -> Result<CheckRunner, Error> {
        Ok(CheckRunner {
            new_checks_rx,
            deleted_checks_rx,
            running: HashMap::new(),
        })
    }
    pub async fn run(mut self, shutdown_handle: DynamicShutdownHandle) -> JoinHandle<()> {
        tokio::spawn(async move {
            tokio::pin!(shutdown_handle);

            loop {
                tokio::select! {
                    _ = &mut shutdown_handle => {
                        info!("Got shutdown signal, shutting down now");
                        break;
                    }
                    Some(check) = self.new_checks_rx.recv() => {
                        self.add_check(check).await;
                    }
                    Some(check) = self.deleted_checks_rx.recv() => {
                        self.delete_check(check).await;
                    }
                    else => {
                        break; // when channels are closed, exit
                    }
                }
            }
        })
    }

    async fn add_check(&mut self, check: RunnableCheckRequest) {
        info!("Adding check request {check}");
        let py_source = tokio::fs::read_to_string(&check.check_source_code).await.unwrap();
        let running = self.running.entry(check.check_source_code).or_default();
        for instance in check.check_request.instances {
            let py_source = py_source.clone();
            let idx = running.len();
            let handle = tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(Duration::from_millis(instance.min_collection_interval_ms.into()));
                loop {
                    interval.tick().await;
                    // run check
                    info!("Running check instance {idx} on {}", py_source);
                }
            });
            running.push(handle);
        }
    }
    async fn delete_check(&self, check: CheckRequest) {
        info!("Deleting check request {check}");
    }
}
