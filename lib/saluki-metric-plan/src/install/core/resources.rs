//! The installer core's resource paths: resource transactions, the hosts' acknowledgments and events, the resources
//! of each generation's preparation, abort, commit, and retirement, and the credits they hold.
//!
//! # Resource transactions
//!
//! Every lowering is followed by a resource transaction when a host's native set, recipient or host settings,
//! withdrawn set, or failed recipients differ from what it published ([`ResourceTable::transaction`]). At most one is
//! in flight, for the newest lowering:
//!
//! | Event | Transaction in flight |
//! |---|---|
//! | a lowering whose resources differ from the one it follows | superseded: the newer one is begun, then the older one is aborted in every host it was sent to, so that what both need stays prepared |
//! | every host answered | completed: each host publishes its native set without the additions that failed, its settings, and its withdrawn set; what the publication leaves without a user is released |
//! | its timeout passed | completed as above, with every host that did not answer aborted |
//! | an addition failed, or a host did not answer | retried by a later transaction, with backoff (1 to 30 s) |
//!
//! A transaction never waits for a cut or a metrics preparation, and a metrics preparation failure does not hold it
//! back: removals and withdrawals publish regardless of any addition.
//!
//! # Generations
//!
//! A generation's preparation sends each host its part ([`HostPrepare`]), and the barrier waits for every host as it
//! waits for every region. A host that answers that a reopened recipient's loop had exited makes the preparation start
//! again once the exit is reported, since the generation's leases name the exited incarnation's slot. An abort drops
//! what the generation created and drains the leases it created (an encode host has no abort, and drains a lease
//! nothing addressed at once); a commit makes them live; a retirement releases what no live generation uses any more.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use saluki_core::{accounting::ComponentRegistry, execution::GenerationId, support::SubsystemIdentifier};

use super::{retry::Wait, Action, Creator, InstallerCore, PrepareOutcome};
use crate::{
    compile::{Frame, KernelKey, SlotId},
    install::{
        diff::SlotKernels,
        resources::{LeaseRef, PartAnswer, RecipientRef, Released, ResourceTable, ResourcesRevision, RetirementReport},
    },
    ir::{HostId, Plan, RecipientId},
    operators::OperatorRegistry,
};

/// What the core keeps about resource transactions beyond the table: their backoff and the failures it reports.
#[derive(Debug, Default)]
pub(super) struct TransactionState {
    /// The transactions in a row that left an addition out, for the backoff.
    pub(super) attempts: u32,

    /// Why each native recipient a transaction could not prepare was left out, until one prepares it: the host
    /// reports itself degraded meanwhile.
    pub(super) failures: BTreeMap<RecipientId, String>,

    /// The hosts that did not acknowledge the latest transaction that left something out, with why.
    pub(super) unanswered: BTreeMap<HostId, String>,
}

impl InstallerCore {
    /// Returns the resource table: the leases and recipients the hosts run, with their states, what each host
    /// published, and the credits outstanding.
    pub fn resource_table(&self) -> &ResourceTable {
        &self.resources
    }

    /// Reports one host's acknowledgment of its part of a resource transaction ([`Action::PrepareResources`]):
    /// [`Prepared`][PrepareOutcome::Prepared] if it prepared every recipient listed for it, which recipients it could
    /// not reopen because their loops had exited ([`ReopenExited`][PrepareOutcome::ReopenExited]), or the error it
    /// failed with, which names the recipient. `Result<(), String>` converts into a [`PrepareOutcome`]. A refusal of
    /// the resource budget for a credit of a host's part is that host's failure: the caller sends the host nothing.
    ///
    /// Once every host answered, the transaction publishes. Acknowledgments of a transaction that is no longer being
    /// prepared are ignored.
    pub fn resources_prepared(
        &mut self, revision: ResourcesRevision, host: HostId, outcome: impl Into<PrepareOutcome>,
    ) -> Vec<Action> {
        let answer = match outcome.into() {
            PrepareOutcome::Prepared => PartAnswer::Prepared,
            PrepareOutcome::ReopenExited { recipients } => PartAnswer::Exited(recipients),
            PrepareOutcome::Failed(message) => PartAnswer::Failed(message),
        };
        let mut actions = Vec::new();
        let holds = !matches!(answer, PartAnswer::Failed(_));
        match self.resources.answer(revision, &host, answer) {
            Some((answered, released)) => {
                self.push_released(released, &mut actions);
                if answered {
                    self.complete_transaction(&mut actions);
                }
            }
            // The host holds a part the table counts as holding nothing: a worker restart sent it again after it was
            // aborted or counted as failed. It is aborted again.
            None if holds && self.resources.is_dropped_part(Creator::Transaction(revision), &host) => {
                actions.push(Action::AbortResources {
                    revision,
                    hosts: vec![host],
                });
            }
            None => {}
        }
        self.settle_resources(actions)
    }

