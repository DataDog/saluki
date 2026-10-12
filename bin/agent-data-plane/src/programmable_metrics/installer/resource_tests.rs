//! The installer's resource actions, performed against fakes of the encode and delivery hosts, and what it hands the
//! core back.
//!
//! The core's resource paths are replaced by a recorder, so each test sees exactly what the installer tells the core.
//! Recipient creation needs pieces only a real delivery host's factory builds, so these tests prepare parts that
//! require, reopen, and fail; the real hosts' tests at stage 7's integration prepare the rest.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use saluki_app::accounting::MemoryMode;
use saluki_components::forwarders::delivery::{Creator, DrainReport, RecipientExit, ReopenReply};
use saluki_core::{accounting::ComponentRegistry, execution::EntryId};
use saluki_metric_plan::{
    channel::{ArtifactConfig, ArtifactConfigId},
    install::{HostPrepare, HostPublish, LeaseKey, RecipientPrepare, RecipientSlot},
    ir::{Params, RecipientId},
    resources::{
        DeclaredAccepts, DeclaredOutput, DeclaredProtocol, DeclaredRecipient, ResourcesError, RESOURCES_VERSION,
    },
    validate::Catalog,
};
use saluki_metrics::test::TestRecorder;
use serde_json::{json, Value};

use super::*;
use crate::programmable_metrics::{
    resources::{specs::delivery_ref, DD_OUT, MRF_DD_OUT},
    test_hosts::{FakeHost, HostEvent},
    test_support::{configuration, programmable_metrics, ConfigCell},
};

/// The longest a test waits for the installer.
const PATIENCE: Duration = Duration::from_secs(10);

/// What the installer handed the core.
#[derive(Clone, Debug, PartialEq)]
enum Call {
    HostPrepared(GenerationId, HostId, PrepareOutcome),
    BudgetRefused(GenerationId, CreditKey),
    ResourcesPrepared(ResourcesRevision, HostId, PrepareOutcome),
    ResourcesTimedOut(ResourcesRevision),
    LeaseReleased(HostId, LeaseRef),
    RecipientRetired(RecipientRef, RetirementReport),
    RecipientFailed(RecipientRef, String),
    OnResources(ResourcesEvent),
    CredentialsChanged(BTreeSet<CredentialRef>),
}

/// A resources artifact the core holds, with the revision and hash it was delivered with.
type HeldArtifact = (ResourcesArtifact, ArtifactRevision);

/// The installer core's rule for the resources artifacts it holds (stage 7 part P7.4's), without its revision checks:
/// it holds the newest accepted artifact and, while that one is not usable, the one in use when it was accepted; an
/// artifact is usable while none of its credentials is among those last reported missing.
#[derive(Default)]
struct DeclaredModel {
    latest: Option<HeldArtifact>,
    previous: Option<HeldArtifact>,
    missing: BTreeSet<CredentialRef>,
}

impl DeclaredModel {
    fn usable(&self, (artifact, _): &HeldArtifact) -> bool {
        artifact
            .recipients
            .iter()
            .all(|recipient| !self.missing.contains(&recipient.credential))
    }

    fn in_use(&self) -> Option<&HeldArtifact> {
        [&self.latest, &self.previous]
            .into_iter()
            .flatten()
            .find(|held| self.usable(held))
    }

    fn normalize(&mut self) {
        if self.latest.as_ref().is_some_and(|latest| self.usable(latest)) {
            self.previous = None;
        }
    }

    /// Takes a resources event, and returns `true` if the artifact in use changed, for which the core asks for a
    /// lowering.
    fn on_resources(&mut self, event: &ResourcesEvent) -> bool {
        let before = self.in_use().map(|(_, revision)| revision.clone());
        match event {
            ResourcesEvent::Revoked => {
                self.latest = None;
                self.previous = None;
            }
            ResourcesEvent::Delivered {
                revision,
                artifact_hash,
                document: Ok(document),
                ..
            } => {
                self.previous = self.in_use().cloned();
                self.latest = Some((
                    document.clone(),
                    ArtifactRevision {
                        revision: *revision,
                        hash: *artifact_hash,
                    },
                ));
            }
            ResourcesEvent::Refused { .. } | ResourcesEvent::Delivered { .. } => {}
        }
        self.normalize();
        self.in_use().map(|(_, revision)| revision.clone()) != before
    }
}

/// A recorder of what the installer hands the core, which holds resources artifacts as the core does, and releases the
/// recipient incarnations a test gives it, each with who declared it.
#[derive(Clone, Default)]
struct Recorder {
    calls: Arc<StdMutex<Vec<Call>>>,
    declared: Arc<StdMutex<DeclaredModel>>,
    releasing: Arc<StdMutex<BTreeMap<RecipientRef, ResourceProvenance>>>,
}

impl Recorder {
    /// Makes the core hold `artifact` as the newest accepted revision, as delivered with `revision`.
    fn hold(&self, artifact: ResourcesArtifact, revision: ArtifactRevision) {
        self.declared.lock().unwrap().latest = Some((artifact, revision));
    }

    /// Makes the core release `recipient`, which `provenance` declared.
    fn release(&self, recipient: &RecipientRef, provenance: ResourceProvenance) {
        self.releasing.lock().unwrap().insert(recipient.clone(), provenance);
    }

    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut self.calls.lock().unwrap())
    }

    fn record(&self, call: Call) -> Vec<Action> {
        self.calls.lock().unwrap().push(call);
        Vec::new()
    }
}

