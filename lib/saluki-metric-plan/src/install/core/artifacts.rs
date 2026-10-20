//! The installer core's artifact paths: snapshots from the channel, acknowledgments, and the status summary.
//!
//! # The program in effect
//!
//! An artifact is a whole program, which runs instead of the default program, and the channel delivers at most one
//! config at a time. The core keeps the artifact it installs, the **program in effect**: the latest artifact it
//! accepted, until a newer one replaces it, the channel revokes it, it is rejected, or it fails under supervision. Each
//! candidate composed from an artifact remembers the artifact it serves. When an installation of the program in effect
//! ends without it running (it is rejected, or it fails), the program in effect becomes the artifact of what the
//! schedule will hold again: that of the generation rolled back to, an older artifact still running, or none, for the
//! default program.
//!
//! A revocation revokes every artifact accepted before it: none becomes the program in effect again, is composed again,
//! or is kept to roll back to, unless the channel delivers it anew and the core accepts it again. A revoked artifact's
//! generation keeps running until the default program, or a newer composition, replaces it.
//!
//! A configuration change while a program is in effect composes it again against the new default program
//! ([`InstallerCore::on_default_program`]), under its own revision; like a default program, the re-composition waits for
//! a preparation in flight instead of cancelling it. An artifact that composes to the plan already running or being
//! prepared, such as an unmodified export of the default program, creates no generation: the generation that holds its
//! plan serves it.
//!
//! # Artifacts that wait for a recipient
//!
//! A new artifact that emits to a recipient the host does not declare yet, such as a program published before the
//! configuration that declares its recipient, or to recipients whose output the host has not lowered yet, is neither
//! rejected nor installed: it **waits** for the recipient. It
//! holds nothing and counts against no overlap budget; its revision counts as seen; its config is acknowledged `pending`
//! with what it waits for; and whatever runs keeps running. Every lowering composes it again, and once it composes it
//! proceeds as a new installation. At most one artifact waits, the newest: a newer artifact replaces it, whether that
//! one installs, waits, or is rejected; a revocation removes it, and so does a snapshot that holds more than one config.
//! A delivery of it again by its config under its own revision keeps it waiting, while a stale or unreadable delivery
//! changes nothing for it, as for an installation in flight. A re-composition of the artifact in effect after an output
//! it emits to was removed is the composer's to make, with that output's emits dropped.
//!
//! # Acknowledgments
//!
//! Every config the channel delivered has an acknowledgment, kept per config ID. A config refused on receipt is
//! `rejected` with the reason. A config whose artifact was accepted, or whose artifact equals the program in effect,
//! follows the installation of that artifact: `pending` while it waits or is prepared, `prepared` while a cut
//! installation of it waits for its cut, or while it is kept to roll back to, `active` while a generation serving it is
//! active, and then how its installation ended: `rejected`, `failed`, or `retired`. A generation that stops serving it
//! without ending it (superseded and draining, released from last-known-good, or taken over by the default program
//! after a revocation) leaves the acknowledgment as it was until the generation retires.
//!
//! A refused or rejected delivery does not hide what becomes of the artifact an earlier delivery of the same config
//! installed: while that artifact runs, and the latest delivery's artifact does not, a change of its installation (a
//! failure, a rollback to it, its retirement) is acknowledged under the earlier delivery's version and revision.
//!
//! A revoked artifact stays `active` while its generation is, until the default program or a newer composition
//! replaces it. When a cut in flight delays that replacement, which can take a cut's whole wait, the acknowledgment
//! says so with the error `revoked; active until replaced`, and keeps it while the artifact stays active; when the
//! default program was rejected, the error says why. An artifact whose bucket widths the default program's do not line
//! up with is rejected, so a rejection of the default program after a revocation takes a default program whose widths
//! changed since.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use saluki_core::execution::GenerationId;

use super::{
    cut::{AfterCancel, CutPhase},
    Action, Arrival, Candidate, CandidateId, InstallerCore, Origin, Outcome, Rejection, Report,
};
use crate::{
    canonical::PlanHash,
    channel::{
        ActiveArtifact, ApplyState, ApplyStates, ArtifactConfig, ArtifactConfigId, ArtifactProduct, ArtifactSnapshot,
        ConfigAcknowledgment, Envelope, NativePolicy, ProgramRuns, ProgramStatus, StatusSummary,
    },
    compose::{receive_artifact, Approval, ComposeError, Composed, Composer},
    elaborate::BindingUses,
    install::lifecycle::{GenerationState, LifecycleEvent},
    ir::{Plan, PolicyArtifact, ReceivedArtifact},
};

/// The most configs absent from the latest snapshot whose final acknowledgment the core keeps.
const CONFIG_HISTORY: usize = 16;

/// The most config IDs the rejection of a snapshot that holds more than one config names, so that its acknowledgments
/// stay linear in the number of configs.
const LISTED_CONFIGS: usize = 10;

/// Why a candidate's installation ended when a newer candidate replaced it before it was prepared.
const SUPERSEDED: &str = "a newer candidate replaced it before it was prepared";

/// Nanoseconds per millisecond: acknowledgments carry cut times in milliseconds.
const NANOS_PER_MILLI: u64 = 1_000_000;

/// Why an artifact in flight was dropped when the channel revoked the policy.
pub(super) const REVOKED: &str = "the channel revoked the policy before the artifact was installed";

/// The error of a revoked artifact's acknowledgment while it stays active because its replacement waits for a cut.
const REVOKED_ACTIVE: &str = "revoked; active until replaced";

/// What an event leaves for the core to settle, once its own actions are known.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Settle {
    /// An event after which the plan the schedule will hold is the one the event chose: the core asks the host for
    /// nothing.
    Quiet,

    /// A preparation failed or timed out, or supervision failed a generation: when nothing in flight will replace the
    /// active generation, and it failed, or it predates the latest configuration change after a rollback or a rejected
    /// program, the core asks for the default program.
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
    /// Returns the approval it composes with: as the artifact in effect, whose emits to an output removed since are
    /// dropped, or as a new one, which such an output keeps from installing.
    fn approval(&self, in_effect: bool) -> Approval {
        Approval {
            revision: self.revision,
            approved_against: self.approved_against,
            active_plan_hash: self.composed_against,
            in_effect,
        }
    }

    /// Composes the artifact through the host's composer: as the artifact in effect if `in_effect`, else as a new one.
    fn compose(&self, composer: &dyn Composer, in_effect: bool) -> Result<Composed, ComposeError> {
        composer.compose(&self.artifact, &self.approval(in_effect))
    }

    /// Returns the origin of a candidate composed from it.
    fn origin(&self) -> Origin {
        Origin::Artifact {
            revision: self.revision,
            hash: self.hash,
        }
    }

    fn active(&self) -> ActiveArtifact {
        ActiveArtifact {
            id: self.config.clone(),
            revision: self.revision,
            artifact_hash: self.hash,
        }
    }
}

