//! Fakes of the encode and delivery hosts, for tests of the installer's resource actions.
//!
//! Each fake serves a host's control mailbox as the real host does, without encoding or sending anything: it marks its
//! mailbox running, records every message it handles, and acknowledges each one, rejecting the next one when a test
//! asks it to.
//!
//! The delivery host keeps the recipient table a real one keeps, as data. It acknowledges a repeated prepare of a
//! reference it holds as done, and rejects one whose slot another live incarnation holds; it ignores a release or a
//! reopen whose reference does not match its table; it answers a reopen of an incarnation whose loop exited with
//! [`ReopenReply::Exited`]; it records which preparations ([`Creator`]) created and reopened each recipient, so that an
//! abort, which names only its generation or revision, undoes what that preparation alone did; and it reports each
//! incarnation's exit exactly once on its event channel, those an abort drops included, after it acknowledged the
//! message that led to it. A released recipient retires at once, unless the test holds drains
//! ([`FakeHost::hold_drains`]): it then drains until the test retires it ([`FakeHost::retire`]).

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use saluki_common::buf::FrozenChunkedBytesBuffer;
use saluki_components::{
    encoders::encode_host::EncodeControl,
    forwarders::delivery::{
        Creator, DeliveryControl, DeliveryEvent, DrainReport, RecipientExit, RecipientRef, RecipientSlot, ReopenReply,
    },
};
use saluki_core::execution::{
    control::{ControlMailbox, ControlReceiver},
    EntryId,
};
use saluki_error::generic_error;
use tokio::{sync::mpsc, task::JoinHandle};

/// The body of the payloads the Datadog delivery hosts carry.
pub(super) type Body = FrozenChunkedBytesBuffer;

/// The state of a recipient in a fake delivery host's table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RecipientState {
    /// Prepared, or reopened.
    Live,

    /// Released, and draining until the test retires it.
    Draining,

    /// Its loop ended on an error, which was reported: its slot stays allocated until its next incarnation is prepared
    /// into it, or it is released.
    Failed,
}

/// What a fake host did with one control message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum HostEvent {
    /// An encode host prepared a lease.
    LeasePrepared(EntryId),

    /// An encode host drained a lease.
    LeaseDrained(EntryId),

    /// A delivery host prepared recipients for a preparation.
    RecipientsPrepared(Creator, Vec<RecipientRef>),

    /// A delivery host published a native set and a withdrawn set.
    Published {
        /// The revision published.
        revision: u64,

        /// The native set.
        native_set: Vec<RecipientRef>,

        /// The withdrawn set, by identity.
        withdrawn: Vec<String>,
    },

    /// A delivery host released the recipients of a release whose references matched its table.
    Released(Vec<RecipientRef>),

    /// A delivery host answered a preparation's reopen, for each recipient.
    Reopened(Creator, Vec<(RecipientRef, ReopenReply)>),

    /// A delivery host undid what an aborted generation's preparation, or an aborted resource transaction, did.
    Aborted {
        /// The aborted preparation.
        creator: Creator,

        /// The recipients it created that no other preparation created too, which the host dropped.
        dropped: Vec<RecipientRef>,

        /// The recipients it reopened that no other preparation holds reopened, which went back to draining.
        drained: Vec<RecipientRef>,
    },

    /// A host rejected a message.
    Rejected(String),
}

/// One recipient of a fake delivery host's table.
struct Held {
    recipient: RecipientRef,
    state: RecipientState,
    /// The preparations that created it: the first, and any that prepared it again.
    created_by: Vec<Creator>,
    /// The preparations that reopened it since it was released, and have not been aborted.
    reopened_by: Vec<Creator>,
    /// Whether it was released, so that it returns to draining once no preparation holds it reopened.
    released: bool,
}

/// What a fake host records, the rejection it answers its next message with, and a delivery host's table.
#[derive(Default)]
struct Shared {
    events: Vec<HostEvent>,
    reject_next: Option<String>,
    recipients: BTreeMap<RecipientSlot, Held>,
    hold_drains: bool,
    exits: Option<mpsc::Sender<DeliveryEvent>>,
}