impl ResourcePaths for Recorder {
    fn host_prepared(
        &mut self, _: &mut InstallerCore, generation: GenerationId, host: HostId, outcome: PrepareOutcome,
    ) -> Vec<Action> {
        self.record(Call::HostPrepared(generation, host, outcome))
    }

    fn budget_refused(&mut self, _: &mut InstallerCore, generation: GenerationId, credit: CreditKey) -> Vec<Action> {
        self.record(Call::BudgetRefused(generation, credit))
    }

    fn resources_prepared(
        &mut self, _: &mut InstallerCore, revision: ResourcesRevision, host: HostId, outcome: PrepareOutcome,
    ) -> Vec<Action> {
        self.record(Call::ResourcesPrepared(revision, host, outcome))
    }

    fn resources_timed_out(&mut self, _: &mut InstallerCore, revision: ResourcesRevision) -> Vec<Action> {
        self.record(Call::ResourcesTimedOut(revision))
    }

    fn lease_released(&mut self, _: &mut InstallerCore, host: HostId, lease: LeaseRef) -> Vec<Action> {
        self.record(Call::LeaseReleased(host, lease))
    }

    fn recipient_retired(
        &mut self, _: &mut InstallerCore, recipient: RecipientRef, report: RetirementReport,
    ) -> (Vec<Action>, Option<ResourceProvenance>) {
        let released = self.releasing.lock().unwrap().remove(&recipient);
        (self.record(Call::RecipientRetired(recipient, report)), released)
    }

    fn recipient_failed(&mut self, _: &mut InstallerCore, recipient: RecipientRef, reason: &str) -> Vec<Action> {
        self.record(Call::RecipientFailed(recipient, reason.to_string()))
    }

    fn on_resources(&mut self, _: &mut InstallerCore, event: ResourcesEvent) -> Vec<Action> {
        let changed = self.declared.lock().unwrap().on_resources(&event);
        self.record(Call::OnResources(event));
        if changed {
            vec![Action::InstallDefaultProgram]
        } else {
            Vec::new()
        }
    }

    fn credentials_changed(&mut self, _: &mut InstallerCore, missing: BTreeSet<CredentialRef>) -> Vec<Action> {
        let mut declared = self.declared.lock().unwrap();
        declared.missing = missing.clone();
        declared.normalize();
        drop(declared);
        self.record(Call::CredentialsChanged(missing))
    }

    fn held_resources(&self, _: &InstallerCore) -> HeldResources {
        let declared = self.declared.lock().unwrap();
        HeldResources {
            held: [&declared.latest, &declared.previous]
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
            accepted: declared.in_use().cloned(),
        }
    }
}

/// The installer of a process started with `yaml`, with fakes serving `dd_out` and `dd_metrics_encode`, and the
/// recorder of what it hands the core.
struct Harness {
    installer: Installer,
    recorder: Recorder,
    delivery: FakeHost,
    encode: FakeHost,
    config: ConfigCell,
}

impl Harness {
    async fn new(yaml: &str) -> Self {
        Self::under(yaml, None).await
    }

    /// The harness of a process started with `yaml`, whose installer registers its telemetry with `telemetry`.
    async fn under(yaml: &str, telemetry: Option<&TestRecorder>) -> Self {
        let config = configuration(yaml).await;
        let cell = ConfigCell::new(config.clone());
        let build = || {
            programmable_metrics(&config, cell.live(), &ComponentRegistry::default())
                .expect("the configuration has a metrics pipeline")
        };
        let mut metrics = match telemetry {
            Some(telemetry) => ::metrics::with_local_recorder(telemetry, build),
            None => build(),
        };
        let recorder = Recorder::default();
        metrics.with_resource_paths(Box::new(recorder.clone()));
        let wiring = metrics
            .boot_topology()
            .hosts
            .take(DD_OUT)
            .expect("`dd_out` is a fixed host");
        assert_eq!(wiring.host.delivery, DD_OUT);
        assert_eq!(wiring.host.encoder, "dd_metrics_encode");
        let delivery = FakeHost::delivery(wiring.delivery, wiring.events);
        let encode = FakeHost::encode(wiring.encode);
        // The fake encode host takes leases the real factory builds, as a built host's would.
        let _ = wiring
            .factories
            .leases
            .set(saluki_components::encoders::encode_host::MetricsEncodeHostConfiguration::new().lease_factory());
        metrics.boot_topology().hosts.close_unbuilt();
        Self {
            installer: metrics.installer,
            recorder,
            delivery,
            encode,
            config: cell,
        }
    }

    /// Performs `action` and every action it queues.
    async fn perform(&mut self, action: Action) {
        self.installer.actions.push_back(action);
        self.installer.perform_actions().await;
    }

    /// Handles the installer's next event.
    async fn next(&mut self) {
        let event = tokio::time::timeout(PATIENCE, self.installer.next_event())
            .await
            .expect("the installer has an event to handle");
        self.installer.handle(event);
        self.installer.perform_actions().await;
    }

    /// Handles events until the recorder holds a call `wanted` accepts, and returns every call but the hosts'
    /// acknowledgments, which the preparations the fake hosts answer add on the way.
    async fn until(&mut self, wanted: impl Fn(&Call) -> bool) -> Vec<Call> {
        while !self.recorder.calls.lock().unwrap().iter().any(&wanted) {
            self.next().await;
        }
        self.recorder
            .take()
            .into_iter()
            .filter(|call| !matches!(call, Call::HostPrepared(..)))
            .collect()
    }

    /// Handles events until the recorder holds `count` calls, and returns them.
    async fn calls(&mut self, count: usize) -> Vec<Call> {
        while self.recorder.calls.lock().unwrap().len() < count {
            self.next().await;
        }
        self.recorder.take()
    }
}

