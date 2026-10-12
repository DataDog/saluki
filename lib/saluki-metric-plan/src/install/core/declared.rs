//! The installer core's resources product: resources artifacts and their revisions, the revision lowering declares
//! resources from, the acknowledgments of resources configs, and the keep rule that follows every lowering.
//!
//! # Revisions
//!
//! The resources product follows the policy product's revision rules on a high-water mark of its own: a config's
//! revision must be newer than any authentic one seen, a redelivery (the same revision and artifact hash) changes
//! nothing, the same document under a newer revision is approved again, and an empty snapshot revokes every declared
//! resource. The caller checks each snapshot's shape, transport hash, and envelope, validates the document, and hands
//! the core one [`ResourcesEvent`].
//!
//! Lowering declares resources from [`accepted_resources`][InstallerCore::accepted_resources]: the newest accepted
//! revision that is neither rejected nor pending, else the revision in use when it was accepted, if that one is
//! neither. A newer revision that is refused, rejected, or pending therefore leaves the one in use in place, as a
//! refused policy delivery does not hide what an earlier one installed: declared resources are removed only by a
//! revocation, or when the revision in use itself becomes pending (a credential it references removed).
//!
//! | State | When |
//! |---|---|
//! | `rejected` | the document failed validation, the revision was stale, or a declared resource it adds failed to prepare (no retry: a corrected artifact is published) |
//! | `pending`, with the error naming the credentials | a credential it references is not configured; re-evaluated at every configuration change |
//! | `pending` | accepted, and the generation lowered with it is not active yet |
//! | `active` | the active generation was lowered with it |
//! | `retired` | a newer revision or a revocation replaced it, and the last generation lowered with it retired |
//!
//! # The keep rule
//!
//! After every event, the generation kept to roll back to, and the plan a cut replaced, are kept only while every
//! output their plan declares is declared by the latest lowered plan with an equal fingerprint, and every recipient
//! their plan declares is declared there with the same protocol: a rollback never resurrects a removed destination,
//! and the withdrawn set covers the time before the core sees the lowering.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use saluki_core::execution::GenerationId;

use super::{Action, CandidateId, InstallerCore};
use crate::{
    canonical::PlanHash,
    channel::{ActiveArtifact, ApplyState, ArtifactConfigId, ArtifactProduct, ConfigAcknowledgment, ResourcesStatus},
    install::{
        lifecycle::{GenerationState, LifecycleEvent},
        resources::{CreditKey, ResourcesEvent},
    },
    ir::{ArtifactRevision, CredentialRef, HostId, Plan, ResourceProvenance},
    resources::ResourcesArtifact,
};

/// The most resources configs absent from the latest snapshot whose final acknowledgment the core keeps.
const CONFIG_HISTORY: usize = 16;

/// One accepted resources artifact: its config, revision, hash, document, and the credentials it references.
#[derive(Debug)]
pub(super) struct Declared {
    config: ArtifactConfigId,
    revision: u64,
    hash: PlanHash,
    document: ResourcesArtifact,
    credentials: BTreeSet<CredentialRef>,
}

impl Declared {
    /// Returns the revision and hash the artifact was delivered with.
    fn artifact_revision(&self) -> ArtifactRevision {
        ArtifactRevision {
            revision: self.revision,
            hash: self.hash,
        }
    }
}

/// What the core keeps about one resources config the channel delivered.
#[derive(Debug)]
struct DeclaredConfig {
    /// The acknowledgment reported.
    ack: ConfigAcknowledgment,

    /// The accepted artifact its latest delivery is, which its acknowledgment follows; `None` for a delivery refused
    /// on receipt.
    follows: Option<Arc<Declared>>,

    /// Whether the latest snapshot holds it.
    delivered: bool,

    /// When the acknowledgment last changed, in the core's order of changes, so that the oldest are forgotten first.
    changed: u64,
}

/// What the core keeps about the resources product.
#[derive(Debug, Default)]
pub(super) struct DeclaredState {
    /// The newest revision of an authentic config seen.
    high_water: Option<u64>,

    /// The newest accepted artifact.
    latest: Option<Arc<Declared>>,

    /// The artifact in use when [`latest`][Self::latest] was accepted, while `latest` has not been usable: it stays in
    /// use while `latest` is rejected or pending.
    previous: Option<Arc<Declared>>,

