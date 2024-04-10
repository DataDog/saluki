use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{ready, Context, Poll},
};

use slab::Slab;
use tokio::sync::oneshot::{channel, error::TryRecvError, Receiver, Sender};

#[derive(Default)]
pub struct ComponentShutdownCoordinator {
    handles: Vec<Sender<()>>,
}

pub struct ComponentShutdownHandle {
    receiver: Receiver<()>,
}

impl ComponentShutdownCoordinator {
    pub fn register(&mut self) -> ComponentShutdownHandle {
        let (sender, receiver) = channel();
        self.handles.push(sender);

        ComponentShutdownHandle { receiver }
    }

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
}

#[derive(Default)]
pub struct DynamicShutdownCoordinator {
    state: Arc<State>,
}

pub struct DynamicShutdownHandle {
    state: Arc<State>,
    wait_tx_idx: usize,
    wait_rx: Receiver<()>,
    notified: bool,
}

impl DynamicShutdownCoordinator {
    pub fn register(&mut self) -> DynamicShutdownHandle {
        let (wait_tx, wait_rx) = channel();
        let mut waiters = self.state.waiters.lock().unwrap();
        let wait_tx_idx = waiters.insert(wait_tx);

        DynamicShutdownHandle {
            state: Arc::clone(&self.state),
            wait_tx_idx,
            wait_rx,
            notified: false,
        }
    }

    pub fn shutdown(self) {
        let mut waiters = self.state.waiters.lock().unwrap();
        for waiter in waiters.drain() {
            let _ = waiter.send(());
        }
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
