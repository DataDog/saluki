//! Fakes of the encode and delivery hosts, for tests of the installer core's resource actions.
//!
//! Each fake keeps the table its real host keeps, as data, and answers as the real host would:
//!
//! - a part of a preparation is acknowledged once its leases or recipients are in the table, and every recipient it
//!   requires is live; a preparation repeated by the same reference is done, and changes nothing;
//! - an incarnation is created at most once: a create of one the host does not hold, no newer than an incarnation of
//!   the same identity it created, fails the part, since that incarnation was dropped;
//! - a message whose `(identifier, key)` pair or [`RecipientRef`] does not match the table is ignored;
//! - an abort drops only what the aborted generation or transaction created and no other preparation prepared again,
//!   and returns a recipient it reopened to the drain it was in, once no other preparation that has not committed
//!   holds it reopened;
//! - a failed recipient's next incarnation takes its slot over, and an abort that drops the next incarnation gives the
//!   slot back to the failed one, so that a slot that leases name never empties;
//! - a released recipient drains until the test ends its drain ([`FakeDeliveryHost::retire`]), and a reopen of a
//!   recipient whose loop already exited, retired or failed, is answered `Exited`
//!   ([`PrepareOutcome::ReopenExited`]);
//! - each recipient incarnation's exit, retired or failed, is reported exactly once, as a host's event channel does,
//!   those an abort dropped included: their loops close with nothing queued, and report a retirement;
//! - a loop an abort dropped owns its queue's directory until its exit is taken ([`FakeDeliveryHost::take_exits`]):
//!   a part that creates another incarnation of the same recipient meanwhile waits, unanswered, until then, and an
//!   abort of its preparation answers it as failed ([`FakeDeliveryHost::take_answers`]);
//! - a publication whose native set names a recipient the host does not hold live is refused, since the core never
//!   publishes one.
//!
//! Neither runs tasks, encodes, or sends anything.

use std::collections::{BTreeMap, BTreeSet};

use saluki_core::execution::{EntryId, GenerationId};

pub use super::resources::Creator;
use super::{
    core::PrepareOutcome,
    resources::{
        HostPrepare, HostPublish, LeaseKey, LeaseRef, RecipientRef, RecipientSlot, ResourcesRevision, RetirementReport,
    },
};
use crate::ir::{HostId, RecipientDecl, RecipientId};

/// A fake encode host: the leases one delivery host's encode host runs.
#[derive(Debug)]
pub struct FakeEncodeHost {
    host: HostId,
    leases: BTreeMap<EntryId, (LeaseKey, Creator)>,
    fail_next: Option<String>,
}

impl FakeEncodeHost {
    /// Creates the encode host of the delivery host `host`, without leases.
    pub fn new(host: HostId) -> Self {
        Self {
            host,
            leases: BTreeMap::new(),
            fail_next: None,
        }
    }

    /// Returns the delivery host this encode host belongs to.
    pub fn host(&self) -> &HostId {
        &self.host
    }

    /// Makes the next preparation fail with `reason`.
    pub fn fail_next_prepare(&mut self, reason: impl Into<String>) {
        self.fail_next = Some(reason.into());
    }

    /// Prepares the leases of `part`, created by `creator`, and returns the acknowledgment.
    ///
    /// # Errors
    ///
    /// Returns the failure set with [`fail_next_prepare`][Self::fail_next_prepare], or an error if a lease identifier
    /// is live with another key: the core reuses an identifier only after its lease was released.
    pub fn prepare(&mut self, creator: Creator, part: &HostPrepare) -> Result<(), String> {
        if let Some(reason) = self.fail_next.take() {
            return Err(reason);
        }
        for lease in &part.leases {
            if let Some((key, _)) = self.leases.get(&lease.lease.lease) {
                if *key != lease.lease.key {
                    return Err(format!(
                        "lease {} is live for output `{}`, and cannot be prepared for output `{}`",
                        lease.lease.lease.get(),
                        key.output,
                        lease.lease.key.output
                    ));
                }
                continue;
            }
            self.leases
                .insert(lease.lease.lease, (lease.lease.key.clone(), creator));
        }
        Ok(())
    }

    /// Drops the leases the generation `generation` created, as an abort does.
    pub fn abort(&mut self, generation: GenerationId) {
        self.leases
            .retain(|_, (_, creator)| *creator != Creator::Generation(generation));
    }

    /// Drains and releases a lease, returning `true` if the host had it: a lease whose pair does not match is ignored.
    pub fn drain(&mut self, lease: &LeaseRef) -> bool {
        match self.leases.get(&lease.lease) {
            Some((key, _)) if *key == lease.key => {
                self.leases.remove(&lease.lease);
                true
            }
            _ => false,
        }
    }

    /// Returns every live lease, by identifier.
    pub fn leases(&self) -> Vec<LeaseRef> {
        self.leases
            .iter()
            .map(|(lease, (key, _))| LeaseRef {
                lease: *lease,
                key: key.clone(),
            })
            .collect()
    }
}

/// The state of a recipient in a fake delivery host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FakeRecipientState {
    /// Prepared, or reopened: routed to by payloads that name it, and by the native set when it names it.
    Live,

    /// Released, and draining its queued work until the test ends the drain.
    Draining,

    /// Its loop ended on an error, which was reported: its slot stays allocated until its next incarnation is
    /// prepared into it, or it is released.
    Failed,
}