    /// The credentials the accepted documents reference that the configuration does not provide, as last reported.
    missing: BTreeSet<CredentialRef>,

    /// Why each revision a prepare failure was attributed to is rejected, by revision.
    rejected: BTreeMap<u64, String>,

    /// Every resources config delivered, by config ID.
    configs: BTreeMap<ArtifactConfigId, DeclaredConfig>,

    /// The artifact each candidate the core holds was lowered with.
    lowered_with: BTreeMap<CandidateId, Arc<Declared>>,

    /// The artifact the latest lowering declared resources from, which later candidates are lowered with.
    pub(super) lowering: Option<Arc<Declared>>,

    /// The number of acknowledgment changes so far.
    changes: u64,
}

impl DeclaredState {
    /// Returns `true` if lowering may declare the artifact's resources: it is not rejected, and every credential it
    /// references is configured.
    fn usable(&self, declared: &Declared) -> bool {
        !self.rejected.contains_key(&declared.revision) && declared.credentials.is_disjoint(&self.missing)
    }

    /// Returns the artifact lowering declares resources from.
    pub(super) fn installed_revision(&self) -> Option<Arc<Declared>> {
        [&self.latest, &self.previous]
            .into_iter()
            .flatten()
            .find(|declared| self.usable(declared))
            .cloned()
    }

    /// Forgets the artifact in use before the latest once the latest is usable: from then on it alone is in use.
    fn normalize(&mut self) {
        if self.latest.as_ref().is_some_and(|latest| self.usable(latest)) {
            self.previous = None;
        }
    }

    /// Records the artifact a candidate was lowered with: the one the latest lowering declared resources from. A
    /// candidate already recorded, such as a rollback that adopted the record of the plan it installs again, keeps
    /// its record.
    pub(super) fn note_candidate(&mut self, candidate: CandidateId) {
        if let Some(lowering) = &self.lowering {
            self.lowered_with
                .entry(candidate)
                .or_insert_with(|| Arc::clone(lowering));
        }
    }

    /// Makes the candidate `target`, whose plan the candidate `served` equals, lowered with what `served` was lowered
    /// with, or with nothing: equal plans declare equal resources, so the generation that holds the plan installs the
    /// latest lowering, such as one with a resources revision that declares nothing new.
    pub(super) fn serve(&mut self, target: CandidateId, served: CandidateId) {
        match self.lowered_with.get(&served).cloned() {
            Some(declared) => {
                self.lowered_with.insert(target, declared);
            }
            None => {
                self.lowered_with.remove(&target);
            }
        }
    }

    /// Makes a candidate lowered with what `from` was lowered with.
    pub(super) fn adopt(&mut self, candidate: CandidateId, from: CandidateId) {
        if let Some(declared) = self.lowered_with.get(&from).cloned() {
            self.lowered_with.insert(candidate, declared);
        }
    }

    /// Forgets the records of candidates the core no longer holds.
    pub(super) fn prune(&mut self, held: &BTreeSet<CandidateId>) {
        self.lowered_with.retain(|candidate, _| held.contains(candidate));
    }

    /// Returns the waiting credentials of an artifact, in words, if it waits for any.
    fn waiting_for(&self, declared: &Declared) -> Option<String> {
        let missing = declared
            .credentials
            .intersection(&self.missing)
            .map(|credential| format!("`{}`", credential))
            .collect::<Vec<_>>();
        (!missing.is_empty()).then(|| {
            format!(
                "waiting for {} {}: the configuration does not provide {}; configure {} under \
                 `experimental.programmable_metrics.resources.credentials`",
                if missing.len() == 1 {
                    "credential"
                } else {
                    "credentials"
                },
                missing.join(", "),
                if missing.len() == 1 { "it" } else { "them" },
                if missing.len() == 1 { "it" } else { "them" },
            )
        })
    }

