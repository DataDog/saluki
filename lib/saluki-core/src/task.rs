//! Helpers for working with asynchronous tasks.

use std::future::Future;

use memory_accounting::allocator::Track as _;
use tokio::{runtime::Handle, task::{AbortHandle, JoinHandle, JoinSet}};
use tracing::Instrument as _;

/// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
///
/// This function is a thin wrapper over [`tokio::spawn`], and provides implicit "tracing" for spawned futures by
/// ensuring that the task is attached to the current `tracing` span and the current allocation component.
pub fn spawn_traced<F, T>(f: F) -> JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(f.in_current_span().in_current_allocation_group())
}

/// Helper trait for providing traced spawning when using `JoinSet<T>`.
pub trait JoinSetExt<T> {
    /// Spawns a new asynchronous task, returning an [`AbortHandle`] for it.
    ///
    /// This is meant to be a thin wrapper over task management types like [`JoinSet`][tokio::task::JoinSet], and
    /// provides implicit "tracing" for spawned futures by ensuring that the task is attached to the current `tracing`
    /// span and the current allocation component.
    fn spawn_traced<F>(&mut self, f: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static;
}

impl<T> JoinSetExt<T> for JoinSet<T> {
    fn spawn_traced<F>(&mut self, f: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn(f.in_current_span().in_current_allocation_group())
    }
}

/// Helper trait for providing traced spawning when using `Handle`.
pub trait HandleExt<T> {
    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// This is meant to be a thin wrapper over task management types like [`Handle`][tokio::runtime::Handle], and
    /// provides implicit "tracing" for spawned futures by ensuring that the task is attached to the current `tracing`
    /// span and the current allocation component.
    fn spawn_traced<F>(&self, f: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static;
}

impl<T> HandleExt<T> for Handle {
    fn spawn_traced<F>(&self, f: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.spawn(f.in_current_span().in_current_allocation_group())
    }
}
