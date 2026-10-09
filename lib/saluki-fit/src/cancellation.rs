//! Process-local cancellation for a setup attempt or an idle consumer.

use std::io;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::wait::{wait, wake, wake_all};

/// Which native wake policy a registered wait address requires. SPSC and
/// producer capacity waits wake one waiter; a broadcast receive must wake every
/// subscriber waiting on the shared write cursor, because waking one can wake a
/// different subscriber and strand the cancelled one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakePolicy {
    One,
    All,
}

#[derive(Default)]
struct State {
    cancelled: bool,
    // The address is registered only while receive owns the mapping. The
    // mutex prevents cancellation from waking it after unregister returns.
    waiting_on: Option<(usize, WakePolicy)>,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

/// Cancels a local setup or receive operation without changing shared queue state.
///
/// Clone this token for the supervisor and give another clone to the worker.
/// One token supports at most one active shared-memory receive. Cancelling it
/// is permanent, idempotent, and may block until that receive leaves its wait.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<Inner>);

impl CancellationToken {
    /// Creates a token in its active state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels current and future operations using this token.
    ///
    /// # Errors
    /// Returns a native wake error if the waiting consumer could not be woken.
    /// The token remains cancelled, and calling `cancel` again retries the wake.
    pub fn cancel(&self) -> io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        state.cancelled = true;
        self.0.changed.notify_all();
        while let Some((address, policy)) = state.waiting_on {
            // SAFETY: receive registers this address while it owns the mapping,
            // and cannot unregister or release the mapping while we hold state.
            let word = unsafe { &*(address as *const AtomicU32) };
            match policy {
                WakePolicy::One => wake(word)?,
                WakePolicy::All => wake_all(word)?,
            }
            // A wake just before the native wait could be missed. Retry only
            // during cancellation until receive unregisters the address.
            state = self.0.changed.wait_timeout(state, Duration::from_millis(10)).unwrap().0;
        }
        Ok(())
    }

    pub(crate) fn check(&self) -> io::Result<()> {
        if self.0.state.lock().unwrap().cancelled {
            Err(io::Error::new(io::ErrorKind::Interrupted, "FIT operation cancelled"))
        } else {
            Ok(())
        }
    }

    pub(crate) fn wait_for(&self, duration: Duration) -> io::Result<()> {
        let state = self.0.state.lock().unwrap();
        if state.cancelled {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "FIT operation cancelled"));
        }
        let (state, _) = self.0.changed.wait_timeout(state, duration).unwrap();
        if state.cancelled {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "FIT operation cancelled"));
        }
        Ok(())
    }

    pub(crate) fn wait_on(&self, word: &AtomicU32, expected: u32) -> io::Result<bool> {
        self.wait_on_policy(word, expected, WakePolicy::One)
    }

    /// Registers a wait whose cancellation must wake every waiter on the word.
    /// A broadcast subscriber uses this because several subscribers share the
    /// published write cursor.
    pub(crate) fn wait_on_all(&self, word: &AtomicU32, expected: u32) -> io::Result<bool> {
        self.wait_on_policy(word, expected, WakePolicy::All)
    }

    pub(crate) fn wait_on_policy(&self, word: &AtomicU32, expected: u32, policy: WakePolicy) -> io::Result<bool> {
        self.wait_on_with_policy(word, expected, policy, wait)
    }

    #[allow(dead_code)]
    fn wait_on_with(
        &self, word: &AtomicU32, expected: u32, wait_fn: impl FnOnce(&AtomicU32, u32) -> io::Result<()>,
    ) -> io::Result<bool> {
        self.wait_on_with_policy(word, expected, WakePolicy::One, wait_fn)
    }

    fn wait_on_with_policy(
        &self, word: &AtomicU32, expected: u32, policy: WakePolicy,
        wait_fn: impl FnOnce(&AtomicU32, u32) -> io::Result<()>,
    ) -> io::Result<bool> {
        let mut state = self.0.state.lock().unwrap();
        if state.cancelled {
            return Ok(false);
        }
        if state.waiting_on.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cancellation token is already used by a receiver",
            ));
        }
        state.waiting_on = Some((word as *const AtomicU32 as usize, policy));
        drop(state);

        let result = wait_fn(word, expected);
        let mut state = self.0.state.lock().unwrap();
        state.waiting_on = None;
        self.0.changed.notify_all();
        if state.cancelled {
            Ok(false)
        } else {
            result.map(|()| true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::Shared;
    use crate::ring::WRITE_OFFSET;
    use std::sync::mpsc;
    use std::thread;

    fn new_shared() -> Shared {
        Shared::create(crate::test_unique_id(), 64, 1).unwrap().0
    }

    #[test]
    fn cancellation_before_wait_prevents_registration() {
        let shared = new_shared();
        let token = CancellationToken::new();
        token.cancel().unwrap();
        let word = unsafe { &*shared.ptr.as_ptr().add(WRITE_OFFSET).cast::<AtomicU32>() };
        assert!(!token.wait_on(word, 0).unwrap());
        token.cancel().unwrap();
    }

    #[test]
    fn cancellation_wakes_an_idle_shared_wait() {
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let (registered_tx, registered_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let shared = new_shared();
            let word = unsafe { &*shared.ptr.as_ptr().add(WRITE_OFFSET).cast::<AtomicU32>() };
            worker_token
                .wait_on_with(word, 0, |word, expected| {
                    registered_tx.send(()).unwrap();
                    wait(word, expected)
                })
                .unwrap()
        });
        registered_rx.recv().unwrap();
        token.cancel().unwrap();
        assert!(!worker.join().unwrap());
    }

    #[test]
    fn cancellation_between_registration_and_native_wait_is_not_lost() {
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let (registered_tx, registered_rx) = mpsc::channel();
        let (proceed_tx, proceed_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let shared = new_shared();
            let word = unsafe { &*shared.ptr.as_ptr().add(WRITE_OFFSET).cast::<AtomicU32>() };
            worker_token
                .wait_on_with(word, 0, |word, expected| {
                    registered_tx.send(()).unwrap();
                    proceed_rx.recv().unwrap();
                    wait(word, expected)
                })
                .unwrap()
        });
        registered_rx.recv().unwrap();
        let cancel = thread::spawn(move || token.cancel().unwrap());
        // The first wake happens while the worker is held before the native wait.
        thread::sleep(Duration::from_millis(30));
        proceed_tx.send(()).unwrap();
        cancel.join().unwrap();
        assert!(!worker.join().unwrap());
    }
}