    /// Replaces a config's acknowledgment with its latest delivery's.
    fn track(
        &mut self, config: ArtifactConfigId, version: u64, declared: Option<(u64, PlanHash)>, state: ApplyState,
        error: Option<String>, follows: Option<Arc<Declared>>,
    ) {
        self.changes += 1;
        let ack = ConfigAcknowledgment {
            product: ArtifactProduct::Resources,
            id: config.clone(),
            version,
            revision: declared.map(|(revision, _)| revision),
            artifact_hash: declared.map(|(_, hash)| hash),
            state,
            generation: None,
            cut_at: None,
            error,
        };
        for tracked in self.configs.values_mut() {
            tracked.delivered = false;
        }
        self.configs.insert(
            config,
            DeclaredConfig {
                ack,
                follows,
                delivered: true,
                changed: self.changes,
            },
        );
    }
}

/// Returns the credentials a resources document references.
fn credentials_of(document: &ResourcesArtifact) -> BTreeSet<CredentialRef> {
    document
        .recipients
        .iter()
        .map(|recipient| recipient.credential.clone())
        .collect()
}

/// Returns `true` if a plan the core keeps, to roll back to or to install again after a failed cut, may stay kept
/// under the latest lowering: every output it declares is declared there with an equal fingerprint, and every recipient
/// it declares is declared there with the same protocol.
pub(in crate::install) fn keeps(latest: &Plan, kept: &Plan) -> bool {
    let outputs = kept.outputs.iter().all(|output| {
        latest
            .outputs
            .iter()
            .any(|declared| declared.id == output.id && declared.fingerprint == output.fingerprint)
    });
    let recipients = kept.resources.recipients.iter().all(|recipient| {
        latest
            .resources
            .recipient(&recipient.id)
            .is_some_and(|declared| declared.protocol == recipient.protocol)
    });
    outputs && recipients
}

impl InstallerCore {
    /// Hands the core what a snapshot of the resources product says, and returns what to do.
    ///
    /// The core applies the policy product's revision rules with the product's own high-water mark: acknowledgments
    /// kept per config, a redelivery recognized by its artifact hash, a stale revision rejected, revocation. It keeps
    /// the accepted document ([`accepted_resources`][Self::accepted_resources]), and asks for
    /// [`Action::InstallDefaultProgram`] when what lowering should see changed. A refused config's revision is not
    /// seen; an invalid document's is, as for a policy artifact that fails composition.
    pub fn on_resources(&mut self, event: ResourcesEvent) -> Vec<Action> {
        let before = self.declared.installed_revision();
        match event {
            ResourcesEvent::Revoked => {
                self.declared.latest = None;
                self.declared.previous = None;
                for tracked in self.declared.configs.values_mut() {
                    tracked.delivered = false;
                }
            }
            ResourcesEvent::Refused {
                config,
                version,
                reason,
            } => self
                .declared
                .track(config, version, None, ApplyState::Rejected, Some(reason), None),
            ResourcesEvent::Delivered {
                config,
                version,
                revision,
                artifact_hash,
                document,
            } => self.deliver_resources(config, version, revision, artifact_hash, document),
        }
        self.declared.normalize();
        let mut actions = Vec::new();
        self.ask_for_lowering(before, &mut actions);
        self.settle(actions, super::artifacts::Settle::Quiet)
    }

    /// Handles the delivery of the product's one config, read and validated.
    fn deliver_resources(
        &mut self, config: ArtifactConfigId, version: u64, revision: u64, hash: PlanHash,
        document: Result<ResourcesArtifact, String>,
    ) {
        let declared = Some((revision, hash));
        let state = &mut self.declared;
        // A redelivery of the latest accepted artifact, such as after a worker restart, changes nothing.
        if let Some(latest) = state
            .latest
            .clone()
            .filter(|latest| latest.revision == revision && latest.hash == hash)
        {
            if state.configs.get(&config).is_none_or(|tracked| {
                !tracked
                    .follows
                    .as_ref()
                    .is_some_and(|follows| Arc::ptr_eq(follows, &latest))
            }) {
                state.track(config, version, declared, ApplyState::Pending, None, Some(latest));
            }
            return;
        }
        if let Some(high_water) = state.high_water.filter(|high_water| revision <= *high_water) {
            let message = format!(
                "revision {} is stale: revision {} has already been seen, and revisions must increase",
                revision, high_water
            );
            state.track(config, version, declared, ApplyState::Rejected, Some(message), None);
            return;
        }
        state.high_water = Some(revision);
        let document = match document {
            Ok(document) => document,
            Err(reason) => {
                state.track(config, version, declared, ApplyState::Rejected, Some(reason), None);
                return;
            }
        };
        let accepted = Arc::new(Declared {
            config: config.clone(),
            revision,
            hash,
            credentials: credentials_of(&document),
            document,
        });
        // The artifact in use stays in use until the newly accepted one is usable.
        state.previous = state.installed_revision();
        state.latest = Some(Arc::clone(&accepted));
        state.track(config, version, declared, ApplyState::Pending, None, Some(accepted));
    }

