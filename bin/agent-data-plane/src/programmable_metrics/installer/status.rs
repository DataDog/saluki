//! The resources section of the installer's status: what the default program delivers through, as the control
//! surface serves it.

use std::collections::BTreeMap;
use std::{
    collections::VecDeque,
    time::{SystemTime, UNIX_EPOCH},
};

use saluki_components::forwarders::delivery::DeliveryHostStatus;
use saluki_metric_plan::{
    canonical::PlanHash,
    channel::{ApplyState, ApplyStates, ArtifactProduct, ConfigAcknowledgment},
    extension::DeliveryPolicy,
    install::{LeaseState, RecipientRef, RecipientState, ResourceTable, RetirementReport},
    ir::{HostId, PayloadClasses, ResourceProvenance},
};
use serde::Serialize;

use super::budget::BudgetUsage;
use crate::programmable_metrics::resources::{
    specs::delivery_ref, spelled_endpoint, FixedHosts, NativeResources, OutputKind,
};

/// What the default program delivers through: the resources product's state, the delivery hosts, the outputs, the
/// recipients, and the resource budget.
///
/// Outputs and recipients are those of the default program last lowered: what the current configuration and the
/// accepted resources artifact declare. Credentials appear by reference only.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ResourcesView {
    /// The resources product's config, as the installer acknowledges it, if one was delivered.
    pub declared: Option<DeclaredView>,

    /// The declared credentials the resources artifacts the installer core holds reference that are not configured,
    /// by reference: an artifact that uses one waits, `pending`, until it is.
    pub missing_credentials: Vec<String>,

    /// The delivery hosts, each with its encode host.
    pub hosts: Vec<HostView>,

    /// Every output.
    pub outputs: Vec<OutputView>,

    /// Every recipient, of every host.
    pub recipients: Vec<RecipientView>,

    /// The resource budget everything added after boot is admitted against.
    pub budget: BudgetView,
}

/// The state of the resources product's config.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeclaredView {
    /// The config's identifier.
    pub config: String,

    /// The config's state: `pending` with an error while it waits for a credential, `active` once the active
    /// generation carries its declarations, `rejected` with the reason, or `retired`.
    pub state: ApplyState,

    /// The revision of the artifact the config last delivered, once its envelope was read.
    pub revision: Option<u64>,

    /// Why the config is held, rejected, or refused, in plain text.
    pub error: Option<String>,

    /// The revision whose declarations lowering uses: the one installing or active. It stays in place while a newer
    /// revision is pending or rejected; `None` once the product is revoked.
    pub installed_revision: Option<u64>,
}

/// A delivery host, with its encode host.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HostView {
    /// The delivery host's component ID.
    pub host: String,

    /// Its encode host's component ID.
    pub encoder: String,

    /// Whether it accepts recipients a resources artifact declares.
    pub declared_recipients: bool,

    /// The leases its encode host runs, by identifier: those generations use, those a generation prepares, and those
    /// draining.
    pub leases: Vec<LeaseView>,

    /// The recipients it drops every payload to, by identity: those the latest lowering no longer declares.
    pub withdrawn: Vec<String>,

    /// The declared recipients that send to the same endpoint with the same key as another of its recipients.
    pub conflicts: Vec<ConflictView>,

    /// Why it could not build a client for its current settings, while it sends through the client it had.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_failure: Option<String>,
}

/// A lease of an encode host: one output's encoder stream.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LeaseView {
    /// The lease's identifier, which every batch to the output names in its header.
    pub lease: u16,

    /// The output it encodes.
    pub output: String,

    /// Its state: `preparing`, `live`, or `draining`.
    pub state: LeaseViewState,
}

/// The state of a lease, as the status lists it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseViewState {
    /// A generation that has not committed prepared it.
    Preparing,

    /// Generations use it.
    Live,

    /// No generation uses it: it flushes what it holds, and is released.
    Draining,
}