#[derive(Clone, Debug)]
struct FakeRecipient {
    recipient: RecipientRef,
    decl: RecipientDecl,
    /// The preparations that created the recipient: the first, and any that prepared it again before it committed.
    creators: Vec<Creator>,
    state: FakeRecipientState,
    /// The release that started the recipient's current drain, kept while preparations that have not committed hold
    /// it reopened, so that an abort returns it to the same drain, with the same deadline.
    drain: Option<u64>,
    /// The creators whose preparations reopened the recipient since its release.
    reopened_by: Vec<Creator>,
    /// The failed incarnation this one took the slot over from, which an abort that drops this one restores, so that
    /// a slot leases name never empties.
    replaces: Option<Box<FakeRecipient>>,
}

/// How a recipient incarnation of a fake delivery host exited, as a host's event channel reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FakeExit {
    /// It retired after a release, with its drain report.
    Retired(RecipientRef, RetirementReport),

    /// It ended on an error outside a drain.
    Failed(RecipientRef, String),
}

/// A fake delivery host: the recipient table, the published native set and withdrawn set, and the revision it last
/// published.
#[derive(Debug)]
pub struct FakeDeliveryHost {
    host: HostId,
    recipients: BTreeMap<RecipientSlot, FakeRecipient>,
    native_set: BTreeSet<RecipientRef>,
    withdrawn: BTreeSet<RecipientId>,
    published: Option<ResourcesRevision>,
    fail_next: Option<String>,
    exits: Vec<FakeExit>,
    releases: u64,
    /// The newest incarnation of each identity the host created: an incarnation is created at most once.
    created: BTreeMap<RecipientId, u64>,
    /// The incarnations an abort dropped whose exits were not taken yet: each still owns its queue's directory.
    exiting: BTreeSet<RecipientRef>,
    /// The parts that wait for a dropped loop's exit, in the order they arrived.
    waiting: Vec<(Creator, HostPrepare)>,
    /// The answers of parts that waited, in the order they were answered, until they are taken.
    answers: Vec<(Creator, PrepareOutcome)>,
}

impl FakeDeliveryHost {
    /// Creates a delivery host without recipients, which has published nothing.
    pub fn new(host: HostId) -> Self {
        Self {
            host,
            recipients: BTreeMap::new(),
            native_set: BTreeSet::new(),
            withdrawn: BTreeSet::new(),
            published: None,
            fail_next: None,
            exits: Vec::new(),
            releases: 0,
            created: BTreeMap::new(),
            exiting: BTreeSet::new(),
            waiting: Vec::new(),
            answers: Vec::new(),
        }
    }

    /// Returns the delivery host's identifier.
    pub fn host(&self) -> &HostId {
        &self.host
    }

    /// Makes the next preparation fail with `reason`.
    pub fn fail_next_prepare(&mut self, reason: impl Into<String>) {
        self.fail_next = Some(reason.into());
    }

    fn held(&self, recipient: &RecipientRef) -> Option<&FakeRecipient> {
        self.recipients
            .get(&recipient.slot)
            .filter(|held| held.recipient == *recipient)
    }

    fn held_mut(&mut self, recipient: &RecipientRef) -> Option<&mut FakeRecipient> {
        self.recipients
            .get_mut(&recipient.slot)
            .filter(|held| held.recipient == *recipient)
    }

    /// Prepares the recipients of `part`, created by `creator`: checks that every recipient it requires is live,
    /// creates those it lists to create, and reopens those it lists to reopen. Returns the acknowledgment.
    ///
    /// A recipient the host already holds under the same reference is acknowledged as done, so a preparation repeated
    /// after an installer restart changes nothing; another creator that prepares it again becomes one of its creators,
    /// so that the abort of the first one keeps it. A recipient to reopen whose loop already exited, because it retired
    /// or failed, is not reopened: the acknowledgment lists it in [`PrepareOutcome::ReopenExited`].
    ///
    /// The acknowledgment is [`PrepareOutcome::Failed`], and nothing changes, on the failure set with
    /// [`fail_next_prepare`][Self::fail_next_prepare]; if a required recipient is not live; if a slot to create a
    /// recipient in holds another incarnation whose loop has not exited; or if another slot holds an incarnation of the
    /// same recipient whose loop has not exited, which still owns its queue's directory.
    ///
    /// Returns `None`, and changes nothing, if a recipient the part creates needs the directory of an incarnation an
    /// abort dropped whose exit was not taken: the part waits, and is prepared, and answered
    /// ([`take_answers`][Self::take_answers]), once that exit is taken.
    pub fn prepare(&mut self, creator: Creator, part: &HostPrepare) -> Option<PrepareOutcome> {
        if let Some(reason) = self.fail_next.take() {
            return Some(PrepareOutcome::Failed(reason));
        }
        let outcome = self.try_prepare(creator, part);
        if outcome.is_none() {
            self.waiting.push((creator, part.clone()));
        }
        outcome
    }