fn host() -> HostId {
    HostId::new(DD_OUT).unwrap()
}

fn recipient(slot: u16, id: &str, incarnation: u64) -> RecipientRef {
    RecipientRef {
        slot: RecipientSlot::new(slot),
        id: RecipientId::new(id).unwrap(),
        incarnation,
    }
}

/// The primary recipient of the base configuration.
const PRIMARY: &str = "dd_out/primary/0/https://app.datadoghq.com";

fn part(requires: Vec<RecipientRef>, reopen: Vec<RecipientPrepare>) -> HostPrepare {
    HostPrepare {
        host: host(),
        leases: Vec::new(),
        create: Vec::new(),
        reopen,
        requires,
    }
}

/// Returns the retired recipient incarnations the status route lists, after those the resource table holds.
fn retired(installer: &mut Installer) -> Vec<status::RecipientEntry> {
    installer
        .current_status()
        .recipients
        .into_iter()
        .filter(|entry| entry.state == status::RecipientEntryState::Retired)
        .collect()
}

/// The primary recipient's declaration, as the plan last lowered declares it.
fn primary_decl(installer: &Installer) -> saluki_metric_plan::ir::RecipientDecl {
    installer
        .last_lowered
        .resources
        .recipient(&RecipientId::new(PRIMARY).unwrap())
        .cloned()
        .expect("the default program declares the primary recipient")
}

#[tokio::test]
async fn every_fixed_host_is_wired_and_an_unbuilt_one_refuses_what_it_is_sent() {
    let mut harness = Harness::new("").await;
    assert_eq!(
        harness.installer.hosts.keys().map(HostId::as_str).collect::<Vec<_>>(),
        ["dd_out", "mrf_dd_out"]
    );
    // `mrf_dd_out` was not built, so a release sent to it fails as stopped, and is only logged.
    harness
        .perform(Action::ReleaseRecipients {
            host: HostId::new(MRF_DD_OUT).unwrap(),
            recipients: vec![recipient(0, "mrf_dd_out/primary/0/https://mrf.example.com", 1)],
        })
        .await;
    harness.next().await;
    assert!(harness.recorder.take().is_empty());
}

#[tokio::test]
async fn a_publication_reaches_its_delivery_host_with_its_native_set_settings_and_withdrawn_set() {
    let mut harness = Harness::new("").await;
    let decl = primary_decl(&harness.installer);
    let primary = recipient(0, PRIMARY, 1);
    let settings = harness.installer.last_lowered.resources.host(&host()).cloned().unwrap();
    let gone: RecipientId = RecipientId::new("dd_out/additional/0/https://gone.example.com").unwrap();
    harness
        .perform(Action::PublishResources {
            revision: ResourcesRevision::new(3),
            hosts: vec![HostPublish {
                host: host(),
                native_set: vec![primary.clone()],
                settings,
                recipients: vec![RecipientPrepare {
                    recipient: primary.clone(),
                    decl,
                }],
                withdrawn: [gone.clone()].into(),
            }],
        })
        .await;
    harness.next().await;
    assert_eq!(
        harness.delivery.events(),
        [HostEvent::Published {
            revision: 3,
            native_set: vec![delivery_ref(&primary)],
            withdrawn: vec![gone.to_string()],
        }]
    );
    assert!(
        harness.recorder.take().is_empty(),
        "the core takes no answer to a publication"
    );
}

#[tokio::test]
async fn a_publication_that_cannot_be_read_is_not_sent() {
    let mut harness = Harness::new("").await;
    let mut settings = harness.installer.last_lowered.resources.host(&host()).cloned().unwrap();
    settings.settings = Params::new();
    harness
        .perform(Action::PublishResources {
            revision: ResourcesRevision::new(3),
            hosts: vec![HostPublish {
                host: host(),
                native_set: Vec::new(),
                settings,
                recipients: Vec::new(),
                withdrawn: Default::default(),
            }],
        })
        .await;
    assert!(harness.installer.acks.is_empty());
    assert!(harness.delivery.events().is_empty());
}

#[tokio::test]
async fn a_release_and_the_retirement_it_leads_to_reach_the_core_once() {
    let mut harness = Harness::new("").await;
    let primary = recipient(0, PRIMARY, 1);
    harness
        .delivery
        .hold(Creator::Transaction(1), &[delivery_ref(&primary)])
        .unwrap();
    harness.recorder.release(&primary, ResourceProvenance::Native);
    harness
        .perform(Action::ReleaseRecipients {
            host: host(),
            recipients: vec![primary.clone()],
        })
        .await;
    // The release is acknowledged, then the exit arrives on the event channel; the core takes only the exit.
    let calls = harness.calls(1).await;
    assert_eq!(
        calls,
        [Call::RecipientRetired(primary.clone(), RetirementReport::default())]
    );
    assert!(harness.delivery.recipients().is_empty());

    // The status route lists the retired incarnation with its drain report, and who declared it.
    let listed = retired(&mut harness.installer);
    let [entry] = listed.as_slice() else {
        panic!("one retired incarnation, not {:?}", listed);
    };
    assert_eq!(
        (entry.host.as_str(), entry.id.as_str(), entry.slot, entry.incarnation),
        (DD_OUT, PRIMARY, 0, 1)
    );
    assert_eq!(entry.state, status::RecipientEntryState::Retired);
    assert_eq!(entry.provenance, Some(status::Provenance::Native));
    let drain = entry.last_drain.expect("a retired incarnation has its drain report");
    assert_eq!(
        (drain.delivered, drain.persisted, drain.dropped, drain.deadline_reached),
        (0, 0, 0, false)
    );
    assert!(drain.retired_ms > 0);
}