impl Shared {
    /// Returns the state of `recipient` if the table holds it under that reference.
    fn state(&self, recipient: &RecipientRef) -> Option<RecipientState> {
        self.held(recipient).map(|held| held.state)
    }

    fn held(&self, recipient: &RecipientRef) -> Option<&Held> {
        self.recipients
            .get(&recipient.slot)
            .filter(|held| held.recipient == *recipient)
    }

    fn held_mut(&mut self, recipient: &RecipientRef) -> Option<&mut Held> {
        self.recipients
            .get_mut(&recipient.slot)
            .filter(|held| held.recipient == *recipient)
    }

    fn set_state(&mut self, recipient: &RecipientRef, state: RecipientState) {
        if let Some(held) = self.held_mut(recipient) {
            held.state = state;
        }
    }

    /// Undoes what `creator` did: drops the recipients only it created, and returns those only it held reopened to
    /// draining. Every dropped incarnation whose loop had not failed reports its exit, a retirement that delivered
    /// nothing, after the acknowledgment of the abort: every incarnation's exit is reported exactly once, and the
    /// installer ignores the exits of incarnations it no longer holds.
    fn abort(&mut self, creator: Creator) -> Handled {
        let mut dropped = Vec::new();
        let mut drained = Vec::new();
        let mut exits = Vec::new();
        self.recipients.retain(|_, held| {
            held.created_by.retain(|by| *by != creator);
            if held.created_by.is_empty() {
                dropped.push(held.recipient.clone());
                if held.state != RecipientState::Failed {
                    exits.push(retired(held.recipient.clone()));
                }
                return false;
            }
            let reopened = held.reopened_by.len();
            held.reopened_by.retain(|by| *by != creator);
            if held.reopened_by.len() < reopened
                && held.reopened_by.is_empty()
                && held.released
                && held.state == RecipientState::Live
            {
                held.state = RecipientState::Draining;
                drained.push(held.recipient.clone());
            }
            true
        });
        Handled {
            event: HostEvent::Aborted {
                creator,
                dropped,
                drained,
            },
            outcome: Ok(()),
            exits,
        }
    }

    /// Reports an exit on the event channel, which, bounded as a real host's is, never waits.
    fn report(&self, exit: DeliveryEvent) {
        if let Some(exits) = &self.exits {
            let _ = exits.try_send(exit);
        }
    }
}

/// What handling one control message did: what is recorded, the acknowledgment, and the exits to report after it.
struct Handled {
    event: HostEvent,
    outcome: Result<(), String>,
    exits: Vec<DeliveryEvent>,
}

impl Handled {
    fn acknowledged(event: HostEvent) -> Self {
        Self {
            event,
            outcome: Ok(()),
            exits: Vec::new(),
        }
    }

    fn rejected(reason: String) -> Self {
        Self {
            event: HostEvent::Rejected(reason.clone()),
            outcome: Err(reason),
            exits: Vec::new(),
        }
    }
}

fn retired(recipient: RecipientRef) -> DeliveryEvent {
    DeliveryEvent::RecipientExited {
        recipient,
        outcome: RecipientExit::Retired(DrainReport::default()),
    }
}

/// A fake host: the task serving its mailbox, and what it records.
pub(super) struct FakeHost {
    shared: Arc<Mutex<Shared>>,
    task: JoinHandle<()>,
}

impl FakeHost {
    /// Serves an encode host's mailbox.
    pub(super) fn encode(mailbox: ControlMailbox<EncodeControl>) -> Self {
        Self::serve(
            mailbox.into_receiver_for("encode host"),
            Shared::default(),
            |message, _| {
                Handled::acknowledged(match message {
                    EncodeControl::Prepare(prepare) => HostEvent::LeasePrepared(prepare.spec.lease),
                    EncodeControl::Drain { lease, .. } => HostEvent::LeaseDrained(lease),
                })
            },
        )
    }

    /// Serves a delivery host's mailbox, reporting each recipient incarnation's exit on `events`.
    pub(super) fn delivery(
        mailbox: ControlMailbox<DeliveryControl<Body>>, events: mpsc::Sender<DeliveryEvent>,
    ) -> Self {
        let shared = Shared {
            exits: Some(events),
            ..Shared::default()
        };
        Self::serve(mailbox.into_receiver_for("delivery host"), shared, delivery_control)
    }