/// Returns `true` if two programs, each in effect or served, are the same accepted artifact, or both the default
/// program.
fn same_program(first: Option<&Arc<Accepted>>, second: Option<&Arc<Accepted>>) -> bool {
    match (first, second) {
        (Some(first), Some(second)) => Arc::ptr_eq(first, second),
        (None, None) => true,
        _ => false,
    }
}

/// The artifact a candidate serves.
#[derive(Clone, Debug)]
struct Source {
    /// The artifact the candidate's plan runs.
    artifact: Arc<Accepted>,

    /// Whether its composition had a stale review.
    stale_review: bool,

    /// Whether the candidate composes again an artifact that was running when the configuration changed, or is a
    /// rollback. Its preparation is tried again after a failure, as a default-program candidate's is, so that
    /// configuration changes are applied eventually.
    retried: bool,

    /// Whether the candidate was created for another plan that the artifact composed to: it serves the artifact
    /// without having been composed from it.
    attributed: bool,

    /// What the composition says beside its state ([`Composed::notes`]): why the artifact runs degraded. Its
    /// acknowledgment carries it while the candidate is active.
    notes: Option<String>,
}

impl Source {
    /// Returns `true` if the candidate serves the artifact.
    fn serves(&self, artifact: &Arc<Accepted>) -> bool {
        Arc::ptr_eq(&self.artifact, artifact)
    }
}

/// Whether a composed program is a re-composition of the program in effect after a configuration change.
#[derive(Clone, Copy, Debug)]
enum Recomposition {
    /// A new acceptance, or a change of the program in effect by the channel.
    No,

    /// A re-composition.
    Of {
        /// Whether the program was running when the configuration changed: its preparation is then tried again after a
        /// failure.
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

/// The installation of an artifact a config's delivery started: the artifact it was accepted as, or the program in
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

/// A config whose envelope and artifact were read and checked: what its delivery says.
struct Delivery<'a> {
    config: &'a ArtifactConfig,
    revision: u64,
    hash: PlanHash,
    approved_against: Option<PlanHash>,
    received: ReceivedArtifact,
}

impl Delivery<'_> {
    fn declared(&self) -> (u64, PlanHash) {
        (self.revision, self.hash)
    }
}

/// A config's identifier and version, which its acknowledgment carries.
#[derive(Clone, Debug)]
struct ConfigRef {
    id: ArtifactConfigId,
    version: u64,
}

impl From<&ArtifactConfig> for ConfigRef {
    fn from(config: &ArtifactConfig) -> Self {
        Self {
            id: config.id.clone(),
            version: config.version,
        }
    }
}

impl From<&ConfigRef> for ConfigRef {
    fn from(config: &ConfigRef) -> Self {
        config.clone()
    }
}

/// A delivery accepted as a new artifact, not composed or installed yet.
#[derive(Clone, Debug)]
struct Acceptance {
    config: ConfigRef,
    declared: (u64, PlanHash),
    artifact: Arc<Accepted>,
}

/// An artifact that waits for a recipient the host does not declare yet: it holds nothing, counts against no overlap
/// budget, and is composed again at every lowering, until it composes, a newer artifact replaces it, or a revocation
/// removes it.
#[derive(Clone, Debug)]
struct Waiting {
    acceptance: Acceptance,

    /// Why it waits.
    reason: String,
}

/// Returns why a new artifact waits, if its composition's error says that something it delivers to is not there yet: a
/// recipient the host's directory does not name, or an output for the recipients an `emit` names, which the host adds
/// once it lowers with the artifact's demand. The artifact then waits rather than being rejected.
fn waiting_reason(error: &ComposeError) -> Option<String> {
    match error {
        ComposeError::UnknownRecipient { recipient, .. } => Some(format!(
            "waiting for recipient `{}`: the artifact emits to it, and the host's recipient directory does not name it; \
             the artifact is installed once the recipient is declared",
            recipient
        )),
        ComposeError::PendingRecipients { .. } => Some(format!("waiting: {}", error)),
        _ => None,
    }
}

/// What the core keeps about the artifacts the channel delivered.
#[derive(Debug, Default)]
pub(super) struct ArtifactState {
    /// Every config seen, by config ID.
    configs: BTreeMap<ArtifactConfigId, Tracked>,

    /// The artifact each candidate the core holds serves, for candidates composed from an artifact or attributed to
    /// one: pending, preparing, cut, active, kept as last-known-good, rolled back to, or with a generation still alive.
    sources: BTreeMap<CandidateId, Source>,

    /// The program in effect, or `None` for the default program.
    policy: Option<Arc<Accepted>>,

    /// Why the program in effect could not be composed again after the latest configuration change, if it could not,
    /// with the artifact: the host reports it degraded while the artifact is in effect or the active generation serves
    /// it, until a re-composition of it succeeds.
    policy_error: Option<(Arc<Accepted>, String)>,

    /// The number of revocations so far: every artifact accepted before the latest one is revoked.
    revocations: u64,

    /// The latest snapshot handled. A redelivery of it, such as after a worker restart, changes nothing, and a config
    /// it holds unchanged in the next snapshot keeps its acknowledgment.
    last_snapshot: Option<ArtifactSnapshot>,

    /// The candidate created for the latest default program handed to the core, to tell whether a generation predates
    /// the latest configuration change.
    lowered: Option<CandidateId>,

    /// Why the latest default-program candidate was rejected, until a later one is prepared or changes nothing: a
    /// revoked artifact that stays active meanwhile says so.
    default_rejected: Option<String>,

    /// The artifact that waits for a recipient, the newest, if any.
    waiting: Option<Waiting>,

    /// The number of acknowledgment changes so far.
    changes: u64,
}

