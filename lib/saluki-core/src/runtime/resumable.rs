//! Workers that keep the state of their work across restarts.

use std::{
    any::Any,
    fmt,
    future::{poll_fn, Future},
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    pin::pin,
    sync::{Arc, Mutex as SyncMutex},
    task::Poll,
    time::Duration,
};

use async_trait::async_trait;
use saluki_common::{
    sync::shutdown::{ShutdownCoordinator, ShutdownHandle, ShutdownView},
    task::spawn_traced_named,
};
use saluki_error::{generic_error, GenericError};
use tokio::{
    sync::{Mutex, OwnedMutexGuard},
    task::JoinHandle,
};
use tracing::{error, warn};

use super::supervisable::{InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture};

const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Work whose state outlives the worker that runs it.
///
/// The state is `self`. A [`ResumableWorker`] calls [`run`][Self::run] each time it starts, and each call continues from
/// whatever the previous call left in `self`.
pub trait Resumable: Send + 'static {
    /// Returns the shutdown strategy of the worker that runs this work.
    ///
    /// [`ResumableWorker`] calls this once, when it is created. Defaults to a graceful shutdown with a five-second
    /// timeout.
    fn shutdown_strategy(&self) -> ShutdownStrategy {
        ShutdownStrategy::Graceful(DEFAULT_SHUTDOWN_TIMEOUT)
    }

    /// Runs the work until `stop` resolves.
    ///
    /// Once `stop` resolves, return at the next point where stopping loses no work. A call can also be dropped at any
    /// await point, when its owner halts it or it is cancelled before supervision, so every await point must leave
    /// `self` able to continue. Record work in flight that can't be repeated, and don't race it against `stop` if
    /// stopping must lose nothing.
    ///
    /// The worker ends when this returns, and its owner then applies its restart policy. Work that can run out of
    /// things to do waits for `stop`, returns an error, or runs in a worker that is added as a hosted child.
    ///
    /// # Errors
    ///
    /// An error ends the worker that runs this call, and its owner restarts it or fails, according to its restart
    /// policy.
    fn run(&mut self, stop: ShutdownHandle) -> impl Future<Output = Result<(), GenericError>> + Send + '_;
}

/// A worker that keeps the state of its work across restarts.
///
/// Most workers build their work again each time they start. A `ResumableWorker` keeps its [`Resumable`] state instead,
/// so that each start continues from where the previous one stopped: a restart resumes rather than resets. This holds
/// even after a panic, so a state that a panic left inconsistent can fail again until the restart limit of the owner
/// passes the failure up the supervision tree.
///
/// Create one with [`new`][Self::new], or with [`start_early`][Self::start_early] for work that has to start before any
/// supervisor runs. The worker isn't `Clone`: add it to one owner only.
///
/// # Starting early
///
/// [`start_early`][Self::start_early] runs the work at once in a plain task. The task isn't a process: it has no
/// process identifier, scope, or dataspace, so the work can't spawn children or assert anything, and it isn't part of
/// any supervision tree. It runs in the span and resource group of the caller.
///
/// When the worker first starts under supervision, it signals the early task to stop, waits for `run` to return at a
/// point that loses no work, and then continues with the same state as a supervised process. If `run` ended before
/// that, the first start ends with its result. If it panicked, the first start panics again. The shutdown strategy of
/// the worker bounds the wait.
///
/// Dropping the worker before it first starts cancels the task.
///
/// # State
///
/// The same state is used before supervision and by each instance of the worker, so it **MUST NOT** hold anything
/// bound to a process, such as a [`Scope`][crate::runtime::Scope], a [`ScopeGuard`][crate::runtime::ScopeGuard], or a
/// [`SupervisorHandle`][crate::runtime::SupervisorHandle]. Set those up in each call to `run` instead.
///
/// Timers and connections stay bound to the Tokio runtime that created them. Don't run the worker on a runtime that is
/// replaced when its owner restarts, such as the runtime of a supervisor with a
/// [dedicated runtime][crate::runtime::RuntimeMode::Dedicated], unless `run` builds them again.
pub struct ResumableWorker<L> {
    name: String,
    shutdown_strategy: ShutdownStrategy,
    state: Arc<Mutex<L>>,
    early: SyncMutex<Option<EarlyStart>>,
}

