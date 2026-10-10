//! The installer core's artifact paths: snapshots from the channel, acknowledgments, and the status summary.
//!
//! # The policy in effect
//!
//! The core keeps the artifact it installs instead of the default program, the **policy in effect**: the latest
//! artifact it accepted, until a newer one replaces it, the channel revokes it, it is rejected, or it fails under
//! supervision. Each candidate composed from an artifact remembers the artifact it serves. When the policy's
//! installation ends without it running (it is rejected, or it fails), the policy in effect becomes the artifact of
//! what the schedule will hold again: the artifact of the generation rolled back to, an older artifact still running,
//! or none, for the default program.
//!
//! A revocation revokes every artifact accepted before it: none becomes the policy in effect again, is composed again,
//! or is kept to roll back to, unless the channel delivers it anew and the core accepts it again. A revoked artifact's
//! generation keeps running until the default program, or a newer artifact, replaces it.
//!
//! A configuration change while a policy is in effect composes the policy again against the new default program
//! ([`InstallerCore::on_default_program`]), under the policy's own revision; like a default program, the
//! re-composition waits for a preparation in flight instead of cancelling it. An artifact that composes to the plan
//! already running or being prepared, such as an unmodified export of the default program, creates no generation: the
//! generation that holds its plan serves it.
//!
//! # Acknowledgments
//!
//! Every config the channel delivered has an acknowledgment, kept per config ID. A config refused on receipt is
//! `rejected` with the reason. A config whose artifact was accepted, or whose artifact equals the policy in effect,
//! follows the installation of that artifact: `pending` while it waits or is prepared, `active` while a generation
//! serving it is active, `prepared` while it is kept to roll back to, and then how its installation ended: `rejected`,
//! `failed`, or `retired`. A generation that stops serving it without ending it (superseded and draining, released
//! from last-known-good, or taken over by the default program after a revocation) leaves the acknowledgment as it was
//! until the generation retires.
//!
//! A refused or rejected delivery does not hide what becomes of the artifact an earlier delivery of the same config
//! installed: while that artifact runs, and the latest delivery's artifact does not, a change of its installation (a
//! failure, a rollback to it, its retirement) is acknowledged under the earlier delivery's version and revision.

use std::{collections::BTreeMap, collections::BTreeSet, sync::Arc};

use saluki_core::execution::GenerationId;

use super::{Action, Arrival, Candidate, CandidateId, InstallerCore, Origin, Outcome, Rejection, Report};
use crate::{
    canonical::PlanHash,
    channel::{
        ActiveArtifact, ApplyState, ApplyStates, ArtifactConfig, ArtifactConfigId, ArtifactProduct, ArtifactSnapshot,
        ConfigAcknowledgment, Envelope, StatusSummary,
    },
    compose::{receive_artifact, Approval, Composed, Composer},
    install::lifecycle::{GenerationState, LifecycleEvent},
    ir::ReceivedArtifact,
};

/// The most configs absent from the latest snapshot whose final acknowledgment the core keeps.
const CONFIG_HISTORY: usize = 16;

/// The most config IDs the rejection of a snapshot with more than one config names, so that its acknowledgments stay
/// linear in the number of configs.
const LISTED_CONFIGS: usize = 10;

/// Why a candidate's installation ended when a newer candidate replaced it before it was prepared.
const SUPERSEDED: &str = "a newer candidate replaced it before it was prepared";

/// Why an artifact in flight was dropped when the channel revoked the policy.
const REVOKED: &str = "the channel revoked the policy before the artifact was installed";

/// What an event leaves for the core to settle, once its own actions are known.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Settle {
    /// An event after which the plan the schedule will hold is the one the event chose: the core asks the host for
    /// nothing.
    Quiet,

    /// A preparation failed or timed out, or supervision failed a generation: when nothing in flight will replace the
    /// active generation, and it failed, or it predates the latest configuration change after a rollback or a rejected
    /// policy, the core asks for the default program.
    AfterFailure {
        /// Whether the event rolled back to last-known-good.
        rolled_back: bool,
    },
}

/// An artifact the core accepted from the channel: its integrity checked, its revision newer than any seen, and
/// composed into the default program.
#[derive(Debug)]
struct Accepted {
    /// The config it was delivered in.
    config: ArtifactConfigId,

    /// Its revision.
    revision: u64,

    /// The hash of the artifact document as received.
    hash: PlanHash,

    /// The artifact.
    artifact: ReceivedArtifact,

    /// The active plan hash its approver reviewed it against, if the envelope says.
    approved_against: Option<PlanHash>,

    /// The active plan hash when it was accepted. Re-compositions are compared with it too, so that composing the
    /// artifact again after a configuration change does not make its review stale.
    composed_against: Option<PlanHash>,

    /// The number of revocations before it was accepted: the next one revokes it.
    epoch: u64,
}

impl Accepted {
    fn origin(&self) -> Origin {
        Origin::Artifact {
            revision: self.revision,
            hash: self.hash,
        }
    }
}

/// The artifact a candidate serves.
#[derive(Clone, Debug)]
struct Source {
    artifact: Arc<Accepted>,

    /// Whether the composition had a stale base.
    stale_base: bool,

    /// Whether the composition had a stale review.
    stale_review: bool,

    /// Whether the candidate composes again an artifact that was running when the configuration changed. Its
    /// preparation is tried again after a failure, as a default-program candidate's is, so that configuration changes
    /// are applied eventually.
    retried: bool,

    /// Whether the candidate was created for another plan that the artifact composed to: it serves the artifact
    /// without having been composed from it.
    attributed: bool,
}

/// Whether a composed artifact is a re-composition of the policy in effect after a configuration change.
#[derive(Clone, Copy, Debug)]
enum Recomposition {
    /// A new acceptance.
    No,

    /// A re-composition.
    Of {
        /// Whether the artifact was running when the configuration changed: its preparation is then tried again after
        /// a failure.
        running: bool,
    },
}

/// How the installation of a followed artifact ended.
#[derive(Clone, Debug)]
struct Ended {
    state: ApplyState,
    error: Option<String>,
    generation: Option<GenerationId>,
}