    /// Reports that a resource transaction's timeout passed ([`Action::PrepareResources`]): every host that has not
    /// acknowledged its part is sent [`Action::AbortResources`], every host whose publication changes publishes what
    /// was prepared, and the transaction is retried with backoff.
    ///
    /// A timeout is not a failure: a host that fails to prepare an addition leaves it out of the publication, while a
    /// host that does not answer is aborted. A timeout of a transaction that is no longer being prepared is ignored.
    pub fn resources_timed_out(&mut self, revision: ResourcesRevision) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.resources.in_flight(revision) {
            self.complete_transaction(&mut actions);
        }
        self.settle_resources(actions)
    }

    /// Reports that an encode host drained a lease ([`Action::ReleaseLeases`]): its last payload is in its delivery
    /// host's input, and its identifier is free again. The recipients only it named are released.
    ///
    /// A report of a lease the core is not releasing is ignored.
    pub fn lease_released(&mut self, host: HostId, lease: LeaseRef) -> Vec<Action> {
        let mut actions = Vec::new();
        match self.resources.lease_released(&host, &lease) {
            Some(released) => self.push_released(released, &mut actions),
            None => tracing::debug!(
                %host,
                lease = lease.lease.get(),
                output = %lease.key.output,
                "Ignoring the release of a lease the installer is not releasing."
            ),
        }
        self.settle_resources(actions)
    }

    /// Reports that a delivery host's recipient incarnation retired, as the host's event channel says, with what its
    /// drain did; its slot is free again, and a preparation that waits for its exit proceeds.
    ///
    /// A report of an incarnation the core does not hold is ignored: one an abort dropped, for example.
    pub fn recipient_retired(&mut self, recipient: RecipientRef, report: RetirementReport) -> Vec<Action> {
        let mut actions = Vec::new();
        match self.resources.recipient_retired(&recipient) {
            Some(released) => {
                tracing::debug!(%recipient, ?report, "A recipient retired.");
                self.push_released(released, &mut actions);
                self.after_exit(&mut actions);
            }
            None => tracing::debug!(%recipient, "Ignoring the exit of a recipient the installer does not hold."),
        }
        self.settle_resources(actions)
    }

    /// Reports that a delivery host's recipient incarnation ended on an error outside a drain, as the host's event
    /// channel says: the core marks it failed, and prepares its next incarnation into the same slot through a resource
    /// transaction, with backoff (1 to 30 s). Leases keep naming its slot, so recovery needs no plan change.
    ///
    /// A report of an incarnation the core does not hold is ignored.
    pub fn recipient_failed(&mut self, recipient: RecipientRef, reason: &str) -> Vec<Action> {
        let mut actions = Vec::new();
        match self.resources.recipient_failed(&recipient) {
            Some(released) => {
                tracing::warn!(
                    %recipient,
                    reason,
                    "A recipient's I/O loop failed. Its next incarnation is prepared after a backoff."
                );
                self.push_released(released, &mut actions);
                // Its own backoff, whatever else waits: its next incarnation is prepared once it passed.
                if let Some((host, failures)) = self.resources.failed(&recipient) {
                    let delay = self.config.retry_delay(failures);
                    self.start_wait(Wait::Recipient(host, recipient.id.clone()), delay, &mut actions);
                }
                self.after_exit(&mut actions);
            }
            None => tracing::debug!(%recipient, "Ignoring the failure of a recipient the installer does not hold."),
        }
        self.settle_resources(actions)
    }

    /// Settles a resource event: the waiting candidate may start, and the artifact state is settled.
    fn settle_resources(&mut self, mut actions: Vec<Action>) -> Vec<Action> {
        self.try_start(&mut actions);
        self.settle(actions, super::artifacts::Settle::Quiet)
    }

    /// After an incarnation exited, or a commit found a reopened recipient failed: a transaction may now prepare its
    /// successor, unless one is in flight, whose completion follows the table again.
    pub(super) fn after_exit(&mut self, actions: &mut Vec<Action>) {
        if self.resources.transaction_in_flight().is_none() {
            self.start_transaction(actions);
        }
    }

    /// Follows the latest lowering with a resource transaction: supersedes the one in flight if it follows another
    /// lowering, and begins one for the latest lowering if its resources differ from what the hosts published.
    pub(super) fn follow_lowering(&mut self, actions: &mut Vec<Action>) {
        let Some(lowered) = self.default_program.clone() else {
            return;
        };
        if self.resources.transaction_in_flight().is_some() {
            if !self.resources.transaction_follows_other(&lowered.resources) {
                return;
            }
            let newer = self.resources.transaction(&lowered.resources);
            let superseded = self.begin_transaction(newer, &lowered, actions);
            if let Some((revision, hosts, released)) = superseded {
                if !hosts.is_empty() {
                    actions.push(Action::AbortResources { revision, hosts });
                }
                self.push_released(released, actions);
            }
            return;
        }
        self.start_transaction(actions);
    }

    /// Begins a resource transaction for the latest lowering if its resources differ from what the hosts published,
    /// and nothing is in flight.
    fn start_transaction(&mut self, actions: &mut Vec<Action>) {
        let Some(lowered) = self.default_program.clone() else {
            return;
        };
        if self.resources.transaction_in_flight().is_some() {
            return;
        }
        let transaction = self.resources.transaction(&lowered.resources);
        self.begin_transaction(transaction, &lowered, actions);
    }

    /// Begins `transaction`, superseding the one in flight, if any, and returns what the superseded one held, for the
    /// caller to abort after the newer one's preparation is sent. A transaction with nothing to prepare publishes at
    /// once.
    fn begin_transaction(
        &mut self, transaction: Option<crate::install::ResourceTransaction>, lowered: &Arc<Plan>,
        actions: &mut Vec<Action>,
    ) -> Option<(ResourcesRevision, Vec<HostId>, Released)> {
        let Some(transaction) = transaction else {
            return self.resources.supersede();
        };
        let superseded = self.resources.begin(&transaction, &lowered.resources);
        if transaction.prepare.is_empty() {
            // Nothing to prepare: the superseded transaction is aborted before the publication, which no longer names
            // what only it prepared.
            if let Some((revision, hosts, released)) = superseded {
                if !hosts.is_empty() {
                    actions.push(Action::AbortResources { revision, hosts });
                }
                self.push_released(released, actions);
            }
            self.complete_transaction(actions);
            return None;
        }
        actions.push(Action::PrepareResources {
            revision: transaction.revision,
            hosts: transaction.prepare,
            timeout: self.config.prepare_timeout,
            admitted: transaction.admitted,
        });
        superseded
    }

    /// Completes the transaction in flight: aborts the hosts that did not answer, publishes, releases what the
    /// publication left without a user, and schedules a retry if an addition was left out.
    fn complete_transaction(&mut self, actions: &mut Vec<Action>) {
        let Some(completed) = self.resources.complete() else {
            return;
        };
        let revision = self.resources.revision();
        if !completed.aborted.is_empty() {
            actions.push(Action::AbortResources {
                revision,
                hosts: completed.aborted.clone(),
            });
        }
        for publish in &completed.publish {
            for recipient in &publish.recipients {
                self.transactions.failures.remove(&recipient.recipient.id);
            }
        }
        for (recipient, reason) in &completed.failures {
            self.transactions.failures.insert(recipient.id.clone(), reason.clone());
        }
        // A recipient the latest lowering no longer declares is not tried again.
        if let Some(lowered) = &self.default_program {
            self.transactions
                .failures
                .retain(|id, _| lowered.resources.recipient(id).is_some());
        }
        self.transactions.unanswered = completed
            .aborted
            .iter()
            .map(|host| {
                let reason = format!(
                    "delivery host `{}` did not acknowledge resource transaction {} within {:?}, and is tried again",
                    host, revision, self.config.prepare_timeout
                );
                (host.clone(), reason)
            })
            .collect();
        if !completed.publish.is_empty() {
            actions.push(Action::PublishResources {
                revision,
                hosts: completed.publish,
            });
        }
        self.push_released(completed.released, actions);
        if completed.left_out {
            self.transactions.attempts = self.transactions.attempts.saturating_add(1);
            let delay = self.config.retry_delay(self.transactions.attempts);
            self.schedule_resources_retry(delay, actions);
        } else {
            self.transactions.attempts = 0;
            // What changed while it was being prepared, such as a failed recipient's next incarnation coming due or an
            // exit reported, follows now. A transaction that prepared nothing was computed from the table as it is.
            if completed.prepared {
                self.start_transaction(actions);
            }
        }
        // A prepared generation that waited for the publication passes its barrier now, unless it waits for the next.
        self.resume_barrier(actions);
    }

    /// Asks for [`retry`][Self::retry] after `delay`, for resource transactions, unless the next transaction waits
    /// already: it then tries again after the delay it waits for.
    fn schedule_resources_retry(&mut self, delay: std::time::Duration, actions: &mut Vec<Action>) {
        if !self.waits.waiting(&Wait::Transaction) {
            self.start_wait(Wait::Transaction, delay, actions);
        }
    }

    /// Handles the waits of resources that ended: each failed recipient whose own backoff passed is due for its next
    /// incarnation, and the transaction of the latest lowering is tried again if a wait of a resource ended.
    pub(super) fn retry_resources(&mut self, due: &[Wait], actions: &mut Vec<Action>) {
        let mut any = false;
        for wait in due {
            match wait {
                Wait::Transaction => any = true,
                Wait::Recipient(host, id) => {
                    self.resources.failure_due(host, id);
                    any = true;
                }
                Wait::Candidate | Wait::Cut => {}
            }
        }
        if any && self.resources.transaction_in_flight().is_none() {
            self.start_transaction(actions);
        }
    }

    /// Returns the degraded state the resources add to the status summary: why each native resource a transaction
    /// could not prepare was left out, until one prepares it.
    pub(super) fn resources_degraded(&self) -> Option<String> {
        let reasons = self
            .transactions
            .failures
            .iter()
            .map(|(id, reason)| {
                format!(
                    "recipient `{}` could not be prepared, and is tried again: {}",
                    id, reason
                )
            })
            .chain(self.transactions.unanswered.values().cloned())
            .collect::<Vec<_>>();
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }

    /// Records the resources of a generation's preparation, returning what each host prepares, and the credits it
    /// lists: the leases and recipients the table plans for the candidate, and the kernel instances each slot builds,
    /// on every lane of the slot.
    pub(super) fn plan_generation(
        &mut self, generation: GenerationId, plan: &Plan, active: Option<GenerationId>, reuse: &[SlotKernels],
    ) -> crate::install::ResourcePlan {
        let mut planned = self.resources.plan(plan);
        let kernels = reuse
            .iter()
            .map(|slot| {
                let mut held = slot
                    .reuse
                    .iter()
                    .map(|key| {
                        let builder = active
                            .and_then(|active| self.resources.builder_of(active, slot.slot, key))
                            .or(active)
                            .unwrap_or(generation);
                        (key.clone(), builder)
                    })
                    .collect::<Vec<_>>();
                held.extend(slot.build.iter().map(|key| (key.clone(), generation)));
                let bytes = slot_instance_bytes(plan, &self.registry, &self.frame, slot.slot, &slot.build);
                (slot.slot, (held, bytes))
            })
            .collect::<BTreeMap<SlotId, (Vec<(KernelKey, GenerationId)>, u64)>>();
        planned.admitted = self.resources.record(generation, plan, &planned, kernels);
        planned
    }

    /// Returns the actions of aborting a generation that never committed, in every slot and in every host it was sent
    /// a part to: its new leases drain, and the credits of what it created return.
    pub(super) fn push_abort(&mut self, generation: GenerationId, actions: &mut Vec<Action>) {
        let hosts = self.resources.hosts_of(generation);
        let released = self.resources.abort(generation);
        actions.push(Action::Abort {
            generation,
            slots: self.all_slots(),
            hosts,
        });
        self.push_released(released, actions);
    }

    /// Releases what a retired generation used.
    pub(super) fn release_retired(&mut self, generation: GenerationId, actions: &mut Vec<Action>) {
        let released = self.resources.retire(generation);
        self.push_released(released, actions);
    }

    /// Turns what the table released into actions: drain leases and release recipients at once, and return the
    /// credits once the event settles ([`take_released_credits`][Self::take_released_credits]), in one action.
    pub(super) fn push_released(&mut self, released: Released, actions: &mut Vec<Action>) {
        for (host, leases) in released.leases {
            if !leases.is_empty() {
                actions.push(Action::ReleaseLeases { host, leases });
            }
        }
        for (host, recipients) in released.recipients {
            if !recipients.is_empty() {
                actions.push(Action::ReleaseRecipients { host, recipients });
            }
        }
        self.released_credits.extend(released.credits);
    }

    /// Returns the action that returns every credit the event released, if it released any: the last action of every
    /// event.
    ///
    /// A credit the event listed again after releasing it, as an idle host's queue is when the host's last recipient
    /// is dropped and another created in one event, is not returned: the caller, which acquires only what it does not
    /// hold, keeps holding it for the new listing.
    pub(super) fn take_released_credits(&mut self) -> Option<Action> {
        let mut credits = std::mem::take(&mut self.released_credits);
        credits.retain(|credit| !self.resources.holds_credit(credit));
        let mut seen = BTreeSet::new();
        credits.retain(|credit| seen.insert(credit.clone()));
        (!credits.is_empty()).then_some(Action::ReleaseCredits { credits })
    }
}

