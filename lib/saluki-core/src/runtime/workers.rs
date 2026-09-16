//! Closure-style supervisable workers.
use std::{future::Future, sync::Mutex, time::Duration};

use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_error::{generic_error, GenericError};

use super::supervisor::{InitializationError, ShutdownStrategy, Supervisable, SupervisorFuture};

/// Fallback graceful shutdown period for a function-based worker.
///
/// Only consulted when nothing else bounds the worker: a child spawned through the builder defers to its supervisor's
/// shutdown budget, and falls back to this when the supervisor has no budget at all.
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// An output type that a function-based worker can produce.
///
/// Implemented for `()` (a worker that can't fail) and `Result<(), GenericError>` (one that can), so both forms can be
/// passed to [`FnWorker::new`] without wrapping.
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

type WorkerBody = Box<dyn FnOnce() -> SupervisorFuture + Send>;

/// A [`Supervisable`] worker built from a plain future.
///
/// This is the ordinary kind of supervised child: a piece of asynchronous work that runs until it reaches its own
/// terminal condition -- an input channel closing, a loop finishing, a request completing.
///
/// # Shutdown
///
/// An `FnWorker` is never handed a shutdown signal, and reports as much through
/// [`wants_shutdown_signal`][Supervisable::wants_shutdown_signal]. Shutdown of a subtree is a _trigger_, not an
/// enforcement: the workers within it keep running until their terminal conditions are reached, which is what lets a
/// set of tasks connected by channels drain in dependency order without any of them having to know that order. The
/// supervisor's [shutdown budget][crate::runtime::Supervisor::with_shutdown_budget] is the backstop for work that
/// takes too long, and [`ShutdownStrategy::Brutal`] is the answer for work that has no terminal condition at all.
///
/// A worker that genuinely needs to observe shutdown -- to run cleanup, or because it has no other way to know it
/// should stop -- should implement [`Supervisable`] directly, which does receive the signal.
///
/// This worker cannot be restarted, as the future is consumed during initialization.
pub struct FnWorker {
    name: String,
    body: Mutex<Option<WorkerBody>>,
}

impl FnWorker {
    /// Creates a worker that runs `fut` to completion.
    ///
    /// Workers may return either `()` or `Result<(), GenericError>`.
    ///
    /// Prefer [`worker`][crate::runtime::worker] and its counterparts on
    /// [`SupervisorHandle`][crate::runtime::SupervisorHandle], which wrap this up with the defaults appropriate to a
    /// dynamically spawned child.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use saluki_core::runtime::FnWorker;
    /// # async fn drain_queue() {}
    /// let worker = FnWorker::new("queue_drainer", drain_queue());
    /// ```
    #[must_use]
    pub fn new<N, Fut>(name: N, fut: Fut) -> Self
    where
        N: Into<String>,
        Fut: Future + Send + 'static,
        Fut::Output: IntoWorkerResult,
    {
        Self {
            name: name.into(),
            body: Mutex::new(Some(Box::new(move || {
                Box::pin(async move { fut.await.into_worker_result() })
            }))),
        }
    }
}

#[async_trait]
impl Supervisable for FnWorker {
    fn name(&self) -> &str {
        &self.name
    }

    fn shutdown_strategy(&self) -> ShutdownStrategy {
        ShutdownStrategy::Graceful(DEFAULT_SHUTDOWN_TIMEOUT)
    }

    fn wants_shutdown_signal(&self) -> bool {
        false
    }

    async fn initialize(&self, _process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let body = self
            .body
            .lock()
            .expect("function worker mutex poisoned")
            .take()
            .ok_or_else(|| InitializationError::from(generic_error!("worker already initialized")))?;

        Ok(body())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use tokio::time::timeout;

    use super::*;

    /// Bound on any worker-body await in these tests.
    ///
    /// A worker that never finishes would otherwise hang the test process until the harness kills it, which reads as a
    /// stall rather than a failure.
    const RUN_TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn worker_runs_its_future_to_completion() {
        let ran = Arc::new(AtomicUsize::new(0));
        let worker_ran = Arc::clone(&ran);

        let worker = FnWorker::new("test", async move {
            worker_ran.fetch_add(1, Ordering::SeqCst);
        });

        let run = worker
            .initialize(ShutdownHandle::noop())
            .await
            .expect("should initialize");

        timeout(RUN_TIMEOUT, run)
            .await
            .expect("worker should run to completion")
            .expect("should exit cleanly");

        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn worker_propagates_error() {
        let worker = FnWorker::new("test", async { Err::<(), _>(generic_error!("worker failed")) });

        let run = worker
            .initialize(ShutdownHandle::noop())
            .await
            .expect("should initialize");

        let error = run.await.expect_err("should surface the worker's error");
        assert!(error.to_string().contains("worker failed"));
    }

    #[tokio::test]
    async fn worker_does_not_want_the_shutdown_signal() {
        // The supervisor uses this to skip allocating a shutdown coordinator it would never fire: an `FnWorker` runs
        // until its own terminal condition regardless of what the supervisor signals.
        assert!(!FnWorker::new("test", async {}).wants_shutdown_signal());
    }

    #[tokio::test]
    async fn second_initialization_fails() {
        // Function-based workers are one-shot: a restart would re-initialize, which must fail loudly rather than
        // silently running nothing.
        let worker = FnWorker::new("test", async {});

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
