use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
    time::{self, Instant, Interval},
};
use tracing::{debug, trace};

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
        interval: Duration, // XXX debug
    }

    impl JobQueue {
        pub fn new(interval: Duration) -> Self {
            trace!("New from duration {} sec", interval.as_secs());

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

            let id = check.id().clone();
            trace!("JobQueue<{}>: Check #{id} to add", self.interval.as_secs());

            {
                let mut state = self.state.lock().await;
                let index = state.add_index;
                let len = state.buckets.len();

                let bucket = &mut state.buckets[index];
                trace!("JobQueue<{}>: Adding job to bucket #{}", self.interval.as_secs(), index);
                bucket.add_job(check).await;

                state.add_index = (index + self.sparse_step) % len;
            }
        }

        pub async fn remove_job(&self, id: &CheckID) -> bool {
            trace!("JobQueue<{}>: Removing check #{id}", self.interval.as_secs());

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

        pub fn run(self: Arc<Self>, scheduler: Arc<Scheduler>) -> JoinHandle<()> {
            tokio::spawn(async move { self.process(scheduler).await })
        }

        pub async fn stop(self: Arc<Self>) {
            self.stop.notify_one()
        }

        pub async fn process(&self, scheduler: Arc<Scheduler>) {
            let mut interval: Interval = time::interval(Duration::from_secs(1));
            let mut last_tick = Instant::now();

            loop {
                select! {
                    _ = self.stop.notified() => {
                        trace!("JobQueue<{}>: Shut down", self.interval.as_secs());
                        break
                    }

                    _ = interval.tick() => {
                        let elasped = Instant::now().duration_since(last_tick);
                        if elasped > Duration::from_secs(2) {
                            warn!(
                                "Previous bucket took over {} secs to schedule. Next checks will be running behind the schedule.",
                                elasped.as_secs()
                            )
                        }
                        last_tick = Instant::now();

                        trace!(
                            "JobQueue<{}>: Looking at bucket #{}",
                            self.interval.as_secs(),
                            self.state.lock().await.schedule_index
                        );

                        let jobs = {
                            let mut state = self.state.lock().await;

                            let index = state.schedule_index;
                            state.schedule_index = (index + self.sparse_step) % state.buckets.len();

                            let bucket = &state.buckets[index];
                            bucket.jobs.iter().map(Arc::clone).collect::<Vec<_>>()
                        };

                        for check in jobs {
                            let id = check.id().to_string();
                            if scheduler.is_check_scheduled(&id) {
                                scheduler.clone().enqueue(check.clone()).await
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
        trace!("Adding check #{}", check.id());

        self.jobs.push_back(check)
    }

    pub async fn remove_job(&mut self, id: &CheckID) -> bool {
        for (index, check) in self.jobs.iter().enumerate() {
            if id == check.id() {
                trace!("JobBucket: Removing Check #{id} found at {index}");
                let prev = self.jobs.remove(index);
                assert!(prev.is_some());
                return true;
            }
        }
        debug!("Check #{id} not found");
        false
    }
}