/// The installation of an artifact a config's delivery started: the artifact it was accepted as, or the policy in
/// effect it equalled.
#[derive(Debug)]
struct Followed {
    artifact: Arc<Accepted>,

    /// Its acknowledgment as last computed, under the version, revision, and hash of the delivery that started it.
    ack: ConfigAcknowledgment,

    /// How it ended, once it has.
    ended: Option<Ended>,
}

/// What the core keeps about one config the channel delivered.
#[derive(Debug)]
struct Tracked {
    /// The acknowledgment reported: the latest delivery's refusal, or the latest change of an installation the config's
    /// deliveries started.
    ack: ConfigAcknowledgment,

    /// The installation the latest delivery started. `None` for a delivery refused on receipt.
    latest: Option<Followed>,

    /// The installation an earlier delivery started, while it has not ended and the latest delivery's artifact does not
    /// run: a refused or rejected delivery does not hide what becomes of the artifact that runs instead.
    earlier: Option<Followed>,

    /// When the acknowledgment last changed, in the core's order of changes, so that the oldest are forgotten first.
    changed: u64,
}

impl Tracked {
    /// Returns `true` if nothing changes the acknowledgment any more but a new delivery.
    fn is_final(&self) -> bool {
        is_final(self.ack.state)
            && [&self.latest, &self.earlier]
                .into_iter()
                .flatten()
                .all(|followed| is_final(followed.ack.state))
    }
}

/// What the core keeps about the artifacts the channel delivered.
#[derive(Debug, Default)]
pub(super) struct ArtifactState {
    /// Every config seen, by config ID.
    configs: BTreeMap<ArtifactConfigId, Tracked>,

    /// The artifact each candidate the core holds serves, for candidates composed from an artifact or attributed to
    /// one: pending, preparing, active, kept as last-known-good, or with a generation still alive.
    sources: BTreeMap<CandidateId, Source>,

    /// The policy in effect.
    policy: Option<Arc<Accepted>>,

    /// Why an artifact could not be composed again after the latest configuration change, if one could not, with the
    /// artifact: the host reports it degraded while that artifact is the policy in effect or the active generation
    /// serves it, until a re-composition of it succeeds.
    policy_error: Option<(Arc<Accepted>, String)>,

    /// The number of revocations so far: every artifact accepted before the latest one is revoked.
    revocations: u64,

    /// The revision and artifact hash of the delivery that last moved the high-water mark: a delivery of the policy in
    /// effect under a revision equal to the mark is a redelivery only if the mark is its own.
    high_water_by: Option<(u64, PlanHash)>,

    /// The latest snapshot handled. A redelivery of it, such as after a worker restart, changes nothing.
    last_snapshot: Option<ArtifactSnapshot>,

    /// The candidate created for the latest default program handed to the core, to tell whether a generation predates
    /// the latest configuration change.
    lowered: Option<CandidateId>,

    /// The number of acknowledgment changes so far.
    changes: u64,
}

impl InstallerCore {
    /// Handles a complete snapshot of the policy product from the artifact channel, returning what to do.
    ///
    /// The snapshot replaces the previous one: it revokes, rejects, acknowledges the current state, or proposes the
    /// artifact it holds, composed through `composer`. Every config it holds gets an acknowledgment, which
    /// [`config_states`][Self::config_states] returns; reports of the generations created follow as
    /// [`Action::Report`]s, as for any candidate.
    ///
    /// # Rules
    ///
    /// - **Snapshots** are complete. A snapshot equal to the latest one handled, such as a redelivery after a worker
    ///   restart, changes nothing. An empty snapshot revokes every artifact accepted so far: the policy is no longer in
    ///   effect, an artifact in flight is dropped, an artifact kept to roll back to is released, and the core asks for
    ///   the default program ([`Action::InstallDefaultProgram`]) if an artifact was in effect or running. A revoked
    ///   artifact is never in effect, composed, or kept again unless it is accepted anew. A policy snapshot with more
    ///   than one config rejects all of them and changes no generation. A transport failure delivers no snapshot, so
    ///   nothing changes.
    /// - **Receipt.** A config whose `sha256` is not its payload's transport hash, whose payload is not an envelope,
    ///   whose artifact fails the [receipt checks](crate::compose::receive_artifact), or whose envelope's
    ///   `artifact_hash` is not the hash of the artifact as received, is rejected. Its revision is not seen: it does
    ///   not move the high-water mark.
    /// - **Idempotency.** An artifact whose hash equals the policy in effect, which is active or being prepared,
    ///   acknowledges the current state when it is a redelivery: its revision is the high-water mark, which its own
    ///   delivery set, or above it, which moves the mark (an approval of the same artifact under a newer revision).
    ///   Nothing is composed again, but the delivery becomes the policy in effect: the status summary reports its
    ///   config and revision, and its review is checked against the active plan, as a new acceptance's is.
    /// - **Ordering.** Revisions strictly increase: any other revision at or below the high-water mark is stale and
    ///   rejected. Any revision above it moves the mark, even if the artifact is then rejected. A newer artifact
    ///   cancels a preparation that has not committed. A failure never changes the high-water mark, which lives in the
    ///   core, outside the restartable worker.
    /// - **Composition** goes through the host's [`Composer`]. A stale base or a stale review still installs, flagged
    ///   in status; an artifact that cannot be composed, or whose composed plan is invalid, is rejected, and what runs
    ///   keeps running.
    /// - **Configuration changes** while an artifact is in effect re-compose it against the new default program, under
    ///   the same revision ([`on_default_program`][Self::on_default_program]).
    /// - **Supervision failures** arrive through [`generation_failed`][Self::generation_failed]: a failed artifact
    ///   generation is acknowledged `failed`, and the core rolls back to last-known-good, or to the default program.
    pub fn on_snapshot(&mut self, snapshot: &ArtifactSnapshot, composer: &dyn Composer) -> Vec<Action> {
        if self.artifacts.last_snapshot.as_ref() == Some(snapshot) {
            return Vec::new();
        }
        let mut actions = Vec::new();
        match snapshot.product {
            ArtifactProduct::Policy => match snapshot.configs.as_slice() {
                [] => self.revoke(&mut actions),
                [config] => self.receive(config, composer, &mut actions),
                configs => self.reject_all(configs),
            },
        }
        self.artifacts.last_snapshot = Some(snapshot.clone());
        self.settle(actions, Settle::Quiet)
    }

