//! Generations.

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
};

/// The identifier of a generation.
///
/// Generation identifiers increase monotonically within a process and are never reused, including when an earlier
/// program is installed again: reinstalling a program creates a new generation with a new identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GenerationId(u64);

impl GenerationId {
    /// Creates a generation identifier from its numeric value.
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Returns the numeric value of this identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for GenerationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A source of monotonically increasing generation identifiers.
///
/// Identifiers start at 1 and increase by one for every call to [`next`][Self::next]. One allocator is meant to serve a
/// whole process, so that no two generations ever share an identifier.
#[derive(Debug)]
pub struct GenerationIdAllocator {
    next: AtomicU64,
}

impl GenerationIdAllocator {
    /// Creates an allocator whose first identifier is 1.
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    /// Returns the next generation identifier.
    ///
    /// # Panics
    ///
    /// Panics if every identifier has been allocated, which would take more than 500 years at a billion allocations per
    /// second.
    pub fn next(&self) -> GenerationId {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(id != u64::MAX, "generation identifiers exhausted");
        GenerationId(id)
    }
}

/// A callback that is invoked once when a generation retires.
///
/// The callback runs inside [`Drop`] of the generation's token, on whichever thread drops the last reference to it,
/// which may be a thread on the data path. It must therefore be cheap and must never block: sending the identifier on
/// an unbounded channel is the intended use.
pub struct RetireNotifier {
    notify: Box<dyn FnOnce(GenerationId) + Send + Sync>,
}

impl RetireNotifier {
    /// Creates a notifier from a callback.
    pub fn new<F>(notify: F) -> Self
    where
        F: FnOnce(GenerationId) + Send + Sync + 'static,
    {
        Self {
            notify: Box::new(notify),
        }
    }
}

impl fmt::Debug for RetireNotifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RetireNotifier").finish_non_exhaustive()
    }
}

/// The runtime representation of one generation.
///
/// A **generation** is one immutable revision of the processing program. Batches are stamped with the generation that
/// processes them, so a new generation can be installed while batches of older ones are still in flight: each batch
/// finishes under the generation it was stamped with.
///
/// A token carries its generation's identifier and, optionally, a [`RetireNotifier`]. Tokens are shared through
/// [`GenerationRef`], and everything that still needs a generation holds a reference to its token: every batch header
/// stamped with it, the schedule that assigns it to new batches, and the component that installed it. When the last
/// reference is dropped, the generation has **retired**, nothing can process a batch of it any more, and the notifier
/// runs. Components that keep per-generation state can hold a [`Weak`][std::sync::Weak] reference instead, and release
/// that state once the weak reference no longer upgrades.
#[derive(Debug)]
pub struct GenerationToken {
    id: GenerationId,
    on_retire: Option<RetireNotifier>,
}

impl GenerationToken {
    /// Creates a token with no retirement notification.
    pub fn new(id: GenerationId) -> Self {
        Self { id, on_retire: None }
    }

    /// Creates a token that invokes `on_retire` with its identifier when it is dropped.
    pub fn with_retire_notifier(id: GenerationId, on_retire: RetireNotifier) -> Self {
        Self {
            id,
            on_retire: Some(on_retire),
        }
    }

    /// Returns the identifier of this generation.
    pub fn id(&self) -> GenerationId {
        self.id
    }
}

impl Drop for GenerationToken {
    fn drop(&mut self) {
        if let Some(on_retire) = self.on_retire.take() {
            (on_retire.notify)(self.id);
        }
    }
}

/// A shared reference to a generation.
///
/// Holding a `GenerationRef` keeps the generation alive. See [`GenerationToken`] for what holds one.
pub type GenerationRef = Arc<GenerationToken>;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Returns a notifier that records every retirement in `retired`.
    fn recording_notifier(retired: &Arc<Mutex<Vec<GenerationId>>>) -> RetireNotifier {
        let retired = Arc::clone(retired);
        RetireNotifier::new(move |id| retired.lock().unwrap().push(id))
    }

    #[test]
    fn allocator_hands_out_increasing_identifiers_starting_at_one() {
        let allocator = GenerationIdAllocator::new();
        let ids = (0..4).map(|_| allocator.next()).collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                GenerationId::new(1),
                GenerationId::new(2),
                GenerationId::new(3),
                GenerationId::new(4)
            ]
        );
    }

    #[test]
    fn dropping_the_last_reference_notifies_retirement_once() {
        let retired = Arc::new(Mutex::new(Vec::new()));
        let generation: GenerationRef = Arc::new(GenerationToken::with_retire_notifier(
            GenerationId::new(5),
            recording_notifier(&retired),
        ));

        // Other holders, such as batch headers, keep the generation alive.
        let in_flight = Arc::clone(&generation);
        drop(generation);
        assert!(retired.lock().unwrap().is_empty(), "a live reference remains");

        drop(in_flight);
        assert_eq!(*retired.lock().unwrap(), vec![GenerationId::new(5)]);
    }

    #[test]
    fn weak_references_do_not_keep_a_generation_alive() {
        let retired = Arc::new(Mutex::new(Vec::new()));
        let generation: GenerationRef = Arc::new(GenerationToken::with_retire_notifier(
            GenerationId::new(9),
            recording_notifier(&retired),
        ));
        let weak = Arc::downgrade(&generation);
        assert_eq!(weak.upgrade().map(|token| token.id()), Some(GenerationId::new(9)));

        drop(generation);
        assert!(weak.upgrade().is_none());
        assert_eq!(*retired.lock().unwrap(), vec![GenerationId::new(9)]);
    }

    #[test]
    fn retirement_is_notified_from_the_dropping_thread() {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = Mutex::new(tx);
        let generation: GenerationRef = Arc::new(GenerationToken::with_retire_notifier(
            GenerationId::new(3),
            RetireNotifier::new(move |id| tx.lock().unwrap().send(id).unwrap()),
        ));

        std::thread::spawn(move || drop(generation)).join().unwrap();
        assert_eq!(rx.try_recv(), Ok(GenerationId::new(3)));
    }
}