#[tokio::test]
async fn an_exit_the_core_is_not_releasing_is_handed_to_it_and_listed_as_no_retirement() {
    let mut harness = Harness::new("").await;
    // What a delivery host reports for a recipient an abort dropped: an exit with an empty drain report, of an
    // incarnation the core no longer holds.
    let dropped = recipient(4, "dd_out/declared/team_d/http://127.0.0.1:17105", 7);
    let actions = harness.installer.host_event(
        host(),
        DeliveryEvent::RecipientExited {
            recipient: delivery_ref(&dropped),
            outcome: RecipientExit::Retired(DrainReport::default()),
        },
    );
    assert!(actions.is_empty());
    assert_eq!(
        harness.recorder.take(),
        [Call::RecipientRetired(dropped, RetirementReport::default())],
        "the core is told, and decides"
    );
    assert!(
        retired(&mut harness.installer).is_empty(),
        "an exit that ends no incarnation the core releases is no retirement"
    );
}

#[tokio::test]
async fn a_retired_recipient_is_listed_as_declared_by_the_incarnation_the_core_released() {
    let mut harness = Harness::new("").await;
    let declared = recipient(3, "dd_out/declared/team_d/http://127.0.0.1:17105", 2);
    // The resources product was revoked: the latest lowering no longer declares the recipient being released.
    assert!(harness
        .installer
        .lowered_resources
        .resources
        .recipient(&declared.id)
        .is_none());
    let revision = ArtifactRevision {
        revision: 4,
        hash: PlanHash::of_bytes(b"revision 4"),
    };
    harness
        .recorder
        .release(&declared, ResourceProvenance::Declared(revision));
    harness
        .delivery
        .hold(Creator::Generation(GenerationId::new(3)), &[delivery_ref(&declared)])
        .unwrap();
    harness
        .perform(Action::ReleaseRecipients {
            host: host(),
            recipients: vec![declared.clone()],
        })
        .await;
    assert_eq!(
        harness.calls(1).await,
        [Call::RecipientRetired(declared.clone(), RetirementReport::default())]
    );
    let listed = retired(&mut harness.installer);
    let [entry] = listed.as_slice() else {
        panic!("one retired incarnation, not {:?}", listed);
    };
    assert_eq!((entry.id.as_str(), entry.incarnation), (declared.id.as_str(), 2));
    assert_eq!(
        entry.provenance,
        Some(status::Provenance::Declared {
            revision: 4,
            hash: PlanHash::of_bytes(b"revision 4"),
        })
    );
}

#[tokio::test]
async fn the_installer_leaves_counting_retirements_to_the_delivery_hosts() {
    let telemetry = TestRecorder::default();
    let mut harness = Harness::under("", Some(&telemetry)).await;
    let primary = recipient(0, PRIMARY, 1);
    harness
        .delivery
        .hold(Creator::Transaction(1), &[delivery_ref(&primary)])
        .unwrap();
    harness.recorder.release(&primary, ResourceProvenance::Native);
    harness
        .perform(Action::ReleaseRecipients {
            host: host(),
            recipients: vec![primary.clone()],
        })
        .await;
    harness.calls(1).await;
    assert_eq!(retired(&mut harness.installer).len(), 1);
    // `plan_recipient_retirements_total` is the delivery host's (stage 7 part P7.1b), which sees each drain end: a
    // second series from the installer would count every retirement twice.
    let counted = telemetry
        .counters()
        .into_iter()
        .filter(|(key, _)| key.name() == "plan_recipient_retirements_total")
        .collect::<Vec<_>>();
    assert!(counted.is_empty(), "the installer counts retirements: {:?}", counted);
}

#[test]
fn the_status_keeps_the_latest_sixteen_retired_incarnations_oldest_first() {
    let mut retired = status::RetiredRecipients::default();
    let at = std::time::UNIX_EPOCH + Duration::from_millis(1_500);
    for incarnation in 1..=20 {
        retired.record(
            &host(),
            &recipient(2, PRIMARY, incarnation),
            &RetirementReport {
                delivered: incarnation,
                persisted: 0,
                dropped: 1,
                deadline_reached: true,
            },
            status::Provenance::Native,
            at,
        );
    }
    let entries = retired.entries();
    assert_eq!(
        entries.iter().map(|entry| entry.incarnation).collect::<Vec<_>>(),
        (5..=20).collect::<Vec<_>>()
    );
    let drain = entries[0].last_drain.unwrap();
    assert_eq!((drain.delivered, drain.dropped, drain.retired_ms), (5, 1, 1_500));
    assert_eq!(entries[0].provenance, Some(status::Provenance::Native));
}

#[tokio::test]
async fn a_failed_loop_reaches_the_core_as_a_failure_with_its_reason() {
    let mut harness = Harness::new("").await;
    let primary = recipient(0, PRIMARY, 2);
    harness
        .delivery
        .hold(Creator::Transaction(1), &[delivery_ref(&primary)])
        .unwrap();
    assert!(harness.delivery.fail(&delivery_ref(&primary), "disk full"));
    assert_eq!(
        harness.calls(1).await,
        [Call::RecipientFailed(primary, "disk full".to_string())]
    );
}