    /// Proposes the default program lowered after a configuration change, returning its candidate's identifier and
    /// what to do.
    ///
    /// While no artifact is in effect, this is [`propose`][Self::propose]. While one is, the artifact is composed again
    /// through `composer`, against the new default program, and proposed under its own revision instead, so that
    /// parameter changes in native configuration reach the artifact through its configuration bindings; `candidate`
    /// is then not used, and the composer must already compose against it. An artifact that failed under supervision,
    /// or was revoked, is not in effect.
    ///
    /// A re-composition that fails, or whose plan is invalid, leaves the artifact as it was composed before, and the
    /// status summary reports the host degraded while that artifact is in effect or runs, until a later re-composition
    /// of it succeeds or a new artifact replaces it; an artifact that is rejected resolves nothing. The re-composition
    /// of an artifact that was running is tried again after a failed preparation, as a default program is. Like a
    /// default program, a re-composition waits for a preparation in flight, replacing any candidate waiting, instead of
    /// cancelling it, so that a stream of configuration changes always commits.
    pub fn on_default_program(&mut self, candidate: Candidate, composer: &dyn Composer) -> (CandidateId, Vec<Action>) {
        if candidate.origin.can_roll_back() {
            // Not a default program: proposed as any artifact candidate is, with its revision checked.
            return self.propose(candidate);
        }
        let Some(policy) = self.artifacts.policy.clone() else {
            let id = self.candidate_id();
            let mut actions = Vec::new();
            if self.submit(id, candidate, Arrival::Queue, &mut actions) {
                // The generation that holds the default program's plan serves the default program, even if it was
                // created for a revoked artifact that composed to the same plan.
                if let Some(target) = self.target_candidate() {
                    self.artifacts.sources.remove(&target);
                }
            }
            self.artifacts.lowered = Some(id);
            return (id, self.settle(actions, Settle::Quiet));
        };
        let mut actions = Vec::new();
        let approval = Approval {
            revision: policy.revision,
            approved_against: policy.approved_against,
            active_plan_hash: policy.composed_against,
        };
        let id = match composer.compose(&policy.artifact, &approval) {
            Ok(composed) => {
                let running = self
                    .active_source()
                    .is_some_and(|source| Arc::ptr_eq(&source.artifact, &policy));
                // A re-composition waits for a preparation in flight, as a default program does, so that a stream of
                // configuration changes always commits.
                let id = self.install(&policy, composed, Recomposition::Of { running }, &mut actions);
                match rejection_of(&actions, id) {
                    Some(rejection) => {
                        let message = format!(
                            "artifact revision {} was not installed again after the configuration changed, and stays \
                             as it was composed before: {}",
                            policy.revision, rejection
                        );
                        self.artifacts.policy_error = Some((Arc::clone(&policy), message));
                    }
                    // The artifact is composed against the current configuration again.
                    None => {
                        if self
                            .artifacts
                            .policy_error
                            .as_ref()
                            .is_some_and(|(failed, _)| Arc::ptr_eq(failed, &policy))
                        {
                            self.artifacts.policy_error = None;
                        }
                    }
                }
                id
            }
            Err(error) => {
                let id = self.candidate_id();
                let message = format!(
                    "artifact revision {} cannot be composed with the changed default program, and stays as it was \
                     composed before: {}",
                    policy.revision, error
                );
                actions.push(Action::Report(Report {
                    candidate: id,
                    origin: policy.origin(),
                    generation: None,
                    outcome: Outcome::Rejected(Rejection::Refused {
                        message: message.clone(),
                    }),
                }));
                self.artifacts.policy_error = Some((Arc::clone(&policy), message));
                id
            }
        };
        self.artifacts.lowered = Some(id);
        (id, self.settle(actions, Settle::Quiet))
    }

    /// Returns the latest acknowledgment of every config the channel delivered, in config ID order.
    pub fn config_states(&self) -> Vec<ConfigAcknowledgment> {
        self.artifacts
            .configs
            .values()
            .map(|tracked| tracked.ack.clone())
            .collect()
    }

    /// Returns the status summary the host reports with its acknowledgments.
    ///
    /// The core knows everything in it but the default program's base program hash, which the host computes from its
    /// export, and whether the host is degraded, which the host decides; the core adds why the artifact in effect, or
    /// the one running, could not be composed again, if it could not. The active artifact is the one the active
    /// generation serves, unless the generation failed; its stale flags are those of the composition that generation
    /// runs. A revoked artifact is active until a generation replaces its own.
    pub fn status_summary(&self, default_program_hash: Option<PlanHash>, degraded: Option<String>) -> StatusSummary {
        let active = self.active.as_ref();
        let source = self.active_source();
        let policy_error = self.artifacts.policy_error.as_ref().map(|(_, message)| message.clone());
        let degraded = match (degraded, policy_error) {
            (Some(host), Some(policy)) => Some(format!("{}; {}", host, policy)),
            (host, policy) => host.or(policy),
        };
        StatusSummary {
            default_program_hash,
            revision_high_water: self.high_water,
            active_generation: active.map(|active| active.generation.id().get()),
            active_plan_hash: active.map(|active| active.plan_hash),
            active_artifact: source.map(|source| ActiveArtifact {
                id: source.artifact.config.clone(),
                revision: source.artifact.revision,
                artifact_hash: source.artifact.hash,
            }),
            stale_base: source.is_some_and(|source| source.stale_base),
            stale_review: source.is_some_and(|source| source.stale_review),
            degraded,
            live_generations: u64::try_from(self.live.len()).unwrap_or(u64::MAX),
        }
    }

    /// Returns everything the host reports through the channel: the status summary and every config's acknowledgment.
    pub fn apply_states(&self, default_program_hash: Option<PlanHash>, degraded: Option<String>) -> ApplyStates {
        ApplyStates {
            status: self.status_summary(default_program_hash, degraded),
            config_states: self.config_states(),
        }
    }

