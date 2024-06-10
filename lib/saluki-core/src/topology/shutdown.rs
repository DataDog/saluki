//! Helpers for signaling the controlled shutdown of tasks.
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{ready, Context, Poll},
};

use slab::Slab;
use tokio::sync::{
    oneshot::{channel, error::TryRecvError, Receiver, Sender},
    Notify,
};

/// A component-specific shutdown coordinator.
///
/// This coordinator is designed for use by the topology to signal components to shutdown. Once a handle is registerd,
/// it can not be unregistered. If you required the ability to unregister handles, consider using [`DynamicShutdownCoordinator`].
#[derive(Default)]
pub struct ComponentShutdownCoordinator {
    handles: Vec<Sender<()>>,
}

/// A component shutdown handle.
pub struct ComponentShutdownHandle {
    receiver: Receiver<()>,
}

impl ComponentShutdownCoordinator {
    /// Registers a shutdown handle.
    pub fn register(&mut self) -> ComponentShutdownHandle {
        let (sender, receiver) = channel();
        self.handles.push(sender);

        ComponentShutdownHandle { receiver }
    }

    /// Triggers shutdown and notifies all registered handles.
    pub fn shutdown(self) {
        for handle in self.handles {
            let _ = handle.send(());
        }
    }
}

impl Future for ComponentShutdownHandle {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `Receiver` is `Unpin`, so we can safely project it.
        let receiver = unsafe { self.map_unchecked_mut(|s| &mut s.receiver) };
        receiver.poll(cx).map(|_| ())
    }
}

#[derive(Default)]
struct State {
    waiters: Mutex<Slab<Sender<()>>>,
    outstanding_handles: AtomicUsize,
    shutdown_complete: Notify,
}

/// A shutdown coordinator that can dynamically register handles for shutdown.
#[derive(Default)]
pub struct DynamicShutdownCoordinator {
    state: Arc<State>,
}

/// A dynamic shutdown handle.
///
/// This handle is a `Future` which resolves when the shutdown coordinator has triggered shutdown. Additionally, it is
/// used to signal to the coordinator, on drop, that the user of the handle has completed shutdown.
pub struct DynamicShutdownHandle {
    state: Arc<State>,
    wait_tx_idx: usize,
    wait_rx: Receiver<()>,
    notified: bool,
}

impl DynamicShutdownCoordinator {
    /// Registers a shutdown handle.
    pub fn register(&mut self) -> DynamicShutdownHandle {
        let (wait_tx, wait_rx) = channel();
        let mut waiters = self.state.waiters.lock().unwrap();
        let wait_tx_idx = waiters.insert(wait_tx);
        self.state.outstanding_handles.fetch_add(1, Ordering::Release);

        DynamicShutdownHandle {
            state: Arc::clone(&self.state),
            wait_tx_idx,
            wait_rx,
            notified: false,
        }
    }

    /// Triggers shutdown and notifies all outstanding handles, waiting until all handles have been dropped.
    ///
    /// If there are any outstanding handles, they are signaled to shutdown and this function will only return once all
    /// outstanding handles have been dropped. If there are no outstanding handles, the function returns immediately.
    pub async fn shutdown(self) {
        // Register ourselves for the shutdown notification here, which ensures that if we do have outstanding handles
        // which are only shutdown when we signal them below, we'll be properly notified when they're all dropped.
        let shutdown_complete = self.state.shutdown_complete.notified();

        {
            let mut waiters = self.state.waiters.lock().unwrap();
            if waiters.is_empty() {
                return;
            }

            for waiter in waiters.drain() {
                let _ = waiter.send(());
            }
        }

        shutdown_complete.await;
    }
}

impl Drop for DynamicShutdownHandle {
    fn drop(&mut self) {
        // If we haven't definitively been notified yet (possibly due simply to not yet being polled), then we do a
        // fallible receive here. We only remove the wait sender if it was never used/dropped.
        if !self.notified {
            let mut waiters = self.state.waiters.lock().unwrap();

            if let Err(TryRecvError::Empty) = self.wait_rx.try_recv() {
                waiters.remove(self.wait_tx_idx);
            }
        }

        if self.state.outstanding_handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            // We're the last handle currently registered to this coordinator, so notify the coordinator.
            //
            // Crucially, we use `notify_waiters` because we don't want to store a wakeup: we may be the last handle,
            // but shutdown may not have been triggered yet. This ensures that the coordinator can properly distinguish
            // when all handles have dropped during actual shutdown.
            self.state.shutdown_complete.notify_waiters();
        }
    }
}

impl Future for DynamicShutdownHandle {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.notified {
            return Poll::Ready(());
        }

        let this = self.get_mut();

        // SAFETY: `Receiver` is `Unpin`, so we can safely project it.
        let _ = ready!(Pin::new(&mut this.wait_rx).poll(cx));

        this.notified = true;
        Poll::Ready(())
    }
}