    /// Tells the core which credentials the accepted resources documents reference that the configuration does not
    /// provide, before every lowering: a revision that references one is pending, and the revision in use before it
    /// keeps running, so a change is lowered once. The caller lowers right after, so the core asks for no lowering.
    pub fn credentials_changed(&mut self, missing: BTreeSet<CredentialRef>) -> Vec<Action> {
        self.declared.missing = missing;
        self.declared.normalize();
        self.settle(Vec::new(), super::artifacts::Settle::Quiet)
    }

    /// Returns the resources artifact lowering declares resources from: the newest accepted revision that is neither
    /// rejected nor pending (the one installing, else the one active), or `None` once the product is revoked.
    ///
    /// A newer revision that is refused, rejected, or pending leaves the revision installed before it in place, as a
    /// refused policy delivery does not hide what an earlier one installed: declared resources are removed only by a
    /// revocation, or when the installed revision itself becomes pending.
    pub fn accepted_resources(&self) -> Option<&ResourcesArtifact> {
        [&self.declared.latest, &self.declared.previous]
            .into_iter()
            .flatten()
            .find(|declared| self.declared.usable(declared))
            .map(|declared| &declared.document)
    }

    /// Returns the revision and hash the resources artifact lowering declares resources from was delivered with
    /// ([`accepted_resources`][Self::accepted_resources]), which the resources it declares record as their provenance.
    pub fn accepted_revision(&self) -> Option<ArtifactRevision> {
        self.declared
            .installed_revision()
            .map(|declared| declared.artifact_revision())
    }

    /// Returns every resources artifact the core holds, newest first, with the revision and hash each was delivered
    /// with: the newest accepted revision, and, while that one is rejected or pending, the revision in use when it was
    /// accepted.
    ///
    /// These are the documents whose credentials [`credentials_changed`][Self::credentials_changed] must be told
    /// about: a held revision is usable while none of its credentials is among those reported missing, so one whose
    /// missing credential is left out of the report is taken as usable. The one lowering declares resources from is
    /// among them.
    pub fn held_resources(&self) -> Vec<(&ResourcesArtifact, ArtifactRevision)> {
        [&self.declared.latest, &self.declared.previous]
            .into_iter()
            .flatten()
            .map(|declared| (&declared.document, declared.artifact_revision()))
            .collect()
    }

    /// Asks for the default program if the artifact lowering declares resources from changed since `before`.
    fn ask_for_lowering(&self, before: Option<Arc<Declared>>, actions: &mut Vec<Action>) {
        let after = self.declared.installed_revision();
        let same = match (&before, &after) {
            (Some(before), Some(after)) => Arc::ptr_eq(before, after),
            (None, None) => true,
            _ => false,
        };
        if !same
            && !actions
                .iter()
                .any(|action| matches!(action, Action::InstallDefaultProgram))
        {
            actions.push(Action::InstallDefaultProgram);
        }
    }

    /// Rejects the resources revision a declared resource that failed to prepare came from, and asks for a lowering
    /// without it: the revision in use before it, if any, keeps running, and native configuration changes keep
    /// applying. Only the newest accepted revision can be rejected so: an older one was prepared before.
    fn reject_declared(&mut self, provenance: &ResourceProvenance, reason: &str, actions: &mut Vec<Action>) {
        let ResourceProvenance::Declared(revision) = provenance else {
            return;
        };
        let is_latest = self
            .declared
            .latest
            .as_ref()
            .is_some_and(|latest| latest.revision == revision.revision && latest.hash == revision.hash);
        if !is_latest || self.declared.rejected.contains_key(&revision.revision) {
            return;
        }
        let before = self.declared.installed_revision();
        self.declared.rejected.insert(
            revision.revision,
            format!("a resource it declares failed to prepare: {}", reason),
        );
        self.declared.normalize();
        self.ask_for_lowering(before, actions);
    }