    /// Handles the one config of a policy snapshot.
    fn receive(&mut self, config: &ArtifactConfig, composer: &dyn Composer, actions: &mut Vec<Action>) {
        if let Err(mismatch) = config.verify_transport_hash() {
            self.refuse(config, None, mismatch.to_string());
            return;
        }
        let envelope = match Envelope::parse(&config.payload) {
            Ok(envelope) => envelope,
            Err(error) => {
                self.refuse(config, None, error.to_string());
                return;
            }
        };
        let Envelope {
            revision,
            artifact_hash,
            approved_against,
            artifact,
        } = envelope;
        let declared = (revision, artifact_hash);
        let received = match receive_artifact(artifact) {
            Ok(received) => received,
            Err(error) => {
                let message = error.to_string();
                self.refuse_reported(config, declared, Rejection::Refused { message }, actions);
                return;
            }
        };
        if received.hash != artifact_hash {
            let message = format!(
                "artifact hash mismatch: the envelope declares {}, but the artifact as received hashes to {}; approval \
                 binds to the hash of the exact document",
                artifact_hash, received.hash
            );
            self.refuse_reported(config, declared, Rejection::Refused { message }, actions);
            return;
        }

        // A redelivery of the policy in effect, or the same artifact under a newer revision, changes nothing. A
        // revision below the mark, or equal to it when another delivery set it, is not a redelivery: it is stale.
        let redelivery = match self.high_water {
            Some(high_water) if revision < high_water => false,
            Some(high_water) if revision == high_water => {
                self.artifacts.high_water_by == Some((revision, artifact_hash))
            }
            _ => true,
        };
        if let Some(policy) = self
            .artifacts
            .policy
            .clone()
            .filter(|policy| policy.hash == artifact_hash && redelivery)
        {
            self.see_revision(revision, artifact_hash);
            let approved_against = approved_against.map(|approved| approved.active_plan_hash);
            let accepted = self.reapprove(&policy, config, revision, approved_against, received);
            self.follow(config, declared, accepted);
            return;
        }
        if let Some(high_water) = self.high_water.filter(|high_water| revision <= *high_water) {
            self.refuse_reported(config, declared, Rejection::Stale { revision, high_water }, actions);
            return;
        }
        self.see_revision(revision, artifact_hash);

        let composed_against = self.active.as_ref().map(|active| active.plan_hash);
        let approved_against = approved_against.map(|approved| approved.active_plan_hash);
        let approval = Approval {
            revision,
            approved_against,
            active_plan_hash: composed_against,
        };
        let composed = match composer.compose(&received, &approval) {
            Ok(composed) => composed,
            Err(error) => {
                let message = error.to_string();
                self.refuse_reported(config, declared, Rejection::Refused { message }, actions);
                return;
            }
        };
        let accepted = Arc::new(Accepted {
            config: config.id.clone(),
            revision,
            hash: artifact_hash,
            artifact: received,
            approved_against,
            composed_against,
            epoch: self.artifacts.revocations,
        });
        self.follow(config, declared, Arc::clone(&accepted));
        // The artifact is the policy in effect from now on, unless it is rejected: settling then makes the policy what
        // the schedule holds again. A re-composition failure of the artifact running stays reported until this one
        // replaces it.
        self.artifacts.policy = Some(Arc::clone(&accepted));
        self.install(&accepted, composed, Recomposition::No, actions);
    }

    /// Makes a delivery of the policy in effect, its own redelivery or an approval of the same document under a newer
    /// revision or in another config, the policy in effect, and returns it.
    ///
    /// Nothing is composed or installed again, since the document is the same: the candidates that serve the policy
    /// serve the delivery instead, as an attribution to a generation composed from the same document does, so the
    /// status summary reports its config and revision, and its review is checked against the active plan as a new
    /// acceptance's is. Every acknowledgment that followed the policy follows the delivery, and re-compositions after
    /// later configuration changes compose under its revision. A redelivery that changes none of this keeps the policy.
    fn reapprove(
        &mut self, policy: &Arc<Accepted>, config: &ArtifactConfig, revision: u64, approved_against: Option<PlanHash>,
        artifact: ReceivedArtifact,
    ) -> Arc<Accepted> {
        let composed_against = self.active.as_ref().map(|active| active.plan_hash);
        if policy.config == config.id
            && policy.revision == revision
            && policy.approved_against == approved_against
            && policy.composed_against == composed_against
        {
            return Arc::clone(policy);
        }
        let accepted = Arc::new(Accepted {
            config: config.id.clone(),
            revision,
            hash: policy.hash,
            artifact,
            approved_against,
            composed_against,
            epoch: self.artifacts.revocations,
        });
        let stale_review = approved_against.is_some_and(|approved| composed_against != Some(approved));
        for source in self.artifacts.sources.values_mut() {
            if Arc::ptr_eq(&source.artifact, policy) {
                source.artifact = Arc::clone(&accepted);
                source.stale_review = stale_review;
            }
        }
        let followers = self
            .artifacts
            .configs
            .values_mut()
            .flat_map(|tracked| [tracked.latest.as_mut(), tracked.earlier.as_mut()])
            .flatten()
            .filter(|followed| Arc::ptr_eq(&followed.artifact, policy));
        for followed in followers {
            followed.artifact = Arc::clone(&accepted);
        }
        if let Some((failed, _)) = &mut self.artifacts.policy_error {
            if Arc::ptr_eq(failed, policy) {
                *failed = Arc::clone(&accepted);
            }
        }
        self.artifacts.policy = Some(Arc::clone(&accepted));
        accepted
    }

    /// Records that an authentic delivery saw a revision: the high-water mark moves to it if it is higher.
    fn see_revision(&mut self, revision: u64, artifact_hash: PlanHash) {
        if self.high_water.is_none_or(|high_water| revision > high_water) {
            self.high_water = Some(revision);
            self.artifacts.high_water_by = Some((revision, artifact_hash));
        }
    }

