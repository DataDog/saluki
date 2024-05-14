use saluki_core::topology::shutdown::DynamicShutdownHandle;
use snafu::Snafu;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::info;

use super::{CheckRequest, RunnableCheckRequest};

#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum Error {}

pub struct CheckRunner {
    new_checks_rx: mpsc::Receiver<RunnableCheckRequest>,
    deleted_checks_rx: mpsc::Receiver<CheckRequest>,
}

impl CheckRunner {
    pub fn new(
        new_checks_rx: mpsc::Receiver<RunnableCheckRequest>, deleted_checks_rx: mpsc::Receiver<CheckRequest>,
    ) -> Result<CheckRunner, Error> {
        Ok(CheckRunner {
            new_checks_rx,
            deleted_checks_rx,
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

    async fn add_check(&self, check: RunnableCheckRequest) {
        info!("Adding check request {check}");
    }
    async fn delete_check(&self, check: CheckRequest) {
        info!("Deleting check request {check}");
    }
}