    fn serve<C, F>(mut receiver: ControlReceiver<C>, shared: Shared, handle: F) -> Self
    where
        C: Send + 'static,
        F: Fn(C, &mut Shared) -> Handled + Send + 'static,
    {
        let shared = Arc::new(Mutex::new(shared));
        receiver.mark_running();
        let task = tokio::spawn({
            let shared = Arc::clone(&shared);
            async move {
                while let Some((message, reply)) = receiver.recv().await {
                    let (outcome, exits) = {
                        let mut shared = shared.lock().unwrap();
                        let handled = match shared.reject_next.take() {
                            Some(reason) => Handled::rejected(reason),
                            None => handle(message, &mut shared),
                        };
                        shared.events.push(handled.event);
                        (handled.outcome, handled.exits)
                    };
                    match outcome {
                        Ok(()) => reply.ack(),
                        Err(reason) => reply.reject(generic_error!("{}", reason)),
                    }
                    // A host reports what a message led to after it accepted the message.
                    let shared = shared.lock().unwrap();
                    for exit in exits {
                        shared.report(exit);
                    }
                }
            }
        });
        Self { shared, task }
    }

    /// Makes the host reject its next message with `reason`, without handling it.
    pub(super) fn reject_next(&self, reason: impl Into<String>) {
        self.shared.lock().unwrap().reject_next = Some(reason.into());
    }

    /// Makes a delivery host keep the recipients it releases from now on draining, until [`retire`][Self::retire]
    /// ends their drains.
    pub(super) fn hold_drains(&self) {
        self.shared.lock().unwrap().hold_drains = true;
    }

    /// Ends the drain of a released recipient: it retires, its slot is free, and its exit is reported. Returns `false`
    /// if the table holds no draining recipient under that reference.
    pub(super) fn retire(&self, recipient: &RecipientRef) -> bool {
        let mut shared = self.shared.lock().unwrap();
        if shared.state(recipient) != Some(RecipientState::Draining) {
            return false;
        }
        shared.recipients.remove(&recipient.slot);
        shared.report(retired(recipient.clone()));
        true
    }

    /// Makes a recipient's loop end on an error: its slot stays allocated, and its exit is reported. Returns `false` if
    /// the table holds no live or draining recipient under that reference.
    pub(super) fn fail(&self, recipient: &RecipientRef, reason: &str) -> bool {
        let mut shared = self.shared.lock().unwrap();
        match shared.state(recipient) {
            Some(RecipientState::Live | RecipientState::Draining) => {
                shared.set_state(recipient, RecipientState::Failed);
                shared.report(DeliveryEvent::RecipientExited {
                    recipient: recipient.clone(),
                    outcome: RecipientExit::Failed(reason.to_string()),
                });
                true
            }
            Some(RecipientState::Failed) | None => false,
        }
    }

    /// Puts recipients in a delivery host's table as a `Prepare` of `creator` does, for a test that cannot build their
    /// pieces: a `Prepare` carries pieces only a delivery host's factory builds, which nothing outside
    /// `saluki-components` can construct.
    pub(super) fn hold(&self, creator: Creator, recipients: &[RecipientRef]) -> Result<(), String> {
        prepare_recipients(&mut self.shared.lock().unwrap(), creator, recipients, &[])
    }

    /// Serves the wiring of every fixed host of `boot` with fakes, as the hosts a topology builds would answer: each
    /// delivery host holds the recipients the boot generation runs, live and committed, and each encode host takes
    /// the leases the real factory builds. Returns the fakes, by host, which the caller keeps for as long as the
    /// installer runs.
    pub(super) fn serve_boot_hosts(boot: &mut super::installer::BootTopology) -> Vec<(FakeHost, FakeHost)> {
        let hosts = boot.hosts_at_boot.iter().collect::<Vec<_>>();
        hosts
            .into_iter()
            .filter_map(|host| boot.hosts.take(host.delivery))
            .map(|wiring| {
                let _ = wiring.factories.leases.set(
                    saluki_components::encoders::encode_host::MetricsEncodeHostConfiguration::new().lease_factory(),
                );
                let delivery = Self::delivery(wiring.delivery, wiring.events);
                let boot_recipients = wiring
                    .boot_publish
                    .iter()
                    .flat_map(|publish| &publish.recipients)
                    .map(|prepare| crate::programmable_metrics::resources::specs::delivery_ref(&prepare.recipient))
                    .collect::<Vec<_>>();
                delivery
                    .hold(
                        Creator::Generation(saluki_core::execution::GenerationId::new(1)),
                        &boot_recipients,
                    )
                    .expect("the boot recipients are held");
                (delivery, Self::encode(wiring.encode))
            })
            .collect()
    }