    /// Prepares a part, or returns `None` if it waits for a dropped loop's exit.
    fn try_prepare(&mut self, creator: Creator, part: &HostPrepare) -> Option<PrepareOutcome> {
        for required in &part.requires {
            if self.state(required) != Some(FakeRecipientState::Live) {
                return Some(PrepareOutcome::Failed(format!("{} is not live", required)));
            }
        }
        let mut waits = false;
        for create in &part.create {
            let recipient = &create.recipient;
            if self.held(recipient).is_some() {
                continue;
            }
            if self
                .created
                .get(&recipient.id)
                .is_some_and(|newest| *newest >= recipient.incarnation)
            {
                return Some(PrepareOutcome::Failed(format!(
                    "{} was dropped, and an incarnation is created once: prepare its next incarnation",
                    recipient
                )));
            }
            if let Some(held) = self.recipients.get(&recipient.slot) {
                if held.state != FakeRecipientState::Failed {
                    return Some(PrepareOutcome::Failed(format!(
                        "slot {} holds {}, and cannot be prepared for {}",
                        recipient.slot.get(),
                        held.recipient,
                        recipient
                    )));
                }
            }
            let owner = self.recipients.values().find(|held| {
                held.recipient.id == recipient.id
                    && held.recipient.slot != recipient.slot
                    && held.state != FakeRecipientState::Failed
            });
            if let Some(owner) = owner {
                return Some(PrepareOutcome::Failed(format!(
                    "the queue directory of {} is owned by {}, whose loop has not exited",
                    recipient, owner.recipient
                )));
            }
            waits |= self.exiting.iter().any(|dropped| dropped.id == recipient.id);
        }
        if waits {
            return None;
        }

        for create in &part.create {
            match self.held_mut(&create.recipient) {
                Some(held) => {
                    if !held.creators.contains(&creator) {
                        held.creators.push(creator);
                    }
                }
                None => {
                    self.created
                        .insert(create.recipient.id.clone(), create.recipient.incarnation);
                    let replaces = self
                        .recipients
                        .remove(&create.recipient.slot)
                        .filter(|held| held.state == FakeRecipientState::Failed)
                        .map(Box::new);
                    self.recipients.insert(
                        create.recipient.slot,
                        FakeRecipient {
                            recipient: create.recipient.clone(),
                            decl: create.decl.clone(),
                            creators: vec![creator],
                            state: FakeRecipientState::Live,
                            drain: None,
                            reopened_by: Vec::new(),
                            replaces,
                        },
                    );
                }
            }
        }
        let mut exited = Vec::new();
        for reopen in &part.reopen {
            match self.held_mut(&reopen.recipient) {
                Some(held) if held.state != FakeRecipientState::Failed => {
                    held.state = FakeRecipientState::Live;
                    if !held.reopened_by.contains(&creator) {
                        held.reopened_by.push(creator);
                    }
                }
                _ => exited.push(reopen.recipient.clone()),
            }
        }
        if exited.is_empty() {
            Some(PrepareOutcome::Prepared)
        } else {
            Some(PrepareOutcome::ReopenExited { recipients: exited })
        }
    }

    /// Drops the recipients `creator` created that no other preparation created too, and releases its reopens, as an
    /// abort of a generation or of a resource transaction does: a recipient that no other preparation holds reopened
    /// goes back to the drain it was in, with the same deadline.
    ///
    /// Each dropped incarnation's loop closes with nothing queued, and its exit is reported, as a retirement that
    /// delivered nothing: every incarnation's exit is reported exactly once, and the core ignores the exits of
    /// incarnations it no longer holds. The loop owns its queue's directory until its exit is taken. A dropped next
    /// incarnation of a failed recipient gives the slot back to the failed one. A part of `creator` that waits for a
    /// dropped loop's exit is answered as failed.
    pub fn abort(&mut self, creator: Creator) {
        let (aborted, waiting) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition::<Vec<_>, _>(|(by, _)| *by == creator);
        self.waiting = waiting;
        for (by, _) in aborted {
            self.answers.push((
                by,
                PrepareOutcome::Failed(format!(
                    "the preparation of {} was aborted while it waited for a dropped loop's exit",
                    by
                )),
            ));
        }
        let mut restored = Vec::new();
        let exits = &mut self.exits;
        let exiting = &mut self.exiting;
        self.recipients.retain(|_, recipient| {
            recipient.creators.retain(|by| *by != creator);
            let mut replaced = recipient.replaces.as_deref_mut();
            while let Some(failed) = replaced {
                failed.creators.retain(|by| *by != creator);
                replaced = failed.replaces.as_deref_mut();
            }
            let kept = !recipient.creators.is_empty();
            if !kept {
                if recipient.state != FakeRecipientState::Failed {
                    exits.push(FakeExit::Retired(
                        recipient.recipient.clone(),
                        RetirementReport::default(),
                    ));
                    exiting.insert(recipient.recipient.clone());
                }
                // The newest failed one a preparation still holds returns: one that failed before it was committed
                // gives way to the one it replaced.
                let mut replaced = recipient.replaces.take();
                while let Some(mut failed) = replaced {
                    if !failed.creators.is_empty() {
                        restored.push(failed);
                        break;
                    }
                    replaced = failed.replaces.take();
                }
            }
            kept
        });
        for failed in restored {
            self.recipients.insert(failed.recipient.slot, *failed);
        }
        for recipient in self.recipients.values_mut() {
            let reopened = recipient.reopened_by.len();
            recipient.reopened_by.retain(|by| *by != creator);
            let released = recipient.reopened_by.len() < reopened;
            if released
                && recipient.reopened_by.is_empty()
                && recipient.drain.is_some()
                && recipient.state == FakeRecipientState::Live
            {
                recipient.state = FakeRecipientState::Draining;
            }
        }
    }