    /// Proposes a composed artifact, returning its candidate's identifier.
    ///
    /// A new acceptance cancels a preparation in flight; a re-composition waits for it.
    fn install(
        &mut self, artifact: &Arc<Accepted>, composed: Composed, recomposition: Recomposition,
        actions: &mut Vec<Action>,
    ) -> CandidateId {
        let Composed {
            plan,
            report,
            stale_base,
            stale_review,
            ..
        } = composed;
        let id = self.candidate_id();
        let (arrival, retried) = match recomposition {
            Recomposition::No => (Arrival::Cancel, false),
            Recomposition::Of { running } => (Arrival::Queue, running),
        };
        let source = Source {
            artifact: Arc::clone(artifact),
            stale_base,
            stale_review,
            retried,
            attributed: false,
        };
        self.artifacts.sources.insert(id, source.clone());
        let candidate = Candidate {
            plan: Arc::new(plan),
            report,
            origin: artifact.origin(),
        };
        if self.submit(id, candidate, arrival, actions) {
            // The candidate equals what the schedule will hold: the generation that holds it serves the artifact.
            if let Some(target) = self.target_candidate() {
                match self.artifacts.sources.get_mut(&target) {
                    // The same document: the generation that holds its composition serves its latest acceptance, such
                    // as an approval under a newer revision, or a delivery after a revocation.
                    Some(existing) if existing.artifact.hash == artifact.hash => {
                        existing.artifact = Arc::clone(artifact);
                        existing.stale_base = stale_base;
                        existing.stale_review = stale_review;
                    }
                    _ => {
                        let attributed = Source {
                            retried: false,
                            attributed: true,
                            ..source
                        };
                        self.artifacts.sources.insert(target, attributed);
                    }
                }
            }
        }
        id
    }

    /// Revokes every artifact accepted so far: the default program runs.
    fn revoke(&mut self, actions: &mut Vec<Action>) {
        let revoked = self.artifacts.policy.take();
        self.artifacts.revocations += 1;
        self.artifacts.sources.retain(|_, source| !source.attributed);
        let dropped = self.drop_in_flight(|_, origin| origin.can_roll_back(), REVOKED, actions);
        // Nothing a revoked artifact installed is kept to roll back to: a failure before the default program commits
        // waits for it instead. A kept default program stays kept.
        if self
            .last_known_good
            .as_ref()
            .is_some_and(|kept| self.artifacts.sources.contains_key(&kept.candidate))
        {
            if let Some(kept) = self.last_known_good.take() {
                let id = kept.generation.id();
                if self.state_of(id) == Some(GenerationState::Prepared) {
                    self.transition(id, LifecycleEvent::Released, None);
                    self.report_generation(id, actions);
                }
            }
        }
        self.try_start(actions);
        let artifact_running = self.active.as_ref().is_some_and(|active| active.origin.can_roll_back());
        let default_in_flight = self
            .pending
            .as_ref()
            .map(|queued| queued.origin)
            .into_iter()
            .chain(self.preparing.as_ref().map(|preparing| preparing.installed.origin))
            .any(|origin| !origin.can_roll_back());
        if (revoked.is_some() || dropped || artifact_running) && !default_in_flight {
            actions.push(Action::InstallDefaultProgram);
        }
    }

    /// Rejects every config of a policy snapshot that holds more than one.
    fn reject_all(&mut self, configs: &[ArtifactConfig]) {
        // The message names a few configs, since every config's acknowledgment carries it.
        let mut ids = configs
            .iter()
            .take(LISTED_CONFIGS)
            .map(|config| format!("`{}`", config.id))
            .collect::<Vec<_>>();
        if configs.len() > LISTED_CONFIGS {
            ids.push(format!("and {} more", configs.len() - LISTED_CONFIGS));
        }
        let message = format!(
            "the policy snapshot holds {} configs ({}), but one policy replaces the policy region: every config is \
             rejected until the snapshot holds one",
            configs.len(),
            ids.join(", ")
        );
        for config in configs {
            self.refuse(config, None, message.clone());
        }
    }