    /// Returns a delivery host's table, by slot.
    pub(super) fn recipients(&self) -> Vec<(RecipientRef, RecipientState)> {
        self.shared
            .lock()
            .unwrap()
            .recipients
            .values()
            .map(|held| (held.recipient.clone(), held.state))
            .collect()
    }

    /// Returns what the host did, in order.
    pub(super) fn events(&self) -> Vec<HostEvent> {
        self.shared.lock().unwrap().events.clone()
    }
}

impl Drop for FakeHost {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Adds prepared recipients to a fake delivery host's table, all or none, once every recipient the preparation requires
/// is live.
///
/// A slot holds one incarnation until it retires, and a failed one's next incarnation takes it over; a repeated prepare
/// of a reference the table holds is done, and only records `creator` as one of the recipient's creators.
fn prepare_recipients(
    shared: &mut Shared, creator: Creator, recipients: &[RecipientRef], requires: &[RecipientRef],
) -> Result<(), String> {
    for required in requires {
        if shared.state(required) != Some(RecipientState::Live) {
            return Err(format!(
                "recipient `{}` (incarnation {}) is not live",
                required.id, required.incarnation
            ));
        }
    }
    for recipient in recipients {
        if let Some(held) = shared.recipients.get(&recipient.slot) {
            if held.recipient != *recipient && held.state != RecipientState::Failed {
                return Err(format!(
                    "slot {} holds recipient `{}` (incarnation {}), and cannot be prepared for recipient `{}` \
                     (incarnation {})",
                    recipient.slot.get(),
                    held.recipient.id,
                    held.recipient.incarnation,
                    recipient.id,
                    recipient.incarnation
                ));
            }
        }
    }
    for recipient in recipients {
        match shared.held_mut(recipient) {
            Some(held) => {
                if !held.created_by.contains(&creator) {
                    held.created_by.push(creator);
                }
            }
            None => {
                shared.recipients.insert(
                    recipient.slot,
                    Held {
                        recipient: recipient.clone(),
                        state: RecipientState::Live,
                        created_by: vec![creator],
                        reopened_by: Vec::new(),
                        released: false,
                    },
                );
            }
        }
    }
    Ok(())
}

/// Handles one control message of a fake delivery host.
fn delivery_control(message: DeliveryControl<Body>, shared: &mut Shared) -> Handled {
    match message {
        DeliveryControl::Prepare {
            creator,
            recipients,
            requires,
        } => {
            let recipients = recipients
                .into_iter()
                .map(|prepared| prepared.recipient)
                .collect::<Vec<_>>();
            match prepare_recipients(shared, creator, &recipients, &requires) {
                Ok(()) => Handled::acknowledged(HostEvent::RecipientsPrepared(creator, recipients)),
                Err(reason) => Handled::rejected(reason),
            }
        }
        DeliveryControl::Publish {
            revision,
            native_set,
            withdrawn,
            ..
        } => Handled::acknowledged(HostEvent::Published {
            revision,
            native_set,
            withdrawn: withdrawn.iter().map(|id| id.to_string()).collect(),
        }),
        DeliveryControl::Release { recipients } => {
            let mut released = Vec::new();
            let mut exits = Vec::new();
            for recipient in recipients {
                match shared.state(&recipient) {
                    Some(RecipientState::Live) if shared.hold_drains => {
                        if let Some(held) = shared.held_mut(&recipient) {
                            // A release starts a new drain, which no earlier reopen holds open.
                            held.state = RecipientState::Draining;
                            held.released = true;
                            held.reopened_by.clear();
                        }
                        released.push(recipient);
                    }
                    // A fake recipient has nothing queued: it retires at once.
                    Some(RecipientState::Live) => {
                        shared.recipients.remove(&recipient.slot);
                        exits.push(retired(recipient.clone()));
                        released.push(recipient);
                    }
                    // Its exit was reported when it failed: the release only frees its slot.
                    Some(RecipientState::Failed) => {
                        shared.recipients.remove(&recipient.slot);
                        released.push(recipient);
                    }
                    // A repeated release keeps the drain it started; a stale reference is ignored.
                    Some(RecipientState::Draining) | None => {}
                }
            }
            Handled {
                event: HostEvent::Released(released),
                outcome: Ok(()),
                exits,
            }
        }
        DeliveryControl::Reopen {
            creator,
            recipients,
            reply,
        } => {
            let answers = recipients
                .into_iter()
                .map(|settings| {
                    let recipient = settings.recipient;
                    let answer = match shared.held_mut(&recipient) {
                        Some(held) if held.state != RecipientState::Failed => {
                            held.state = RecipientState::Live;
                            if !held.reopened_by.contains(&creator) {
                                held.reopened_by.push(creator);
                            }
                            ReopenReply::Reopened
                        }
                        // The incarnation's loop exited: its exit is, or was, reported on the event channel.
                        _ => ReopenReply::Exited,
                    };
                    (recipient, answer)
                })
                .collect::<Vec<_>>();
            let _ = reply.send(answers.clone());
            Handled::acknowledged(HostEvent::Reopened(creator, answers))
        }
        DeliveryControl::Abort { generation } => shared.abort(Creator::Generation(generation)),
        DeliveryControl::AbortResources { revision } => shared.abort(Creator::Transaction(revision)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use saluki_components::forwarders::delivery::{
        DeliverySettings, IntakeAuthorities, LeaseKeyDigest, PayloadClasses, RecipientOrigin, RecipientPolicy,
        RecipientProtocol, RecipientSettings, RecipientSpec, RetrySettings, V3Acceptance,
    };
    use saluki_core::execution::{
        control::{ControlError, ControlSender},
        control_mailbox, GenerationId,
    };
    use tokio::sync::oneshot;

    use super::*;

    /// The resource transaction most tests prepare their recipients with.
    const TRANSACTION: Creator = Creator::Transaction(1);

    /// The generation most tests prepare and reopen with.
    const GENERATION: Creator = Creator::Generation(GenerationId::new(1));

    fn recipient(slot: u16, id: &str, incarnation: u64) -> RecipientRef {
        RecipientRef {
            slot: RecipientSlot::new(slot),
            id: id.into(),
            incarnation,
        }
    }

    fn settings(recipient: &RecipientRef) -> RecipientSettings {
        RecipientSettings {
            recipient: recipient.clone(),
            spec: RecipientSpec {
                id: recipient.id.clone(),
                endpoint: "https://team-d.example.com".to_string(),
                credential: "credentials/team_d".into(),
                classes: PayloadClasses::MetricsOnly,
                protocol: RecipientProtocol::DatadogIntake {
                    authorities: IntakeAuthorities::default(),
                    allow_arbitrary_tags: false,
                    v3: V3Acceptance::default(),
                },
                policy: RecipientPolicy::default(),
                origin: RecipientOrigin::Declared {
                    name: "team_d".into(),
                    credential: "team_d".into(),
                },
            },
        }
    }

    fn delivery_settings() -> Arc<DeliverySettings> {
        Arc::new(DeliverySettings {
            endpoint_concurrency: 1,
            endpoint_concurrency_multiplier: 1,
            endpoint_buffer_size: 100,
            request_timeout: Duration::from_secs(20),
            retry: RetrySettings {
                backoff_factor: 2.0,
                backoff_base: 2.0,
                backoff_max: 64.0,
                recovery_error_decrease_factor: 2,
                recovery_reset: false,
                queue_max_size_bytes: 15 * 1024 * 1024,
                storage_max_size_bytes: 0,
                flush_to_disk_mem_ratio: 0.5,
                storage_max_disk_ratio: 0.8,
                outdated_file_in_days: 10,
            },
            proxy: Default::default(),
            http_protocol: Default::default(),
            connection_reset_interval: Duration::ZERO,
            skip_ssl_validation: false,
            min_tls_version: Default::default(),
            tls_handshake_timeout: Duration::from_secs(10),
            api_key_validation_interval: Duration::from_secs(3600),
        })
    }

    /// A fake delivery host with its sender and event channel.
    fn delivery_host() -> (
        FakeHost,
        ControlSender<DeliveryControl<Body>>,
        mpsc::Receiver<DeliveryEvent>,
    ) {
        let (sender, mailbox) = control_mailbox::<DeliveryControl<Body>>();
        let (events_tx, events) = mpsc::channel(8);
        (FakeHost::delivery(mailbox, events_tx), sender, events)
    }

    /// Prepares recipients for `creator` as a `Prepare` message does. A `Prepare` carries pieces only a delivery host's
    /// factory builds, which nothing outside `saluki-components` can construct yet, so the test applies the message's
    /// table rule directly.
    fn prepare(host: &FakeHost, creator: Creator, recipients: &[RecipientRef]) -> Result<(), String> {
        prepare_recipients(&mut host.shared.lock().unwrap(), creator, recipients, &[])
    }

    /// Sends a `Prepare` that adds nothing and requires `requires`, and returns its acknowledgment.
    async fn require(
        sender: &ControlSender<DeliveryControl<Body>>, requires: &[RecipientRef],
    ) -> Result<(), ControlError> {
        sender
            .send(DeliveryControl::Prepare {
                creator: GENERATION,
                recipients: vec![],
                requires: requires.to_vec(),
            })
            .await
    }

    async fn reopen(
        sender: &ControlSender<DeliveryControl<Body>>, creator: Creator, recipients: &[RecipientRef],
    ) -> Vec<(RecipientRef, ReopenReply)> {
        let (reply, answers) = oneshot::channel();
        sender
            .send(DeliveryControl::Reopen {
                creator,
                recipients: recipients.iter().map(settings).collect(),
                reply,
            })
            .await
            .unwrap();
        answers.await.unwrap()
    }

    async fn release(sender: &ControlSender<DeliveryControl<Body>>, recipients: &[RecipientRef]) {
        sender
            .send(DeliveryControl::Release {
                recipients: recipients.to_vec(),
            })
            .await
            .unwrap();
    }

    /// Returns every exit reported so far.
    async fn exits(events: &mut mpsc::Receiver<DeliveryEvent>) -> Vec<DeliveryEvent> {
        // The fake reports right after it acknowledges, in the same turn of its task.
        tokio::task::yield_now().await;
        let mut exits = Vec::new();
        while let Ok(exit) = events.try_recv() {
            exits.push(exit);
        }
        exits
    }

    #[tokio::test]
    async fn a_fake_encode_host_acknowledges_drains_and_rejects_on_request() {
        let (sender, mailbox) = control_mailbox::<EncodeControl>();
        let host = FakeHost::encode(mailbox);
        assert!(sender.running().await);
        let drain = || EncodeControl::Drain {
            lease: EntryId::new(5),
            key: LeaseKeyDigest {
                output: "dd_metrics_encode".into(),
                binding: [0; 32],
            },
        };
        sender.send(drain()).await.unwrap();
        host.reject_next("unknown lease");
        let rejected = sender.send(drain()).await.unwrap_err();
        assert!(matches!(rejected, ControlError::Rejected { .. }));
        assert_eq!(
            rejected.to_string(),
            "The encode host could not apply the control message: unknown lease"
        );
        assert_eq!(
            host.events(),
            vec![
                HostEvent::LeaseDrained(EntryId::new(5)),
                HostEvent::Rejected("unknown lease".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn a_fake_delivery_host_publishes_and_reports_each_incarnation_retired_once() {
        let (host, sender, mut events) = delivery_host();
        let primary = recipient(0, "dd_out/primary/0/https://app.datadoghq.com", 1);
        sender
            .send(DeliveryControl::Publish {
                revision: 1,
                native_set: vec![primary.clone()],
                settings: delivery_settings(),
                recipients: Vec::new(),
                withdrawn: ["dd_out/additional/0/https://gone.example.com".into()].into(),
            })
            .await
            .unwrap();
        prepare(&host, TRANSACTION, std::slice::from_ref(&primary)).unwrap();

        // A release naming another incarnation in the slot is ignored; the matching one retires the recipient once,
        // however often it is repeated.
        let stale = recipient(0, "dd_out/primary/0/https://app.datadoghq.com", 0);
        release(&sender, std::slice::from_ref(&stale)).await;
        assert!(exits(&mut events).await.is_empty());
        release(&sender, std::slice::from_ref(&primary)).await;
        release(&sender, std::slice::from_ref(&primary)).await;
        assert_eq!(exits(&mut events).await, vec![retired(primary.clone())]);
        assert!(host.recipients().is_empty());

        // A reopen of the retired incarnation answers that its loop exited.
        assert_eq!(
            reopen(&sender, GENERATION, std::slice::from_ref(&primary)).await,
            vec![(primary.clone(), ReopenReply::Exited)]
        );
        assert_eq!(
            host.events(),
            vec![
                HostEvent::Published {
                    revision: 1,
                    native_set: vec![primary.clone()],
                    withdrawn: vec!["dd_out/additional/0/https://gone.example.com".to_string()],
                },
                HostEvent::Released(vec![]),
                HostEvent::Released(vec![primary.clone()]),
                HostEvent::Released(vec![]),
                HostEvent::Reopened(GENERATION, vec![(primary, ReopenReply::Exited)]),
            ]
        );
    }

    #[tokio::test]
    async fn a_fake_delivery_host_holds_drains_reopens_them_and_returns_an_aborted_reopen_to_draining() {
        let (host, sender, mut events) = delivery_host();
        host.hold_drains();
        let declared = recipient(3, "dd_out/declared/team_d/https://team-d.example.com", 7);
        assert!(
            matches!(
                require(&sender, std::slice::from_ref(&declared)).await,
                Err(ControlError::Rejected { .. })
            ),
            "a preparation requiring a recipient the host does not hold is rejected"
        );
        prepare(&host, TRANSACTION, std::slice::from_ref(&declared)).unwrap();
        require(&sender, std::slice::from_ref(&declared)).await.unwrap();

        release(&sender, std::slice::from_ref(&declared)).await;
        assert_eq!(host.recipients(), vec![(declared.clone(), RecipientState::Draining)]);
        assert!(
            exits(&mut events).await.is_empty(),
            "a drain reports nothing until it ends"
        );
        assert!(
            matches!(
                require(&sender, std::slice::from_ref(&declared)).await,
                Err(ControlError::Rejected { .. })
            ),
            "a draining recipient is not live"
        );

        // A generation, twice, and a resource transaction reopen it: it stays live until both are aborted, and the
        // abort of the last one returns it to draining; its drain then ends.
        let transaction = Creator::Transaction(2);
        for creator in [GENERATION, GENERATION, transaction] {
            assert_eq!(
                reopen(&sender, creator, std::slice::from_ref(&declared)).await,
                vec![(declared.clone(), ReopenReply::Reopened)]
            );
        }
        assert_eq!(host.recipients(), vec![(declared.clone(), RecipientState::Live)]);
        sender
            .send(DeliveryControl::Abort {
                generation: GenerationId::new(1),
            })
            .await
            .unwrap();
        assert_eq!(host.recipients(), vec![(declared.clone(), RecipientState::Live)]);
        sender
            .send(DeliveryControl::AbortResources { revision: 2 })
            .await
            .unwrap();
        assert_eq!(host.recipients(), vec![(declared.clone(), RecipientState::Draining)]);
        assert_eq!(
            host.events().last(),
            Some(&HostEvent::Aborted {
                creator: transaction,
                dropped: vec![],
                drained: vec![declared.clone()],
            })
        );
        assert!(host.retire(&declared));
        assert!(!host.retire(&declared), "an incarnation retires once");
        assert_eq!(exits(&mut events).await, vec![retired(declared)]);
    }

    #[tokio::test]
    async fn a_fake_delivery_host_reports_a_failed_loop_once_and_gives_its_slot_to_the_next_incarnation() {
        let (host, sender, mut events) = delivery_host();
        let id = "dd_out/additional/0/https://dual.example.com";
        let failing = recipient(1, id, 2);
        prepare(&host, TRANSACTION, std::slice::from_ref(&failing)).unwrap();
        assert_eq!(
            prepare(&host, TRANSACTION, std::slice::from_ref(&failing)),
            Ok(()),
            "a repeated prepare is done"
        );

        assert!(host.fail(&failing, "disk full"));
        assert!(!host.fail(&failing, "disk full"));
        let failed = |recipient: &RecipientRef| DeliveryEvent::RecipientExited {
            recipient: recipient.clone(),
            outcome: RecipientExit::Failed("disk full".to_string()),
        };
        assert_eq!(exits(&mut events).await, vec![failed(&failing)]);
        assert_eq!(
            reopen(&sender, GENERATION, std::slice::from_ref(&failing)).await,
            vec![(failing.clone(), ReopenReply::Exited)],
            "a failed incarnation is never reopened"
        );

        // The failed incarnation's slot takes its next one, which a live incarnation's slot refuses to another
        // recipient; a release naming the failed one is then stale.
        let next = recipient(1, id, 3);
        prepare(&host, TRANSACTION, std::slice::from_ref(&next)).unwrap();
        assert_eq!(host.recipients(), vec![(next.clone(), RecipientState::Live)]);
        let other = recipient(1, "dd_out/additional/0/https://other.example.com", 4);
        assert!(prepare(&host, TRANSACTION, &[other])
            .unwrap_err()
            .starts_with("slot 1 holds recipient"));
        release(&sender, std::slice::from_ref(&failing)).await;
        assert_eq!(host.recipients(), vec![(next.clone(), RecipientState::Live)]);

        // A release of a failed incarnation, which reported its exit when it failed, frees its slot and reports
        // nothing more.
        assert!(host.fail(&next, "disk full"));
        release(&sender, std::slice::from_ref(&next)).await;
        assert!(host.recipients().is_empty());
        assert_eq!(exits(&mut events).await, vec![failed(&next)]);
    }

    #[tokio::test]
    async fn a_fake_delivery_host_aborts_only_what_no_other_preparation_prepared_again() {
        let (host, sender, mut events) = delivery_host();
        let kept = recipient(0, "dd_out/additional/0/https://kept.example.com", 1);
        let dropped = recipient(1, "dd_out/additional/0/https://dropped.example.com", 2);
        let created = recipient(2, "dd_out/declared/team_d/https://team-d.example.com", 3);

        // A transaction creates two recipients, and the newer one that replaces it creates one of them again; a
        // generation creates a third.
        let newer = Creator::Transaction(2);
        prepare(&host, TRANSACTION, &[kept.clone(), dropped.clone()]).unwrap();
        prepare(&host, newer, std::slice::from_ref(&kept)).unwrap();
        prepare(&host, GENERATION, std::slice::from_ref(&created)).unwrap();

        // Aborting another generation, which prepared nothing, does nothing.
        sender
            .send(DeliveryControl::Abort {
                generation: GenerationId::new(9),
            })
            .await
            .unwrap();
        assert_eq!(host.recipients().len(), 3);

        sender
            .send(DeliveryControl::AbortResources { revision: 1 })
            .await
            .unwrap();
        sender
            .send(DeliveryControl::Abort {
                generation: GenerationId::new(1),
            })
            .await
            .unwrap();
        assert_eq!(host.recipients(), vec![(kept, RecipientState::Live)]);
        let aborts = host.events().into_iter().skip(1).collect::<Vec<_>>();
        assert_eq!(
            aborts,
            vec![
                HostEvent::Aborted {
                    creator: TRANSACTION,
                    dropped: vec![dropped.clone()],
                    drained: vec![],
                },
                HostEvent::Aborted {
                    creator: GENERATION,
                    dropped: vec![created.clone()],
                    drained: vec![],
                },
            ]
        );
        // Every incarnation an abort drops reports its exit once, as a retirement that delivered nothing.
        assert_eq!(exits(&mut events).await, vec![retired(dropped), retired(created)]);
    }
}