impl From<LeaseState> for LeaseViewState {
    fn from(state: LeaseState) -> Self {
        match state {
            LeaseState::Preparing => Self::Preparing,
            LeaseState::Live => Self::Live,
            LeaseState::Draining => Self::Draining,
        }
    }
}

/// A declared recipient that sends to the same endpoint with the same key as another recipient of its host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConflictView {
    /// The declared recipient's identity.
    pub declared: String,

    /// The identity of the recipient it duplicates.
    pub duplicates: String,
}

/// An output.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OutputView {
    /// The output's identifier, which artifacts emit to.
    pub id: String,

    /// Its display label.
    pub label: String,

    /// The delivery host it is bound to, or `None` for a fixed destination.
    pub host: Option<String>,

    /// Its fingerprint, version 2, which an artifact's base records.
    pub fingerprint: PlanHash,

    /// Its fingerprint as stage 5 computed it, for a native output: a base that recorded it keeps composing while it
    /// matches.
    pub legacy_fingerprint: Option<PlanHash>,

    /// The identities of its recipients, sorted.
    pub recipients: Vec<String>,

    /// Who declared it: `native`, or the resources artifact's revision.
    pub provenance: Provenance,
}

/// A recipient.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecipientView {
    /// The recipient's identity.
    pub id: String,

    /// Its delivery host.
    pub host: String,

    /// Its endpoint, or, for one that may carry credentials or is too long to spell out, its digest.
    pub endpoint: String,

    /// The reference of the credential it sends with.
    pub credential: String,

    /// Which payloads it receives when a payload does not name its recipients: `all`, or `metrics_only`.
    pub classes: PayloadClasses,

    /// Its delivery policy.
    pub delivery: DeliveryPolicy,

    /// Who declared it: `native`, or the resources artifact's revision.
    pub provenance: Provenance,
}

/// Who declared a resource: `"native"`, or `{"declared": {"revision": 2, "hash": "sha256:…"}}`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Native configuration.
    Native,

    /// A resources artifact, by its revision and hash.
    Declared {
        /// The artifact's revision.
        revision: u64,

        /// The artifact's hash.
        hash: PlanHash,
    },
}

impl From<&ResourceProvenance> for Provenance {
    fn from(provenance: &ResourceProvenance) -> Self {
        match provenance {
            ResourceProvenance::Native => Self::Native,
            ResourceProvenance::Declared(artifact) => Self::Declared {
                revision: artifact.revision,
                hash: artifact.hash,
            },
        }
    }
}

/// The status route's document: the apply states, as a poll reports them, and the recipient incarnations, which only
/// this route lists, as `recipients`.
#[derive(Serialize)]
pub struct StatusDocument<'a> {
    /// The apply states, whose members are the document's own.
    #[serde(flatten)]
    pub apply_states: &'a ApplyStates,

    /// The recipient incarnations.
    pub recipients: &'a [RecipientEntry],
}

impl<'a> StatusDocument<'a> {
    /// Returns the document of the installer's status.
    pub fn of(status: &'a super::PlanStatus) -> Self {
        Self {
            apply_states: &status.apply_states,
            recipients: &status.recipients,
        }
    }
}

/// How many retired recipient incarnations the status route keeps listing after their exit.
const KEPT_RETIRED: usize = 16;

/// One incarnation of a recipient of a delivery host, as the status route lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecipientEntry {
    /// Its delivery host.
    pub host: String,

    /// Its identity.
    pub id: String,

    /// Its slot in its host.
    pub slot: u16,

    /// Its incarnation.
    pub incarnation: u64,

    /// Its state.
    pub state: RecipientEntryState,

    /// Who declared the incarnation, as the installer core held it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,

    /// When its drain started, in milliseconds since the Unix epoch, while it drains.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draining_since_ms: Option<u64>,

    /// What its drain did, once it retired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_drain: Option<LastDrain>,
}