impl<L: Resumable> ResumableWorker<L> {
    /// Creates a worker named `name` that runs `state` each time it starts.
    #[must_use]
    pub fn new<N: Into<String>>(name: N, state: L) -> Self {
        Self {
            name: name.into(),
            shutdown_strategy: state.shutdown_strategy(),
            state: Arc::new(Mutex::new(state)),
            early: SyncMutex::new(None),
        }
    }

    /// Starts running `state` now, in a task named `name`, and returns the worker that takes it over when it first
    /// starts.
    ///
    /// See [Starting early](Self#starting-early).
    ///
    /// # Panics
    ///
    /// Panics if called outside of a Tokio runtime.
    #[must_use = "dropping the worker before it starts cancels the work"]
    pub fn start_early<N: Into<String>>(name: N, state: L) -> Self {
        let mut worker = Self::new(name, state);

        // Take an initial lock on the state, and spawn a task to do our early start run.
        //
        // We capture our shutdown handle to allow the worker to trigger handoff when it is added to a supervision tree
        // and spawned.
        let guard = Arc::clone(&worker.state)
            .try_lock_owned()
            .expect("a new mutex should be unlocked");
        let (handoff, stop) = ShutdownHandle::paired();
        let handed_off = stop.view();
        let worker_name = worker.name.clone();
        let task = spawn_traced_named(
            worker_name.clone(),
            run_resumable_early(worker_name, stop, handed_off, guard),
        );

        *worker.early.get_mut().unwrap_or_else(|e| e.into_inner()) = Some(EarlyStart {
            handoff: Some(handoff),
            task,
        });
        worker
    }
}

#[async_trait]
impl<L: Resumable> Supervisable for ResumableWorker<L> {
    fn name(&self) -> &str {
        &self.name
    }

    fn shutdown_strategy(&self) -> ShutdownStrategy {
        self.shutdown_strategy
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let early = self.early.lock().unwrap_or_else(|e| e.into_inner()).take();
        let state = Arc::clone(&self.state);

        Ok(Box::pin(async move {
            // If this is the first time the worker is running after an early start, signal the task to handoff to us
            // and wait until the early task returns.
            if let Some(early) = early {
                if let Some(result) = early.hand_off().await {
                    return result;
                }
            }

            // Lock the worker state and run like normal.
            let mut state = state.lock_owned().await;
            state.run(process_shutdown).await
        }))
    }
}

impl<L> fmt::Debug for ResumableWorker<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResumableWorker")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Early start task state.
struct EarlyStart {
    handoff: Option<ShutdownCoordinator>,
    task: JoinHandle<HandoffOutcome>,
}

/// Outcome of the early start task handoff.
enum HandoffOutcome {
    /// Task successfully stopped and the state can be handed off to the worker.
    Ready,

    /// Task completed prematurely before handoff was requested.
    EndedPrematurely(Result<(), GenericError>),

    /// Task panicked before handoff was requested.
    Panicked(Box<dyn Any + Send>),
}

impl EarlyStart {
    /// Tells the task to stop and waits for it, returning the result of the work if it ended by itself.
    async fn hand_off(mut self) -> Option<Result<(), GenericError>> {
        if let Some(handoff) = self.handoff.take() {
            handoff.shutdown();
        }

        match (&mut self.task).await {
            Ok(HandoffOutcome::Ready) => None,
            Ok(HandoffOutcome::EndedPrematurely(result)) => Some(result),
            Ok(HandoffOutcome::Panicked(panic)) => resume_unwind(panic),
            Err(e) => Some(Err(generic_error!(
                "Work was cancelled before its worker started: {}",
                e
            ))),
        }
    }
}

