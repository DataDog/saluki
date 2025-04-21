//! Asynchronous task supervision.

use std::{collections::{HashMap, VecDeque}, future::Future, pin::Pin, sync::Arc, time::Duration};

use saluki_error::GenericError;
use snafu::Snafu;
use tokio::{task::{AbortHandle, Id, JoinSet}, time::Instant};
use tracing::{debug, error, warn};

/// Restart mode for workers.
#[derive(Clone, Copy)]
pub enum RestartMode {
    /// Restarts the failed worker only.
    OneForOne,

    /// Restarts all workers, including the failed one.
    OneForAll,
}

/// Restart strategy for a supervisor.
///
/// Defaults to one-to-one mode (only restart the failed worker) and a restart intensity of 1 over a period of 5 seconds.
///
/// # Restarts and permanent failure
///
/// A supervisor will allow up to `intensity` process restarts, across all supervised children, over a given `period`.
/// When this limit is exceeded, the supervisor will stop all child processes and return an error itself, indicating
/// that the supervisor has failed overall.
///
/// Permanent failure bubbles up to the parent supervisor, until reaching the root supervisor. Once permanent failure
/// reaches the root supervisor, and the root supervisor exceeds its own restart limits, the root supervisor will fail
/// and cease execution.
#[derive(Clone, Copy)]
pub struct RestartStrategy {
    mode: RestartMode,
    intensity: usize,
    period: Duration,
}

impl RestartStrategy {
    /// Creates a new `RestartStrategy` with the given mode, intensity, and period.
    pub fn new(mode: RestartMode, intensity: usize, period: Duration) -> Self {
        Self {
            mode,
            intensity,
            period,
        }
    }

    /// Creates a new `RestartStrategy` with the one-to-one restart mode, and the default intensity/period.
    pub fn one_to_one() -> Self {
        let mut strategy = Self::default();
        strategy.mode = RestartMode::OneForOne;
        strategy
    }

    /// Creates a new `RestartStrategy` with the one-for-all restart mode, and the default intensity/period.
    pub fn one_for_all() -> Self {
        let mut strategy = Self::default();
        strategy.mode = RestartMode::OneForAll;
        strategy
    }

    /// Sets the restart intensity and period for the strategy.
    pub const fn with_intensity_and_period(mut self, intensity: usize, period: Duration) -> Self {
        self.intensity = intensity;
        self.period = period;
        self
    }
}

impl Default for RestartStrategy {
    fn default() -> Self {
        Self::new(RestartMode::OneForOne, 1, Duration::from_secs(5))
    }
}

enum RestartAction {
    /// Execute a restart with the given mode.
    Restart(RestartMode),

    /// Supervisor must shutdown as the maximum number of restarts has been reached.
    Shutdown,
}

struct RestartState {
    strategy: RestartStrategy,
    restart_history: VecDeque<Instant>,
}

impl RestartState {
    /// Creates a new `RestartState` with the given strategy.
    fn new(strategy: RestartStrategy) -> Self {
        Self {
            strategy,
            restart_history: VecDeque::with_capacity(strategy.intensity),
        }
    }

    /// Evaluates a restart based on the current state and determine the action the supervisor should take in response.
    fn evaluate_restart(&mut self) -> RestartAction {
        // Short circuit if our intensity is zero.
        if self.strategy.intensity == 0 {
            debug!("Supervisor has zero restart intensity, shutting down.");
            return RestartAction::Shutdown;
        }

        // Since we only keep track of the last `intensity` restarts, we simply need to check if the oldest restart
        // we're tracking is within `period` of the current time, and if the number of tracked restarts is equal to
        // `intensity`.
        //
        // When both of these are true, we have exceeded the restart intensity limit and must shutdown.
        let now = Instant::now();
        if self.restart_history.len() == self.strategy.intensity {
            let oldest = self.restart_history.front().expect("restart history cannot be empty");
            if now.saturating_duration_since(*oldest) < self.strategy.period {
                debug!("Supervisor has exceeded restart limits ({} in {:?}), shutting down.", self.strategy.intensity, self.strategy.period);
                return RestartAction::Shutdown;
            }

            // Remove the oldest restart from the history since it is outside the period.
            self.restart_history.pop_front();
        }

        // Track this latest restart.
        self.restart_history.push_back(now);

        debug!("Restart limit not exceeded, restarting worker.");
        RestartAction::Restart(self.strategy.mode)
    }
}