#[tokio::test]
async fn each_lease_is_reported_released_once_its_encode_host_answers_whatever_it_says() {
    let mut harness = Harness::new("").await;
    let lease = |entry: u16| LeaseRef {
        lease: EntryId::new(entry),
        key: LeaseKey {
            output: OutputId::new("dd_metrics_encode").unwrap(),
            binding: PlanHash::of_bytes(&entry.to_be_bytes()),
        },
    };
    // A host that does not hold a lease, or is gone, holds nothing of it.
    harness.encode.reject_next("unknown lease");
    harness
        .perform(Action::ReleaseLeases {
            host: host(),
            leases: vec![lease(1), lease(2)],
        })
        .await;
    let mut calls = harness.calls(2).await;
    calls.sort_by_key(|call| format!("{:?}", call));
    assert_eq!(
        calls,
        [
            Call::LeaseReleased(host(), lease(1)),
            Call::LeaseReleased(host(), lease(2))
        ]
    );
    assert_eq!(
        harness.encode.events(),
        [
            HostEvent::Rejected("unknown lease".to_string()),
            HostEvent::LeaseDrained(EntryId::new(2))
        ]
    );
}

#[tokio::test]
async fn a_generations_part_sends_what_it_requires_and_reopens_and_reports_exited_reopens() {
    let mut harness = Harness::new("").await;
    let live = recipient(0, PRIMARY, 1);
    harness
        .delivery
        .hold(Creator::Transaction(1), &[delivery_ref(&live)])
        .unwrap();
    // A reopen of an incarnation the host does not hold any more: its loop exited.
    let exited = recipient(1, "dd_out/additional/0/https://gone.example.com", 4);
    let mut decl = primary_decl(&harness.installer);
    decl.id = exited.id.clone();
    let generation = GenerationId::new(7);
    harness.installer.prepare_hosts(
        generation,
        vec![part(
            vec![live.clone()],
            vec![RecipientPrepare {
                recipient: exited.clone(),
                decl,
            }],
        )],
    );
    assert_eq!(
        harness.calls(1).await,
        [Call::HostPrepared(
            generation,
            host(),
            PrepareOutcome::ReopenExited {
                recipients: vec![exited.clone()]
            }
        )]
    );
    assert_eq!(
        harness.delivery.events(),
        [
            HostEvent::RecipientsPrepared(Creator::Generation(generation), Vec::new()),
            HostEvent::Reopened(
                Creator::Generation(generation),
                vec![(delivery_ref(&exited), ReopenReply::Exited)]
            ),
        ]
    );
}

#[tokio::test]
async fn a_part_that_requires_a_recipient_that_is_not_live_fails() {
    let mut harness = Harness::new("").await;
    let generation = GenerationId::new(3);
    harness
        .installer
        .prepare_hosts(generation, vec![part(vec![recipient(0, PRIMARY, 1)], Vec::new())]);
    let calls = harness.calls(1).await;
    let [Call::HostPrepared(failed, failed_host, PrepareOutcome::Failed(reason))] = calls.as_slice() else {
        panic!("one failed part, not {:?}", calls);
    };
    assert_eq!((*failed, failed_host), (generation, &host()));
    assert!(reason.contains("is not live"), "{}", reason);
}

#[tokio::test(start_paused = true)]
async fn a_part_that_cannot_be_built_fails_at_once_and_an_unanswered_transaction_times_out() {
    let mut harness = Harness::new("").await;
    let decl = primary_decl(&harness.installer);
    let revision = ResourcesRevision::new(2);
    let create = HostPrepare {
        host: host(),
        leases: Vec::new(),
        create: vec![RecipientPrepare {
            recipient: recipient(0, PRIMARY, 1),
            decl,
        }],
        reopen: Vec::new(),
        requires: Vec::new(),
    };
    harness
        .perform(Action::PrepareResources {
            revision,
            hosts: vec![create],
            timeout: Duration::from_secs(10),
            admitted: Vec::new(),
        })
        .await;
    // The topology built no delivery host, so the recipient's pieces cannot be built: nothing is sent.
    assert_eq!(
        harness.recorder.take(),
        [Call::ResourcesPrepared(
            revision,
            host(),
            PrepareOutcome::Failed(
                "delivery host `dd_out` was not built, so its recipients cannot be prepared".to_string()
            )
        )]
    );
    assert!(harness.delivery.events().is_empty());
    // The transaction still times out, which the core takes as such, not as a failure.
    assert_eq!(harness.calls(1).await, [Call::ResourcesTimedOut(revision)]);
}

#[tokio::test]
async fn a_publication_stops_the_transactions_timeout() {
    let mut harness = Harness::new("").await;
    let revision = ResourcesRevision::new(2);
    harness
        .perform(Action::PrepareResources {
            revision,
            hosts: Vec::new(),
            timeout: Duration::from_secs(10),
            admitted: Vec::new(),
        })
        .await;
    assert!(harness.installer.resources_deadline.is_some());
    harness
        .perform(Action::PublishResources {
            revision,
            hosts: Vec::new(),
        })
        .await;
    assert_eq!(harness.installer.resources_deadline, None);
}

#[tokio::test]
async fn aborts_reach_every_listed_delivery_host_naming_their_preparation() {
    let mut harness = Harness::new("").await;
    let generation = GenerationId::new(5);
    harness
        .perform(Action::Abort {
            generation,
            slots: Vec::new(),
            hosts: vec![host()],
        })
        .await;
    harness
        .perform(Action::AbortResources {
            revision: ResourcesRevision::new(6),
            hosts: vec![host()],
        })
        .await;
    harness.next().await;
    harness.next().await;
    assert_eq!(
        harness.delivery.events(),
        [
            HostEvent::Aborted {
                creator: Creator::Generation(generation),
                dropped: Vec::new(),
                drained: Vec::new(),
            },
            HostEvent::Aborted {
                creator: Creator::Transaction(6),
                dropped: Vec::new(),
                drained: Vec::new(),
            },
        ]
    );
}