impl Drop for EarlyStart {
    fn drop(&mut self) {
        // Does nothing if the task already finished.
        self.task.abort();
    }
}

async fn run_resumable_early<L: Resumable>(
    worker_name: String, stop: ShutdownHandle, handed_off: ShutdownView, mut guard: OwnedMutexGuard<L>,
) -> HandoffOutcome {
    // Run our resumable worker directly until we're instructed to stop, taking care to
    // capture any panics that occur.
    let mut run = pin!(guard.run(stop));
    let outcome = poll_fn(|cx| match catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(cx))) {
        Ok(Poll::Ready(result)) => Poll::Ready(Ok(result)),
        Ok(Poll::Pending) => Poll::Pending,
        Err(e) => Poll::Ready(Err(e)),
    })
    .await;

    match outcome {
        Err(e) => {
            error!(%worker_name, "Task panicked before supervised worker started.");
            HandoffOutcome::Panicked(e)
        }
        Ok(Ok(())) if handed_off.is_triggered() => HandoffOutcome::Ready,
        Ok(result) => {
            if !handed_off.is_triggered() {
                match &result {
                    Ok(()) => warn!(%worker_name, "Task ended before supervised worker started."),
                    Err(e) => {
                        warn!(%worker_name, error = %e, "Task failed before supervised worker started.")
                    }
                }
            }
            HandoffOutcome::EndedPrematurely(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use saluki_common::sync::shutdown::ShutdownView;
    use tokio::{
        select,
        sync::{Notify, Semaphore},
    };

    use super::*;
    use crate::{
        runtime::{
            state::DataspaceRegistry, test_util::TestSupervisor, NodeSnapshot, ProcessId, RestartStrategy, Supervisor,
        },
        test_support::wait_until,
    };

    /// What the probe recorded for one completed step, or for a stop.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Entry {
        Step {
            n: u64,
            in_process: bool,
            has_dataspace: bool,
        },
        Stopped {
            in_process: bool,
        },
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Knob {
        Panic,
        Fail,
        End,
    }

    /// The parts of a probe that the test keeps.
    #[derive(Clone)]
    struct Controls {
        journal: Arc<SyncMutex<Vec<Entry>>>,
        begin: Arc<Semaphore>,
        finish: Arc<Semaphore>,
        stepping: Arc<AtomicBool>,
        stops: Arc<SyncMutex<Vec<ShutdownView>>>,
    }

    impl Controls {
        /// Lets `n` more steps begin and finish.
        fn release(&self, n: usize) {
            self.begin.add_permits(n);
            self.finish.add_permits(n);
        }

        fn journal(&self) -> Vec<Entry> {
            self.journal.lock().unwrap().clone()
        }

        fn steps(&self) -> Vec<(u64, bool)> {
            self.journal()
                .into_iter()
                .filter_map(|entry| match entry {
                    Entry::Step { n, in_process, .. } => Some((n, in_process)),
                    Entry::Stopped { .. } => None,
                })
                .collect()
        }

        fn is_stepping(&self) -> bool {
            self.stepping.load(Ordering::SeqCst)
        }

        /// Returns `true` once the stop signal of the first call to `run` fired.
        fn handoff_fired(&self) -> bool {
            self.stops
                .lock()
                .unwrap()
                .first()
                .is_some_and(ShutdownView::is_triggered)
        }

        async fn wait_for_steps(&self, count: usize) {
            wait_until(&format!("{count} steps finished"), || self.steps().len() == count).await;
        }

        async fn wait_for_handoff(&self) {
            wait_until("the handoff fired", || self.handoff_fired()).await;
        }
    }

    /// Clears the flag that marks a step in progress when the step finishes, or when the call is dropped.
    struct SteppingGuard(Arc<AtomicBool>);

    impl SteppingGuard {
        fn new(flag: &Arc<AtomicBool>) -> Self {
            flag.store(true, Ordering::SeqCst);
            Self(Arc::clone(flag))
        }
    }

    impl Drop for SteppingGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }

    /// Work that runs numbered steps.
    ///
    /// A step begins once `begin` gives a permit, racing `stop`, and finishes once `finish` gives one, without racing
    /// `stop`. A step that began is retried if a call is dropped before it finishes.
    struct Probe {
        controls: Controls,
        strategy: ShutdownStrategy,
        done: u64,
        in_flight: bool,
        knob: Option<(u64, Knob)>,
    }

    impl Probe {
        fn new(strategy: ShutdownStrategy) -> (Self, Controls) {
            let controls = Controls {
                journal: Arc::default(),
                begin: Arc::new(Semaphore::new(0)),
                finish: Arc::new(Semaphore::new(0)),
                stepping: Arc::default(),
                stops: Arc::default(),
            };
            let probe = Self {
                controls: controls.clone(),
                strategy,
                done: 0,
                in_flight: false,
                knob: None,
            };
            (probe, controls)
        }

        fn graceful() -> (Self, Controls) {
            Self::new(ShutdownStrategy::Graceful(DEFAULT_SHUTDOWN_TIMEOUT))
        }

        fn brutal() -> (Self, Controls) {
            Self::new(ShutdownStrategy::Brutal)
        }

        /// Acts on `knob` once, right after step `n` finishes.
        fn with_knob(mut self, n: u64, knob: Knob) -> Self {
            self.knob = Some((n, knob));
            self
        }

        fn record(&self, entry: Entry) {
            self.controls.journal.lock().unwrap().push(entry);
        }
    }

    fn in_process() -> bool {
        ProcessId::current() != ProcessId::ROOT
    }

    impl Resumable for Probe {
        fn shutdown_strategy(&self) -> ShutdownStrategy {
            self.strategy
        }

        async fn run(&mut self, stop: ShutdownHandle) -> Result<(), GenericError> {
            self.controls.stops.lock().unwrap().push(stop.view());
            let mut stop = pin!(stop);

            loop {
                if !self.in_flight {
                    select! {
                        biased;
                        _ = &mut stop => {
                            self.record(Entry::Stopped { in_process: in_process() });
                            return Ok(());
                        }
                        permit = self.controls.begin.acquire() => permit.expect("gate stays open").forget(),
                    }
                    self.in_flight = true;
                }

                let stepping = SteppingGuard::new(&self.controls.stepping);
                self.controls.finish.acquire().await.expect("gate stays open").forget();
                drop(stepping);

                self.done += 1;
                self.in_flight = false;
                self.record(Entry::Step {
                    n: self.done,
                    in_process: in_process(),
                    has_dataspace: DataspaceRegistry::try_current().is_some(),
                });

                match self.knob {
                    Some((n, knob)) if n == self.done => {
                        self.knob = None;
                        match knob {
                            Knob::Panic => panic!("probe panicked after step {n}"),
                            Knob::Fail => return Err(generic_error!("probe failed after step {}", n)),
                            Knob::End => return Ok(()),
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// A restartable sibling that fails each time it is tripped.
    struct Tripwire(Arc<Notify>);

    #[async_trait]
    impl Supervisable for Tripwire {
        fn name(&self) -> &str {
            "tripwire"
        }

        async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
            let trip = Arc::clone(&self.0);
            Ok(Box::pin(async move {
                select! {
                    _ = process_shutdown => Ok(()),
                    _ = trip.notified() => Err(generic_error!("tripped")),
                }
            }))
        }
    }

    fn find<'a>(node: &'a NodeSnapshot, name: &str) -> Option<&'a NodeSnapshot> {
        if node.name == name {
            return Some(node);
        }
        node.children.iter().find_map(|child| find(child, name))
    }

    fn restart_count(supervisor: &TestSupervisor, name: &str) -> u32 {
        let snapshot = supervisor.tree_handle().snapshot();
        find(&snapshot.root, name).expect("node should exist").restart_count
    }

    async fn start(worker: ResumableWorker<Probe>) -> TestSupervisor {
        TestSupervisor::start_with("test", |supervisor| supervisor.add_worker(worker)).await
    }

    async fn wait_for_release(state: &Arc<Mutex<Probe>>) {
        wait_until("the state is released", || state.try_lock().is_ok()).await;
    }

    #[tokio::test]
    async fn work_started_early_runs_outside_any_process() {
        let (probe, controls) = Probe::graceful();
        let _worker = ResumableWorker::start_early("probe", probe);

        controls.release(2);
        controls.wait_for_steps(2).await;

        assert_eq!(
            controls.journal(),
            vec![
                Entry::Step {
                    n: 1,
                    in_process: false,
                    has_dataspace: false
                },
                Entry::Step {
                    n: 2,
                    in_process: false,
                    has_dataspace: false
                },
            ]
        );
    }

    #[tokio::test]
    async fn the_worker_continues_the_work_from_the_state_the_task_left() {
        let (probe, controls) = Probe::graceful();
        let worker = ResumableWorker::start_early("probe", probe);
        controls.release(2);
        controls.wait_for_steps(2).await;

        let supervisor = start(worker).await;
        controls.wait_for_handoff().await;
        controls.release(1);
        controls.wait_for_steps(3).await;

        assert_eq!(
            controls.journal()[2..],
            [
                Entry::Stopped { in_process: false },
                Entry::Step {
                    n: 3,
                    in_process: true,
                    has_dataspace: true
                },
            ]
        );
        supervisor.shutdown().await.expect("shutdown should be clean");
    }

    #[tokio::test]
    async fn the_handoff_waits_for_a_step_in_flight_and_does_not_repeat_it() {
        let (probe, controls) = Probe::graceful();
        let worker = ResumableWorker::start_early("probe", probe);
        controls.begin.add_permits(1);
        wait_until("step 1 began", || controls.is_stepping()).await;

        let supervisor = start(worker).await;
        controls.wait_for_handoff().await;
        controls.finish.add_permits(1);
        controls.wait_for_steps(1).await;
        controls.release(1);
        controls.wait_for_steps(2).await;

        assert_eq!(controls.steps(), vec![(1, false), (2, true)]);
        supervisor.shutdown().await.expect("shutdown should be clean");
    }

    #[tokio::test]
    async fn a_restart_resumes_the_work_and_retries_an_interrupted_step() {
        let (probe, controls) = Probe::brutal();
        let worker = ResumableWorker::new("probe", probe);
        let trip = Arc::new(Notify::new());
        let mut inner = Supervisor::new("inner")
            .unwrap()
            .with_restart_strategy(RestartStrategy::one_for_all());
        inner.add_worker(worker);
        inner.add_worker(Tripwire(Arc::clone(&trip)));
        let supervisor = TestSupervisor::start_with("test", |supervisor| supervisor.add_worker(inner)).await;

        controls.release(1);
        controls.wait_for_steps(1).await;

        // Step 2 is halted while it is in flight, when the failing sibling restarts the probe.
        controls.begin.add_permits(1);
        wait_until("step 2 began", || controls.is_stepping()).await;
        trip.notify_one();
        wait_until("the probe was restarted", || controls.stops.lock().unwrap().len() == 2).await;
        controls.finish.add_permits(1);
        controls.wait_for_steps(2).await;

        assert_eq!(controls.steps(), vec![(1, true), (2, true)]);
        supervisor.shutdown().await.expect("shutdown should be clean");
    }

    #[tokio::test]
    async fn halting_the_first_instance_during_the_handoff_cancels_the_task() {
        let (probe, controls) = Probe::brutal();
        let worker = ResumableWorker::start_early("probe", probe);
        let state = Arc::clone(&worker.state);
        controls.begin.add_permits(1);
        wait_until("step 1 began", || controls.is_stepping()).await;

        let supervisor = start(worker).await;
        controls.wait_for_handoff().await;
        supervisor.shutdown().await.expect("a brutal halt is a clean shutdown");

        wait_until("the step is cancelled", || !controls.is_stepping()).await;
        wait_for_release(&state).await;
        assert!(controls.steps().is_empty());
    }

    #[tokio::test]
    async fn dropping_the_first_instance_during_the_handoff_cancels_the_task() {
        let (probe, controls) = Probe::graceful();
        let worker = ResumableWorker::start_early("probe", probe);
        let state = Arc::clone(&worker.state);
        controls.begin.add_permits(1);
        wait_until("step 1 began", || controls.is_stepping()).await;

        let (_coordinator, shutdown) = ShutdownHandle::paired();
        let run = worker.initialize(shutdown).await.expect("initialize never fails");
        let instance = tokio::spawn(run);
        controls.wait_for_handoff().await;
        instance.abort();

        wait_until("the step is cancelled", || !controls.is_stepping()).await;
        wait_for_release(&state).await;
        assert!(controls.steps().is_empty());
    }

    #[tokio::test]
    async fn dropping_a_worker_that_never_started_cancels_the_task() {
        let (probe, controls) = Probe::graceful();
        let worker = ResumableWorker::start_early("probe", probe);
        let state = Arc::clone(&worker.state);
        controls.begin.add_permits(1);
        wait_until("step 1 began", || controls.is_stepping()).await;

        drop(worker);

        // A task that was only signalled would still wait for `finish`.
        wait_until("the step is cancelled", || !controls.is_stepping()).await;
        wait_for_release(&state).await;
        assert!(controls.steps().is_empty());
    }

    #[tokio::test]
    async fn a_panic_before_the_handoff_fails_the_first_instance_and_the_restart_resumes() {
        let (probe, controls) = Probe::graceful();
        let worker = ResumableWorker::start_early("probe", probe.with_knob(1, Knob::Panic));
        controls.release(1);
        controls.wait_for_steps(1).await;

        let supervisor = start(worker).await;
        wait_until("the probe was restarted", || restart_count(&supervisor, "probe") == 1).await;
        controls.release(1);
        controls.wait_for_steps(2).await;

        assert_eq!(controls.steps(), vec![(1, false), (2, true)]);
        supervisor.shutdown().await.expect("shutdown should be clean");
    }

    async fn assert_early_end_becomes_the_first_result(knob: Knob) {
        let (probe, controls) = Probe::graceful();
        let worker = ResumableWorker::start_early("probe", probe.with_knob(1, knob));
        controls.release(1);
        controls.wait_for_steps(1).await;

        let supervisor = start(worker).await;
        wait_until("the probe was restarted", || restart_count(&supervisor, "probe") == 1).await;
        controls.release(1);
        controls.wait_for_steps(2).await;

        // The first instance ended with the early result without running the work, so only the restart called `run`.
        assert_eq!(controls.steps(), vec![(1, false), (2, true)]);
        assert_eq!(controls.stops.lock().unwrap().len(), 2);
        supervisor.shutdown().await.expect("shutdown should be clean");
    }

    #[tokio::test]
    async fn work_that_ends_before_the_handoff_ends_the_first_instance() {
        assert_early_end_becomes_the_first_result(Knob::End).await;
    }

    #[tokio::test]
    async fn work_that_fails_before_the_handoff_fails_the_first_instance() {
        assert_early_end_becomes_the_first_result(Knob::Fail).await;
    }

    #[tokio::test]
    async fn a_stop_during_the_handoff_waits_for_the_step_then_stops() {
        let (probe, controls) = Probe::graceful();
        let worker = ResumableWorker::start_early("probe", probe);
        let state = Arc::clone(&worker.state);
        controls.begin.add_permits(1);
        wait_until("step 1 began", || controls.is_stepping()).await;

        let mut supervisor = start(worker).await;
        controls.wait_for_handoff().await;
        supervisor.signal_shutdown();
        controls.finish.add_permits(1);
        supervisor.wait().await.expect("shutdown should be clean");

        // The instance returned only after the task ended, so the state is already free.
        assert!(state.try_lock().is_ok());
        assert_eq!(
            controls.journal(),
            vec![
                Entry::Step {
                    n: 1,
                    in_process: false,
                    has_dataspace: false
                },
                Entry::Stopped { in_process: false },
                Entry::Stopped { in_process: true },
            ]
        );
    }

    #[tokio::test]
    async fn a_graceful_worker_gives_the_work_its_stop_signal() {
        let (probe, controls) = Probe::graceful();
        let supervisor = start(ResumableWorker::new("probe", probe)).await;
        controls.release(1);
        controls.wait_for_steps(1).await;

        supervisor.shutdown().await.expect("shutdown should be clean");

        assert_eq!(controls.journal()[1], Entry::Stopped { in_process: true });
    }

    #[tokio::test]
    async fn the_worker_declares_the_strategy_of_its_work() {
        let (probe, controls) = Probe::brutal();
        let worker = ResumableWorker::start_early("probe", probe);
        controls.begin.add_permits(1);
        wait_until("step 1 began", || controls.is_stepping()).await;

        // The task holds the state while the step is in flight.
        assert!(matches!(worker.shutdown_strategy(), ShutdownStrategy::Brutal));
        assert!(matches!(
            ResumableWorker::new("probe", Probe::graceful().0).shutdown_strategy(),
            ShutdownStrategy::Graceful(timeout) if timeout == DEFAULT_SHUTDOWN_TIMEOUT
        ));

        controls.finish.add_permits(1);
        let supervisor = start(worker).await;
        controls.wait_for_handoff().await;
        controls.release(1);
        controls.wait_for_steps(2).await;
        supervisor.shutdown().await.expect("a brutal halt is a clean shutdown");

        // Only the task saw a stop signal: the worker was halted without one.
        assert_eq!(
            controls
                .journal()
                .into_iter()
                .filter(|entry| matches!(entry, Entry::Stopped { .. }))
                .collect::<Vec<_>>(),
            vec![Entry::Stopped { in_process: false }]
        );
    }

    #[tokio::test]
    async fn shutdown_under_a_supervisor_releases_the_state() {
        let (probe, controls) = Probe::graceful();
        let worker = ResumableWorker::start_early("probe", probe);
        let state = Arc::clone(&worker.state);
        let supervisor = start(worker).await;
        controls.wait_for_handoff().await;
        controls.release(1);
        controls.wait_for_steps(1).await;

        supervisor.shutdown().await.expect("shutdown should be clean");

        assert!(state.try_lock().is_ok());
    }

    #[tokio::test]
    async fn a_failed_instance_is_restarted_and_resumes() {
        let (probe, controls) = Probe::graceful();
        let supervisor = start(ResumableWorker::new("probe", probe.with_knob(1, Knob::Fail))).await;
        controls.release(1);
        wait_until("the probe was restarted", || restart_count(&supervisor, "probe") == 1).await;
        controls.release(1);
        controls.wait_for_steps(2).await;

        assert_eq!(controls.steps(), vec![(1, true), (2, true)]);
        supervisor.shutdown().await.expect("shutdown should be clean");
    }

    #[tokio::test]
    async fn an_instance_that_returns_is_restarted_and_resumes() {
        let (probe, controls) = Probe::graceful();
        let supervisor = start(ResumableWorker::new("probe", probe.with_knob(1, Knob::End))).await;
        controls.release(1);
        wait_until("the probe was restarted", || restart_count(&supervisor, "probe") == 1).await;
        controls.release(1);
        controls.wait_for_steps(2).await;

        assert_eq!(controls.steps(), vec![(1, true), (2, true)]);
        supervisor.shutdown().await.expect("shutdown should be clean");
    }
}
