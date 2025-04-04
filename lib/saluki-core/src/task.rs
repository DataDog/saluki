//! Helpers for working with asynchronous tasks.

use std::future::Future;

use memory_accounting::allocator::Track as _;
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tracing::Instrument as _;

/// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
///
/// This function is a thin wrapper over [`tokio::spawn`], and provides implicit "tracing" for spawned futures by
/// ensuring that the task is attached to the current `tracing` span and the current allocation component.
#[track_caller]
pub fn spawn_traced<F, T>(name: &str, f: F) -> JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::Builder::new()
        .name(name)
        .spawn(f.in_current_span().in_current_allocation_group())
        .expect("Failed to spawn task")
}

/// Helper trait for providing traced spawning when using `JoinSet<T>`.
pub trait JoinSetExt<T> {
    /// Spawns a new asynchronous task, returning an [`AbortHandle`] for it.
    ///
    /// This is meant to be a thin wrapper over task management types like [`JoinSet`][tokio::task::JoinSet], and
    /// provides implicit "tracing" for spawned futures by ensuring that the task is attached to the current `tracing`
    /// span and the current allocation component.
    fn spawn_traced<F>(&mut self, name: &str, f: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static;
}

impl<T> JoinSetExt<T> for JoinSet<T> {
    #[track_caller]
    fn spawn_traced<F>(&mut self, name: &str, f: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        //self.spawn(f.in_current_span().in_current_allocation_group())
        self.build_task()
            .name(name)
            .spawn(f.in_current_span().in_current_allocation_group())
            .expect("Failed to spawn task")
    }
}