/// A type that can be supervised.
pub trait Supervisable: Send + Sync {
    /// Initializes the worker, returning a `Future` that represents the worker's execution.
    ///
    /// When `Some` is returned, the worker is spawned and managed by the supervisor. When `None` is returned, the
    /// worker is considered to be permanently failed. This can be useful for supervised tasks that are not expected to
    /// ever fail, or cannot support restart, but should still be managed within the same supervision hierarchy as other
    /// tasks.
    fn initialize(&self) -> Option<Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>>;
}

/// Supervisor errors.
#[derive(Debug, Snafu)]
pub enum SupervisorError {
    /// A child specification failed to initialize.
    #[snafu(display("Child specification failed to initialize."))]
    FailedToInitialize,

    /// The supervisor exceeded its restart limits and was forced to shutdown.
    #[snafu(display("Supervisor has exceeded restart limits and was forced to shutdown."))]
    Shutdown,
}

/// A supervision tree.
pub struct Supervisor {
    supervisor_id: Arc<str>,
    child_specs: Vec<Arc<dyn Supervisable>>,
    restart_strategy: RestartStrategy,
}

impl Supervisor {
    /// Creates a new `Supervisor` with the default restart strategy and no children.
    pub fn new<S: AsRef<str>>(supervisor_id: S) -> Self {
        Self {
            supervisor_id: supervisor_id.as_ref().into(),
            child_specs: Vec::new(),
            restart_strategy: RestartStrategy::default(),
        }
    }

    /// Sets the restart strategy for the supervisor.
    pub fn with_restart_strategy(mut self, strategy: RestartStrategy) -> Self {
        self.restart_strategy = strategy;
        self
    }

    /// Adds a child process to the supervisor.
    pub fn add_process<T: Supervisable + 'static>(&mut self, process: T) {
        debug!(supervisor_id = %self.supervisor_id, "Adding new static child process #{}.", self.child_specs.len());
        self.child_specs.push(Arc::new(process));
    }

    fn spawn_child(&self, child_spec_idx: usize, worker_state: &mut WorkerState) -> Result<(), SupervisorError> {
        match self.child_specs.get(child_spec_idx) {
            Some(child_spec) => match child_spec.initialize() {
                Some(worker) => {
                    debug!(supervisor_id = %self.supervisor_id, "Spawning static child process #{}.", child_spec_idx);
                    worker_state.add_worker(child_spec_idx, worker)
                },
                None => return Err(SupervisorError::FailedToInitialize),
            },
            None => unreachable!("child spec index should never be out of bounds"),
        }

        Ok(())
    }

    fn spawn_all_children(&self, worker_state: &mut WorkerState) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Spawning all static child processes.");
        for child_spec_idx in 0..self.child_specs.len() {
            self.spawn_child(child_spec_idx, worker_state)?;
        }

        Ok(())
    }

    async fn run_inner(&self) -> Result<(), SupervisorError> {
        let mut restart_state = RestartState::new(self.restart_strategy.clone());
        let mut worker_state = WorkerState::new();

        // Do the initial spawn of all child processes and supervisors.
        self.spawn_all_children(&mut worker_state)?;

        // Now we supervise.
        while let Some(result) = worker_state.wait_for_next_worker().await {
            match result {
                // Worker processed terminated.
                //
                // TODO: Erlang/OTP defaults to always trying to restart a worker, even if it doesn't terminate due to a
                // legitimate failure. It does allow configuring this behavior on a per-worker basis, however. We don't
                // support dynamically adding child processes, which is the only real use case I can think of for having
                // non-long-lived child processes... so I think for now, we're OK just always try to restart.
                (child_spec_idx, worker_result) => match restart_state.evaluate_restart() {
                    RestartAction::Restart(mode) => match mode {
                        RestartMode::OneForOne => {
                            warn!(supervisor_id = %self.supervisor_id, ?worker_result, "Supervised worker terminated, restarting.");
                            self.spawn_child(child_spec_idx, &mut worker_state)?;                       
                        },
                        RestartMode::OneForAll => {
                            warn!(supervisor_id = %self.supervisor_id, ?worker_result, "Supervised worker terminated, restarting all workers.");
                            worker_state.abort_all_workers().await;
                            self.spawn_all_children(&mut worker_state)?;
                        },
                    },
                    RestartAction::Shutdown => {
                        error!(supervisor_id = %self.supervisor_id, ?worker_result, "Supervisor shutting down due to restart limits.");
                        return Err(SupervisorError::Shutdown)
                    }
                },
            }
        }

        Ok(())
    }

    async fn run_nested(&self) -> Result<(), GenericError> {
        // Simple wrapper around `run_inner` to satisfy the return type signature needed when running the supervisor as
        // a nested child process in another supervisor.
        debug!(supervisor_id = %self.supervisor_id, "Nested supervisor starting.");
        self.run_inner().await?;
        Ok(())
    }

    /// Runs the supervisor.
    ///
    /// # Errors
    ///
    /// If the supervisor exceeds its restart limits, an error is returned.
    pub async fn run(&mut self) -> Result<(), SupervisorError> {
        debug!(supervisor_id = %self.supervisor_id, "Supervisor starting.");
        self.run_inner().await
    }
}

