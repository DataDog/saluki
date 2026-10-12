//! The credentials a delivery host follows: one follower per distinct credential reference, which stores every value
//! the reference's live view reports into the credential cell of each recipient that uses it.

use std::{collections::BTreeMap, future::Future, hash::BuildHasher as _, pin::Pin, sync::Arc};

use agent_data_plane_config::Live;
use futures::{
    future::{abortable, AbortHandle, Aborted},
    stream::FuturesUnordered,
};
use stringtheory::MetaString;

use super::{CredentialSource, RecipientRef};
use crate::common::datadog::api_key::{ApiKeyCell, StoreOutcome};

/// The live view of one credential.
type View = Live<Option<MetaString>>;

/// A change a followed credential's view reported: the reference, the follower it is for, the view to keep following,
/// and the new value.
pub(super) type CredentialChange = (MetaString, u64, View, Option<MetaString>);

/// A change a follower's view reported, or the end of a follower that stopped.
type Change = Result<CredentialChange, Aborted>;

/// What a credential change did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct ChangeOutcome {
    /// Whether a recipient's cell now holds a new key.
    pub(super) replaced: bool,

    /// Whether the credential is a proxy's, whose user information the host's client is built with.
    pub(super) proxy: bool,
}

/// One followed credential.
struct Follower {
    /// Tells this follower's pending change apart from that of a follower the reference had before.
    id: u64,

    /// Stops the follower's pending change.
    abort: AbortHandle,

    /// The value the view reported last.
    value: Option<MetaString>,

    /// The credential cell of each recipient that uses the credential.
    cells: BTreeMap<RecipientRef, ApiKeyCell>,

    /// Whether the host's client follows the credential, as a proxy's user information.
    proxy: bool,
}

/// The credentials a delivery host follows.
///
/// A follower lives while a recipient uses its reference or the host's client follows it, and is dropped, with its
/// view, when the last one stops: rotation is a store into the cells, never a change of plan.
pub(super) struct Credentials {
    source: Arc<dyn CredentialSource>,
    followers: BTreeMap<MetaString, Follower>,
    pending: FuturesUnordered<Pin<Box<dyn Future<Output = Change> + Send>>>,
    next_id: u64,
    fingerprints: std::hash::RandomState,
}

impl Credentials {
    /// Creates the credentials of a host that resolves references through `source`.
    pub(super) fn new(source: Arc<dyn CredentialSource>) -> Self {
        Self {
            source,
            followers: BTreeMap::new(),
            pending: FuturesUnordered::new(),
            next_id: 0,
            fingerprints: std::hash::RandomState::new(),
        }
    }

    /// Returns the number of credentials followed.
    #[cfg(test)]
    pub(super) fn followed(&self) -> usize {
        self.followers.len()
    }

    /// Returns whether a follower waits for a change.
    pub(super) fn is_idle(&self) -> bool {
        self.pending.is_empty()
    }

    /// Returns the keyed fingerprint of a key, which tells whether two recipients send with the same key without the
    /// key leaving the host.
    pub(super) fn fingerprint(&self, key: &str) -> u64 {
        self.fingerprints.hash_one(key)
    }

    /// Makes `cell`, the credential cell of `recipient`, follow `reference`, and stores the reference's current
    /// value into it, so a rotation between the cell's creation and now is not missed.
    ///
    /// Returns `false` if the source does not know the reference: the cell then keeps the key it was built with.
    pub(super) fn register(&mut self, recipient: &RecipientRef, reference: &MetaString, cell: ApiKeyCell) -> bool {
        let Some(follower) = self.follow(reference) else {
            return false;
        };
        cell.store(reference, follower.value.as_deref());
        follower.cells.insert(recipient.clone(), cell);
        true
    }

    /// Stops `recipient`'s cell from following `reference`, and drops the follower if nothing else uses it.
    pub(super) fn unregister(&mut self, recipient: &RecipientRef, reference: &MetaString) {
        if let Some(follower) = self.followers.get_mut(reference) {
            follower.cells.remove(recipient);
        }
        self.drop_if_unused(reference);
    }

    /// Makes the host's client follow `reference`, a proxy's credential, and returns its current value.
    pub(super) fn follow_proxy(&mut self, reference: &MetaString) -> Option<MetaString> {
        let follower = self.follow(reference)?;
        follower.proxy = true;
        follower.value.clone()
    }

    /// Stops the host's client from following every proxy credential but those in `keep`.
    pub(super) fn unfollow_proxies(&mut self, keep: &[&MetaString]) {
        let stopped = self
            .followers
            .iter_mut()
            .filter(|(reference, follower)| follower.proxy && !keep.contains(reference))
            .map(|(reference, follower)| {
                follower.proxy = false;
                reference.clone()
            })
            .collect::<Vec<_>>();
        for reference in stopped {
            self.drop_if_unused(&reference);
        }
    }

    /// Waits for the next change of a followed credential. Never returns while nothing is followed.
    pub(super) async fn next_change(&mut self) -> Option<CredentialChange> {
        use futures::StreamExt as _;

        loop {
            match self.pending.next().await {
                Some(Ok(change)) => return Some(change),
                // A stopped follower: its view is dropped with it.
                Some(Err(Aborted)) => continue,
                None => return std::future::pending().await,
            }
        }
    }

    /// Applies a change of a followed credential: stores the new value into each cell that follows it, with the native
    /// rules (trimmed; a missing, blank, or invalid value keeps the last usable key), and keeps following.
    pub(super) fn apply(&mut self, (reference, id, view, value): CredentialChange) -> ChangeOutcome {
        let Some(follower) = self.followers.get_mut(&reference).filter(|follower| follower.id == id) else {
            // The follower stopped while its change was on the way.
            return ChangeOutcome::default();
        };
        let mut outcome = ChangeOutcome {
            replaced: false,
            proxy: follower.proxy,
        };
        for cell in follower.cells.values() {
            outcome.replaced |= cell.store(&reference, value.as_deref()) == StoreOutcome::Replaced;
        }
        follower.value = value;
        let (pending, abort) = abortable(wait_for_change(reference.clone(), id, view));
        follower.abort = abort;
        self.pending.push(Box::pin(pending));
        outcome
    }

    /// Returns the follower of `reference`, which starts following its view if it does not yet, or `None` if the source
    /// does not know the reference.
    fn follow(&mut self, reference: &MetaString) -> Option<&mut Follower> {
        if !self.followers.contains_key(reference) {
            let view = self.source.resolve(reference)?;
            let id = self.next_id;
            self.next_id += 1;
            let value = (*view).clone();
            let (pending, abort) = abortable(wait_for_change(reference.clone(), id, view));
            self.pending.push(Box::pin(pending));
            self.followers.insert(
                reference.clone(),
                Follower {
                    id,
                    abort,
                    value,
                    cells: BTreeMap::new(),
                    proxy: false,
                },
            );
        }
        self.followers.get_mut(reference)
    }

    fn drop_if_unused(&mut self, reference: &MetaString) {
        if let Some(follower) = self.followers.get(reference) {
            if follower.cells.is_empty() && !follower.proxy {
                follower.abort.abort();
                self.followers.remove(reference);
            }
        }
    }
}

async fn wait_for_change(
    reference: MetaString, id: u64, mut view: View,
) -> (MetaString, u64, View, Option<MetaString>) {
    let value = view.changed().await;
    (reference, id, view, value)
}
