//! Closure-style supervisable workers.
use std::{future::Future, sync::Mutex, time::Duration};

use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::{generic_error, GenericError};
use tracing::debug;

use super::supervisor::{InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture};

/// Default graceful shutdown period for a function-based worker.
///
/// Matches the [`Supervisable`] trait default, and applies only when nothing else sets a strategy for the child.
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// An output type that a function-based worker can produce.
///
/// Implemented for `()` (a worker that can't fail) and `Result<(), GenericError>` (one that can), so both forms can be
/// passed to [`noninterruptible_worker`] and [`interruptible_worker`] without wrapping.
pub trait IntoWorkerResult {
    /// Converts this output into a worker result.
    fn into_worker_result(self) -> Result<(), GenericError>;
}

impl IntoWorkerResult for () {
    fn into_worker_result(self) -> Result<(), GenericError> {
        Ok(())
    }
}

impl IntoWorkerResult for Result<(), GenericError> {
    fn into_worker_result(self) -> Result<(), GenericError> {
        self
    }
}

type WorkerBody = Box<dyn FnOnce(ShutdownHandle) -> SupervisorFuture + Send>;

/// A [`Supervisable`] worker built from a closure.
///
/// This worker cannot be restarted as the closure is consumed during initialization.
pub struct FnWorker {
    name: String,
    shutdown_strategy: ShutdownStrategy,
    body: Mutex<Option<WorkerBody>>,
}

impl FnWorker {
    fn new(name: String, body: WorkerBody) -> Self {
        Self {
            name,
            shutdown_strategy: ShutdownStrategy::Graceful(DEFAULT_SHUTDOWN_TIMEOUT),
            body: Mutex::new(Some(body)),
        }
    }

    /// Sets the shutdown timeout for this worker.
    #[must_use]
    pub const fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_strategy = ShutdownStrategy::Graceful(timeout);
        self
    }
}

#[async_trait]
impl Supervisable for FnWorker {
    fn name(&self) -> &str {
        &self.name
    }

    fn shutdown_strategy(&self) -> ShutdownStrategy {
        self.shutdown_strategy
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let body = self
            .body
            .lock()
            .expect("function worker mutex poisoned")
            .take()
            .ok_or_else(|| InitializationError::from(generic_error!("worker already initialized")))?;

        Ok(body(process_shutdown))
    }
}

/// Creates a one-shot [`Supervisable`] worker that is never interrupted mid-operation.
///
/// The future that is created is solely responsible for handling the shutdown signal exposed to it via
/// `ShutdownHandle`. Callers should ensure that they respond to shutdown signals in a timely manner otherwise they risk
/// failing to run to completion when the supervisor forcefully exits. Use this for workers that need to drain in
/// response to shutdown; for workers that are safe to stop at an arbitrary await point, [`interruptible_worker`] is
/// simpler.
///
/// Workers may return either `()` or `Result<(), GenericError>`.
///
/// The worker cannot be restarted as the given closure is consumed during initialization.
///
/// # Examples
///
/// ```no_run
/// # use saluki_core::runtime::noninterruptible_worker;
/// # async fn drain_queue() {}
/// let worker = noninterruptible_worker("queue_drainer", |shutdown| async move {
///     tokio::select! {
///         _ = shutdown => {},
///         _ = drain_queue() => {},
///     }
/// });
/// ```
#[must_use]
pub fn noninterruptible_worker<N, F, Fut>(name: N, f: F) -> FnWorker
where
    N: Into<String>,
    F: FnOnce(ShutdownHandle) -> Fut + Send + 'static,
    Fut: Future + Send + 'static,
    Fut::Output: IntoWorkerResult,
{
    FnWorker::new(
        name.into(),
        Box::new(move |shutdown| Box::pin(async move { f(shutdown).await.into_worker_result() })),
    )
}

