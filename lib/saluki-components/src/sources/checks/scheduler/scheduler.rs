use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::{
    sync::{mpsc, Mutex},
    task::JoinHandle,
};
use tracing::{debug, error, info};

use crate::sources::checks::check::Check;

use super::*;
use job::JobQueue;

struct JobQueueWrapper {
    job_queue: Arc<JobQueue>,
    join_handle: JoinHandle<()>,
}

pub struct Scheduler {
    checks_tx: CheckSender,
    job_queues: Mutex<HashMap<Duration, JobQueueWrapper>>, // FIXME can we avoid the Arc, despite the ref in checks_queue?
    checks_queue: RwLock<HashMap<String, Arc<JobQueue>>>,
}

impl Scheduler {
    pub fn new() -> (Self, CheckReceiver) {
        let (checks_tx, checks_rx) = mpsc::channel(1); // FIXME
        let scheduler = Self {
            checks_tx,
            job_queues: Mutex::new(HashMap::new()),
            checks_queue: RwLock::new(HashMap::new()),
        };
        (scheduler, checks_rx)
    }

    // TODO prevent any further call to `enter`?
    pub async fn shutdown(self: Arc<Self>) {
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

    pub async fn schedule(self: Arc<Self>, check: Arc<dyn Check + Send + Sync>) {
        let id = check.id();
        let interval = check.interval();

        debug!(check.id = id, check.interval = interval.as_secs(), "Scheduling check.");

        let job_queue = self.clone().get_job_queue(interval).await;
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

    pub async fn unschedule(self: Arc<Self>, id: &str) {
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

    pub async fn enqueue(&self, check: Arc<dyn Check + Send + Sync>) {
        if let Err(err) = self.checks_tx.send(check).await {
            error!(error = %err, "Failed to enqueue check.")
        }
    }

    // FIXME can we avoid going through an Arc?
    async fn get_job_queue(self: Arc<Self>, interval: Duration) -> Arc<JobQueue> {
        debug!(job_queue.interval = interval.as_secs(), "Getting a JobQueue.");

        let mut job_queues = self.job_queues.lock().await;

        if let Some(wrapper) = job_queues.get(&interval) {
            return wrapper.job_queue.clone();
        }

        let queue = Arc::new(JobQueue::new(interval));
        let handle = queue.clone().run(self.clone());
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