#[tokio::test]
async fn credits_return_to_the_budget_once_when_the_core_releases_them() {
    let mut harness = Harness::new("").await;
    let queue = CreditKey::HostQueue(HostId::new(MRF_DD_OUT).unwrap());
    let verified = VerifiedMemory {
        grant_bytes: 1_000,
        firm_bytes: 400,
        mode: MemoryMode::Strict,
    };
    harness.installer.budget = ResourceBudget::new(Some(verified), &[(queue.clone(), 100)]);
    assert_eq!(harness.installer.budget.usage().used, 100);
    for _ in 0..2 {
        harness
            .perform(Action::ReleaseCredits {
                credits: vec![queue.clone()],
            })
            .await;
    }
    assert_eq!(
        harness.installer.budget.usage(),
        budget::BudgetUsage {
            granted: Some(700),
            used: 0,
        }
    );
    assert_eq!(harness.installer.current_status().resources.budget.granted, Some(700));
}

#[tokio::test]
async fn a_restarted_worker_keeps_the_credit_ledger_and_hands_the_core_an_exit_reported_while_it_was_stopped() {
    let config = configuration("").await;
    let cell = ConfigCell::new(config.clone());
    let mut metrics = programmable_metrics(&config, cell.live(), &ComponentRegistry::default())
        .expect("the configuration has a metrics pipeline");
    let recorder = Recorder::default();
    metrics.with_resource_paths(Box::new(recorder.clone()));
    // The regions run and never read their mailboxes; `dd_out` and its encode host are fakes.
    let mailboxes = std::mem::take(&mut metrics.boot_topology().mailboxes);
    for mailbox in mailboxes.iter().flatten() {
        mailbox.mark_running();
    }
    let wiring = metrics.boot_topology().hosts.take(DD_OUT).unwrap();
    let delivery = FakeHost::delivery(wiring.delivery, wiring.events);
    let _encode = FakeHost::encode(wiring.encode);
    metrics.boot_topology().hosts.close_unbuilt();
    // The boot generation's own credits (its main output's lease) join the headroom and are held from the start.
    let boot = metrics
        .installer
        .boot_credits
        .iter()
        .map(|(_, bytes)| *bytes)
        .sum::<u64>();
    assert!(boot > 0, "the boot lease holds a credit");
    metrics.admit_against(Some(VerifiedMemory {
        grant_bytes: 1_000,
        firm_bytes: 400,
        mode: MemoryMode::Strict,
    }));
    let queue = CreditKey::HostQueue(HostId::new(MRF_DD_OUT).unwrap());
    metrics.installer.budget.acquire(&[(queue.clone(), 250)]).unwrap();
    let worker = Arc::new(metrics.into_installer());
    let status = worker.status();
    let run = |shutdown: ShutdownHandle| {
        let worker = Arc::clone(&worker);
        async move {
            let Ok(run) = worker.initialize(shutdown).await else {
                panic!("the installer initializes");
            };
            run.await.expect("the installer stops cleanly");
        }
    };

    // The first run stops, as a panicking worker would; while it is stopped, a recipient of `dd_out` fails.
    let first = tokio::spawn(run(ShutdownHandle::noop()));
    tokio::task::yield_now().await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let primary = recipient(0, PRIMARY, 1);
    delivery
        .hold(Creator::Transaction(1), &[delivery_ref(&primary)])
        .unwrap();
    assert!(delivery.fail(&delivery_ref(&primary), "disk full"));

    // The restarted worker takes the exit from the event channel the installer kept, and the credit is still held.
    let (coordinator, shutdown) = ShutdownHandle::paired();
    let second = tokio::spawn(run(shutdown));
    tokio::time::timeout(PATIENCE, async {
        while !recorder
            .calls
            .lock()
            .unwrap()
            .contains(&Call::RecipientFailed(primary.clone(), "disk full".to_string()))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the restarted worker hands the core the exit");
    assert_eq!(
        status.load().resources.budget,
        status::BudgetView {
            granted: Some(600 + boot),
            used: boot + 250,
        }
    );
    coordinator.shutdown();
    tokio::time::timeout(PATIENCE, second).await.unwrap().unwrap();
}

/// The artifact `written("team_d", 17105)` validates to.
fn team_d() -> ResourcesArtifact {
    validated("team_d", 17105)
}

/// Revision `revision` of the resources product, with a hash of its own.
fn revision(revision: u64) -> ArtifactRevision {
    ArtifactRevision {
        revision,
        hash: PlanHash::of_bytes(&revision.to_be_bytes()),
    }
}

/// Returns `config` with the declared credentials `names` configured, each with a key.
fn with_credentials(mut config: SalukiConfiguration, names: &[&str]) -> SalukiConfiguration {
    config.control.programmable_metrics.resources.credentials = names
        .iter()
        .map(|name| (name.to_string(), format!("key-{}", name)))
        .collect();
    config
}

/// A resources document as an operator writes it: the recipient `team_d_intake`, which sends with the declared
/// credential `credential` to the local port `port`, written in a form that is not canonical, and the output `team_d`.
fn written(credential: &str, port: u16) -> Value {
    json!({
        "resources_version": 1,
        "recipients": [{
            "name": "team_d_intake",
            "host": "dd_out",
            "endpoint": format!("HTTP://127.0.0.1:{}/", port),
            "credential": format!("credentials/{}", credential)
        }],
        "outputs": [{"name": "team_d", "recipients": ["team_d_intake"], "label": "Team D"}]
    })
}

/// The artifact validation makes of `written(credential, port)`: its endpoint in canonical form.
fn validated(credential: &str, port: u16) -> ResourcesArtifact {
    ResourcesArtifact {
        resources_version: RESOURCES_VERSION,
        recipients: vec![DeclaredRecipient {
            name: "team_d_intake".to_string(),
            host: host(),
            endpoint: format!("http://127.0.0.1:{}", port),
            credential: CredentialRef::new(format!("credentials/{}", credential)).unwrap(),
            protocol: DeclaredProtocol::default(),
            delivery: None,
        }],
        outputs: vec![DeclaredOutput {
            name: "team_d".to_string(),
            recipients: vec!["team_d_intake".to_string()],
            accepts: DeclaredAccepts::Any,
            label: Some("Team D".to_string()),
        }],
    }
}

/// Validation of the documents [`written`] writes, which it validates to what [`validated`] says, refusing any other.
fn validate_written(document: Value, _: &dyn Catalog) -> Result<ResourcesArtifact, ResourcesError> {
    let recipient = &document["recipients"][0];
    let credential = recipient["credential"]
        .as_str()
        .and_then(|credential| credential.strip_prefix("credentials/"));
    let port = recipient["endpoint"]
        .as_str()
        .and_then(|endpoint| endpoint.strip_prefix("HTTP://127.0.0.1:"))
        .and_then(|port| port.strip_suffix('/'))
        .and_then(|port| port.parse().ok());
    match (credential, port) {
        (Some(credential), Some(port)) if document == written(credential, port) => Ok(validated(credential, port)),
        _ => Err(ResourcesError::Invalid {
            message: format!("not a document the test writes: {}", document),
        }),
    }
}

/// A snapshot of the resources product that holds `document` as revision `revision`.
fn resources_snapshot(revision: u64, document: &Value) -> Arc<ArtifactSnapshot> {
    let hash = PlanHash::of_value(document).unwrap();
    let payload = json!({"revision": revision, "artifact_hash": hash, "artifact": document}).to_string();
    Arc::new(ArtifactSnapshot::new(
        ArtifactProduct::Resources,
        vec![ArtifactConfig {
            id: ArtifactConfigId::new("resources"),
            version: revision,
            sha256: ArtifactConfig::transport_hash(&payload),
            payload,
        }],
    ))
}

/// Returns the provenance of the declared output `resources_team_d` in the plan last lowered, if it declares it.
fn team_d_provenance(installer: &Installer) -> Option<ResourceProvenance> {
    installer
        .last_lowered
        .output(&OutputId::new("resources_team_d").unwrap())
        .and_then(|output| output.binding.as_ref())
        .map(|binding| binding.provenance.clone())
}

#[tokio::test]
async fn a_valid_snapshot_reaches_the_core_after_the_credentials_it_misses_and_is_lowered_with_its_provenance() {
    let mut harness = Harness::new("").await;
    harness.installer.validate_resources = validate_written;
    let document = written("team_d", 17105);
    let delivery = ResourcesEvent::Delivered {
        config: ArtifactConfigId::new("resources"),
        version: 5,
        revision: 5,
        // The hash of the document as received, and the document validation made of it.
        artifact_hash: PlanHash::of_value(&document).unwrap(),
        document: Ok(validated("team_d", 17105)),
    };
    let team_d = CredentialRef::new("credentials/team_d").unwrap();

    // `credentials/team_d` is not configured: the core is told so before it takes the delivery, which then waits.
    harness
        .installer
        .handle(Event::Snapshot(resources_snapshot(5, &document)));
    harness.installer.perform_actions().await;
    assert_eq!(
        harness.recorder.take(),
        [
            Call::CredentialsChanged([team_d.clone()].into()),
            Call::OnResources(delivery)
        ]
    );
    assert_eq!(team_d_provenance(&harness.installer), None);
    assert_eq!(
        harness.installer.current_status().resources.missing_credentials,
        ["credentials/team_d"]
    );

    // Configured, it is told before the lowering, which declares the artifact's resources with its revision and hash.
    harness
        .config
        .set(with_credentials(configuration("").await, &["team_d"]));
    assert_eq!(
        harness.until(|call| matches!(call, Call::CredentialsChanged(_))).await,
        [Call::CredentialsChanged(BTreeSet::new())]
    );
    assert_eq!(
        team_d_provenance(&harness.installer),
        Some(ResourceProvenance::Declared(ArtifactRevision {
            revision: 5,
            hash: PlanHash::of_value(&document).unwrap(),
        }))
    );
    let canonical = RecipientId::new("dd_out/declared/team_d/http://127.0.0.1:17105").unwrap();
    assert_eq!(
        harness
            .installer
            .last_lowered
            .resources
            .recipient(&canonical)
            .map(|recipient| recipient.endpoint.as_str()),
        Some("http://127.0.0.1:17105"),
        "lowering declares what validation made of the document"
    );

    // A revocation is lowered without it.
    harness.installer.handle(Event::Snapshot(Arc::new(ArtifactSnapshot::new(
        ArtifactProduct::Resources,
        Vec::new(),
    ))));
    harness.installer.perform_actions().await;
    assert_eq!(harness.recorder.take(), [Call::OnResources(ResourcesEvent::Revoked)]);
    assert_eq!(team_d_provenance(&harness.installer), None);
}

#[tokio::test]
async fn a_revoked_artifact_stops_counting_the_credentials_it_waited_for() {
    let mut harness = Harness::new("").await;
    harness.installer.validate_resources = validate_written;
    harness
        .installer
        .handle(Event::Snapshot(resources_snapshot(5, &written("team_d", 17105))));
    harness.recorder.take();
    harness.installer.handle(Event::Snapshot(Arc::new(ArtifactSnapshot::new(
        ArtifactProduct::Resources,
        Vec::new(),
    ))));
    // Once the core dropped the artifact, no artifact it holds waits for `credentials/team_d`.
    assert_eq!(
        harness.recorder.take(),
        [
            Call::OnResources(ResourcesEvent::Revoked),
            Call::CredentialsChanged(BTreeSet::new())
        ]
    );
    assert!(harness
        .installer
        .current_status()
        .resources
        .missing_credentials
        .is_empty());
}

#[tokio::test]
async fn the_revision_in_use_keeps_its_credentials_counted_while_newer_revisions_wait_for_another() {
    let mut harness = Harness::new("").await;
    harness.installer.validate_resources = validate_written;
    let config = with_credentials(configuration("").await, &["team_d"]);
    harness.config.set(config.clone());
    harness.next().await;
    let first = written("team_d", 17105);
    let first_revision = ArtifactRevision {
        revision: 1,
        hash: PlanHash::of_value(&first).unwrap(),
    };
    harness.installer.handle(Event::Snapshot(resources_snapshot(1, &first)));
    harness.installer.perform_actions().await;
    assert_eq!(
        team_d_provenance(&harness.installer),
        Some(ResourceProvenance::Declared(first_revision.clone()))
    );

    // Four newer revisions each wait for `credentials/team_e`: the core keeps revision 1 in use all along.
    for revision in 2..=5 {
        let port = 17100 + revision as u16;
        harness
            .installer
            .handle(Event::Snapshot(resources_snapshot(revision, &written("team_e", port))));
        harness.installer.perform_actions().await;
    }
    harness.installer.propose_default_program(&config);
    assert_eq!(
        team_d_provenance(&harness.installer),
        Some(ResourceProvenance::Declared(first_revision)),
        "revision 1 is lowered with its own revision and hash"
    );
    assert_eq!(
        harness.installer.current_status().resources.missing_credentials,
        ["credentials/team_e"]
    );
    harness.recorder.take();

    // Removing `credentials/team_d` makes revision 1 wait too: its recipient is withdrawn at the next lowering.
    harness.config.set(with_credentials(configuration("").await, &[]));
    let team_d = CredentialRef::new("credentials/team_d").unwrap();
    let team_e = CredentialRef::new("credentials/team_e").unwrap();
    assert_eq!(
        harness.until(|call| matches!(call, Call::CredentialsChanged(_))).await,
        [Call::CredentialsChanged([team_d, team_e].into())]
    );
    assert_eq!(team_d_provenance(&harness.installer), None);
    assert!(harness
        .installer
        .last_lowered
        .resources
        .recipients
        .iter()
        .all(|recipient| recipient.provenance == ResourceProvenance::Native));
}

#[tokio::test]
async fn missing_credentials_are_told_before_lowering_and_only_when_they_change() {
    let mut harness = Harness::new("").await;
    harness.recorder.hold(team_d(), revision(4));
    let config = configuration("").await;
    harness.installer.propose_default_program(&config);
    let team_d = CredentialRef::new("credentials/team_d").unwrap();
    assert!(harness
        .recorder
        .take()
        .contains(&Call::CredentialsChanged([team_d.clone()].into())));
    // The same configuration says nothing new.
    harness.installer.propose_default_program(&config);
    assert!(!harness
        .recorder
        .take()
        .iter()
        .any(|call| matches!(call, Call::CredentialsChanged(_))));
    let config = with_credentials(config, &["team_d"]);
    harness.installer.propose_default_program(&config);
    assert!(harness
        .recorder
        .take()
        .contains(&Call::CredentialsChanged(BTreeSet::new())));
    assert!(harness
        .installer
        .current_status()
        .resources
        .missing_credentials
        .is_empty());
}

#[tokio::test]
async fn the_accepted_resources_are_lowered_with_their_provenance_and_shown_in_status() {
    let mut harness = Harness::new("").await;
    harness.recorder.hold(team_d(), revision(4));
    let config = with_credentials(configuration("").await, &["team_d"]);
    harness.config.set(config.clone());
    harness.installer.propose_default_program(&config);

    assert_eq!(
        team_d_provenance(&harness.installer),
        Some(ResourceProvenance::Declared(revision(4)))
    );

    let status = harness.installer.current_status();
    let view = status
        .resources
        .outputs
        .iter()
        .find(|output| output.id == "resources_team_d")
        .expect("status lists the declared output");
    assert_eq!(view.label, "Team D");
    assert_eq!(view.host.as_deref(), Some("dd_out"));
    assert_eq!(
        view.provenance,
        status::Provenance::Declared {
            revision: 4,
            hash: PlanHash::of_bytes(&4u64.to_be_bytes()),
        }
    );
    let recipient = status
        .resources
        .recipients
        .iter()
        .find(|recipient| recipient.id == "dd_out/declared/team_d/http://127.0.0.1:17105")
        .expect("status lists the declared recipient");
    assert_eq!(recipient.credential, "credentials/team_d");
    // The declared recipient is named after the artifact's name for its task.
    assert_eq!(
        harness
            .installer
            .resource_paths
            .held_resources(&harness.installer.core)
            .declared_name(&RecipientId::new(recipient.id.as_str()).unwrap())
            .as_deref(),
        Some("team_d_intake")
    );
}