impl Supervisable for Supervisor {
    fn initialize(&self) -> Option<Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>> {
        // Clone ourselves so that we can move a copy into the resulting future, allowing us to be fulfill the
        // `Send` requirement of the worker future.
        let supervisor = Supervisor {
            supervisor_id: self.supervisor_id.clone(),
            child_specs: self.child_specs.clone(),
            restart_strategy: self.restart_strategy.clone(),
        };

        let f = async move {
            supervisor.run_nested().await?;
            Ok(())
        };

        Some(Box::pin(f))
    }
}

struct WorkerState {
    worker_tasks: JoinSet<Result<(), GenericError>>,
    worker_map: HashMap<Id, (usize, AbortHandle)>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            worker_tasks: JoinSet::new(),
            worker_map: HashMap::new(),
        }
    }

    fn add_worker<F>(&mut self, worker_id: usize, f: F)
    where
        F: Future<Output = Result<(), GenericError>> + Send + 'static,
    {
        let handle = self.worker_tasks.spawn(f);
        self.worker_map.insert(handle.id(), (worker_id, handle));
    }

    async fn wait_for_next_worker(&mut self) -> Option<(usize, Result<(), GenericError>)> {
        debug!("Waiting for next worker to complete.");

        match self.worker_tasks.join_next_with_id().await {
            Some(Ok((worker_task_id, worker_result))) => {
                let (worker_id, _) = self.worker_map.remove(&worker_task_id).expect("worker task ID not found");
                Some((worker_id, worker_result))
            },
            Some(Err(e)) => {
                let worker_task_id = e.id();
                let (worker_id, _) = self.worker_map.remove(&worker_task_id).expect("worker task ID not found");
                Some((worker_id, Err(GenericError::new(e).context("Worker task panicked during execution."))))
            },
            None => None,
        }
    }

    async fn abort_all_workers(&mut self) {
        // TODO: Abort/shutdown the workers in reverse order of how they were started.
        debug!("Aborting all workers.");

        // Clear the worker map to reset our worker state.
        self.worker_map.clear();

        // Abort all of the worker tasks, waiting for them to complete.
        self.worker_tasks.shutdown().await;
    }
}