    /// Takes the artifact of a failing active generation out of effect, before the failure is handled: the artifact's
    /// compositions in flight, under any revision that approved the same document, are dropped rather than prepared
    /// again against the generation rolled back to, and no other generation serves it any more. A delivery after the
    /// failure is accepted anew.
    pub(super) fn withdraw_failing(&mut self, generation: GenerationId, reason: &str, actions: &mut Vec<Action>) {
        let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.generation.id() == generation && active.origin.can_roll_back())
        else {
            return;
        };
        if self.has_failed(generation) {
            return;
        }
        let failing = active.candidate;
        let Some(artifact) = self
            .artifacts
            .sources
            .get(&failing)
            .map(|source| Arc::clone(&source.artifact))
        else {
            return;
        };
        // The failure is the document's, whichever revisions approved it: candidates of the same artifact accepted
        // under another revision are dropped too. The failing generation keeps its own source, even an attribution, so
        // that its failure is acknowledged.
        self.artifacts.sources.retain(|candidate, source| {
            *candidate == failing || !(source.attributed && source.artifact.hash == artifact.hash)
        });
        let message = format!(
            "artifact revision {} failed under supervision: {}",
            artifact.revision, reason
        );
        let serving = self
            .artifacts
            .sources
            .iter()
            .filter(|(_, source)| source.artifact.hash == artifact.hash)
            .map(|(candidate, _)| *candidate)
            .collect::<BTreeSet<_>>();
        self.drop_in_flight(|candidate, _| serving.contains(&candidate), &message, actions);
    }

    /// Drops the preparation and the waiting candidate for which `matches` holds, rejecting them with `message`, and
    /// returns `true` if it dropped either.
    fn drop_in_flight<F>(&mut self, matches: F, message: &str, actions: &mut Vec<Action>) -> bool
    where
        F: Fn(CandidateId, &Origin) -> bool,
    {
        let rejection = || {
            Outcome::Rejected(Rejection::Refused {
                message: message.to_string(),
            })
        };
        let mut dropped = false;
        if self
            .preparing
            .as_ref()
            .is_some_and(|preparing| matches(preparing.installed.candidate, &preparing.installed.origin))
        {
            if let Some(preparing) = self.preparing.take() {
                let installed = preparing.installed;
                let id = installed.generation.id();
                actions.push(Action::Abort {
                    generation: id,
                    slots: self.frame.slots().iter().map(|slot| slot.id).collect(),
                });
                self.transition(id, LifecycleEvent::Rejected, Some(message.to_string()));
                actions.push(Action::Report(Report {
                    candidate: installed.candidate,
                    origin: installed.origin,
                    generation: Some(id),
                    outcome: rejection(),
                }));
                dropped = true;
            }
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|queued| matches(queued.candidate, &queued.origin))
        {
            if let Some(queued) = self.pending.take() {
                actions.push(Self::report(&queued, rejection()));
                dropped = true;
            }
        }
        dropped
    }

    /// Returns `true` if two candidates serve the same artifact document, whichever revisions approved it.
    pub(super) fn serve_same_artifact(&self, first: CandidateId, second: CandidateId) -> bool {
        match (self.artifacts.sources.get(&first), self.artifacts.sources.get(&second)) {
            (Some(first), Some(second)) => first.artifact.hash == second.artifact.hash,
            _ => false,
        }
    }

    /// Returns `true` if a candidate serves an artifact that a revocation revoked.
    pub(super) fn serves_revoked(&self, candidate: CandidateId) -> bool {
        self.artifacts
            .sources
            .get(&candidate)
            .is_some_and(|source| self.is_revoked(&source.artifact))
    }

    /// Returns `true` if a revocation came after the artifact was accepted.
    fn is_revoked(&self, artifact: &Accepted) -> bool {
        artifact.epoch < self.artifacts.revocations
    }

    /// Returns `true` if a candidate's failed preparation is tried again although it is an artifact's: it composes
    /// again an artifact that was running when the configuration changed.
    pub(super) fn is_retried(&self, candidate: CandidateId) -> bool {
        self.artifacts
            .sources
            .get(&candidate)
            .is_some_and(|source| source.retried)
    }

    /// Settles the artifact state after an event whose actions are `actions`, and returns the actions with any the
    /// settling adds.
    ///
    /// Every public event of the core ends here: it records how the installations reported on ended, forgets the
    /// sources of candidates the core no longer holds, makes the policy in effect what the schedule will hold if the
    /// policy's installation ended, forgets a re-composition failure whose artifact neither is in effect nor runs, asks
    /// for the default program if an event leaves the active generation behind, and refreshes every acknowledgment.
    pub(super) fn settle(&mut self, mut actions: Vec<Action>, event: Settle) -> Vec<Action> {
        for action in &actions {
            if let Action::Report(report) = action {
                self.record_end(report);
            }
        }
        self.prune_sources();

        let mut reverted = false;
        if let Some(policy) = &self.artifacts.policy {
            let serving = self.serving();
            if !serving.as_ref().is_some_and(|serving| Arc::ptr_eq(serving, policy)) {
                self.artifacts.policy = serving;
                reverted = true;
            }
        }
        if let Some((failed, _)) = &self.artifacts.policy_error {
            let in_effect = self
                .artifacts
                .policy
                .as_ref()
                .is_some_and(|policy| Arc::ptr_eq(policy, failed));
            let runs = self
                .active_source()
                .is_some_and(|source| Arc::ptr_eq(&source.artifact, failed));
            if !in_effect && !runs {
                self.artifacts.policy_error = None;
            }
        }
        if let Settle::AfterFailure { rolled_back } = event {
            self.ask_for_default_program(reverted || rolled_back, &mut actions);
        }

        self.refresh_acknowledgments();
        self.prune_configs();
        actions
    }

    /// Records how the installation of the artifact a report concerns ended, if the report ends it.
    fn record_end(&mut self, report: &Report) {
        let Some(source) = self.artifacts.sources.get(&report.candidate) else {
            return;
        };
        let ended = match &report.outcome {
            Outcome::Rejected(rejection) => Ended {
                state: ApplyState::Rejected,
                error: Some(rejection.to_string()),
                generation: report.generation,
            },
            Outcome::Superseded => Ended {
                state: ApplyState::Rejected,
                error: Some(SUPERSEDED.to_string()),
                generation: report.generation,
            },
            Outcome::Generation(GenerationState::Failed) => Ended {
                state: ApplyState::Failed,
                error: report
                    .generation
                    .and_then(|generation| self.records.get(&generation))
                    .and_then(|record| record.reason.clone()),
                generation: report.generation,
            },
            Outcome::Generation(GenerationState::Retired) => Ended {
                state: ApplyState::Retired,
                error: None,
                generation: report.generation,
            },
            _ => return,
        };
        let artifact = Arc::clone(&source.artifact);
        let followers = self
            .artifacts
            .configs
            .values_mut()
            .flat_map(|tracked| [tracked.latest.as_mut(), tracked.earlier.as_mut()])
            .flatten()
            .filter(|followed| Arc::ptr_eq(&followed.artifact, &artifact));
        for followed in followers {
            // A preparation that supervision failed is also reported rejected: the failure is what the
            // acknowledgment reports.
            let failed = followed
                .ended
                .as_ref()
                .is_some_and(|ended| ended.state == ApplyState::Failed);
            if !(failed && ended.state == ApplyState::Rejected) {
                followed.ended = Some(ended.clone());
            }
        }
    }

    /// Forgets the sources of candidates the core no longer holds and whose generations are no longer alive.
    fn prune_sources(&mut self) {
        let mut held = self
            .records
            .values()
            .filter(|record| self.live.contains(&record.id))
            .map(|record| record.candidate)
            .collect::<BTreeSet<_>>();
        held.extend(self.active.as_ref().map(|active| active.candidate));
        held.extend(self.last_known_good.as_ref().map(|kept| kept.candidate));
        held.extend(self.preparing.as_ref().map(|preparing| preparing.installed.candidate));
        held.extend(self.pending.as_ref().map(|queued| queued.candidate));
        self.artifacts.sources.retain(|candidate, _| held.contains(candidate));
    }

    /// Asks for the default program if the event changed something, nothing in flight will replace the active
    /// generation, and the active generation is an artifact's that failed, or, when `revisited` (the event rolled back,
    /// or the policy's installation ended without running), it predates the latest configuration change.
    ///
    /// A failed default-program generation is not replaced: its component failed, as a native component does. An event
    /// that changed nothing, such as a failure reported twice, asks for nothing.
    fn ask_for_default_program(&self, revisited: bool, actions: &mut Vec<Action>) {
        let asked = actions
            .iter()
            .any(|action| matches!(action, Action::InstallDefaultProgram));
        if actions.is_empty() || asked || self.pending.is_some() || self.preparing.is_some() {
            return;
        }
        let Some(active) = &self.active else { return };
        let failed = self.has_failed(active.generation.id()) && active.origin.can_roll_back();
        let behind = revisited && self.artifacts.lowered.is_some_and(|lowered| lowered > active.candidate);
        if failed || behind {
            actions.push(Action::InstallDefaultProgram);
        }
    }

    /// Recomputes the acknowledgment of every config that follows an artifact's installation.
    ///
    /// The acknowledgment reported is the latest change: the latest delivery's installation is computed first, then the
    /// earlier one's, which is no longer followed once the latest delivery's artifact is active, or once it ended.
    fn refresh_acknowledgments(&mut self) {
        let computed = self
            .artifacts
            .configs
            .iter()
            .map(|(id, tracked)| {
                let latest = tracked.latest.as_ref().map(|followed| self.followed_ack(followed));
                let earlier = tracked.earlier.as_ref().map(|followed| self.followed_ack(followed));
                (id.clone(), latest, earlier)
            })
            .collect::<Vec<_>>();
        for (id, latest, earlier) in computed {
            let Some(tracked) = self.artifacts.configs.get_mut(&id) else {
                continue;
            };
            let mut shown = None;
            if let (Some(followed), Some((ack, serving))) = (tracked.latest.as_mut(), latest) {
                shown = followed.update(ack, serving);
                if followed.ack.state == ApplyState::Active {
                    tracked.earlier = None;
                }
            }
            if let (Some(followed), Some((ack, serving))) = (tracked.earlier.as_mut(), earlier) {
                shown = followed.update(ack, serving).or(shown);
                if is_final(followed.ack.state) {
                    tracked.earlier = None;
                }
            }
            if let Some(ack) = shown.filter(|ack| *ack != tracked.ack) {
                self.artifacts.changes += 1;
                tracked.ack = ack;
                tracked.changed = self.artifacts.changes;
            }
        }
    }

    /// Returns the acknowledgment of a followed installation, and whether a candidate the core holds serves its
    /// artifact.
    ///
    /// Once the installation ends, the acknowledgment keeps naming the last generation it had. When nothing serves the
    /// artifact any more and nothing ended its installation, the generation that served it was superseded and drains,
    /// was released from last-known-good, or was taken over by the default program after a revocation: the
    /// acknowledgment stays as it was while that generation is alive, and reads `retired` once it is not.
    fn followed_ack(&self, followed: &Followed) -> (ConfigAcknowledgment, bool) {
        if let Some((state, generation)) = self.role(&followed.artifact) {
            let ack = ConfigAcknowledgment {
                state,
                generation: generation.map(GenerationId::get),
                error: None,
                ..followed.ack.clone()
            };
            return (ack, true);
        }
        let ack = match &followed.ended {
            Some(ended) => ConfigAcknowledgment {
                state: ended.state,
                generation: ended.generation.map(GenerationId::get).or(followed.ack.generation),
                error: ended.error.clone(),
                ..followed.ack.clone()
            },
            None if followed
                .ack
                .generation
                .is_some_and(|generation| self.live.contains(&GenerationId::new(generation))) =>
            {
                followed.ack.clone()
            }
            None => ConfigAcknowledgment {
                state: ApplyState::Retired,
                error: None,
                ..followed.ack.clone()
            },
        };
        (ack, false)
    }

    /// Forgets the oldest configs that are absent from the latest snapshot and whose acknowledgment is final, beyond
    /// the history kept.
    fn prune_configs(&mut self) {
        let delivered = self
            .artifacts
            .last_snapshot
            .iter()
            .flat_map(|snapshot| snapshot.configs.iter().map(|config| config.id.clone()))
            .collect::<BTreeSet<_>>();
        let mut forgettable = self
            .artifacts
            .configs
            .iter()
            .filter(|(id, tracked)| !delivered.contains(*id) && tracked.is_final())
            .map(|(id, tracked)| (tracked.changed, id.clone()))
            .collect::<Vec<_>>();
        if forgettable.len() > CONFIG_HISTORY {
            forgettable.sort_unstable();
            for (_, id) in &forgettable[..forgettable.len() - CONFIG_HISTORY] {
                self.artifacts.configs.remove(id);
            }
        }
    }

    /// Returns what an artifact's installation is doing, and the generation it concerns: `active` while a generation
    /// serving it is active, unless it failed; `pending` while one waits or is being prepared; and `prepared` while one
    /// is kept as last-known-good. Returns `None` once no candidate the core holds serves it.
    fn role(&self, artifact: &Arc<Accepted>) -> Option<(ApplyState, Option<GenerationId>)> {
        let serves = |candidate: CandidateId| {
            self.artifacts
                .sources
                .get(&candidate)
                .is_some_and(|source| Arc::ptr_eq(&source.artifact, artifact))
        };
        if let Some(active) = &self.active {
            let id = active.generation.id();
            if !self.has_failed(id) && serves(active.candidate) {
                return Some((ApplyState::Active, Some(id)));
            }
        }
        if let Some(preparing) = &self.preparing {
            if serves(preparing.installed.candidate) {
                return Some((ApplyState::Pending, Some(preparing.installed.generation.id())));
            }
        }
        if self.pending.as_ref().is_some_and(|queued| serves(queued.candidate)) {
            return Some((ApplyState::Pending, None));
        }
        // A superseded or released generation still finishes the batches stamped with it: the protocol has no draining
        // state, so the acknowledgment stays as it was until the generation retires (see `followed_ack`).
        self.last_known_good
            .as_ref()
            .filter(|kept| serves(kept.candidate))
            .map(|kept| (ApplyState::Prepared, Some(kept.generation.id())))
    }

    /// Returns the candidate whose plan the schedule will hold once what is in flight ends: the waiting one, else the
    /// one being prepared, else the active one, unless it failed.
    fn target_candidate(&self) -> Option<CandidateId> {
        self.pending
            .as_ref()
            .map(|queued| queued.candidate)
            .or_else(|| self.preparing.as_ref().map(|preparing| preparing.installed.candidate))
            .or_else(|| {
                self.active
                    .as_ref()
                    .filter(|active| !self.has_failed(active.generation.id()))
                    .map(|active| active.candidate)
            })
    }

    /// Returns the artifact the schedule will hold, if it holds one that is not revoked: a revoked artifact's
    /// generation runs only until it is replaced, and the default program is in effect meanwhile.
    fn serving(&self) -> Option<Arc<Accepted>> {
        self.target_candidate()
            .and_then(|candidate| self.artifacts.sources.get(&candidate))
            .map(|source| Arc::clone(&source.artifact))
            .filter(|artifact| !self.is_revoked(artifact))
    }

    /// Returns the source of the active generation, unless it failed.
    fn active_source(&self) -> Option<&Source> {
        self.active
            .as_ref()
            .filter(|active| !self.has_failed(active.generation.id()))
            .and_then(|active| self.artifacts.sources.get(&active.candidate))
    }

    /// Makes a config's acknowledgment follow an artifact's installation.
    fn follow(&mut self, config: &ArtifactConfig, (revision, hash): (u64, PlanHash), artifact: Arc<Accepted>) {
        self.track(
            config,
            Some((revision, hash)),
            ApplyState::Pending,
            None,
            Some(artifact),
        );
    }

    /// Acknowledges a config refused before its artifact was accepted.
    fn refuse(&mut self, config: &ArtifactConfig, declared: Option<(u64, PlanHash)>, message: String) {
        self.track(config, declared, ApplyState::Rejected, Some(message), None);
    }

    /// Acknowledges a config refused after its envelope was read, and reports it like any rejected candidate.
    fn refuse_reported(
        &mut self, config: &ArtifactConfig, declared: (u64, PlanHash), rejection: Rejection, actions: &mut Vec<Action>,
    ) {
        self.refuse(config, Some(declared), rejection.to_string());
        let (revision, hash) = declared;
        actions.push(Action::Report(Report {
            candidate: self.candidate_id(),
            origin: Origin::Artifact { revision, hash },
            generation: None,
            outcome: Outcome::Rejected(rejection),
        }));
    }

    /// Replaces a config's acknowledgment with its latest delivery's, which follows the installation of `follows`, if
    /// given.
    ///
    /// The installation an earlier delivery of the config started stays followed while it continues: when the delivery
    /// is refused, an installation that runs or is in flight continues; when it is accepted, it replaces an
    /// installation in flight, so only one that runs (active, or kept to roll back to) continues.
    fn track(
        &mut self, config: &ArtifactConfig, declared: Option<(u64, PlanHash)>, state: ApplyState,
        error: Option<String>, follows: Option<Arc<Accepted>>,
    ) {
        let ack = ConfigAcknowledgment {
            product: ArtifactProduct::Policy,
            id: config.id.clone(),
            version: config.version,
            revision: declared.map(|(revision, _)| revision),
            artifact_hash: declared.map(|(_, hash)| hash),
            state,
            generation: None,
            cut_at: None,
            error,
        };
        let earlier = self.artifacts.configs.remove(&config.id).and_then(|previous| {
            [previous.latest, previous.earlier]
                .into_iter()
                .flatten()
                .find(|followed| {
                    let continues = match followed.ack.state {
                        ApplyState::Active | ApplyState::Prepared => true,
                        ApplyState::Pending => follows.is_none(),
                        ApplyState::Retired | ApplyState::Rejected | ApplyState::Failed => false,
                    };
                    let same = follows
                        .as_ref()
                        .is_some_and(|artifact| Arc::ptr_eq(artifact, &followed.artifact));
                    continues && !same
                })
        });
        let latest = follows.map(|artifact| Followed {
            artifact,
            ack: ack.clone(),
            ended: None,
        });
        self.artifacts.changes += 1;
        self.artifacts.configs.insert(
            config.id.clone(),
            Tracked {
                ack,
                latest,
                earlier,
                changed: self.artifacts.changes,
            },
        );
    }
}