impl InstallerCore {
    /// Handles a complete snapshot of the policy product from the artifact channel, returning what to do.
    ///
    /// The snapshot replaces the previous one: it revokes, rejects, acknowledges the current state, or proposes the one
    /// artifact it holds, composed through `composer`. Every config it holds gets an acknowledgment, which
    /// [`config_states`][Self::config_states] returns; reports of the generations created follow as
    /// [`Action::Report`]s, as for any candidate.
    ///
    /// # Rules
    ///
    /// - **Snapshots** are complete, and hold one program. A snapshot equal to the latest one handled, such as a
    ///   redelivery after a worker restart, changes nothing, and its configs keep their acknowledgments. An empty
    ///   snapshot revokes every artifact accepted so far: the program is no longer in effect, an artifact in flight is
    ///   dropped, an artifact kept to roll back to is released, and the core asks for the default program
    ///   ([`Action::InstallDefaultProgram`]) if an artifact was in effect or running. A revoked artifact is never in
    ///   effect, composed, or kept again unless it is accepted anew. A snapshot with more than one config rejects all of
    ///   them without reading them, and changes nothing else: what runs keeps running. A transport failure delivers no
    ///   snapshot, so nothing changes.
    /// - **Receipt.** A config whose `sha256` is not its payload's transport hash, whose payload is not an envelope,
    ///   whose artifact fails the [receipt checks](crate::compose::receive_artifact), or whose envelope's
    ///   `artifact_hash` is not the hash of the artifact as received, is rejected. Its revision is not seen: it does
    ///   not move the high-water mark. What runs keeps running.
    /// - **Idempotency.** A delivery of the program in effect, which is active or being installed, acknowledges the
    ///   current state: the same artifact under the same revision, or the same artifact under a revision above the
    ///   mark, which moves the mark (an approval of the same artifact under a newer revision). Nothing is composed
    ///   again, but the delivery becomes the program in effect: the status summary reports its config and revision,
    ///   and its review is checked against the active plan, as a new acceptance's is.
    /// - **Ordering.** Revisions strictly increase: any other revision at or below the high-water mark is stale and
    ///   rejected, and what runs keeps running. Any other revision above the mark moves it, even if the artifact is
    ///   then rejected. A newer artifact cancels a preparation that has not committed. A failure never changes the
    ///   high-water mark, which lives in the core, outside the restartable worker.
    /// - **Composition** goes through the host's [`Composer`]. A stale review still installs, flagged in status; an
    ///   artifact that cannot be composed, or whose composed plan is invalid, is rejected, and what runs keeps running.
    /// - **Configuration changes** while an artifact is in effect re-compose it against the new default program, under
    ///   its own revision ([`on_default_program`][Self::on_default_program]).
    /// - **Supervision failures** arrive through [`generation_failed`][Self::generation_failed]: a failed artifact
    ///   generation's artifact is acknowledged `failed`, and the core rolls back to last-known-good, or to the default
    ///   program.
    pub fn on_snapshot(&mut self, snapshot: &ArtifactSnapshot, composer: &dyn Composer) -> Vec<Action> {
        if snapshot.product != ArtifactProduct::Policy || self.artifacts.last_snapshot.as_ref() == Some(snapshot) {
            return Vec::new();
        }
        let mut actions = Vec::new();
        match snapshot.configs.as_slice() {
            [] => self.revoke_all(&mut actions),
            [config] => self.receive_one(config, composer, &mut actions),
            configs => self.reject_snapshot(configs),
        }
        self.artifacts.last_snapshot = Some(snapshot.clone());
        self.settle(actions, Settle::Quiet)
    }

    /// Proposes the default program lowered after a configuration change, returning its candidate's identifier and
    /// what to do.
    ///
    /// While no artifact is in effect, this is [`propose`][Self::propose]. While one is, it is composed again through
    /// `composer`, against the new default program, and proposed under its revision instead, so that parameter changes
    /// in native configuration reach the artifact through its configuration bindings; `candidate` is then not used, and
    /// the composer must already compose against it. An artifact that failed under supervision, or was revoked, is not
    /// in effect.
    ///
    /// A re-composition that fails, or whose plan is invalid, leaves the artifact as it was composed before, and the
    /// status summary reports the host degraded while it is in effect or runs, until a later re-composition succeeds
    /// or a new artifact replaces it; an artifact that is rejected resolves nothing. The re-composition of an artifact
    /// that was running is tried again after a failed preparation, as a default program is. Like a default program, a
    /// re-composition waits for a preparation in flight, replacing any candidate waiting, instead of cancelling it, so
    /// that a stream of configuration changes always commits.
    pub fn on_default_program(&mut self, candidate: Candidate, composer: &dyn Composer) -> (CandidateId, Vec<Action>) {
        if candidate.origin.can_roll_back() {
            // Not a default program: proposed as any artifact candidate is, with its revision checked.
            return self.propose(candidate);
        }
        let mut actions = Vec::new();
        self.lowered(Arc::clone(&candidate.plan), &mut actions);
        let id = match self.artifacts.policy.clone() {
            None => {
                let id = self.candidate_id();
                let origin = candidate.origin;
                let plan = Arc::clone(&candidate.plan);
                let bindings = candidate.bindings.clone();
                if self.submit(id, candidate, Arrival::Queue, &mut actions) {
                    // The generation that holds the default program's plan serves the default program.
                    if let Some(target) = self.target_candidate() {
                        self.serve_unchanged(id, origin, &plan, &bindings, target, &mut actions);
                    }
                }
                id
            }
            Some(artifact) => self.compose_again(&artifact, composer, &mut actions),
        };
        self.artifacts.lowered = Some(id);
        // An artifact that waited for a recipient may compose now: it proceeds as a new installation.
        self.compose_waiting(composer, &mut actions);
        (id, self.settle(actions, Settle::Quiet))
    }

