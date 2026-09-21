//! Contract for supervised processes.
//!
//! This module holds the *contract* a supervised process implements -- [`Supervisable`] and the types in its
//! signature -- while the machinery that drives it (supervisors, restart strategies, the supervision tree) lives in
//! `saluki-core`. The split mirrors `http`/`hyper`: the contract is a leaf that anything can implement, so it sits low
//! enough for crates beneath `saluki-core` to describe background work without depending on the engine that runs it.
//!
//! Most code should reach for these through `saluki_core::runtime`, which re-exports them alongside the supervisor
//! itself.

use std::{future::Future, pin::Pin, time::Duration};

use async_trait::async_trait;
use saluki_error::GenericError;
use snafu::Snafu;

use crate::sync::shutdown::ShutdownHandle;

/// A `Future` that represents the execution of a supervised process.
pub type SupervisorFuture = Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>;

/// Initialization errors.
///
/// Initialization errors are distinct from runtime errors: they indicate that a process couldn't be started at all
/// (for example, failed to bind a port, missing configuration). These errors don't trigger restart logic; instead, they
/// immediately propagate up and fail the supervisor.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum InitializationError {
    /// The process couldn't be initialized due to an error.
    #[snafu(display("Process failed to initialize: {}", source))]
    Failed {
        /// The underlying error that caused initialization to fail.
        source: GenericError,
    },
}

impl From<GenericError> for InitializationError {
    fn from(source: GenericError) -> Self {
        Self::Failed { source }
    }
}

/// Strategy for shutting down a process.
#[derive(Clone, Copy, Debug)]
pub enum ShutdownStrategy {
    /// Waits for the configured duration for the process to exit, and then forcefully aborts it otherwise.
    Graceful(Duration),

    /// Forcefully aborts the process without waiting.
    Brutal,
}

/// A supervisable process.
#[async_trait]
pub trait Supervisable: Send + Sync {
    /// Returns the name of the process.
    fn name(&self) -> &str;

    /// Returns the shutdown strategy for the process.
    fn shutdown_strategy(&self) -> ShutdownStrategy {
        ShutdownStrategy::Graceful(Duration::from_secs(5))
    }

    /// Returns whether this process observes the shutdown signal it is given.
    ///
    /// Shutting a subtree down is a _trigger_, not an enforcement: many workers ignore the signal entirely and stop
    /// only when they reach their own terminal condition, such as an input channel closing. Reporting `false` lets the
    /// supervisor skip creating a shutdown coordinator it would never usefully fire, and hand the process a
    /// [`ShutdownHandle::noop`] instead.
    ///
    /// This says nothing about _whether_ the supervisor waits for the process -- that's
    /// [`shutdown_strategy`][Self::shutdown_strategy]. A process that ignores the signal is still waited for, up to
    /// whatever deadline applies to it.
    ///
    /// Defaults to `true`.
    fn wants_shutdown_signal(&self) -> bool {
        true
    }

    /// Initializes the process asynchronously.
    ///
    /// During initialization, any resources or configuration for the process can be created asynchronously, and the
    /// same runtime that's used for running the process is used for initialization. The resulting future is expected to
    /// complete as soon as reasonably possible after `shutdown` resolves.
    ///
    /// **Important:** The `process_shutdown` signal must be moved into the returned [`SupervisorFuture`] so the worker
    /// can respond to supervisor-initiated shutdown. If `process_shutdown` is dropped during initialization, the worker
    /// will be unable to shut down gracefully and will be forcefully aborted after the shutdown timeout.
    ///
    /// # Errors
    ///
    /// If the process can't be initialized, an error is returned.
    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest possible implementation, existing only to be erased below.
    struct Noop;

    #[async_trait]
    impl Supervisable for Noop {
        fn name(&self) -> &str {
            "noop"
        }

        async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
            Ok(Box::pin(async move {
                process_shutdown.await;
                Ok(())
            }))
        }
    }

    #[test]
    fn trait_is_object_safe() {
        // Producers hand their background work over as `Vec<Box<dyn Supervisable>>`, so object safety is load-bearing
        // here rather than incidental: a default method taking `self` by value, or a generic one, would break every
        // one of those call sites.
        let worker: Box<dyn Supervisable> = Box::new(Noop);

        assert_eq!(worker.name(), "noop");
        assert!(worker.wants_shutdown_signal());
        assert!(matches!(
            worker.shutdown_strategy(),
            ShutdownStrategy::Graceful(timeout) if timeout == Duration::from_secs(5)
        ));
    }
}
