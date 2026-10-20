//! The resources section of the installer's status: what the default program delivers through, as the control
//! surface serves it; and the status route's document, with the telemetry contract's series the active plan reports.

use std::collections::BTreeMap;
use std::{
    collections::VecDeque,
    time::{SystemTime, UNIX_EPOCH},
};

use saluki_components::forwarders::delivery::DeliveryHostStatus;
use saluki_metric_plan::{
    channel::ApplyStates,
    extension::DeliveryPolicy,
    install::{LeaseState, RecipientRef, RecipientState, ResourceTable, RetirementReport},
    ir::{HostId, PayloadClasses},
    validate::Unmet,
};
use serde::Serialize;

use super::budget::BudgetUsage;
use crate::programmable_metrics::{
    compatibility,
    resources::{specs::delivery_ref, spelled_endpoint, FixedHosts, NativeResources},
};

/// What the default program delivers through: the delivery hosts, the outputs, the recipients, and the resource
/// budget.
///
/// Outputs and recipients are those of the default program last lowered: what the current configuration declares.
/// Credentials appear by reference only.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ResourcesView {
    /// The delivery hosts, each with its encode host.
    pub hosts: Vec<HostView>,

    /// Every output.
    pub outputs: Vec<OutputView>,

    /// Every recipient, of every host.
    pub recipients: Vec<RecipientView>,

    /// The resource budget everything added after boot is admitted against.
    pub budget: BudgetView,
}

/// A delivery host, with its encode host.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HostView {
    /// The delivery host's component ID.
    pub host: String,

    /// Its encode host's component ID.
    pub encoder: String,

    /// The leases its encode host runs, by identifier: those generations use, those a generation prepares, and those
    /// draining.
    pub leases: Vec<LeaseView>,

    /// The recipients it drops every payload to, by identity: those the latest lowering no longer declares.
    pub withdrawn: Vec<String>,

    /// The recipients that send to the same endpoint with the same key as another of its recipients, which the host
    /// detects for recipients it holds under a declared origin: ADP creates none, so the list is empty.
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

/// A recipient that sends to the same endpoint with the same key as another recipient of its host.
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

    /// The identities of its recipients, sorted.
    pub recipients: Vec<String>,
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
}

/// The status route's document: the apply states, as a poll reports them, the recipient incarnations, which only this
/// route lists, as `recipients`, and the telemetry contract's series, as `telemetry`.
#[derive(Serialize)]
pub struct StatusDocument<'a> {
    /// The apply states, whose members are the document's own.
    #[serde(flatten)]
    pub apply_states: &'a ApplyStates,

    /// The recipient incarnations.
    pub recipients: &'a [RecipientEntry],

    /// The series the remapper reads by node name, and which the active plan reports; absent while no generation is
    /// active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryView>,
}

impl<'a> StatusDocument<'a> {
    /// Returns the document of the installer's status.
    pub fn of(status: &'a super::PlanStatus) -> Self {
        Self {
            apply_states: &status.apply_states,
            recipients: &status.recipients,
            telemetry: status
                .active
                .as_ref()
                .map(|active| TelemetryView::of(&status.default_program, &active.plan)),
        }
    }
}

/// Which series of the telemetry contract the active plan reports: for each series the remapper reads from a node of
/// the default program by its name, the node producing it, or why none does.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TelemetryView {
    /// The series the active plan reports, each with its node.
    pub met: Vec<MetSeries>,

    /// The series the active plan does not report, each with why.
    pub unmet: Vec<UnmetSeries>,
}

/// A series of the telemetry contract the active plan reports.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MetSeries {
    /// The series, as the remapper produces it.
    pub series: String,

    /// The node reporting it.
    pub node: String,
}

/// A series of the telemetry contract the active plan does not report.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UnmetSeries {
    /// The series, as the remapper produces it.
    pub series: String,

    /// The node the remapper reads it from.
    pub node: String,

    /// Why the plan does not report it: it has no node of that name, or the node runs another operator.
    pub reason: String,
}

impl TelemetryView {
    /// Returns which series of the contract `default_program` meets the plan `active` reports.
    fn of(default_program: &saluki_metric_plan::ir::Plan, active: &saluki_metric_plan::ir::Plan) -> Self {
        let report = compatibility::report(active, &compatibility::of_default(default_program));
        Self {
            met: report
                .met
                .into_iter()
                .map(|(series, node)| MetSeries {
                    series,
                    node: node.to_string(),
                })
                .collect(),
            unmet: report
                .unmet
                .into_iter()
                .map(|(series, unmet)| UnmetSeries {
                    series,
                    node: unmet.node().to_string(),
                    reason: match &unmet {
                        Unmet::NoNode { node } => format!("no node is named `{}`", node),
                        Unmet::WrongOperator { operator, expected, .. } => {
                            format!("the node runs `{}`, not `{}`", operator, expected)
                        }
                    },
                })
                .collect(),
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
    /// Records that `recipient` of `host` retired at `at` after the drain `report` describes.
    pub(super) fn record(
        &mut self, host: &HostId, recipient: &RecipientRef, report: &RetirementReport, at: SystemTime,
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
        budget,
    } = parts;
    let outputs = lowered
        .outputs
        .iter()
        .map(|output| {
            let decl = &output.decl;
            OutputView {
                id: decl.id.to_string(),
                label: labels.get(&decl.id).cloned().unwrap_or_else(|| decl.id.to_string()),
                host: decl.binding.as_ref().map(|binding| binding.host.to_string()),
                recipients: decl.recipients.iter().map(ToString::to_string).collect(),
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
        })
        .collect();
    ResourcesView {
        hosts: hosts
            .iter()
            .map(|host| {
                let status = host_status.get(host.delivery);
                HostView {
                    host: host.delivery.to_string(),
                    encoder: host.encoder.to_string(),
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