    /// Composes the program in effect again after a configuration change and proposes it, recording why it was not
    /// installed again if it was not.
    fn compose_again(
        &mut self, artifact: &Arc<Accepted>, composer: &dyn Composer, actions: &mut Vec<Action>,
    ) -> CandidateId {
        match artifact.compose(composer, true) {
            Ok(composed) => {
                let running = self.active_source().is_some_and(|source| source.serves(artifact));
                // A re-composition waits for a preparation in flight, as a default program does, so that a stream of
                // configuration changes always commits.
                let id = self.install(artifact, composed, Recomposition::Of { running }, actions);
                match rejection_of(actions, id) {
                    Some(rejection) => {
                        let message = format!(
                            "artifact revision {} was not installed again after the configuration changed, and stays \
                             as it was composed before: {}",
                            artifact.revision, rejection
                        );
                        self.artifacts.policy_error = Some((Arc::clone(artifact), message));
                    }
                    // The program is composed against the current configuration again.
                    None => {
                        if self
                            .artifacts
                            .policy_error
                            .as_ref()
                            .is_some_and(|(failed, _)| Arc::ptr_eq(failed, artifact))
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
                    artifact.revision, error
                );
                actions.push(Action::Report(Report {
                    candidate: id,
                    origin: artifact.origin(),
                    generation: None,
                    outcome: Outcome::Rejected(Rejection::Refused {
                        message: message.clone(),
                    }),
                }));
                self.artifacts.policy_error = Some((Arc::clone(artifact), message));
                id
            }
        }
    }

    /// Returns every policy artifact the core holds, once each, in no particular order: the program in effect, those
    /// the candidates it holds serve (installing, active, or kept to roll back to), and the one that waits.
    ///
    /// These are what a host's lowering must keep outputs for: the implied outputs their `emit`s addressed to
    /// recipients need ([`demanded_outputs`][crate::compose::demanded_outputs]), so that none of them, composed again,
    /// finds its output gone.
    pub fn held_policy_artifacts(&self) -> Vec<&PolicyArtifact> {
        let mut seen = BTreeSet::new();
        self.artifacts
            .policy
            .iter()
            .chain(self.artifacts.sources.values().map(|source| &source.artifact))
            .chain(
                self.artifacts
                    .waiting
                    .iter()
                    .map(|waiting| &waiting.acceptance.artifact),
            )
            .filter(|accepted| seen.insert(accepted.hash))
            .map(|accepted| &accepted.artifact.artifact)
            .collect()
    }

    /// Returns the latest acknowledgment of every config the channel delivered, in config ID order.
    pub fn config_states(&self) -> Vec<ConfigAcknowledgment> {
        self.artifacts
            .configs
            .values()
            .map(|tracked| tracked.ack.clone())
            .collect()
    }

    /// Returns the status summary the host reports with its acknowledgments, given the hash of the default program's
    /// core plan.
    ///
    /// The core knows everything in it but the default program's hash, which the host computes as it elaborates the
    /// default program it lowers, and whether the
    /// host is degraded, which the host decides; the core adds why the program in effect, or the one running, could not
    /// be composed again, if it could not. The active artifact is the one the active generation serves, unless the
    /// generation failed; its stale review is that of the composition that generation runs. A revoked artifact is
    /// active until a generation replaces its own.
    pub fn status_summary(&self, default_program_hash: Option<PlanHash>, degraded: Option<String>) -> StatusSummary {
        let active = self.active.as_ref();
        let source = self.active_source();
        let policy_error = self.artifacts.policy_error.as_ref().map(|(_, message)| message.clone());
        let degraded = [degraded, policy_error, self.resources_degraded()]
            .into_iter()
            .flatten()
            .reduce(|reported, more| format!("{}; {}", reported, more));
        StatusSummary {
            default_program_hash,
            revision_high_water: self.high_water,
            active_generation: active.map(|active| active.generation.id().get()),
            active_plan_hash: active.map(|active| active.plan_hash),
            active_artifact: source.map(|source| source.artifact.active()),
            stale_review: source.is_some_and(|source| source.stale_review),
            degraded,
            live_generations: u64::try_from(self.live.len()).unwrap_or(u64::MAX),
            program: active.map(|active| {
                let published = active.origin.can_roll_back();
                ProgramStatus {
                    runs: if published {
                        ProgramRuns::Published
                    } else {
                        ProgramRuns::Default
                    },
                    native_policy: if published {
                        NativePolicy::NotApplied
                    } else {
                        NativePolicy::Applied
                    },
                    differs: Vec::new(),
                }
            }),
        }
    }

    /// Returns everything the host reports through the channel: the status summary and every config's acknowledgment,
    /// given the hash of the default program's core plan.
    pub fn apply_states(&self, default_program_hash: Option<PlanHash>, degraded: Option<String>) -> ApplyStates {
        ApplyStates {
            status: self.status_summary(default_program_hash, degraded),
            config_states: self.config_states(),
        }
    }

    /// Handles the one config of a policy snapshot: reads it, then acknowledges it as a delivery of the program in
    /// effect, refuses it, makes it wait, or installs its artifact as the program in effect.
    ///
    /// A config that cannot be read, or whose artifact is refused, revokes nothing: what runs keeps running, as when a
    /// transport failure delivers nothing.
    fn receive_one(&mut self, config: &ArtifactConfig, composer: &dyn Composer, actions: &mut Vec<Action>) {
        let Some(delivery) = self.read(config, actions) else {
            return;
        };
        if let Some(acceptance) = self.receive(delivery, actions) {
            self.accept(acceptance, composer, actions);
        }
    }

    /// Rejects every config of a snapshot that holds more than one, without reading them: their revisions are not
    /// seen. What runs keeps running, but nothing new is installed: the artifact that waited for a recipient is no
    /// longer delivered on its own.
    fn reject_snapshot(&mut self, configs: &[ArtifactConfig]) {
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
            "the policy snapshot holds {} configs ({}), but a snapshot holds one program: every config is rejected, \
             and what runs keeps running, until the snapshot holds one",
            configs.len(),
            ids.join(", ")
        );
        for config in configs {
            self.refuse(config, None, message.clone());
        }
        self.drop_waiting(&message);
    }

    /// Composes a new artifact and installs it as the program in effect.
    ///
    /// An artifact that emits to a recipient the host does not declare yet waits for it ([`Waiting`]) instead of being
    /// rejected: whatever runs keeps running, and it is composed again at every lowering. One that cannot be composed
    /// otherwise is rejected; one whose composed plan is invalid is rejected when it is prepared.
    fn accept(&mut self, acceptance: Acceptance, composer: &dyn Composer, actions: &mut Vec<Action>) {
        match acceptance.artifact.compose(composer, false) {
            Ok(composed) => self.install_accepted(acceptance, composed, actions),
            Err(error) => match waiting_reason(&error) {
                Some(reason) => self.wait_for_recipient(acceptance, reason),
                None => {
                    let rejection = Rejection::Refused {
                        message: error.to_string(),
                    };
                    self.refuse_reported(&acceptance.config, acceptance.declared, rejection, actions);
                }
            },
        }
    }

    /// Makes an accepted artifact wait for a recipient: its config is acknowledged `pending` with the reason, and it
    /// replaces any artifact that waited before it. Its revision counts as seen.
    fn wait_for_recipient(&mut self, acceptance: Acceptance, reason: String) {
        self.supersede_waiting();
        self.track(
            &acceptance.config,
            Some(acceptance.declared),
            ApplyState::Pending,
            Some(reason.clone()),
            None,
        );
        self.artifacts.waiting = Some(Waiting { acceptance, reason });
    }

    /// Removes the artifact that waits for a recipient, if any, because a newer artifact replaces it: its config is
    /// acknowledged `rejected`, unless it delivers the newer one.
    fn supersede_waiting(&mut self) {
        self.drop_waiting(SUPERSEDED);
    }

    /// Removes the artifact that waits for a recipient, if any: a newer artifact replaces it, or the channel revoked it
    /// or no longer delivers it on its own. Its config is acknowledged `rejected` with `message`, unless the config's
    /// latest delivery is another artifact.
    fn drop_waiting(&mut self, message: &str) {
        let Some(waiting) = self.artifacts.waiting.take() else {
            return;
        };
        let config = &waiting.acceptance.config;
        let current = self
            .artifacts
            .configs
            .get(&config.id)
            .is_some_and(|tracked| tracked.ack.revision == Some(waiting.acceptance.declared.0));
        if current {
            self.refuse(config, Some(waiting.acceptance.declared), message.to_string());
        }
    }

