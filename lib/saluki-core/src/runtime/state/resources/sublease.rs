//! Resource subleasing.

use std::sync::{Arc, Weak};

use tokio::sync::watch;

/// The ledger a resource's subleases are recorded in.
///
/// One per registry entry, outliving the individual leases taken out on it.
pub(super) struct SubleaseLedger {
    outstanding: watch::Sender<usize>,
}

impl SubleaseLedger {
    /// Creates an empty ledger.
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            outstanding: watch::Sender::new(0),
        })
    }

    /// Returns the number of subleases currently outstanding.
    pub(super) fn outstanding(&self) -> usize {
        *self.outstanding.borrow()
    }

    /// Waits until every sublease has been returned.
    ///
    /// Returns immediately when none are outstanding, which is the case for any resource that never subdivides
    /// itself, and for one whose holder finished with its subresources before releasing the head lease.
    pub(super) async fn settled(&self) {
        // The borrow is released before awaiting, and `wait_for` checks the current value before it waits, so a
        // sublease returned in between is accounted for rather than missed.
        let mut receiver = self.outstanding.subscribe();
        let _ = receiver.wait_for(|outstanding| *outstanding == 0).await;
    }

    /// Records a new sublease.
    fn acquire(&self) {
        self.outstanding.send_modify(|outstanding| *outstanding += 1);
    }

    /// Records the return of a sublease.
    fn release(&self) {
        self.outstanding.send_modify(|outstanding| {
            *outstanding = outstanding.saturating_sub(1);
        });
    }
}

/// Subleases on a leased resource.
///
/// In some scenarios, a leased resource may actually represent a _collection_ of "subresources" that can be handed out
/// piecemeal, at arbitrary points in time. These subresources are often wrapped in their own types to ensure proper
/// behavior (RAII style), but ultimately must roll back up to the original leased resource to ensure proper behavior
/// within the resource registry. This requires knowing, deterministically, when a leased resource has returned
/// completely.
///
/// Subleases provide an RAII guard mechanism that allows an owned guard to be paired with a subresource such that when
/// the subresource is dropped, the sublease is dropped with it. Subleases are tracked by a parent leased resource, and
/// only when all subleases have been dropped is the parent leased resource able to be reacquired by another caller.
///
/// [`Subleases`] is not itself a sublease. It deliberately doesn't keep the resource's lease alive: a handle kept in
/// order to issue subleases would otherwise hold the resource's lease open for as long as the resource existed, and
/// no acquirer would ever be handed it again.
#[derive(Clone)]
pub struct Subleases {
    ledger: Weak<SubleaseLedger>,
}

impl Subleases {
    pub(super) fn from_ledger(ledger: &Arc<SubleaseLedger>) -> Self {
        Self {
            ledger: Arc::downgrade(ledger),
        }
    }

    /// Issues a sublease, held until the returned value is dropped.
    ///
    /// Returns `None` once the resource's registry entry is gone, leaving nothing to record a sublease against. That
    /// only happens after the resource itself has been dropped -- a
    /// [`discard`][super::ResourceLease::discard]ed resource keeps its entry until its subleases are returned -- so a
    /// resource still in use always gets its sublease.
    ///
    /// # Subleasing after the head lease is returned
    ///
    /// Nothing here prevents a sublease being issued after the head lease has gone back to the registry, which under
    /// the property-law reading of the name shouldn't be possible. In practice a resource is only reachable through
    /// its head lease, so a holder has nothing to issue against once it has released it. Genuinely preventing it
    /// would mean fixing the number of subleases up front, or issuing them all eagerly and handing them out, which
    /// costs more than the case is worth.
    pub fn issue(&self) -> Option<Sublease> {
        let ledger = self.ledger.upgrade()?;
        ledger.acquire();

        Some(Sublease { ledger })
    }
}

impl std::fmt::Debug for Subleases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subleases")
            .field("outstanding", &self.ledger.upgrade().map(|ledger| ledger.outstanding()))
            .finish()
    }
}

/// A sublease on part of a leased resource.
///
/// Held by a subresource for as long as it is in use. Outstanding subleases keep the resource's lease alive: the
/// registry won't hand the resource to another acquirer while one is held, even once the head
/// [`ResourceLease`][super::ResourceLease] has been dropped -- nor build a replacement for one that was
/// [`discard`][super::ResourceLease::discard]ed, since that replacement would exist alongside whatever this sublease
/// is still holding open.
pub struct Sublease {
    ledger: Arc<SubleaseLedger>,
}

impl Drop for Sublease {
    fn drop(&mut self) {
        self.ledger.release();
    }
}

impl std::fmt::Debug for Sublease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sublease").finish_non_exhaustive()
    }
}