/// Creates a one-shot [`Supervisable`] worker that runs `fut` until it completes or shutdown is signalled, whichever
/// happens first.
///
/// The future that is given is subsequently wrapped such that shutdown is always handled: the underlying worker cannot
/// ignore or defer honoring it. The future is dropped at whatever await point it happens to be parked on when shutdown
/// fires, so use this only for work that is safe to interrupt: a server accept loop, a background refresher, a
/// connection handler. Anything that must finish what it started should use [`noninterruptible_worker`] and observe the
/// shutdown signal itself.
///
/// Workers may return either `()` or `Result<(), GenericError>`.
///
/// The worker cannot be restarted as the given future is consumed during initialization.
///
/// # Examples
///
/// ```no_run
/// # use saluki_core::runtime::interruptible_worker;
/// # async fn run_accept_loop() {}
/// let worker = interruptible_worker("acceptor", run_accept_loop());
/// ```
#[must_use]
pub fn interruptible_worker<N, Fut>(name: N, fut: Fut) -> FnWorker
where
    N: Into<String>,
    Fut: Future + Send + 'static,
    Fut::Output: IntoWorkerResult,
{
    let name = name.into();
    FnWorker::new(
        name.clone(),
        Box::new(move |shutdown| {
            Box::pin(async move {
                tokio::select! {
                    _ = shutdown => {
                        debug!(worker_name = %name, "Worker interrupted by shutdown signal.");
                        Ok(())
                    },
                    output = fut => output.into_worker_result(),
                }
            })
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use saluki_common::sync::shutdown::ShutdownCoordinator;
    use tokio::time::timeout;

    use super::*;

    /// Bound on any worker-body await in these tests.
    ///
    /// A worker that stops observing shutdown would otherwise hang the test process until the harness kills it, which
    /// reads as a stall rather than a failure.
    const RUN_TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn noninterruptible_worker_receives_shutdown_signal() {
        // A non-interruptible worker is handed the shutdown signal and is expected to observe it and return.
        let observed = Arc::new(AtomicUsize::new(0));
        let worker_observed = Arc::clone(&observed);

        let worker = noninterruptible_worker("test", move |shutdown| async move {
            shutdown.await;
            worker_observed.fetch_add(1, Ordering::SeqCst);
        });

        let mut coordinator = ShutdownCoordinator::default();
        let handle = coordinator.register();
        let run = worker.initialize(handle).await.expect("should initialize");

        coordinator.shutdown();
        timeout(RUN_TIMEOUT, run)
            .await
            .expect("worker should observe shutdown and exit")
            .expect("should exit cleanly");

        assert_eq!(observed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn noninterruptible_worker_propagates_error() {
        let worker = noninterruptible_worker("test", |_shutdown| async move {
            Err::<(), _>(generic_error!("worker failed"))
        });

        let run = worker
            .initialize(ShutdownHandle::noop())
            .await
            .expect("should initialize");

        let error = run.await.expect_err("should surface the worker's error");
        assert!(error.to_string().contains("worker failed"));
    }

    #[tokio::test]
    async fn interruptible_worker_is_interrupted_at_shutdown() {
        // The future never completes on its own, so the only way out is being interrupted -- which is reported as a
        // clean exit rather than an error.
        let worker = interruptible_worker("test", std::future::pending::<()>());

        let mut coordinator = ShutdownCoordinator::default();
        let handle = coordinator.register();
        let run = worker.initialize(handle).await.expect("should initialize");

        coordinator.shutdown();
        timeout(RUN_TIMEOUT, run)
            .await
            .expect("worker should be interrupted by shutdown and exit")
            .expect("being interrupted should be reported as a clean exit");
    }

    #[tokio::test]
    async fn interruptible_worker_returns_future_output_when_it_completes_first() {
        let worker = interruptible_worker("test", async { Err::<(), _>(generic_error!("boom")) });

        let run = worker
            .initialize(ShutdownHandle::noop())
            .await
            .expect("should initialize");

        let error = run.await.expect_err("should surface the future's error");
        assert!(error.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn second_initialization_fails() {
        // Function-based workers are one-shot: a restart would re-initialize, which must fail loudly rather than
        // silently running nothing.
        let worker = noninterruptible_worker("test", |_shutdown| async {});

        // Drop the run-future without polling it; we only care that the body was consumed.
        drop(
            worker
                .initialize(ShutdownHandle::noop())
                .await
                .expect("first initialization should succeed"),
        );

        // `SupervisorFuture` isn't `Debug`, so match rather than using `expect_err`.
        match worker.initialize(ShutdownHandle::noop()).await {
            Ok(_) => panic!("second initialization should fail"),
            Err(e) => assert!(e.to_string().contains("already initialized")),
        }
    }
}