    /// Attributes a host's failed acknowledgment of a generation's preparation to the declared resources the message
    /// names, if any: a declared recipient the generation creates first in the host, or a declared output whose lease
    /// it creates there.
    ///
    /// A recipient the part requires or reopens, or creates again under another preparation's reference, was prepared
    /// before: a failure that names it, such as a required recipient whose loop failed before its exit was reported,
    /// is a race the candidate's next preparation recovers from, as for a native recipient, and rejects nothing.
    pub(super) fn attribute_host_failure(
        &mut self, generation: GenerationId, host: &HostId, message: &str, actions: &mut Vec<Action>,
    ) {
        let Some(plan) = self
            .preparing
            .as_ref()
            .filter(|preparing| preparing.installed.generation.id() == generation)
            .map(|preparing| Arc::clone(&preparing.installed.plan))
        else {
            return;
        };
        let (created, leased) = self.resources.created_first(generation, host);
        let recipients = plan
            .resources
            .recipients
            .iter()
            .filter(|recipient| {
                recipient.host == *host && created.contains(&&recipient.id) && message.contains(recipient.id.as_str())
            })
            .map(|recipient| recipient.provenance.clone());
        let outputs = plan
            .outputs
            .iter()
            .filter_map(|output| output.binding.as_ref().map(|binding| (output, binding)))
            .filter(|(output, binding)| {
                binding.host == *host
                    && output.id.is_declared()
                    && leased.contains(&&output.id)
                    && message.contains(output.id.as_str())
            })
            .map(|(_, binding)| binding.provenance.clone());
        let provenances = recipients.chain(outputs).collect::<Vec<_>>();
        for provenance in provenances {
            self.reject_declared(&provenance, message, actions);
        }
    }

    /// Attributes a refusal of the resource budget to the declared resource whose credit it refused, if it is one.
    pub(super) fn attribute_credit_failure(&mut self, credit: &CreditKey, message: &str, actions: &mut Vec<Action>) {
        let Some(plan) = self
            .preparing
            .as_ref()
            .map(|preparing| Arc::clone(&preparing.installed.plan))
        else {
            return;
        };
        let provenance = match credit {
            CreditKey::Recipient(host, recipient) => plan
                .resources
                .recipient(&recipient.id)
                .filter(|declared| declared.host == *host)
                .map(|declared| declared.provenance.clone()),
            CreditKey::Lease(host, lease, _) => self.resources.lease_output(host, *lease).and_then(|output| {
                plan.outputs
                    .iter()
                    .find(|declared| declared.id == *output)
                    .and_then(|declared| declared.binding.as_ref())
                    .map(|binding| binding.provenance.clone())
            }),
            CreditKey::HostQueue(_) | CreditKey::Instances(..) => None,
        };
        if let Some(provenance) = provenance {
            self.reject_declared(&provenance, message, actions);
        }
    }

    /// Releases the generation kept to roll back to, and drops the plan a cut replaced, if the latest lowering no
    /// longer declares an output either declares with its fingerprint, or a recipient either declares with its
    /// protocol.
    pub(super) fn enforce_keep_rule(&mut self, actions: &mut Vec<Action>) {
        let Some(latest) = self.default_program.clone() else {
            return;
        };
        if self
            .last_known_good
            .as_ref()
            .is_some_and(|kept| !keeps(&latest, &kept.plan))
        {
            if let Some(kept) = self.last_known_good.take() {
                let id = kept.generation.id();
                if self.state_of(id) == Some(GenerationState::Prepared) {
                    self.transition(id, LifecycleEvent::Released, None);
                    self.report_generation(id, actions);
                }
            }
        }
        if self
            .rollback
            .as_ref()
            .is_some_and(|rollback| !keeps(&latest, &rollback.candidate.plan))
        {
            self.rollback = None;
        }
    }

    /// Returns the acknowledgment of every resources config, in config ID order.
    pub(super) fn resources_config_states(&self) -> Vec<ConfigAcknowledgment> {
        self.declared
            .configs
            .values()
            .map(|tracked| tracked.ack.clone())
            .collect()
    }

