use std::collections::HashMap;
use std::sync::{Arc, Weak};

use tokio::{sync::Mutex, task::JoinHandle};
use tracing::{debug, error, info, trace};

use super::tracker::RunningCheckTracker;
use super::worker::{Worker, WorkerID};
use super::*;

struct Config {
    workers_count: Option<usize>,
}

struct FixedWorkers {
    workers: HashMap<WorkerID, (Arc<Worker>, CheckSender, JoinHandle<()>)>,
    next: WorkerID, // Identify Worker to send check to
}

struct EphemeralWorkers {
    workers: HashMap<WorkerID, (Weak<Worker>, JoinHandle<()>)>,
    next: WorkerID, // WorkerID for creating next Worker
}

pub struct Runner {
    fixed_workers: Mutex<FixedWorkers>,
    ephemeral_workers: Mutex<EphemeralWorkers>,
    tracker: Arc<RunningCheckTracker>,
    pending: Mutex<CheckReceiver>,
    config: Config,
}

impl Runner {
    pub async fn new(pending: CheckReceiver, workers_count: Option<usize>) -> Self {
        let config = Config { workers_count };
        let workers_count = config.workers_count.unwrap_or(4);

        let fixed_workers = FixedWorkers {
            workers: HashMap::new(),
            next: 0,
        };

        let ephemeral_workers = EphemeralWorkers {
            workers: HashMap::new(),
            next: 0,
        };

        let runner = Self {
            fixed_workers: Mutex::new(fixed_workers),
            ephemeral_workers: Mutex::new(ephemeral_workers),
            tracker: Arc::new(RunningCheckTracker::new()),
            pending: Mutex::new(pending),
            config,
        };

        runner.ensure_min_workers(workers_count).await;
        runner
    }

    pub async fn ensure_min_workers(&self, count: usize) {
        debug!("Ensure minimum workers: {count}");

        let mut workers_state = self.fixed_workers.lock().await;

        for i in workers_state.workers.len()..count {
            let id = i as WorkerID;
            let (worker, check_tx) = Worker::new(id, self.tracker.clone());
            let worker = Arc::new(worker);
            let handle = worker.clone().run().await;
            let old = workers_state.workers.insert(id, (worker, check_tx, handle));
            assert!(old.is_none());
        }
    }

    pub async fn update_number_workers(&self) {
        if self.config.workers_count.is_some() {
            return;
        }

        let checks = self.tracker.running();
        let workers = self.fixed_workers.lock().await.workers.len();

        let desired = match checks {
            0..=10 => 4,
            11..=15 => 10,
            16..=20 => 15,
            21..=25 => 20,
            _ => 25, // TODO value from core Agent
        };

        if desired != workers {
            self.ensure_min_workers(desired).await
        }
    }

    pub async fn run(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                trace!("Waiting to receive a check to run...");

                match self.pending.lock().await.recv().await {
                    Some(check) => self.handle_check(check).await,
                    None => {
                        debug!("Shutting down...");
                        let mut workers_state = self.fixed_workers.lock().await;
                        let handles = workers_state
                            .workers
                            .drain()
                            .map(|(_, (_, _, h))| h)
                            .collect::<Vec<_>>(); // drop check_tx to signal shutdown
                        for handle in handles {
                            handle.await.expect("joinable worker")
                        }
                        break;
                    }
                }
            }
        })
    }

    async fn handle_check(&self, check: Arc<dyn Check + Send + Sync>) {
        let id = check.id();
        debug!("Got Check #{id} to run");

        if self.tracker.is_running(id) {
            info!("Check #{id} is already running");
            return;
        }

        if check.interval().as_secs() > 0 {
            self.update_number_workers().await;
            self.send_to_worker(check).await
        } else {
            self.clean_ephemeral_worker().await;
            let check_tx = self.add_ephemeral_worker().await;
            if let Err(err) = check_tx.send(check).await {
                error!("Unable to send Check to ephemeral worker: {err}")
            }
        }
    }

    // FIXME should wait for the first one to be ready
    pub async fn send_to_worker(&self, check: Arc<dyn Check + Send + Sync>) {
        let mut workers_state = self.fixed_workers.lock().await;
        let index = workers_state.next;
        let workers = &mut workers_state.workers;

        for i in 0..(workers.len() as WorkerID) {
            // FIXME overflow
            let i = (index + i) % (workers.len() as WorkerID);
            let (worker, check_tx, _) = &workers[&i];
            match check_tx.try_send(check.clone()) {
                Ok(()) => {
                    debug!("Non blocking send of check #{} to worker #{}", check.id(), worker.id);
                    return;
                }
                Err(_) => {
                    trace!("Worker #{} is busy", worker.id);
                }
            }
        }

        let (worker, check_tx, _) = &workers[&index];
        debug!("Blocking send of check #{} to worker #{}", check.id(), worker.id);
        if let Err(err) = check_tx.send(check.clone()).await {
            error!("Unable to send check to worker #{}: {err}", worker.id)
        } else {
            debug!("Check #{} sent to worker #{}", check.id(), worker.id);
        }

        workers_state.next = (index + 1) % (workers_state.workers.len() as WorkerID)
    }

    async fn add_ephemeral_worker(&self) -> CheckSender {
        let mut ephemeral_workers = self.ephemeral_workers.lock().await;

        let id = ephemeral_workers.next as WorkerID;
        let (worker, check_tx) = Worker::new(id, self.tracker.clone()); // FIXME don't track?
        let worker = Arc::new(worker);
        let handle = worker.clone().run().await;
        let worker = Arc::downgrade(&worker);
        let old = ephemeral_workers.workers.insert(id, (worker, handle));
        assert!(old.is_none());

        ephemeral_workers.next += 1;
        check_tx
    }

    async fn clean_ephemeral_worker(&self) {
        let mut ephemeral_workers = self.ephemeral_workers.lock().await;

        let stale = ephemeral_workers
            .workers
            .iter()
            .filter_map(
                |(id, (worker, _))| {
                    if worker.strong_count() == 0 {
                        Some(*id)
                    } else {
                        None
                    }
                },
            )
            .collect::<Vec<_>>();

        stale.iter().for_each(|id| {
            ephemeral_workers.workers.remove(id);
        });
    }
}
