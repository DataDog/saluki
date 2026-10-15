//! Forced stops of processes, called halts.
//!
//! An owner that signals a process to stop waits only as long as the process's [shutdown
//! strategy][super::ShutdownStrategy] allows, and then _halts_ it. The runtime code wrapping the process drops its work
//! and halts its children in turn, so a shutdown timeout bounds the whole subtree below it. The process itself never
//! observes the halt. Each level counts the processes that it halted.

use std::{
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll},
};

use tokio::sync::oneshot;

/// Why a process was halted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Halt {
    /// The process, or a process above it, didn't stop within its shutdown timeout.
    ///
    /// Logged and counted towards an unclean shutdown.
    Overdue,

    /// The process, or a process above it, is [brutal][super::ShutdownStrategy::Brutal]. Expected, so not counted.
    Immediate,
}

impl Halt {
    /// Returns whether this halt counts towards an unclean shutdown.
    pub(super) fn is_counted(self) -> bool {
        matches!(self, Self::Overdue)
    }
}

/// Creates the [`Halter`] kept by an owner and the [`HaltSignal`] watched by the runtime wrapper of its process.
pub(super) fn pair() -> (Halter, HaltSignal) {
    let (tx, rx) = oneshot::channel();
    (Halter { tx }, HaltSignal { rx: Some(rx) })
}

/// Halts one process.
pub(super) struct Halter {
    tx: oneshot::Sender<Halt>,
}

impl Halter {
    /// Halts the process.
    pub(super) fn halt(self, reason: Halt) {
        // The process may have already exited.
        let _ = self.tx.send(reason);
    }
}

/// Resolves when the process is halted.
///
/// Never resolves if the [`Halter`] is dropped without halting: the owner went away, and its tasks are aborted with it.
pub(super) struct HaltSignal {
    rx: Option<oneshot::Receiver<Halt>>,
}

impl Future for HaltSignal {
    type Output = Halt;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Halt> {
        let Some(rx) = self.rx.as_mut() else {
            return Poll::Pending;
        };

        match ready!(Pin::new(rx).poll(cx)) {
            Ok(reason) => {
                self.rx = None;
                Poll::Ready(reason)
            }
            Err(_) => {
                self.rx = None;
                Poll::Pending
            }
        }
    }
}

/// Watches for the halt of an owner, and remembers it once it happens.
///
/// A process with nothing above it to halt it, such as the root of a tree, uses [`HaltWatch::never`].
pub(crate) struct HaltWatch {
    signal: Option<HaltSignal>,
    halted: Option<Halt>,
}

impl HaltWatch {
    /// Watches `signal`.
    pub(super) fn new(signal: HaltSignal) -> Self {
        Self {
            signal: Some(signal),
            halted: None,
        }
    }

    /// Watches nothing: the owner is never halted.
    pub(crate) fn never() -> Self {
        Self {
            signal: None,
            halted: None,
        }
    }

    /// Returns why the owner was halted, if it was halted.
    pub(super) fn halted(&self) -> Option<Halt> {
        self.halted
    }

    /// Polls for the halt of the owner, and resolves immediately if the owner was already halted.
    pub(super) fn poll_halted(&mut self, cx: &mut Context<'_>) -> Poll<Halt> {
        if let Some(reason) = self.halted {
            return Poll::Ready(reason);
        }

        let Some(signal) = self.signal.as_mut() else {
            return Poll::Pending;
        };
        let reason = ready!(Pin::new(signal).poll(cx));
        self.signal = None;
        self.halted = Some(reason);
        Poll::Ready(reason)
    }

    /// Waits for the halt of the owner, and resolves immediately if the owner was already halted.
    pub(super) async fn wait(&mut self) -> Halt {
        std::future::poll_fn(|cx| self.poll_halted(cx)).await
    }
}

#[cfg(test)]
mod tests {
    use std::task::Waker;

    use super::*;

    fn poll(watch: &mut HaltWatch) -> Poll<Halt> {
        watch.poll_halted(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn a_watch_resolves_with_the_reason_and_remembers_it() {
        let (halter, signal) = pair();
        let mut watch = HaltWatch::new(signal);
        assert!(poll(&mut watch).is_pending());
        assert_eq!(watch.halted(), None);

        halter.halt(Halt::Overdue);
        assert_eq!(poll(&mut watch), Poll::Ready(Halt::Overdue));
        assert_eq!(poll(&mut watch), Poll::Ready(Halt::Overdue));
        assert_eq!(watch.halted(), Some(Halt::Overdue));
    }

    #[test]
    fn a_halter_dropped_without_halting_never_halts() {
        let (halter, signal) = pair();
        let mut watch = HaltWatch::new(signal);

        drop(halter);
        assert!(poll(&mut watch).is_pending());
        assert!(poll(&mut watch).is_pending());
        assert_eq!(watch.halted(), None);
    }

    #[test]
    fn watching_nothing_never_halts() {
        let mut watch = HaltWatch::never();
        assert!(poll(&mut watch).is_pending());
        assert_eq!(watch.halted(), None);
    }
}