/// The state of a recipient incarnation the status route lists.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipientEntryState {
    /// Prepared by a preparation that has not committed: nothing addresses it yet.
    Preparing,

    /// Live: payloads that name it reach it.
    Live,

    /// Released, and draining its queued work until it exits.
    Draining,

    /// Its loop ended on an error: its next incarnation replaces it after a backoff.
    Failed,

    /// Released, its exit reported, and a preparation that declared it again waits for its next incarnation.
    Exiting,

    /// It drained after its release, and its loop exited: the entry stays listed for a while with its drain report.
    Retired,
}

impl From<RecipientState> for RecipientEntryState {
    fn from(state: RecipientState) -> Self {
        match state {
            RecipientState::Preparing => Self::Preparing,
            RecipientState::Live => Self::Live,
            RecipientState::Draining => Self::Draining,
            RecipientState::Failed => Self::Failed,
            RecipientState::Exiting => Self::Exiting,
        }
    }
}

/// What a retired recipient's drain did.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct LastDrain {
    /// Requests delivered while draining.
    pub delivered: u64,

    /// Requests persisted at the close.
    pub persisted: u64,

    /// Requests dropped, and counted, at the close.
    pub dropped: u64,

    /// Whether the drain reached its deadline before its queue was empty.
    pub deadline_reached: bool,

    /// When its host reported its exit, in milliseconds since the Unix epoch.
    pub retired_ms: u64,
}

/// The latest recipient incarnations the delivery hosts reported retired, oldest first, at most [`KEPT_RETIRED`].
#[derive(Debug, Default)]
pub(super) struct RetiredRecipients {
    entries: VecDeque<RecipientEntry>,
}

impl RetiredRecipients {
    /// Records that `recipient` of `host`, which `provenance` declared, retired at `at` after the drain `report`
    /// describes.
    pub(super) fn record(
        &mut self, host: &HostId, recipient: &RecipientRef, report: &RetirementReport, provenance: Provenance,
        at: SystemTime,
    ) {
        let retired_ms = unix_millis(at);
        if self.entries.len() == KEPT_RETIRED {
            self.entries.pop_front();
        }
        self.entries.push_back(RecipientEntry {
            host: host.to_string(),
            id: recipient.id.to_string(),
            slot: recipient.slot.get(),
            incarnation: recipient.incarnation,
            state: RecipientEntryState::Retired,
            provenance: Some(provenance),
            draining_since_ms: None,
            last_drain: Some(LastDrain {
                delivered: report.delivered,
                persisted: report.persisted,
                dropped: report.dropped,
                deadline_reached: report.deadline_reached,
                retired_ms,
            }),
        });
    }

    /// Returns the entries, oldest first.
    pub(super) fn entries(&self) -> Vec<RecipientEntry> {
        self.entries.iter().cloned().collect()
    }
}

/// Returns every recipient incarnation the status route lists: those the resource table holds, by host and slot, each
/// with when its drain started as its host reports it, then those retired lately, oldest first.
pub(super) fn recipient_entries(
    table: &ResourceTable, hosts: &BTreeMap<String, DeliveryHostStatus>, retired: &RetiredRecipients,
) -> Vec<RecipientEntry> {
    let mut entries = table
        .recipients()
        .into_iter()
        .map(|held| {
            let reported = delivery_ref(&held.recipient);
            let draining_since_ms = hosts
                .get(held.host.as_str())
                .and_then(|status| status.recipients.iter().find(|status| status.recipient == reported))
                .and_then(|status| status.draining_since)
                .map(unix_millis);
            RecipientEntry {
                host: held.host.to_string(),
                id: held.recipient.id.to_string(),
                slot: held.recipient.slot.get(),
                incarnation: held.recipient.incarnation,
                state: held.state.into(),
                provenance: Some(Provenance::from(&held.provenance)),
                draining_since_ms,
                last_drain: None,
            }
        })
        .collect::<Vec<_>>();
    entries.extend(retired.entries());
    entries
}

