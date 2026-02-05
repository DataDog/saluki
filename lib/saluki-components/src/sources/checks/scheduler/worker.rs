use tokio::{sync::mpsc, sync::Mutex, task::JoinHandle};
use tracing::{debug, info, trace};

use super::tracker::RunningCheckTracker;
use super::*;

pub type WorkerID = usize;

pub struct Worker {
    pub id: WorkerID,
    check_rx: Mutex<CheckReceiver>,
    tracker: Arc<RunningCheckTracker>,
}

impl Worker {
    pub fn new(id: WorkerID, tracker: Arc<RunningCheckTracker>) -> (Self, CheckSender) {
        trace!("New worker #{id}");

        let (check_tx, check_rx) = mpsc::channel(1);
        let worker = Worker {
            id,
            check_rx: Mutex::new(check_rx),
            tracker,
        };
        (worker, check_tx)
    }

    pub async fn run(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move { self.run_impl().await })
    }

    async fn run_impl(self: Arc<Self>) {
        let mut check_rx = self.check_rx.lock().await;

        debug!("Woker<{}>: Ready to process checks", self.id);

        loop {
            match check_rx.recv().await {
                Some(check) => {
                    let id = check.id();
                    debug!("Woker<{}>: Check #{id} to run", self.id);

                    if !self.tracker.add_check(check.clone()).await {
                        info!("Woker<{}>: Check #{id} is already running, skipping", self.id);
                        break;
                    }

                    let _ = check.run().await; // FIXME err

                    self.tracker.remove_check(check).await
                }
                None => {
                    debug!("Woker<{}>: Finished processing checks", self.id);
                    break;
                }
            }
        }
    }
}
