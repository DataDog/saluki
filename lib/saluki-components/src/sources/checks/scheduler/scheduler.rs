use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::select;
use tokio::{
    sync::{mpsc, Mutex, Notify},
    task::JoinHandle,
};
use tracing::{debug, error, info, trace};

use crate::sources::checks::check::Check;

use super::*;
use job::JobQueue;

struct JobQueueWrapper {
    job_queue: Arc<JobQueue>,
    join_handle: JoinHandle<()>,
}

pub struct Scheduler {
    job_queues: Mutex<HashMap<Duration, JobQueueWrapper>>, // FIXME can we avoid the Arc, despite the ref in checks_queue?
    checks_queue: RwLock<HashMap<String, Arc<JobQueue>>>,
    jobs_tx: CheckSender,
    jobs_rx: Mutex<CheckReceiver>,
    stop: Notify,
}

impl Scheduler {
    pub fn new() -> Self {
        let (jobs_tx, jobs_rx) = mpsc::channel(1);
        let scheduler = Self {
            job_queues: Mutex::new(HashMap::new()),
            checks_queue: RwLock::new(HashMap::new()),
            jobs_tx,
            jobs_rx: Mutex::new(jobs_rx),
            stop: Notify::new(),
        };
        scheduler
    }

    pub fn run(self: Arc<Self>) -> (CheckReceiver, JoinHandle<()>) {
        let (runner_tx, runner_rx) = mpsc::channel(1);

        let handle = tokio::spawn(async move { self.run_impl(runner_tx).await });
        (runner_rx, handle)
    }

    async fn run_impl(&self, runner_tx: CheckSender) {
        let mut jobs_rx = self.jobs_rx.lock().await;

        loop {
            trace!("Waiting to receive a check to forward...");

            select! {
                _ = self.stop.notified() => {
                    info!("Shutting down.");
                    break
                }

                maybe_check = jobs_rx.recv() => match maybe_check {
                    Some(check) => {
                        debug!(check.id = check.id(), "Check to forward");

                        if !self.is_check_scheduled(check.id()) {
                            debug!(check.id = check.id(), "Dropping already unscheduled Check.");
                            continue;
                        }

                        if let Err(err) = runner_tx.send(check).await {
                            error!(error = %err, "Unable to forward check to Runner.")
                        }
                    },
                    None => {
                        error!("Unexpected closure of job's checks channel");
                        break
                    }
                }
            }
        }

        {
            let mut job_queues = self.job_queues.lock().await;

            for (_, jqw) in job_queues.iter() {
                jqw.job_queue.clone().stop().await
            }
            for (_, jqw) in job_queues.drain() {
                if let Err(err) = jqw.join_handle.await {
                    error!(error = %err, "Job task failed.");
                }
            }
        }

        // Drops runner_rx signals Runner to shutdown
    }

    pub async fn shutdown(&self) {
        self.stop.notify_one()
    }

    // TODO prevent any call to `schedule` when shutting down?
    pub async fn schedule(&self, check: Arc<dyn Check + Send + Sync>) {
        let id = check.id();
        let interval = check.interval();

        debug!(check.id = id, check.interval = interval.as_secs(), "Scheduling check.");

        let job_queue = self.get_job_queue(interval).await;
        job_queue.add_job(check.clone()).await;

        if let Some(_) = self
            .checks_queue
            .write()
            .unwrap()
            .insert(id.to_string(), job_queue.clone())
        {
            error!(check.id = id, "Check was already scheduled.")
        }
    }

    pub async fn unschedule(&self, id: &str) {
        debug!(check.id = id, "Unscheduling check.");

        let maybe_queue = self.checks_queue.write().unwrap().remove(id);
        if maybe_queue.is_none() {
            info!(check.id = id, "Check not scheduled.");
            return;
        }
        let job_queue = maybe_queue.unwrap();

        if job_queue.remove_job(id).await == false {
            error!(check.id = id, "Check not found in JobQueue.");
        }
    }

    async fn get_job_queue(&self, interval: Duration) -> Arc<JobQueue> {
        debug!(job_queue.interval = interval.as_secs(), "Getting a JobQueue.");

        let mut job_queues = self.job_queues.lock().await;

        if let Some(wrapper) = job_queues.get(&interval) {
            return wrapper.job_queue.clone();
        }

        let queue = Arc::new(JobQueue::new(interval));
        let handle = queue.clone().run(self.jobs_tx.clone());
        let wrapper = JobQueueWrapper {
            job_queue: queue.clone(),
            join_handle: handle,
        };
        let maybe = job_queues.insert(interval, wrapper);
        assert!(maybe.is_none());

        queue
    }

    pub fn is_check_scheduled(&self, id: &str) -> bool {
        self.checks_queue.read().unwrap().contains_key(id)
    }
}
