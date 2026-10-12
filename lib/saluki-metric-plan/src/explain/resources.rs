//! The resources section of explain documents: every output binding, delivery host setting, and recipient a plan
//! carries, with who declared it, and, once compared with the active plan, whether each is reused or new.
//!
//! The plan crate does not read what host parameters mean, so encoder parameters, host settings, and recipient
//! protocols are shown as the plan carries them. Credentials are references, never values.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::sorted;
use crate::{
    extension::DeliveryPolicy,
    install::diff::{resource_changes, ResourceChange},
    ir::{
        CredentialRef, HostId, NativeComponentId, OutputId, ParamName, PayloadClasses, Plan, RecipientId,
        ResourceProvenance, Selector,
    },
};

/// The resources a plan's outputs deliver through.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ResourcesEntry {
    /// Every output bound to a delivery host, in plan order.
    pub outputs: Vec<BoundOutputEntry>,

    /// The settings of every delivery host the plan declares, in plan order.
    pub hosts: Vec<HostSettingsEntry>,

    /// Every recipient the plan declares, in plan order.
    pub recipients: Vec<RecipientEntry>,

    /// What the active plan declares and this plan no longer does, once compared with it
    /// ([`Explain::with_resource_changes`][super::Explain::with_resource_changes]).
    #[serde(default, skip_serializing_if = "RemovedResources::is_empty")]
    pub removed: RemovedResources,
}

/// One output's binding to a delivery host.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BoundOutputEntry {
    /// The output.
    pub id: OutputId,

    /// The delivery host it delivers through, whose encode host runs its encoder stream as a lease.
    pub host: HostId,

    /// What its declaration says it delivers to, which its fingerprint covers.
    pub selector: Selector,

    /// Its encoder's parameters, as the host's schema declares them.
    pub encoder: BTreeMap<ParamName, Value>,

    /// The component identifier its encoder telemetry reports under.
    pub telemetry: NativeComponentId,

    /// Who declared it: native configuration, or a resources artifact.
    pub provenance: ResourceProvenance,

    /// Whether the active plan binds the output alike (`reused`), otherwise (`rebound`), or not at all (`new`), once
    /// compared with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
}

/// One delivery host's settings.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct HostSettingsEntry {
    /// The delivery host.
    pub host: HostId,

    /// Its settings, as its schema declares them.
    pub settings: BTreeMap<ParamName, Value>,

    /// Whether the active plan declares them alike (`reused`), otherwise (`reconfigured`), or not at all (`new`), once
    /// compared with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
}

/// One recipient.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RecipientEntry {
    /// The recipient's identity.
    pub id: RecipientId,

    /// Its delivery host.
    pub host: HostId,

    /// Its endpoint.
    pub endpoint: String,

    /// The reference of the credential it sends with.
    pub credential: CredentialRef,

    /// Which payloads it receives when a payload does not name its recipients.
    pub classes: PayloadClasses,

    /// Its protocol, as its host's schema declares it.
    pub protocol: BTreeMap<ParamName, Value>,

    /// Its delivery policy.
    pub delivery: DeliveryPolicy,

    /// Who declared it: native configuration, or a resources artifact.
    pub provenance: ResourceProvenance,

    /// The bound outputs that deliver to it, in plan order.
    pub outputs: Vec<OutputId>,

    /// Whether the active plan declares it alike (`reused`), otherwise (`reconfigured`), or not at all (`new`), once
    /// compared with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
}

/// What the active plan declares and the explained plan no longer does.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct RemovedResources {
    /// The outputs, in the active plan's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputId>,

    /// The delivery hosts' settings, in the active plan's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<HostId>,

    /// The recipients, in the active plan's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<RecipientId>,
}

impl RemovedResources {
    /// Returns `true` if nothing was removed, which serialization leaves out.
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty() && self.hosts.is_empty() && self.recipients.is_empty()
    }
}

fn params(params: &crate::ir::Params) -> BTreeMap<ParamName, Value> {
    params
        .iter()
        .map(|(name, value)| (name.clone(), sorted(value)))
        .collect()
}

impl ResourcesEntry {
    /// Describes the resources of a plan, or returns `None` if it has no bound output and no resources.
    pub(super) fn of(plan: &Plan) -> Option<Self> {
        let outputs = plan
            .outputs
            .iter()
            .filter_map(|output| {
                let binding = output.binding.as_ref()?;
                Some(BoundOutputEntry {
                    id: output.id.clone(),
                    host: binding.host.clone(),
                    selector: binding.selector.clone(),
                    encoder: params(&binding.encoder),
                    telemetry: binding.telemetry.clone(),
                    provenance: binding.provenance.clone(),
                    change: None,
                })
            })
            .collect::<Vec<_>>();
        if outputs.is_empty() && plan.resources.is_empty() {
            return None;
        }
        let hosts = plan
            .resources
            .hosts
            .iter()
            .map(|binding| HostSettingsEntry {
                host: binding.host.clone(),
                settings: params(&binding.settings),
                change: None,
            })
            .collect();
        let recipients = plan
            .resources
            .recipients
            .iter()
            .map(|recipient| RecipientEntry {
                id: recipient.id.clone(),
                host: recipient.host.clone(),
                endpoint: recipient.endpoint.clone(),
                credential: recipient.credential.clone(),
                classes: recipient.classes,
                protocol: params(&recipient.protocol),
                delivery: recipient.delivery,
                provenance: recipient.provenance.clone(),
                outputs: plan
                    .outputs
                    .iter()
                    .filter(|output| output.binding.is_some() && output.recipients.contains(&recipient.id))
                    .map(|output| output.id.clone())
                    .collect(),
                change: None,
            })
            .collect();
        Some(Self {
            outputs,
            hosts,
            recipients,
            removed: RemovedResources::default(),
        })
    }

    /// Marks each resource of `candidate`, the explained plan, as reused or new against `active`, and lists what only
    /// `active` declares.
    pub(super) fn compare(&mut self, active: &Plan, candidate: &Plan) {
        let name = |change: ResourceChange| match change {
            ResourceChange::Added => "new",
            other => other.as_str(),
        };
        for output in &mut self.outputs {
            if candidate.output(&output.id).is_none() {
                continue;
            }
            output.change = Some(
                match active.output(&output.id) {
                    None => "new",
                    Some(_) if active.binds_alike(candidate, &output.id) => "reused",
                    Some(_) => "rebound",
                }
                .to_string(),
            );
        }
        let (recipients, hosts) = resource_changes(active, candidate);
        for diff in recipients {
            match self.recipients.iter_mut().find(|entry| entry.id == diff.id) {
                Some(entry) => entry.change = Some(name(diff.change).to_string()),
                None if diff.change == ResourceChange::Removed => self.removed.recipients.push(diff.id),
                None => {}
            }
        }
        for diff in hosts {
            match self.hosts.iter_mut().find(|entry| entry.host == diff.host) {
                Some(entry) => entry.change = Some(name(diff.change).to_string()),
                None if diff.change == ResourceChange::Removed => self.removed.hosts.push(diff.host),
                None => {}
            }
        }
        let bound = candidate
            .outputs
            .iter()
            .filter(|output| output.binding.is_some())
            .map(|output| &output.id)
            .collect::<BTreeSet<_>>();
        self.removed.outputs = active
            .outputs
            .iter()
            .filter(|output| output.binding.is_some() && !bound.contains(&output.id))
            .map(|output| output.id.clone())
            .collect();
    }
}
