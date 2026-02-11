use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
    time::{self, Instant, Interval},
};
use tracing::{debug, error, trace};

use super::*;

pub use queue::JobQueue;

mod queue {
    use tokio::select;
    use tracing::warn;

    use super::*;

    struct State {
        pub buckets: Vec<JobBucket>,
        pub add_index: usize,
        pub schedule_index: usize,
    }

    pub struct JobQueue {
        state: Mutex<State>,
        sparse_step: usize,
        stop: Notify,
        interval: Duration,
    }

    impl JobQueue {
        pub fn new(interval: Duration) -> Self {
            trace!(interval = interval.as_secs(), "JobQueue created.");

            let mut state = queue::State {
                buckets: Vec::with_capacity(interval.as_secs() as usize),
                add_index: 0,
                schedule_index: 0,
            };
            for _ in 0..interval.as_secs() {
                state.buckets.push(JobBucket::new())
            }

            let sparse_step = 1; // TODO
            let stop = Notify::new();

            Self {
                state: Mutex::new(state),
                sparse_step,
                stop,
                interval,
            }
        }

        pub async fn add_job(&self, check: Arc<dyn Check + Send + Sync>) {
            assert!(self.interval == check.interval());

            let id = check.id();
            trace!(
                job_queue.interval = self.interval.as_secs(),
                check.id = id,
                "Adding check."
            );

            {
                let mut state = self.state.lock().await;
                let index = state.add_index;
                let len = state.buckets.len();

                let bucket = &mut state.buckets[index];
                trace!(
                    job_queue.interval = self.interval.as_secs(),
                    job_queue.bucket = index,
                    "Adding job to bucket."
                );
                bucket.add_job(check).await;

                state.add_index = (index + self.sparse_step) % len;
            }
        }

        pub async fn remove_job(&self, id: &str) -> bool {
            trace!(
                job_queue.interval = self.interval.as_secs(),
                check.id = id,
                "Removing check."
            );

            {
                let mut state = self.state.lock().await;
                for bucket in &mut state.buckets {
                    if bucket.remove_job(id).await {
                        return true;
                    }
                }
            }
            false
        }

        pub fn run(self: Arc<Self>, checks_tx: CheckSender) -> JoinHandle<()> {
            tokio::spawn(async move { self.process(checks_tx).await })
        }

        pub async fn stop(self: Arc<Self>) {
            self.stop.notify_one()
        }

        pub async fn process(&self, checks_tx: CheckSender) {
            let mut interval: Interval = time::interval(Duration::from_secs(1));
            let mut last_tick = Instant::now();

            loop {
                select! {
                    _ = self.stop.notified() => {
                        trace!(job_queue.interval = self.interval.as_secs(), "Shutting down.");
                        break
                    }

                    _ = interval.tick() => {
                        let elasped = Instant::now().duration_since(last_tick);
                        if elasped > Duration::from_secs(2) {
                            warn!(
                                elapsed_secs = elasped.as_secs(),
                                "Previous bucket took over 2 secs to schedule. Next checks will be running behind the schedule."
                            )
                        }
                        last_tick = Instant::now();

                        let schedule_index = self.state.lock().await.schedule_index;
                        trace!(
                            job_queue.interval = self.interval.as_secs(),
                            job_queue.bucket = schedule_index,
                            "Looking at bucket."
                        );

                        let jobs = {
                            let mut state = self.state.lock().await;

                            let index = state.schedule_index;
                            state.schedule_index = (index + self.sparse_step) % state.buckets.len();

                            let bucket = &state.buckets[index];
                            bucket.jobs.iter().map(Arc::clone).collect::<Vec<_>>()
                        };

                        for check in jobs {
                            if let Err(err) = checks_tx.send(check).await {
                                error!(job_queue.interval = self.interval.as_secs(), error = %err, "Scheduler checks channel closed")
                            }
                        }
                    }
                }
            }
        }
    }
}

pub struct JobBucket {
    jobs: VecDeque<Arc<dyn Check + Send + Sync>>,
}

impl JobBucket {
    pub fn new() -> Self {
        Self { jobs: VecDeque::new() }
    }

    // TODO is it okay to add a check twice?
    pub async fn add_job(&mut self, check: Arc<dyn Check + Send + Sync>) {
        trace!(check.id = check.id(), "Adding check.");

        self.jobs.push_back(check)
    }

    pub async fn remove_job(&mut self, id: &str) -> bool {
        for (index, check) in self.jobs.iter().enumerate() {
            if id == check.id() {
                trace!(check.id = id, job.index = index, "Removing check.");
                let prev = self.jobs.remove(index);
                assert!(prev.is_some());
                return true;
            }
        }
        debug!(check.id = id, "Check not found.");
        false
    }
}