    /// Composes again the artifact that waits for a recipient, after a lowering: if it now composes, it proceeds as a
    /// new installation; if it still waits, it keeps waiting; and if it now fails otherwise, it is rejected.
    fn compose_waiting(&mut self, composer: &dyn Composer, actions: &mut Vec<Action>) {
        let Some(waiting) = self.artifacts.waiting.take() else {
            return;
        };
        match waiting.acceptance.artifact.compose(composer, false) {
            Ok(composed) => self.install_accepted(waiting.acceptance, composed, actions),
            Err(error) => match waiting_reason(&error) {
                Some(reason) => {
                    if reason != waiting.reason {
                        let acceptance = &waiting.acceptance;
                        self.track(
                            &acceptance.config,
                            Some(acceptance.declared),
                            ApplyState::Pending,
                            Some(reason.clone()),
                            None,
                        );
                    }
                    self.artifacts.waiting = Some(Waiting {
                        acceptance: waiting.acceptance,
                        reason,
                    });
                }
                None => {
                    let rejection = Rejection::Refused {
                        message: error.to_string(),
                    };
                    let acceptance = waiting.acceptance;
                    self.refuse_reported(&acceptance.config, acceptance.declared, rejection, actions);
                }
            },
        }
    }

    /// Makes an accepted artifact, composed, the program in effect, and installs its composition.
    fn install_accepted(&mut self, acceptance: Acceptance, composed: Composed, actions: &mut Vec<Action>) {
        // A newer artifact replaces the one that waited for a recipient.
        self.supersede_waiting();
        let artifact = Arc::clone(&acceptance.artifact);
        self.follow(&acceptance.config, acceptance.declared, acceptance.artifact);
        // The artifact is the program in effect from now on, unless it is rejected: settling then makes the program
        // in effect what the schedule holds again. A re-composition failure of the artifact running stays reported
        // until this one replaces it.
        self.artifacts.policy = Some(Arc::clone(&artifact));
        self.install(&artifact, composed, Recomposition::No, actions);
    }