    /// Publishes a native set, every recipient's settings, and a withdrawn set, as a resource transaction does: a
    /// revision older than the one published is ignored, and one equal to it, which a worker restart repeats, is
    /// applied already.
    ///
    /// Returns `true` if the publication took effect.
    ///
    /// # Errors
    ///
    /// Returns an error, and publishes nothing, if the native set names a recipient the host does not hold under that
    /// reference, or holds draining: the core publishes only recipients it prepared and has not released. A failed
    /// recipient is accepted, since its failure may race the publication.
    pub fn publish(&mut self, revision: ResourcesRevision, publish: &HostPublish) -> Result<bool, String> {
        if self.published.is_some_and(|published| published >= revision) {
            return Ok(false);
        }
        for recipient in &publish.native_set {
            match self.state(recipient) {
                Some(FakeRecipientState::Live | FakeRecipientState::Failed) => {}
                Some(FakeRecipientState::Draining) => {
                    return Err(format!("the native set names {}, which is draining", recipient));
                }
                None => {
                    return Err(format!(
                        "the native set names {}, which the host does not hold",
                        recipient
                    ))
                }
            }
        }
        self.published = Some(revision);
        self.native_set = publish.native_set.iter().cloned().collect();
        self.withdrawn = publish.withdrawn.clone();
        for settings in &publish.recipients {
            if let Some(held) = self.held_mut(&settings.recipient) {
                held.decl = settings.decl.clone();
            }
        }
        Ok(true)
    }

    /// Releases recipients: each live one whose reference matches the table starts draining, until
    /// [`retire`][Self::retire] ends its drain. A failed one, whose exit was reported when it failed, frees its slot
    /// without another exit. A draining one keeps the drain it is in, and a reference that does not match is ignored.
    pub fn release(&mut self, recipients: &[RecipientRef]) {
        for recipient in recipients {
            let Some(held) = self.recipients.get(&recipient.slot) else {
                continue;
            };
            if held.recipient != *recipient {
                continue;
            }
            match held.state {
                FakeRecipientState::Live => {
                    self.releases += 1;
                    let release = self.releases;
                    let held = self.recipients.get_mut(&recipient.slot).expect("the recipient is held");
                    held.state = FakeRecipientState::Draining;
                    held.drain = Some(release);
                    held.reopened_by.clear();
                }
                FakeRecipientState::Draining => {}
                FakeRecipientState::Failed => {
                    self.recipients.remove(&recipient.slot);
                }
            }
        }
    }

    /// Ends the drain of a released recipient: it retires with `report`, its slot is free, and its exit is reported.
    /// Returns `false`, and reports nothing, if the host holds no draining recipient under that reference.
    pub fn retire(&mut self, recipient: &RecipientRef, report: RetirementReport) -> bool {
        if self.state(recipient) != Some(FakeRecipientState::Draining) {
            return false;
        }
        self.recipients.remove(&recipient.slot);
        self.exits.push(FakeExit::Retired(recipient.clone(), report));
        true
    }

    /// Makes a recipient's loop end on an error: its slot stays allocated, and its exit is reported. Returns `false`,
    /// and reports nothing, if the host holds no live or draining recipient under that reference: an incarnation exits
    /// once.
    pub fn fail(&mut self, recipient: &RecipientRef, reason: impl Into<String>) -> bool {
        let Some(held) = self.held_mut(recipient) else {
            return false;
        };
        if held.state == FakeRecipientState::Failed {
            return false;
        }
        held.state = FakeRecipientState::Failed;
        self.exits.push(FakeExit::Failed(recipient.clone(), reason.into()));
        true
    }

    /// Returns the exits reported since the last call, in order, as the host's event channel delivers them.
    ///
    /// A dropped loop whose exit is taken no longer owns its queue's directory: each part that waited for it is
    /// prepared now, in the order the parts arrived, and answered ([`take_answers`][Self::take_answers]).
    pub fn take_exits(&mut self) -> Vec<FakeExit> {
        let exits = std::mem::take(&mut self.exits);
        for exit in &exits {
            let (FakeExit::Retired(recipient, _) | FakeExit::Failed(recipient, _)) = exit;
            self.exiting.remove(recipient);
        }
        for (creator, part) in std::mem::take(&mut self.waiting) {
            match self.try_prepare(creator, &part) {
                Some(outcome) => self.answers.push((creator, outcome)),
                None => self.waiting.push((creator, part)),
            }
        }
        exits
    }

    /// Returns the answers of the parts that waited for a dropped loop's exit, answered since the last call, in order.
    pub fn take_answers(&mut self) -> Vec<(Creator, PrepareOutcome)> {
        std::mem::take(&mut self.answers)
    }

    /// Returns every recipient in the table, by slot, with its state.
    pub fn recipients(&self) -> Vec<(RecipientRef, FakeRecipientState)> {
        self.recipients
            .values()
            .map(|held| (held.recipient.clone(), held.state))
            .collect()
    }

    /// Returns the state of a recipient, if the table holds it under that reference.
    pub fn state(&self, recipient: &RecipientRef) -> Option<FakeRecipientState> {
        self.held(recipient).map(|held| held.state)
    }