impl Followed {
    /// Records the installation's acknowledgment as just computed, and whether a candidate serves its artifact, and
    /// returns the acknowledgment if it changed.
    fn update(&mut self, ack: ConfigAcknowledgment, serving: bool) -> Option<ConfigAcknowledgment> {
        if serving {
            // The installation continues: a later end is recorded anew.
            self.ended = None;
        }
        (ack != self.ack).then(|| {
            self.ack = ack.clone();
            ack
        })
    }
}

#[cfg(test)]
impl InstallerCore {
    /// Returns the policy in effect: the artifact the core runs or installs instead of the default program, if any.
    ///
    /// While one is in effect, [`on_default_program`][Self::on_default_program] composes it again rather than proposing
    /// the default program. The status summary reports the artifact the active generation serves instead.
    pub(in crate::install) fn policy_in_effect(&self) -> Option<ActiveArtifact> {
        self.artifacts.policy.as_ref().map(|policy| ActiveArtifact {
            id: policy.config.clone(),
            revision: policy.revision,
            artifact_hash: policy.hash,
        })
    }

    /// Returns the revision and hash of the artifact a generation the core remembers serves, if it serves one.
    pub(in crate::install) fn served_by(&self, generation: GenerationId) -> Option<(u64, PlanHash)> {
        let candidate = self.records.get(&generation)?.candidate;
        self.artifacts
            .sources
            .get(&candidate)
            .map(|source| (source.artifact.revision, source.artifact.hash))
    }
}

/// Returns `true` if an acknowledgment state is final: nothing changes it but a new delivery.
const fn is_final(state: ApplyState) -> bool {
    matches!(state, ApplyState::Retired | ApplyState::Rejected | ApplyState::Failed)
}

/// Returns the rejection the actions report for a candidate, if any.
fn rejection_of(actions: &[Action], candidate: CandidateId) -> Option<&Rejection> {
    actions.iter().find_map(|action| match action {
        Action::Report(Report {
            candidate: reported,
            outcome: Outcome::Rejected(rejection),
            ..
        }) if *reported == candidate => Some(rejection),
        _ => None,
    })
}