    /// Reads one config: checks its transport hash, envelope, artifact receipt, and artifact hash, refusing it if any
    /// check fails.
    fn read<'a>(&mut self, config: &'a ArtifactConfig, actions: &mut Vec<Action>) -> Option<Delivery<'a>> {
        if let Err(mismatch) = config.verify_transport_hash() {
            self.refuse(config, None, mismatch.to_string());
            return None;
        }
        let envelope = match Envelope::parse(&config.payload) {
            Ok(envelope) => envelope,
            Err(error) => {
                self.refuse(config, None, error.to_string());
                return None;
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
                return None;
            }
        };
        if received.hash != artifact_hash {
            let message = format!(
                "artifact hash mismatch: the envelope declares {}, but the artifact as received hashes to {}; approval \
                 binds to the hash of the exact document",
                artifact_hash, received.hash
            );
            self.refuse_reported(config, declared, Rejection::Refused { message }, actions);
            return None;
        }
        Some(Delivery {
            config,
            revision,
            hash: artifact_hash,
            approved_against: approved_against.map(|approved| approved.active_plan_hash),
            received,
        })
    }

    /// Handles a config that was read: acknowledges it as a delivery of the program in effect, refuses it, or returns
    /// it accepted as a new artifact, for [`accept`][Self::accept] to compose and install.
    fn receive(&mut self, delivery: Delivery<'_>, actions: &mut Vec<Action>) -> Option<Acceptance> {
        let declared = delivery.declared();
        let Delivery {
            config,
            revision,
            hash: artifact_hash,
            approved_against,
            received,
        } = delivery;

        // The program in effect, delivered again under its own revision, or under a newer revision, changes nothing.
        // Under any other revision at or below the mark it is stale.
        let newer = self.high_water.is_none_or(|high_water| revision > high_water);
        if let Some(policy) = self
            .artifacts
            .policy
            .clone()
            .filter(|policy| policy.hash == artifact_hash && (newer || policy.revision == revision))
        {
            self.see_revision(revision);
            // The one config delivers the program in effect: an artifact that waited is no longer delivered.
            self.supersede_waiting();
            let accepted = self.reapprove(&policy, config, revision, approved_against, received);
            self.follow(config, declared, accepted);
            return None;
        }
        // The artifact that waits for a recipient, delivered again by its config under its own revision, such as under
        // a newer config version, keeps waiting.
        if let Some(waiting) = self.artifacts.waiting.as_mut().filter(|waiting| {
            waiting.acceptance.config.id == config.id && waiting.acceptance.declared == (revision, artifact_hash)
        }) {
            waiting.acceptance.config = config.into();
            let reason = waiting.reason.clone();
            self.track(config, Some(declared), ApplyState::Pending, Some(reason), None);
            return None;
        }
        if let Some(high_water) = self.high_water.filter(|high_water| revision <= *high_water) {
            self.refuse_reported(config, declared, Rejection::Stale { revision, high_water }, actions);
            return None;
        }
        self.see_revision(revision);
        // A newer artifact replaces the one that waited for a recipient, whatever becomes of it: installed, waiting
        // itself, or rejected, as one that cannot be composed is.
        self.supersede_waiting();

        let composed_against = self.active.as_ref().map(|active| active.plan_hash);
        let artifact = Arc::new(Accepted {
            config: config.id.clone(),
            revision,
            hash: artifact_hash,
            artifact: received,
            approved_against,
            composed_against,
            epoch: self.artifacts.revocations,
        });
        Some(Acceptance {
            config: config.into(),
            declared,
            artifact,
        })
    }

    /// Makes a delivery of the program in effect, its own redelivery or an approval of the same document under a newer
    /// revision or in another config, the program in effect, and returns it.
    ///
    /// Nothing is composed or installed again, since the document is the same: the candidates that serve the program
    /// serve the delivery instead, as an attribution to a generation composed from the same document does, so the
    /// status summary reports its config and revision, and its review is checked against the active plan as a new
    /// acceptance's is. Every acknowledgment that followed the program follows the delivery, and re-compositions after
    /// later configuration changes compose under its revision. A redelivery that changes none of this keeps the
    /// program.
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
            if source.serves(policy) {
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
    fn see_revision(&mut self, revision: u64) {
        if self.high_water.is_none_or(|high_water| revision > high_water) {
            self.high_water = Some(revision);
        }
    }

    /// Proposes a composed artifact, returning its candidate's identifier.
    ///
    /// A new acceptance cancels a preparation in flight; a re-composition waits for it.
    fn install(
        &mut self, artifact: &Arc<Accepted>, composed: Composed, recomposition: Recomposition,
        actions: &mut Vec<Action>,
    ) -> CandidateId {
        let notes = composed.notes();
        let Composed {
            plan,
            report,
            bindings,
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
            stale_review,
            retried,
            attributed: false,
            notes,
        };
        self.artifacts.sources.insert(id, source);
        let origin = artifact.origin();
        let plan = Arc::new(plan);
        let candidate = Candidate {
            plan: Arc::clone(&plan),
            bindings: bindings.clone(),
            report,
            origin,
        };
        if self.submit(id, candidate, arrival, actions) {
            // The candidate equals what the schedule will hold: the generation that holds it serves the artifact.
            if let Some(target) = self.target_candidate() {
                self.serve_unchanged(id, origin, &plan, &bindings, target, actions);
            }
        }
        id
    }

    /// Makes the candidate `target`, whose plan a candidate reported unchanged equals, serve what that candidate was
    /// created for: the artifact it serves, or, for a default program while no artifact is in effect, the default
    /// program, even if `target` was created for a revoked artifact that composed to the same plan; and the resources
    /// revision it was lowered with. Its generation then runs the program of the candidate's origin: it is adopted
    /// ([`adopt`][Self::adopt]).
    pub(super) fn serve_unchanged(
        &mut self, candidate: CandidateId, origin: Origin, plan: &Arc<Plan>, bindings: &BindingUses,
        target: CandidateId, actions: &mut Vec<Action>,
    ) {
        let Some(source) = self.artifacts.sources.get(&candidate).cloned() else {
            if matches!(origin, Origin::DefaultProgram { .. }) && self.artifacts.policy.is_none() {
                self.artifacts.sources.remove(&target);
                self.adopt(target, origin, plan, bindings, actions);
            }
            return;
        };
        self.adopt(target, origin, plan, bindings, actions);
        match self.artifacts.sources.get_mut(&target) {
            // The same document: the generation that holds its composition serves its latest acceptance, such as an
            // approval under a newer revision, or a delivery after a revocation.
            Some(existing) if existing.artifact.hash == source.artifact.hash => {
                existing.artifact = source.artifact;
                existing.stale_review = source.stale_review;
                existing.notes = source.notes;
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

    /// Revokes every artifact accepted so far, and asks for the default program if the revocation changed anything or
    /// an artifact runs, and nothing in flight installs the default program already.
    fn revoke_all(&mut self, actions: &mut Vec<Action>) {
        let revocation = self.revoke(actions);
        let cancelling = self
            .cut
            .as_ref()
            .is_some_and(|cut| cut.after_cancel == Some(AfterCancel::InstallDefaultProgram));
        let changed = revocation.revoked || revocation.dropped || revocation.artifact_running;
        if changed && !self.default_in_flight() && !cancelling {
            actions.push(Action::InstallDefaultProgram);
        }
    }

    /// Revokes every artifact accepted so far: it leaves effect, its installations in flight are dropped, a generation
    /// serving it is no longer kept to roll back to, and a cut installing it is aborted or cancelled, or, once
    /// irrevocable, waits for its finalization.
    fn revoke(&mut self, actions: &mut Vec<Action>) -> Revocation {
        self.drop_waiting(REVOKED);
        self.artifacts.revocations += 1;
        let revoked = self.artifacts.policy.take();
        let attributions = self
            .artifacts
            .sources
            .iter()
            .filter(|(_, source)| source.attributed && self.is_revoked(&source.artifact))
            .map(|(candidate, _)| *candidate)
            .collect::<Vec<_>>();
        for candidate in attributions {
            self.artifacts.sources.remove(&candidate);
        }
        let serving_revoked = self
            .artifacts
            .sources
            .iter()
            .filter(|(_, source)| self.is_revoked(&source.artifact))
            .map(|(candidate, _)| *candidate)
            .collect::<BTreeSet<_>>();
        // A revocation drops every artifact in flight, whether or not it came from the channel.
        let drops =
            |candidate: CandidateId, origin: &Origin| origin.can_roll_back() || serving_revoked.contains(&candidate);
        let mut dropped = self.drop_in_flight(drops, REVOKED, actions);

        // The cut in flight of a revoked artifact: one not armed yet is aborted, one that is armed is cancelled, after
        // which the default program is asked for, and one that is irrevocable is waited for.
        if let Some(phase) = self
            .cut
            .as_ref()
            .filter(|cut| drops(cut.installed.candidate, &cut.installed.origin))
            .map(|cut| cut.phase)
        {
            match phase {
                CutPhase::Barrier => {
                    let rejection = Rejection::Refused {
                        message: REVOKED.to_string(),
                    };
                    self.abort_cut(rejection, actions);
                    dropped = true;
                }
                CutPhase::Armed { .. } => {
                    self.request_cancel(Some(AfterCancel::InstallDefaultProgram), actions);
                }
                CutPhase::Irrevocable { .. } => {}
            }
        }
        // Nothing a revoked artifact installed is kept to roll back to: a failure before the default program commits
        // waits for it instead. A kept default program stays kept.
        if self
            .last_known_good
            .as_ref()
            .is_some_and(|kept| serving_revoked.contains(&kept.candidate))
        {
            if let Some(kept) = self.last_known_good.take() {
                let id = kept.generation.id();
                if self.state_of(id) == Some(GenerationState::Prepared) {
                    self.transition(id, LifecycleEvent::Released, None);
                    self.report_generation(id, actions);
                }
            }
        }
        // A revoked artifact's plan is not rolled back to either.
        self.rollback = None;
        self.try_start(actions);
        Revocation {
            revoked: revoked.is_some(),
            dropped,
            artifact_running: self
                .active
                .as_ref()
                .is_some_and(|active| active.origin.can_roll_back() && drops(active.candidate, &active.origin)),
        }
    }

    /// Takes the artifact of a failing active or kept generation out of effect, before the failure is handled: its
    /// compositions in flight, under any revision that approved the same document, are dropped rather than prepared
    /// again against the generation rolled back to, and no other generation serves it any more. A delivery after the
    /// failure is accepted anew. The failure is the document's whichever generation runs or keeps it: a kept
    /// generation that fails takes it out of effect as the active one does, though nothing rolls back.
    ///
    /// A cut in flight that serves it is dropped as a preparation is: aborted if it is not armed yet, and, if it is
    /// armed, rejected rather than prepared again once the cancellation the failure asks for succeeds. One that is
    /// irrevocable runs already.
    pub(super) fn withdraw_failing(&mut self, generation: GenerationId, reason: &str, actions: &mut Vec<Action>) {
        let Some(installed) = [self.active.as_ref(), self.last_known_good.as_ref()]
            .into_iter()
            .flatten()
            .find(|installed| installed.generation.id() == generation && installed.origin.can_roll_back())
        else {
            return;
        };
        if self.has_failed(generation) {
            return;
        }
        let failing = installed.candidate;
        let Some(artifact) = self
            .artifacts
            .sources
            .get(&failing)
            .map(|source| Arc::clone(&source.artifact))
        else {
            return;
        };
        let serves_document = |source: &Source| source.artifact.hash == artifact.hash;
        // The failure is the document's, whichever revisions approved it: candidates of the same artifact accepted
        // under another revision are dropped too. The failing generation keeps its own source, even an attribution, so
        // that its failure is acknowledged.
        self.artifacts
            .sources
            .retain(|candidate, source| *candidate == failing || !(source.attributed && serves_document(source)));
        let message = format!(
            "artifact revision {} failed under supervision: {}",
            artifact.revision, reason
        );
        let serving = self
            .artifacts
            .sources
            .iter()
            .filter(|(_, source)| serves_document(source))
            .map(|(candidate, _)| *candidate)
            .collect::<BTreeSet<_>>();
        self.drop_in_flight(|candidate, _| serving.contains(&candidate), &message, actions);
        let phase = self
            .cut
            .as_ref()
            .filter(|cut| cut.installed.candidate != failing && serving.contains(&cut.installed.candidate))
            .map(|cut| cut.phase);
        match phase {
            Some(CutPhase::Barrier) => self.abort_cut(Rejection::Refused { message }, actions),
            Some(CutPhase::Armed { .. }) => {
                if let Some(cut) = self.cut.as_mut() {
                    cut.withdrawn = Some(message);
                }
            }
            Some(CutPhase::Irrevocable { .. }) | None => {}
        }
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
                self.push_abort(id, actions);
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
    /// again an artifact that was running when the configuration changed, or it is a rollback.
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
    /// sources of candidates the core no longer holds, makes the program in effect what the schedule will hold if the
    /// program's installation ended, forgets a re-composition failure whose artifact neither is in effect nor runs,
    /// asks for the default program if an event leaves the active generation behind, and refreshes every
    /// acknowledgment.
    pub(super) fn settle(&mut self, mut actions: Vec<Action>, event: Settle) -> Vec<Action> {
        self.enforce_keep_rule(&mut actions);
        // The withdrawn sets follow the table and the latest lowering, whatever changed them: an exit, the release of
        // a failed recipient or an abort that ended a withdrawn identity's incarnation, or a generation of an older
        // lowering that created a recipient the latest one does not declare. Before the reports are read, so that
        // those of what the transaction leads to are too.
        let withdrawal_differs = self
            .default_program
            .as_ref()
            .is_some_and(|lowered| self.resources.withdrawal_differs(&lowered.resources));
        if withdrawal_differs {
            self.after_exit(&mut actions);
            // The transaction may commit a generation that waited for it, which changes what is kept.
            self.enforce_keep_rule(&mut actions);
        }
        for action in &actions {
            if let Action::Report(report) = action {
                self.record_end(report);
                // A default-program candidate that is rejected, other than for a newer candidate, or that gets a
                // generation or changes nothing.
                if !report.origin.can_roll_back() {
                    match &report.outcome {
                        Outcome::Rejected(Rejection::Cancelled) => {}
                        Outcome::Rejected(rejection) => self.artifacts.default_rejected = Some(rejection.to_string()),
                        Outcome::Unchanged | Outcome::Generation(GenerationState::Pending) => {
                            self.artifacts.default_rejected = None
                        }
                        _ => {}
                    }
                }
            }
        }
        self.prune_sources();

        let mut reverted = false;
        if self.artifacts.policy.is_some() {
            let serving = self.serving();
            if !same_program(serving.as_ref(), self.artifacts.policy.as_ref()) {
                self.artifacts.policy = serving;
                reverted = true;
            }
        }
        if let Some((failed, _)) = &self.artifacts.policy_error {
            let in_effect = same_program(Some(failed), self.artifacts.policy.as_ref());
            let runs = self.active_source().is_some_and(|source| source.serves(failed));
            if !in_effect && !runs {
                self.artifacts.policy_error = None;
            }
        }
        if let Settle::AfterFailure { rolled_back } = event {
            self.ask_for_default_program(reverted || rolled_back, &mut actions);
        }

        self.refresh_acknowledgments();
        self.prune_configs();
        actions.extend(self.take_released_credits());
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
        let held = self.held_candidates();
        self.artifacts.sources.retain(|candidate, _| held.contains(candidate));
    }

    /// Makes a rollback candidate serve the artifact of the candidate whose plan it installs again, if that one serves
    /// one. Its preparation is tried again after a failure, so that the rollback installs eventually.
    pub(super) fn adopt_source(&mut self, candidate: CandidateId, from: CandidateId) {
        if let Some(source) = self.artifacts.sources.get(&from).cloned() {
            let adopted = Source {
                retried: true,
                attributed: false,
                ..source
            };
            self.artifacts.sources.insert(candidate, adopted);
        }
    }

    /// Returns `true` if a default program was lowered after the candidate was created: its plan predates the latest
    /// configuration change.
    pub(super) fn lowered_after(&self, candidate: CandidateId) -> bool {
        self.artifacts.lowered.is_some_and(|lowered| lowered > candidate)
    }

    /// Asks for the default program if the event changed something, nothing in flight will replace the active
    /// generation, and the active generation is an artifact's that failed, or, when `revisited` (the event rolled back,
    /// or the program's installation ended without running), it predates the latest configuration change.
    ///
    /// A failed default-program generation is not replaced: its component failed, as a native component does. An event
    /// that changed nothing, such as a failure reported twice, asks for nothing.
    fn ask_for_default_program(&self, revisited: bool, actions: &mut Vec<Action>) {
        let asked = actions
            .iter()
            .any(|action| matches!(action, Action::InstallDefaultProgram));
        // A cut in flight decides what follows it once it is finalized or cancelled.
        if actions.is_empty() || asked || self.pending.is_some() || self.preparing.is_some() || self.cut.is_some() {
            return;
        }
        let Some(active) = &self.active else { return };
        // A default-program generation that fails fails its component, as a native one does, and the core asks for
        // nothing: unless the default program adopted it from an artifact, and its region decided the failure under
        // the artifact's budget, as when a revocation is processed before the failure report. The generation runs
        // the default program, which is asked for again: a candidate equal to a failed generation is never
        // unchanged, so it gets a new one.
        let failed =
            self.has_failed(active.generation.id()) && (active.origin.can_roll_back() || active.adopted_from_artifact);
        let behind = revisited && self.artifacts.lowered.is_some_and(|lowered| lowered > active.candidate);
        if failed || behind {
            actions.push(Action::InstallDefaultProgram);
        }
    }

    /// Recomputes the acknowledgment of every config that follows an artifact's installation.
    ///
    /// The acknowledgment reported is the latest change: the latest delivery's installation is computed first, then the
    /// earlier one's, which is no longer followed once the latest delivery's artifact is active, or once it ended.
    /// While a newer delivery stands refused, rejected, or failed, the earlier installation's moves to another
    /// generation, such as after a lowering, are not changes of the config's acknowledgment: only a change of its state
    /// is.
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
                // The config's acknowledgment as it stands: a newer delivery refused, rejected, or failed stays it.
                let current = shown.as_ref().unwrap_or(&tracked.ack);
                let newer_ended = matches!(current.state, ApplyState::Rejected | ApplyState::Failed)
                    && (current.revision, current.artifact_hash) != (followed.ack.revision, followed.ack.artifact_hash);
                let state = followed.ack.state;
                let changed = followed.update(ack, serving);
                shown = changed.filter(|ack| !newer_ended || ack.state != state).or(shown);
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
            // A cut in flight takes effect at its cut time, once it is armed.
            let cut_at = self
                .cut
                .as_ref()
                .filter(|cut| generation == Some(cut.installed.generation.id()))
                .and_then(|cut| cut.phase.at())
                .map(|at| at.as_unix_nanos() / NANOS_PER_MILLI);
            // A revoked artifact whose replacement waits for a cut in flight, which can take a cut's whole wait, or
            // was rejected, says so while it stays active, with the rejection while it stands.
            let revoked = state == ApplyState::Active
                && self.is_revoked(&followed.artifact)
                && (self.cut.is_some()
                    || self.artifacts.default_rejected.is_some()
                    || followed
                        .ack
                        .error
                        .as_deref()
                        .is_some_and(|error| error.starts_with(REVOKED_ACTIVE)));
            // An active artifact's acknowledgment says what its composition says beside its state: why it runs
            // degraded, with its emits to an output that was removed dropped.
            let notes = (state == ApplyState::Active)
                .then(|| self.active_notes(&followed.artifact))
                .flatten();
            let error = revoked
                .then(|| match &self.artifacts.default_rejected {
                    Some(rejection) => format!("{}: the default program was rejected: {}", REVOKED_ACTIVE, rejection),
                    None => REVOKED_ACTIVE.to_string(),
                })
                .or(notes);
            let ack = ConfigAcknowledgment {
                state,
                generation: generation.map(GenerationId::get),
                cut_at,
                error,
                ..followed.ack.clone()
            };
            return (ack, true);
        }
        let ack = match &followed.ended {
            Some(ended) => ConfigAcknowledgment {
                state: ended.state,
                generation: ended.generation.map(GenerationId::get).or(followed.ack.generation),
                cut_at: None,
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
                cut_at: None,
                error: None,
                ..followed.ack.clone()
            },
        };
        (ack, false)
    }

    /// Returns what the active candidate's composition says about an artifact it serves beside its state, if anything.
    fn active_notes(&self, artifact: &Arc<Accepted>) -> Option<String> {
        let source = self.artifacts.sources.get(&self.active.as_ref()?.candidate)?;
        source.serves(artifact).then(|| source.notes.clone()).flatten()
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
    /// serving it is active, unless it failed; `prepared` while a cut installation serving it waits for its cut;
    /// `pending` while one waits or is being prepared; and `prepared` while one is kept as last-known-good. Returns
    /// `None` once no candidate the core holds serves it.
    fn role(&self, artifact: &Arc<Accepted>) -> Option<(ApplyState, Option<GenerationId>)> {
        let serves = |candidate: CandidateId| {
            self.artifacts
                .sources
                .get(&candidate)
                .is_some_and(|source| source.serves(artifact))
        };
        if let Some(active) = &self.active {
            let id = active.generation.id();
            if !self.has_failed(id) && serves(active.candidate) {
                return Some((ApplyState::Active, Some(id)));
            }
        }
        // A cut generation past its barrier is prepared until it is irrevocable, when it is the active one.
        if let Some(cut) = &self.cut {
            if !matches!(cut.phase, CutPhase::Irrevocable { .. }) && serves(cut.installed.candidate) {
                return Some((ApplyState::Prepared, Some(cut.installed.generation.id())));
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
    /// one being prepared, else the cut's, else the active one, unless the cut's or the active one failed.
    fn target_candidate(&self) -> Option<CandidateId> {
        self.pending
            .as_ref()
            .map(|queued| queued.candidate)
            .or_else(|| self.preparing.as_ref().map(|preparing| preparing.installed.candidate))
            .or_else(|| {
                self.cut
                    .as_ref()
                    .filter(|cut| cut.failed.is_none())
                    .map(|cut| cut.installed.candidate)
            })
            .or_else(|| {
                self.active
                    .as_ref()
                    .filter(|active| !self.has_failed(active.generation.id()))
                    .map(|active| active.candidate)
            })
    }

    /// Returns the artifact the schedule will hold, unless it was revoked: a revoked artifact's generation runs only
    /// until it is replaced, and the default program is in effect meanwhile.
    fn serving(&self) -> Option<Arc<Accepted>> {
        self.target_candidate()
            .and_then(|candidate| self.artifacts.sources.get(&candidate))
            .map(|source| &source.artifact)
            .filter(|artifact| !self.is_revoked(artifact))
            .cloned()
    }

    /// Returns the source of the active generation, unless it failed.
    fn active_source(&self) -> Option<&Source> {
        self.active
            .as_ref()
            .filter(|active| !self.has_failed(active.generation.id()))
            .and_then(|active| self.artifacts.sources.get(&active.candidate))
    }

    /// Makes a config's acknowledgment follow an artifact's installation.
    fn follow(&mut self, config: impl Into<ConfigRef>, (revision, hash): (u64, PlanHash), artifact: Arc<Accepted>) {
        self.track(
            config,
            Some((revision, hash)),
            ApplyState::Pending,
            None,
            Some(artifact),
        );
    }

    /// Acknowledges a config refused before its artifact was accepted.
    fn refuse(&mut self, config: impl Into<ConfigRef>, declared: Option<(u64, PlanHash)>, message: String) {
        self.track(config, declared, ApplyState::Rejected, Some(message), None);
    }

    /// Acknowledges a config refused after its envelope was read, and reports it like any rejected candidate.
    fn refuse_reported(
        &mut self, config: impl Into<ConfigRef>, declared: (u64, PlanHash), rejection: Rejection,
        actions: &mut Vec<Action>,
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
        &mut self, config: impl Into<ConfigRef>, declared: Option<(u64, PlanHash)>, state: ApplyState,
        error: Option<String>, follows: Option<Arc<Accepted>>,
    ) {
        let config = config.into();
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

/// What a revocation changed.
#[derive(Clone, Copy, Debug)]
struct Revocation {
    /// Whether an artifact was in effect.
    revoked: bool,

    /// Whether an installation in flight was dropped.
    dropped: bool,

    /// Whether the active generation serves a revoked artifact.
    artifact_running: bool,
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
    /// Returns the program in effect: the artifact the core runs or installs instead of the default program, if any.
    ///
    /// While one is in effect, [`on_default_program`][Self::on_default_program] composes it again rather than proposing
    /// the default program. The status summary reports the artifact the active generation serves instead.
    pub(in crate::install) fn policy_in_effect(&self) -> Option<ActiveArtifact> {
        self.artifacts.policy.as_ref().map(|artifact| artifact.active())
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