    /// Returns which release started a draining recipient's drain, numbered from 1 in the order the host received
    /// them, or `None` if the table holds no draining recipient under that reference.
    ///
    /// The number stands for the drain's deadline: a reopen that is aborted returns the recipient to the same drain.
    pub fn drain(&self, recipient: &RecipientRef) -> Option<u64> {
        self.held(recipient)
            .filter(|held| held.state == FakeRecipientState::Draining)
            .and_then(|held| held.drain)
    }

    /// Returns the declaration a recipient was prepared with, or last published with, if the table has it.
    pub fn declaration(&self, recipient: &RecipientRef) -> Option<&RecipientDecl> {
        self.held(recipient).map(|held| &held.decl)
    }

    /// Returns the published native set.
    pub fn native_set(&self) -> &BTreeSet<RecipientRef> {
        &self.native_set
    }

    /// Returns the published withdrawn set.
    pub fn withdrawn(&self) -> &BTreeSet<RecipientId> {
        &self.withdrawn
    }

    /// Returns the revision published last, if any.
    pub fn published(&self) -> Option<ResourcesRevision> {
        self.published
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        canonical::PlanHash,
        install::{LeasePrepare, RecipientPrepare},
        ir::{OutputDecl, OutputId},
    };

    fn host() -> HostId {
        HostId::new("dd_out").unwrap()
    }

    fn lease(id: u16, output: &str) -> LeaseRef {
        LeaseRef {
            lease: EntryId::new(id),
            key: LeaseKey {
                output: OutputId::new(output).unwrap(),
                binding: PlanHash::of_bytes(output.as_bytes()),
            },
        }
    }

    fn output(id: &str) -> OutputDecl {
        serde_json::from_value(json!({"id": id, "accepts": {"kinds": "all", "timestamps": "mixed"},
                                      "fingerprint": format!("sha256:{}", "0".repeat(64))}))
        .unwrap()
    }

    fn recipient(slot: u16, endpoint: &str, incarnation: u64) -> RecipientPrepare {
        let id = RecipientId::new(format!("dd_out/additional/0/{}", endpoint)).unwrap();
        RecipientPrepare {
            recipient: RecipientRef {
                slot: RecipientSlot::new(slot),
                id: id.clone(),
                incarnation,
            },
            decl: serde_json::from_value(json!({
                "id": id, "host": "dd_out", "endpoint": endpoint, "credential": "native:api_key", "classes": "all",
                "provenance": "native"
            }))
            .unwrap(),
        }
    }

    fn part(
        leases: Vec<LeaseRef>, create: Vec<RecipientPrepare>, reopen: Vec<RecipientPrepare>,
        requires: Vec<RecipientRef>,
    ) -> HostPrepare {
        HostPrepare {
            host: host(),
            leases: leases
                .into_iter()
                .map(|lease| LeasePrepare {
                    output: output(lease.key.output.as_str()),
                    lease,
                    recipients: Vec::new(),
                })
                .collect(),
            create,
            reopen,
            requires,
        }
    }

    fn create(recipients: &[&RecipientPrepare]) -> HostPrepare {
        part(
            vec![],
            recipients.iter().map(|r| (*r).clone()).collect(),
            vec![],
            vec![],
        )
    }

    fn reopen(recipients: &[&RecipientPrepare]) -> HostPrepare {
        part(
            vec![],
            vec![],
            recipients.iter().map(|r| (*r).clone()).collect(),
            vec![],
        )
    }

    fn publish(native_set: Vec<RecipientRef>, withdrawn: Vec<RecipientId>) -> HostPublish {
        HostPublish {
            host: host(),
            native_set,
            settings: crate::ir::HostBinding {
                host: host(),
                settings: Default::default(),
            },
            recipients: Vec::new(),
            withdrawn: withdrawn.into_iter().collect(),
        }
    }

    #[test]
    fn an_encode_host_holds_prepared_leases_ignores_mismatched_drains_and_aborts_what_a_generation_created() {
        let mut encode = FakeEncodeHost::new(host());
        let (one, two) = (GenerationId::new(1), GenerationId::new(2));
        assert_eq!(
            encode.prepare(
                Creator::Generation(one),
                &part(vec![lease(0, "main")], vec![], vec![], vec![])
            ),
            Ok(())
        );
        // A second generation reuses lease 0 and creates lease 1; an identifier live for another output is refused.
        assert_eq!(
            encode.prepare(
                Creator::Generation(two),
                &part(vec![lease(0, "main"), lease(1, "group")], vec![], vec![], vec![])
            ),
            Ok(())
        );
        assert!(encode
            .prepare(
                Creator::Generation(two),
                &part(vec![lease(1, "other")], vec![], vec![], vec![])
            )
            .is_err());

        encode.abort(two);
        assert_eq!(encode.leases(), vec![lease(0, "main")]);
        assert!(!encode.drain(&lease(0, "other")), "a mismatched pair is ignored");
        assert!(encode.drain(&lease(0, "main")));
        assert!(encode.leases().is_empty());

        encode.fail_next_prepare("no builders");
        assert_eq!(
            encode.prepare(
                Creator::Generation(one),
                &part(vec![lease(2, "main")], vec![], vec![], vec![])
            ),
            Err("no builders".to_string())
        );
    }