    /// Returns the resources product's status: its high-water mark, and the artifact the active generation was lowered
    /// with. `None` until a resources config was delivered.
    pub(super) fn resources_status(&self) -> Option<ResourcesStatus> {
        if self.declared.high_water.is_none() && self.declared.configs.is_empty() {
            return None;
        }
        Some(ResourcesStatus {
            revision_high_water: self.declared.high_water,
            active_artifact: self.active_lowered_with().map(|declared| ActiveArtifact {
                id: declared.config.clone(),
                revision: declared.revision,
                artifact_hash: declared.hash,
            }),
        })
    }

    /// Returns the resources artifact the active generation was lowered with, unless it failed.
    fn active_lowered_with(&self) -> Option<&Arc<Declared>> {
        let active = self.active.as_ref()?;
        self.declared.lowered_with.get(&active.candidate)
    }

    /// Recomputes the acknowledgment of every resources config that follows an accepted artifact, and forgets the
    /// oldest configs absent from the latest snapshot whose acknowledgment is final.
    pub(super) fn refresh_resources_acknowledgments(&mut self) {
        let held = self.held_candidates();
        self.declared.prune(&held);
        let computed = self
            .declared
            .configs
            .iter()
            .filter_map(|(id, tracked)| {
                let follows = tracked.follows.as_ref()?;
                Some((id.clone(), self.declared_ack(follows, &tracked.ack)))
            })
            .collect::<Vec<_>>();
        for (id, ack) in computed {
            let Some(tracked) = self.declared.configs.get_mut(&id) else {
                continue;
            };
            if tracked.ack != ack {
                self.declared.changes += 1;
                tracked.ack = ack;
                tracked.changed = self.declared.changes;
            }
        }
        let mut forgettable = self
            .declared
            .configs
            .iter()
            .filter(|(_, tracked)| {
                !tracked.delivered && matches!(tracked.ack.state, ApplyState::Rejected | ApplyState::Retired)
            })
            .map(|(id, tracked)| (tracked.changed, id.clone()))
            .collect::<Vec<_>>();
        if forgettable.len() > CONFIG_HISTORY {
            forgettable.sort_unstable();
            for (_, id) in &forgettable[..forgettable.len() - CONFIG_HISTORY] {
                self.declared.configs.remove(id);
            }
        }
    }

    /// Returns the acknowledgment of a config that follows the accepted artifact `declared`, given its acknowledgment
    /// as last computed.
    fn declared_ack(&self, declared: &Arc<Declared>, previous: &ConfigAcknowledgment) -> ConfigAcknowledgment {
        let state = &self.declared;
        let current = [&state.latest, &state.previous]
            .into_iter()
            .flatten()
            .any(|current| Arc::ptr_eq(current, declared));
        let lowered_with = |candidate: CandidateId| {
            state
                .lowered_with
                .get(&candidate)
                .is_some_and(|lowered| Arc::ptr_eq(lowered, declared))
        };
        let active = self
            .active
            .as_ref()
            .filter(|active| !self.has_failed(active.generation.id()) && lowered_with(active.candidate))
            .map(|active| active.generation.id().get());
        let ack = |state: ApplyState, generation: Option<u64>, error: Option<String>| ConfigAcknowledgment {
            state,
            generation,
            cut_at: None,
            error,
            ..previous.clone()
        };
        if let Some(reason) = state.rejected.get(&declared.revision) {
            return ack(ApplyState::Rejected, previous.generation, Some(reason.clone()));
        }
        if current {
            if let Some(waiting) = state.waiting_for(declared) {
                return ack(ApplyState::Pending, None, Some(waiting));
            }
        }
        if let Some(generation) = active {
            return ack(ApplyState::Active, Some(generation), None);
        }
        if current {
            let preparing = self
                .preparing
                .as_ref()
                .filter(|preparing| lowered_with(preparing.installed.candidate))
                .map(|preparing| preparing.installed.generation.id().get());
            return ack(ApplyState::Pending, preparing, None);
        }
        // Replaced by a newer revision or a revocation: retired once no generation lowered with it is alive.
        let alive = self.live.iter().any(|generation| {
            self.records
                .get(generation)
                .is_some_and(|record| lowered_with(record.candidate))
        });
        if alive && previous.state == ApplyState::Active {
            return previous.clone();
        }
        ack(ApplyState::Retired, previous.generation, None)
    }
}