/// Returns the bytes the kernel instances `built` of slot `slot` hold on every lane `frame` lays the slot out on: what
/// [`instance_bytes`] returns for one lane, times the slot's lanes, since every lane builds instances of its own from
/// the same specs.
pub(super) fn slot_instance_bytes(
    plan: &Plan, registry: &OperatorRegistry, frame: &Frame, slot: SlotId, built: &[KernelKey],
) -> u64 {
    let lanes = frame.slot(slot).map_or(1, |slot| u64::from(slot.lanes.max(1)));
    instance_bytes(plan, registry, built).saturating_mul(lanes)
}

/// Returns the bytes the kernel instances `built` declare, as their operators specify their bounds: what the
/// instances of a generation's new kernels in one slot hold on one lane. A kernel whose node or operator cannot be
/// found declares nothing: compilation already checked them.
fn instance_bytes(plan: &Plan, registry: &OperatorRegistry, built: &[KernelKey]) -> u64 {
    built
        .iter()
        .filter_map(|key| plan.node(&key.node))
        .filter_map(|node| registry.configure(node.op.as_str(), &node.params).ok())
        .map(|operator| {
            let bounds = ComponentRegistry::default();
            operator.specify_bounds(&mut bounds.bounds_builder(&SubsystemIdentifier::from_segments(["kernel"])));
            u64::try_from(bounds.as_bounds().total_firm_limit_bytes()).unwrap_or(u64::MAX)
        })
        .fold(0, u64::saturating_add)
}