    #[test]
    fn a_delivery_host_requires_live_recipients_and_publishes_in_revision_order() {
        let mut delivery = FakeDeliveryHost::new(host());
        let one = GenerationId::new(1);
        let transaction = Creator::Transaction(ResourcesRevision::new(1));
        let a = recipient(0, "https://a.example.com", 1);

        // A generation that requires a recipient no transaction prepared fails its acknowledgment.
        assert!(matches!(
            delivery
                .prepare(
                    Creator::Generation(one),
                    &part(vec![], vec![], vec![], vec![a.recipient.clone()])
                )
                .expect("the part is answered at once"),
            PrepareOutcome::Failed(_)
        ));
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&a]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&a]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared,
            "a repeated prepare is done"
        );
        assert_eq!(
            delivery
                .prepare(
                    Creator::Generation(one),
                    &part(vec![], vec![], vec![], vec![a.recipient.clone()])
                )
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );

        let withdrawn = RecipientId::new("dd_out/additional/0/https://gone.example.com").unwrap();
        assert_eq!(
            delivery.publish(
                ResourcesRevision::new(2),
                &publish(vec![a.recipient.clone()], vec![withdrawn.clone()])
            ),
            Ok(true)
        );
        assert_eq!(
            delivery.publish(ResourcesRevision::new(1), &publish(vec![], vec![])),
            Ok(false),
            "an older revision is ignored"
        );
        assert_eq!(delivery.native_set().len(), 1);
        assert!(delivery.withdrawn().contains(&withdrawn));
        assert_eq!(delivery.published(), Some(ResourcesRevision::new(2)));
        assert_eq!(delivery.declaration(&a.recipient), Some(&a.decl));
    }

    #[test]
    fn a_publication_naming_a_recipient_the_host_does_not_hold_live_is_refused() {
        let mut delivery = FakeDeliveryHost::new(host());
        let transaction = Creator::Transaction(ResourcesRevision::new(1));
        let a = recipient(0, "https://a.example.com", 1);
        let never_prepared = recipient(1, "https://b.example.com", 2);
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&a]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );

        // A failed addition the core left in the native set, and a stale incarnation, are refused.
        let stale = RecipientRef {
            incarnation: 0,
            ..a.recipient.clone()
        };
        for named in [never_prepared.recipient.clone(), stale] {
            assert!(delivery
                .publish(ResourcesRevision::new(1), &publish(vec![named], vec![]))
                .is_err());
        }
        assert_eq!(delivery.published(), None, "a refused publication publishes nothing");

        // So is a draining recipient, which the core releases only once no native set names it; a failed one is
        // accepted, since its failure may race the publication.
        delivery.release(std::slice::from_ref(&a.recipient));
        assert!(delivery
            .publish(ResourcesRevision::new(1), &publish(vec![a.recipient.clone()], vec![]))
            .is_err());
        let b = recipient(2, "https://c.example.com", 3);
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&b]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        assert!(delivery.fail(&b.recipient, "disk full"));
        assert_eq!(
            delivery.publish(ResourcesRevision::new(1), &publish(vec![b.recipient.clone()], vec![])),
            Ok(true)
        );
    }

    #[test]
    fn a_released_recipient_drains_until_retired_and_exits_once() {
        let mut delivery = FakeDeliveryHost::new(host());
        let transaction = Creator::Transaction(ResourcesRevision::new(1));
        let a = recipient(0, "https://a.example.com", 1);
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&a]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );

        // A release naming another incarnation is ignored; the matching one starts a drain, which a repeated release
        // keeps.
        let stale = RecipientRef {
            incarnation: 0,
            ..a.recipient.clone()
        };
        delivery.release(&[stale]);
        assert_eq!(delivery.state(&a.recipient), Some(FakeRecipientState::Live));
        delivery.release(std::slice::from_ref(&a.recipient));
        delivery.release(std::slice::from_ref(&a.recipient));
        assert_eq!(delivery.drain(&a.recipient), Some(1));
        assert!(
            delivery.take_exits().is_empty(),
            "a drain reports nothing until it ends"
        );

        let report = RetirementReport {
            delivered: 3,
            ..RetirementReport::default()
        };
        assert!(delivery.retire(&a.recipient, report));
        assert!(!delivery.retire(&a.recipient, report));
        assert!(!delivery.fail(&a.recipient, "late"));
        delivery.release(std::slice::from_ref(&a.recipient));
        assert_eq!(
            delivery.take_exits(),
            vec![FakeExit::Retired(a.recipient.clone(), report)]
        );
        assert!(delivery.recipients().is_empty());
    }

    #[test]
    fn a_failed_loop_exits_once_is_never_reopened_and_its_slot_takes_its_next_incarnation() {
        let mut delivery = FakeDeliveryHost::new(host());
        let transaction = Creator::Transaction(ResourcesRevision::new(1));
        let b = recipient(1, "https://b.example.com", 2);
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&b]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );

        // A failure then a release, or a second failure, report no second exit.
        assert!(delivery.fail(&b.recipient, "disk full"));
        assert!(!delivery.fail(&b.recipient, "disk full"));
        assert_eq!(
            delivery.take_exits(),
            vec![FakeExit::Failed(b.recipient.clone(), "disk full".to_string())]
        );
        assert_eq!(
            delivery
                .prepare(Creator::Generation(GenerationId::new(1)), &reopen(&[&b]))
                .expect("the part is answered at once"),
            PrepareOutcome::ReopenExited {
                recipients: vec![b.recipient.clone()]
            },
            "a failed incarnation is never reopened"
        );
        assert_eq!(delivery.state(&b.recipient), Some(FakeRecipientState::Failed));

        // Its next incarnation takes the slot over.
        let next = recipient(1, "https://b.example.com", 3);
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&next]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        assert_eq!(
            delivery.recipients(),
            vec![(next.recipient.clone(), FakeRecipientState::Live)]
        );
        assert!(!delivery.fail(&b.recipient, "late"), "the failed incarnation is gone");

        // A failed recipient that is released frees its slot without another exit.
        assert!(delivery.fail(&next.recipient, "disk full"));
        delivery.release(std::slice::from_ref(&next.recipient));
        assert!(delivery.recipients().is_empty());
        assert_eq!(
            delivery.take_exits(),
            vec![FakeExit::Failed(next.recipient, "disk full".to_string())]
        );
    }

    #[test]
    fn an_incarnation_is_created_once_so_a_dropped_one_is_never_created_again() {
        let mut delivery = FakeDeliveryHost::new(host());
        let (older, newer) = (
            Creator::Transaction(ResourcesRevision::new(1)),
            Creator::Generation(GenerationId::new(2)),
        );
        let a = recipient(0, "https://a.example.com", 1);
        assert_eq!(
            delivery
                .prepare(older, &create(&[&a]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        delivery.abort(older);
        // A preparation that counted on the dropped incarnation fails, changing nothing; its next one prepares.
        assert!(matches!(
            delivery.prepare(newer, &create(&[&a])).expect("the part is answered at once"),
            PrepareOutcome::Failed(message) if message.ends_with("prepare its next incarnation")
        ));
        assert!(delivery.recipients().is_empty());
        // The next incarnation waits until the dropped loop's exit is taken, since that loop owns its directory.
        let next = recipient(1, "https://a.example.com", 2);
        assert_eq!(delivery.prepare(newer, &create(&[&next])), None);
        assert!(delivery.recipients().is_empty());
        assert_eq!(
            delivery.take_exits(),
            vec![FakeExit::Retired(a.recipient, RetirementReport::default())]
        );
        assert_eq!(delivery.take_answers(), vec![(newer, PrepareOutcome::Prepared)]);
        assert_eq!(delivery.state(&next.recipient), Some(FakeRecipientState::Live));
    }

    #[test]
    fn a_part_that_waits_for_a_dropped_loop_is_answered_failed_by_an_abort_of_its_preparation() {
        let mut delivery = FakeDeliveryHost::new(host());
        let (older, newer) = (
            Creator::Generation(GenerationId::new(1)),
            Creator::Generation(GenerationId::new(2)),
        );
        let a = recipient(0, "https://a.example.com", 1);
        assert_eq!(delivery.prepare(older, &create(&[&a])), Some(PrepareOutcome::Prepared));
        delivery.abort(older);
        let next = recipient(1, "https://a.example.com", 2);
        assert_eq!(delivery.prepare(newer, &create(&[&next])), None);
        delivery.abort(newer);
        assert!(matches!(
            delivery.take_answers().as_slice(),
            [(creator, PrepareOutcome::Failed(message))] if *creator == newer && message.contains("was aborted")
        ));
        delivery.take_exits();
        assert!(delivery.take_answers().is_empty(), "an aborted part is never prepared");
        assert!(delivery.recipients().is_empty());
    }

    #[test]
    fn an_aborted_next_incarnation_gives_the_slot_back_to_the_failed_one() {
        let mut delivery = FakeDeliveryHost::new(host());
        let generation = Creator::Generation(GenerationId::new(1));
        let b = recipient(1, "https://b.example.com", 2);
        assert_eq!(
            delivery
                .prepare(generation, &create(&[&b]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        assert!(delivery.fail(&b.recipient, "disk full"));
        let transaction = Creator::Transaction(ResourcesRevision::new(2));
        let next = recipient(1, "https://b.example.com", 3);
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&next]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        delivery.abort(transaction);
        assert_eq!(
            delivery.recipients(),
            vec![(b.recipient.clone(), FakeRecipientState::Failed)]
        );
        assert_eq!(
            delivery.take_exits(),
            vec![
                FakeExit::Failed(b.recipient, "disk full".to_string()),
                FakeExit::Retired(next.recipient, RetirementReport::default()),
            ]
        );
    }

    #[test]
    fn a_reopen_of_a_recipient_whose_loop_retired_is_answered_exited() {
        let mut delivery = FakeDeliveryHost::new(host());
        let transaction = Creator::Transaction(ResourcesRevision::new(1));
        let a = recipient(0, "https://a.example.com", 1);
        let c = recipient(2, "https://c.example.com", 2);
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&a, &c]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        delivery.release(&[a.recipient.clone(), c.recipient.clone()]);
        assert!(delivery.retire(&a.recipient, RetirementReport::default()));

        // The generation reopens both: the draining one is reopened, the retired one answered `Exited`.
        let generation = Creator::Generation(GenerationId::new(1));
        assert_eq!(
            delivery
                .prepare(generation, &reopen(&[&a, &c]))
                .expect("the part is answered at once"),
            PrepareOutcome::ReopenExited {
                recipients: vec![a.recipient.clone()]
            }
        );
        assert_eq!(delivery.state(&c.recipient), Some(FakeRecipientState::Live));
        assert_eq!(delivery.state(&a.recipient), None);
    }

    #[test]
    fn an_aborted_reopen_returns_the_recipient_to_its_original_drain_however_often_it_was_repeated() {
        let mut delivery = FakeDeliveryHost::new(host());
        let transaction = Creator::Transaction(ResourcesRevision::new(1));
        let a = recipient(0, "https://a.example.com", 1);
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&a]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        delivery.release(std::slice::from_ref(&a.recipient));
        assert_eq!(delivery.drain(&a.recipient), Some(1));

        // A generation reopens it, and repeats its prepare after a worker restart; another transaction reopens it too.
        let (two, three) = (
            Creator::Generation(GenerationId::new(2)),
            Creator::Transaction(ResourcesRevision::new(3)),
        );
        for _ in 0..2 {
            assert_eq!(
                delivery
                    .prepare(two, &reopen(&[&a]))
                    .expect("the part is answered at once"),
                PrepareOutcome::Prepared
            );
        }
        assert_eq!(
            delivery
                .prepare(three, &reopen(&[&a]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        assert_eq!(delivery.state(&a.recipient), Some(FakeRecipientState::Live));

        // The recipient stays live while either holds it, and returns to the drain it was in once neither does.
        delivery.abort(three);
        assert_eq!(delivery.state(&a.recipient), Some(FakeRecipientState::Live));
        delivery.abort(two);
        assert_eq!(delivery.drain(&a.recipient), Some(1));

        // A recipient reopened for good, by a preparation that committed, starts a new drain when released again.
        assert_eq!(
            delivery
                .prepare(two, &reopen(&[&a]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        delivery.release(std::slice::from_ref(&a.recipient));
        assert_eq!(delivery.drain(&a.recipient), Some(2));
        let four = Creator::Generation(GenerationId::new(4));
        assert_eq!(
            delivery
                .prepare(four, &reopen(&[&a]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        delivery.abort(four);
        assert_eq!(delivery.drain(&a.recipient), Some(2));
        assert!(delivery.take_exits().is_empty());
    }

    #[test]
    fn an_aborted_transaction_drops_only_what_no_newer_preparation_prepared_again() {
        let mut delivery = FakeDeliveryHost::new(host());
        let (older, newer) = (
            Creator::Transaction(ResourcesRevision::new(1)),
            Creator::Transaction(ResourcesRevision::new(2)),
        );
        let kept = recipient(0, "https://kept.example.com", 1);
        let dropped = recipient(1, "https://dropped.example.com", 2);
        assert_eq!(
            delivery
                .prepare(older, &create(&[&kept, &dropped]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );

        // The newer transaction, which replaces the older one before it published, still adds one of its recipients.
        assert_eq!(
            delivery
                .prepare(newer, &create(&[&kept]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        delivery.abort(older);
        assert_eq!(
            delivery.recipients(),
            vec![(kept.recipient.clone(), FakeRecipientState::Live)]
        );
        delivery.abort(newer);
        assert!(delivery.recipients().is_empty());
        // Every dropped incarnation's exit is reported once, as a retirement that delivered nothing.
        assert_eq!(
            delivery.take_exits(),
            vec![
                FakeExit::Retired(dropped.recipient.clone(), RetirementReport::default()),
                FakeExit::Retired(kept.recipient.clone(), RetirementReport::default()),
            ]
        );
    }

    #[test]
    fn a_prepare_that_fails_changes_nothing_and_a_live_loop_keeps_its_slot_and_its_directory() {
        let mut delivery = FakeDeliveryHost::new(host());
        let transaction = Creator::Transaction(ResourcesRevision::new(1));
        let a = recipient(0, "https://a.example.com", 1);
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&a]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );

        // Another recipient in a slot a live incarnation holds.
        let b = recipient(1, "https://b.example.com", 2);
        let taken = recipient(0, "https://c.example.com", 3);
        assert!(matches!(
            delivery.prepare(transaction, &create(&[&b, &taken])).expect("the part is answered at once"),
            PrepareOutcome::Failed(message) if message.starts_with("slot 0 holds")
        ));
        // The next incarnation of a recipient whose draining loop still owns its directory, in another slot.
        delivery.release(std::slice::from_ref(&a.recipient));
        let successor = recipient(2, "https://a.example.com", 4);
        assert!(matches!(
            delivery.prepare(transaction, &create(&[&successor])).expect("the part is answered at once"),
            PrepareOutcome::Failed(message) if message.starts_with("the queue directory of")
        ));
        delivery.fail_next_prepare("no client");
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&b]))
                .expect("the part is answered at once"),
            PrepareOutcome::Failed("no client".to_string())
        );
        assert_eq!(
            delivery.recipients(),
            vec![(a.recipient.clone(), FakeRecipientState::Draining)]
        );

        // Once the drain ends, the successor prepares, and an abort of its transaction drops it.
        assert!(delivery.retire(&a.recipient, RetirementReport::default()));
        assert_eq!(
            delivery
                .prepare(transaction, &create(&[&successor]))
                .expect("the part is answered at once"),
            PrepareOutcome::Prepared
        );
        delivery.abort(transaction);
        assert!(delivery.recipients().is_empty());
    }
}