/// Returns a time in milliseconds since the Unix epoch.
fn unix_millis(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// The resource budget.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct BudgetView {
    /// The bytes the memory limit's headroom grants resources created after boot, or `None` when nothing is counted.
    pub granted: Option<u64>,

    /// The bytes the credits held take from it.
    pub used: u64,
}

/// What the resources view is made of.
pub(super) struct Parts<'a> {
    pub(super) hosts: &'a FixedHosts,
    pub(super) table: &'a ResourceTable,
    pub(super) host_status: &'a BTreeMap<String, DeliveryHostStatus>,
    pub(super) lowered: &'a NativeResources,
    pub(super) labels: &'a std::collections::BTreeMap<saluki_metric_plan::ir::OutputId, String>,
    pub(super) config_states: &'a [ConfigAcknowledgment],
    pub(super) installed_revision: Option<u64>,
    pub(super) missing_credentials: Vec<String>,
    pub(super) budget: BudgetUsage,
}

/// Returns the resources view of the installer's state.
pub(super) fn resources_view(parts: Parts<'_>) -> ResourcesView {
    let Parts {
        hosts,
        table,
        host_status,
        lowered,
        labels,
        config_states,
        installed_revision,
        missing_credentials,
        budget,
    } = parts;
    let declared = config_states
        .iter()
        .find(|state| state.product == ArtifactProduct::Resources)
        .map(|state| DeclaredView {
            config: state.id.to_string(),
            state: state.state,
            revision: state.revision,
            error: state.error.clone(),
            installed_revision,
        });
    let outputs = lowered
        .outputs
        .iter()
        .map(|output| {
            let decl = &output.decl;
            let provenance = decl
                .binding
                .as_ref()
                .map_or(Provenance::Native, |binding| Provenance::from(&binding.provenance));
            OutputView {
                id: decl.id.to_string(),
                label: labels.get(&decl.id).cloned().unwrap_or_else(|| match &output.kind {
                    OutputKind::Declared { name, .. } => name.clone(),
                    _ => decl.id.to_string(),
                }),
                host: decl.binding.as_ref().map(|binding| binding.host.to_string()),
                fingerprint: decl.fingerprint,
                legacy_fingerprint: output.legacy_fingerprint,
                recipients: decl.recipients.iter().map(ToString::to_string).collect(),
                provenance,
            }
        })
        .collect();
    let recipients = lowered
        .resources
        .recipients
        .iter()
        .map(|recipient| RecipientView {
            id: recipient.id.to_string(),
            host: recipient.host.to_string(),
            endpoint: spelled_endpoint(&recipient.endpoint),
            credential: recipient.credential.to_string(),
            classes: recipient.classes,
            delivery: recipient.delivery,
            provenance: Provenance::from(&recipient.provenance),
        })
        .collect();
    ResourcesView {
        declared,
        missing_credentials,
        hosts: hosts
            .iter()
            .map(|host| {
                let status = host_status.get(host.delivery);
                HostView {
                    host: host.delivery.to_string(),
                    encoder: host.encoder.to_string(),
                    declared_recipients: host.declared_recipients,
                    leases: table
                        .lease_states(&host.id())
                        .into_iter()
                        .map(|(lease, state)| LeaseView {
                            lease: lease.lease.get(),
                            output: lease.key.output.to_string(),
                            state: state.into(),
                        })
                        .collect(),
                    withdrawn: status
                        .map(|status| status.withdrawn.iter().map(ToString::to_string).collect())
                        .unwrap_or_default(),
                    conflicts: status
                        .map(|status| {
                            status
                                .conflicts
                                .iter()
                                .map(|conflict| ConflictView {
                                    declared: conflict.declared.id.to_string(),
                                    duplicates: conflict.duplicates.id.to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    client_failure: status.and_then(|status| status.client_failure.clone()),
                }
            })
            .collect(),
        outputs,
        recipients,
        budget: BudgetView {
            granted: budget.granted,
            used: budget.used,
        },
    }
}
