//! The installer core's artifact paths: snapshots from the channel, acknowledgments, and the status summary.
//!
//! # The policy in effect
//!
//! Each artifact replaces one scope of the default program, and the channel delivers at most one config per scope. The
//! core keeps, per scope, the artifact it installs instead of the default program's region, the **policy in effect**:
//! the latest artifact for the scope it accepted, until a newer one replaces it, the channel revokes it, it is
//! rejected, or it fails under supervision. The artifacts in effect are composed together, each into its scope, so
//! each candidate composed from artifacts remembers the artifacts it serves, one per scope. When an installation of the
//! policy ends without it running (it is rejected, or it fails), the policy in effect becomes the artifacts of what the
//! schedule will hold again: those of the generation rolled back to, older artifacts still running, or none, for the
//! default program.
//!
//! A revocation revokes every artifact of the scopes it revokes accepted before it: none becomes the policy in effect
//! again, is composed again, or is kept to roll back to, unless the channel delivers it anew and the core accepts it
//! again. A revoked artifact's generation keeps running until the default program, or a newer composition, replaces
//! it.
//!
//! A configuration change while a policy is in effect composes the policy again against the new default program
//! ([`InstallerCore::on_default_program`]), every artifact under its own revision; like a default program, the
//! re-composition waits for a preparation in flight instead of cancelling it. An artifact that composes to the plan
//! already running or being prepared, such as an unmodified export of the default program, creates no generation: the
//! generation that holds its plan serves it.
//!
//! # Artifacts that wait for an output
//!
//! A new artifact whose base records an output the host does not declare, such as a policy published before the
//! resources artifact that declares its output, is neither rejected nor installed: it **waits** for the output. It
//! holds nothing and counts against no overlap budget; its revision counts as seen; its config is acknowledged
//! `pending` with the output it waits for; and whatever runs keeps running. Every lowering composes it again, and once
//! it composes it proceeds as a new installation. At most one artifact waits per scope, the newest: a newer artifact
//! for the scope replaces it, whether that one installs, waits, or is rejected; a revocation of the scope removes it,
//! and so does a snapshot that no longer names the scope, or names it in more than one config. A delivery of it again
//! by its config under its own revision keeps it waiting, while a stale or unreadable delivery changes nothing for it,
//! as for an installation in flight. A re-composition of the artifact in effect after its output was removed is the
//! composer's to make, with that output's emits dropped.
//!
//! # Acknowledgments
//!
//! Every config the channel delivered has an acknowledgment, kept per config ID. A config refused on receipt is
//! `rejected` with the reason. A config whose artifact was accepted, or whose artifact equals the policy in effect for
//! its scope, follows the installation of that artifact: `pending` while it waits or is prepared, `prepared` while a
//! cut installation of it waits for its cut, or while it is kept to roll back to, `active` while a generation serving
//! it is active, and then how its installation ended: `rejected`, `failed`, or `retired`. A generation that stops
//! serving it without ending it (superseded and draining, released from last-known-good, or taken over by the default
//! program after a revocation) leaves the acknowledgment as it was until the generation retires.
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
    sync::{Arc, Mutex, PoisonError},
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
        ConfigAcknowledgment, Envelope, ScopeStatus, StatusSummary, POLICY_SCOPE,
    },
    compose::{receive_artifact, Approval, ComposeError, Composed, Composer, ScopeComposition},
    install::lifecycle::{GenerationState, LifecycleEvent},
    ir::{OutputId, ReceivedArtifact, ScopeName},
};

/// The most configs absent from the latest snapshot whose final acknowledgment the core keeps.
const CONFIG_HISTORY: usize = 16;

/// The most config IDs the rejection of a scope's configs, when a snapshot holds more than one for it, names, so that
/// its acknowledgments stay linear in the number of configs.
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

    /// The scope it replaces.
    scope: ScopeName,

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

    /// The number of revocations before it was accepted: the next one of its scope revokes it.
    epoch: u64,

    /// The outputs its base records with a version 1 fingerprint that one of its compositions accepted, each with the
    /// version 2 fingerprint the output had then ([`ScopeComposition::legacy_outputs`]): from then on the recorded
    /// fingerprint stands for that one, so that a version 1 base follows version 2 once it composed. An entry never
    /// changes once set, and the memory goes with the artifact.
    upgraded: Mutex<BTreeMap<OutputId, PlanHash>>,
}

impl Accepted {
    /// Returns the approval it composes with: as the artifact in effect for its scope, whose emits to an output
    /// removed or rebound since are dropped, or as a new one, which such an output keeps from installing.
    fn approval(&self, in_effect: bool) -> Approval {
        Approval {
            revision: self.revision,
            approved_against: self.approved_against,
            active_plan_hash: self.composed_against,
            in_effect,
            upgraded: self.upgraded.lock().unwrap_or_else(PoisonError::into_inner).clone(),
        }
    }

    /// Keeps the version 1 outputs a composition of it accepted, for its later compositions.
    fn upgrade(&self, composition: &ScopeComposition) {
        if !composition.legacy_outputs.is_empty() {
            let mut upgraded = self.upgraded.lock().unwrap_or_else(PoisonError::into_inner);
            for (output, fingerprint) in &composition.legacy_outputs {
                upgraded.entry(output.clone()).or_insert(*fingerprint);
            }
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

/// Artifacts by the scope each replaces: the policy in effect, or what a candidate serves.
type Policy = BTreeMap<ScopeName, Arc<Accepted>>;

/// Returns the origin of a candidate composed from a policy: the artifact with the newest revision's, the latest
/// approval among them.
fn origin_of(policy: &Policy) -> Origin {
    policy.values().max_by_key(|artifact| artifact.revision).map_or(
        Origin::DefaultProgram { config_change: 0 },
        |artifact| Origin::Artifact {
            revision: artifact.revision,
            hash: artifact.hash,
        },
    )
}

/// Returns `true` if two policies hold the same accepted artifacts, scope by scope.
fn same_policy(first: &Policy, second: &Policy) -> bool {
    first.len() == second.len()
        && first
            .iter()
            .zip(second)
            .all(|((first_scope, first), (second_scope, second))| {
                first_scope == second_scope && Arc::ptr_eq(first, second)
            })
}

/// Says which artifacts a policy holds, for messages, such as `artifact revision 7` or `artifact revisions 7 and 9`.
fn describe_revisions(policy: &Policy) -> String {
    let revisions = policy
        .values()
        .map(|artifact| artifact.revision.to_string())
        .collect::<Vec<_>>();
    match revisions.as_slice() {
        [only] => format!("artifact revision {}", only),
        [init @ .., last] => format!("artifact revisions {} and {}", init.join(", "), last),
        [] => "no artifact".to_string(),
    }
}

/// The artifacts a candidate serves.
#[derive(Clone, Debug)]
struct Source {
    /// One artifact per scope the candidate's plan replaces.
    artifacts: Policy,

    /// The scopes whose composition had a stale base.
    stale_base: BTreeSet<ScopeName>,

    /// The scopes whose composition had a stale review.
    stale_review: BTreeSet<ScopeName>,

    /// Whether the candidate composes again artifacts that were running when the configuration changed, or is a
    /// rollback. Its preparation is tried again after a failure, as a default-program candidate's is, so that
    /// configuration changes are applied eventually.
    retried: bool,

    /// Whether the candidate was created for another plan that the artifacts composed to: it serves them without having
    /// been composed from them.
    attributed: bool,

    /// What each scope's composition says beside its state ([`ScopeComposition::notes`]): why the artifact runs
    /// degraded, and which outputs to rebase for. Its acknowledgment carries them while the candidate is active.
    notes: BTreeMap<ScopeName, String>,
}

impl Source {
    /// Returns `true` if the candidate serves the artifact.
    fn serves(&self, artifact: &Arc<Accepted>) -> bool {
        self.artifacts.values().any(|served| Arc::ptr_eq(served, artifact))
    }

    /// Returns the hash of the document of each scope's artifact.
    fn documents(&self) -> BTreeMap<&ScopeName, PlanHash> {
        self.artifacts
            .iter()
            .map(|(scope, artifact)| (scope, artifact.hash))
            .collect()
    }
}

/// Whether a composed policy is a re-composition of the policy in effect after a configuration change.
#[derive(Clone, Copy, Debug)]
enum Recomposition {
    /// A new acceptance, or a change of the policy in effect by the channel.
    No,

    /// A re-composition.
    Of {
        /// Whether the policy was running when the configuration changed: its preparation is then tried again after a
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

    /// The scope the latest delivery's artifact replaces, once it was received.
    scope: Option<ScopeName>,

    /// Whether the latest delivery was rejected for what the other configs of its snapshot held, another config for
    /// its scope or another new revision equal to its own: held unchanged in the next snapshot, it is handled anew.
    reread: bool,

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

/// A delivery accepted as a new artifact for its scope, not composed or installed yet.
#[derive(Clone, Debug)]
struct Acceptance {
    config: ConfigRef,
    declared: (u64, PlanHash),
    artifact: Arc<Accepted>,
}

/// An artifact that waits for an output its base records and the host does not declare yet: it holds nothing, counts
/// against no overlap budget, and is composed again at every lowering, until it composes, a newer artifact for its
/// scope replaces it, or a revocation removes it.
#[derive(Clone, Debug)]
struct Waiting {
    acceptance: Acceptance,

    /// Why it waits.
    reason: String,
}

/// Returns the output a composition waits for, if its error says that an output the artifact's base records is not
/// declared: a new artifact then waits for it rather than being rejected.
fn pending_output(error: &ComposeError) -> Option<&OutputId> {
    match error {
        ComposeError::PendingOutput { output } => Some(output),
        _ => None,
    }
}

/// Returns why an artifact waits for `output`.
fn waiting_reason(output: &OutputId) -> String {
    format!(
        "waiting for output `{}`: the artifact's base records it, and the host does not declare it; the artifact is \
         installed once the output is declared",
        output
    )
}

/// What the core keeps about the artifacts the channel delivered.
#[derive(Debug, Default)]
pub(super) struct ArtifactState {
    /// Every config seen, by config ID.
    configs: BTreeMap<ArtifactConfigId, Tracked>,

    /// The artifacts each candidate the core holds serves, for candidates composed from artifacts or attributed to
    /// them: pending, preparing, cut, active, kept as last-known-good, rolled back to, or with a generation still
    /// alive.
    sources: BTreeMap<CandidateId, Source>,

    /// The policy in effect, by scope.
    policy: Policy,

    /// Why the policy could not be composed again after the latest configuration change, if it could not, with the
    /// policy: the host reports it degraded while one of its artifacts is in effect or the active generation serves
    /// one, until a re-composition of it succeeds.
    policy_error: Option<(Policy, String)>,

    /// The number of revocations so far.
    revocations: u64,

    /// The number of revocations when every scope was last revoked: every artifact accepted before is revoked.
    revoked_all_at: u64,

    /// The number of revocations when each scope was last revoked on its own: every artifact of the scope accepted
    /// before is revoked.
    revoked_at: BTreeMap<ScopeName, u64>,

    /// The latest snapshot handled. A redelivery of it, such as after a worker restart, changes nothing, and a config
    /// it holds unchanged in the next snapshot keeps its acknowledgment.
    last_snapshot: Option<ArtifactSnapshot>,

    /// The candidate created for the latest default program handed to the core, to tell whether a generation predates
    /// the latest configuration change.
    lowered: Option<CandidateId>,

    /// Why the latest default-program candidate was rejected, until a later one is prepared or changes nothing: a
    /// revoked artifact that stays active meanwhile says so.
    default_rejected: Option<String>,

    /// The artifacts that wait for an output, by scope: the newest per scope.
    waiting: BTreeMap<ScopeName, Waiting>,

    /// The number of acknowledgment changes so far.
    changes: u64,
}

impl InstallerCore {
    /// Handles a complete snapshot of the policy product from the artifact channel, returning what to do.
    ///
    /// The snapshot replaces the previous one: it revokes, rejects, acknowledges the current state, or proposes the
    /// artifacts it holds, at most one per scope, composed together through `composer`. Every config it holds gets an
    /// acknowledgment, which [`config_states`][Self::config_states] returns; reports of the generations created follow
    /// as [`Action::Report`]s, as for any candidate.
    ///
    /// # Rules
    ///
    /// - **Snapshots** are complete. A snapshot equal to the latest one handled, such as a redelivery after a worker
    ///   restart, changes nothing, and a config it holds unchanged keeps its acknowledgment. An empty snapshot revokes
    ///   every artifact accepted so far: the policy is no longer in effect, an artifact in flight is dropped, an
    ///   artifact kept to roll back to is released, and the core asks for the default program
    ///   ([`Action::InstallDefaultProgram`]) if an artifact was in effect or running. A snapshot that holds no config
    ///   for a scope whose artifact is in effect, and whose every config was read, revokes that scope's artifacts, even
    ///   if the config that delivered the artifact now delivers another scope's: the other scopes' artifacts in effect
    ///   are composed and installed again without it. A revoked artifact is never in effect, composed, or kept again
    ///   unless it is accepted anew. A snapshot with more than one config for a scope rejects all of them, and changes
    ///   nothing for the scope. A transport failure delivers no snapshot, so nothing changes.
    /// - **Receipt.** A config whose `sha256` is not its payload's transport hash, whose payload is not an envelope,
    ///   whose artifact fails the [receipt checks](crate::compose::receive_artifact), or whose envelope's
    ///   `artifact_hash` is not the hash of the artifact as received, is rejected. Its revision is not seen: it does
    ///   not move the high-water mark.
    /// - **Idempotency.** A delivery of the policy in effect for its scope, which is active or being installed,
    ///   acknowledges the current state: the same artifact under the same revision, whatever the high-water mark (the
    ///   other scope may have moved it since), or the same artifact under a revision above the mark, which moves the
    ///   mark (an approval of the same artifact under a newer revision). Nothing is composed again, but the delivery
    ///   becomes the policy in effect: the status summary reports its config and revision, and its review is checked
    ///   against the active plan, as a new acceptance's is.
    /// - **Ordering.** Revisions strictly increase across scopes: any other revision at or below the high-water mark is
    ///   stale and rejected. The configs of one snapshot are ordered against the mark as the snapshot found it, so
    ///   two scopes' new revisions in one snapshot are both newer; two equal new revisions are neither, and both are
    ///   rejected, without their revision being seen. Any other revision above the mark moves it, even if the artifact
    ///   is then rejected. A newer artifact cancels a preparation that has not committed. A failure never changes the
    ///   high-water mark, which lives in the core, outside the restartable worker.
    /// - **Composition** goes through the host's [`Composer`], with the artifacts in effect for the other scopes. A
    ///   stale base or a stale review still installs, flagged in status; an artifact that cannot be composed, or whose
    ///   composed plan is invalid, is rejected, and what runs keeps running. New artifacts of several scopes in one
    ///   snapshot install together; when their plan is rejected, each is checked alone with the artifacts in effect
    ///   for the other scopes, and those rejected alone are rejected with their own reasons while the others install.
    /// - **Configuration changes** while artifacts are in effect re-compose them against the new default program, each
    ///   under its own revision ([`on_default_program`][Self::on_default_program]).
    /// - **Supervision failures** arrive through [`generation_failed`][Self::generation_failed]: a failed artifact
    ///   generation's artifacts are acknowledged `failed`, and the core rolls back to last-known-good, or to the
    ///   default program.
    ///
    /// A snapshot of the resources product changes nothing here: the caller reads it and hands what it says to
    /// [`on_resources`][Self::on_resources].
    pub fn on_snapshot(&mut self, snapshot: &ArtifactSnapshot, composer: &dyn Composer) -> Vec<Action> {
        if snapshot.product != ArtifactProduct::Policy || self.artifacts.last_snapshot.as_ref() == Some(snapshot) {
            return Vec::new();
        }
        let mut actions = Vec::new();
        match snapshot.configs.as_slice() {
            [] => self.revoke_all(&mut actions),
            configs => self.receive_all(configs, composer, &mut actions),
        }
        self.artifacts.last_snapshot = Some(snapshot.clone());
        self.settle(actions, Settle::Quiet)
    }

    /// Proposes the default program lowered after a configuration change, returning its candidate's identifier and
    /// what to do.
    ///
    /// While no artifact is in effect, this is [`propose`][Self::propose]. While some are, they are composed again
    /// through `composer`, against the new default program, and proposed under the newest revision among them instead,
    /// so that parameter changes in native configuration reach the artifacts through their configuration bindings;
    /// `candidate` is then not used, and the composer must already compose against it. An artifact that failed under
    /// supervision, or was revoked, is not in effect.
    ///
    /// A re-composition that fails, or whose plan is invalid, leaves the artifacts as they were composed before, and
    /// the status summary reports the host degraded while one of them is in effect or runs, until a later
    /// re-composition succeeds or a new artifact replaces them; an artifact that is rejected resolves nothing. The
    /// re-composition of artifacts that were running is tried again after a failed preparation, as a default program
    /// is. Like a default program, a re-composition waits for a preparation in flight, replacing any candidate
    /// waiting, instead of cancelling it, so that a stream of configuration changes always commits.
    pub fn on_default_program(&mut self, candidate: Candidate, composer: &dyn Composer) -> (CandidateId, Vec<Action>) {
        if candidate.origin.can_roll_back() {
            // Not a default program: proposed as any artifact candidate is, with its revision checked.
            return self.propose(candidate);
        }
        let mut actions = Vec::new();
        self.lowered(Arc::clone(&candidate.plan), &mut actions);
        let id = if self.artifacts.policy.is_empty() {
            let id = self.candidate_id();
            let origin = candidate.origin;
            if self.submit(id, candidate, Arrival::Queue, &mut actions) {
                // The generation that holds the default program's plan serves the default program.
                if let Some(target) = self.target_candidate() {
                    self.serve_unchanged(id, origin, target);
                }
            }
            id
        } else {
            let policy = self.artifacts.policy.clone();
            self.compose_again(&policy, composer, &mut actions)
        };
        self.artifacts.lowered = Some(id);
        // An artifact that waited for an output may compose now: it proceeds as a new installation.
        self.compose_waiting(composer, &mut actions);
        (id, self.settle(actions, Settle::Quiet))
    }

    /// Composes the policy in effect again after a configuration change and proposes it, recording why it was not
    /// installed again if it was not.
    fn compose_again(&mut self, policy: &Policy, composer: &dyn Composer, actions: &mut Vec<Action>) -> CandidateId {
        match compose_policy(composer, policy, &self.artifacts.policy) {
            Ok(composed) => {
                let running = self
                    .active_source()
                    .is_some_and(|source| same_policy(&source.artifacts, policy));
                // A re-composition waits for a preparation in flight, as a default program does, so that a stream of
                // configuration changes always commits.
                let id = self.install(policy, composed, Recomposition::Of { running }, actions);
                match rejection_of(actions, id) {
                    Some(rejection) => {
                        let message = format!(
                            "{} {} not installed again after the configuration changed, and {} as {} composed \
                             before: {}",
                            describe_revisions(policy),
                            if policy.len() == 1 { "was" } else { "were" },
                            if policy.len() == 1 { "stays" } else { "stay" },
                            if policy.len() == 1 { "it was" } else { "they were" },
                            rejection
                        );
                        self.artifacts.policy_error = Some((policy.clone(), message));
                    }
                    // The policy is composed against the current configuration again.
                    None => {
                        if self
                            .artifacts
                            .policy_error
                            .as_ref()
                            .is_some_and(|(failed, _)| same_policy(failed, policy))
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
                    "{} cannot be composed with the changed default program, and {} as {} composed before: {}",
                    describe_revisions(policy),
                    if policy.len() == 1 { "stays" } else { "stay" },
                    if policy.len() == 1 { "it was" } else { "they were" },
                    error
                );
                actions.push(Action::Report(Report {
                    candidate: id,
                    origin: origin_of(policy),
                    generation: None,
                    outcome: Outcome::Rejected(Rejection::Refused {
                        message: message.clone(),
                    }),
                }));
                self.artifacts.policy_error = Some((policy.clone(), message));
                id
            }
        }
    }

    /// Returns the latest acknowledgment of every config the channel delivered: the policy product's, then the
    /// resources product's, each in config ID order.
    pub fn config_states(&self) -> Vec<ConfigAcknowledgment> {
        self.artifacts
            .configs
            .values()
            .map(|tracked| tracked.ack.clone())
            .chain(self.resources_config_states())
            .collect()
    }

    /// Returns the status summary the host reports with its acknowledgments, given the base program hash of the
    /// default program's policy scope (see [`POLICY_SCOPE`]).
    ///
    /// This is [`status_summary_with_bases`][Self::status_summary_with_bases] with the policy scope's base alone: the
    /// summary's `scopes` are written only once an artifact is active in another scope.
    pub fn status_summary(&self, default_program_hash: Option<PlanHash>, degraded: Option<String>) -> StatusSummary {
        let bases = policy_scope_base(default_program_hash);
        let mut summary = self.status_summary_with_bases(&bases, degraded);
        let other_scope_active = summary
            .scopes
            .iter()
            .any(|(scope, status)| scope.as_str() != POLICY_SCOPE && status.active_artifact.is_some());
        if !other_scope_active {
            summary.scopes.clear();
        }
        summary
    }

    /// Returns the status summary the host reports with its acknowledgments, given the base program hash of each scope
    /// of the default program, by scope.
    ///
    /// The core knows everything in it but the base program hashes, which the host computes from its exports, and
    /// whether the host is degraded, which the host decides; the core adds why the policy in effect, or the one
    /// running, could not be composed again, if it could not. The active artifacts are those the active generation
    /// serves, unless the generation failed; their stale flags are those of the composition that generation runs. A
    /// revoked artifact is active until a generation replaces its own. The top-level members describe the policy scope
    /// ([`POLICY_SCOPE`]), and `scopes` every scope with a base or an active artifact; `stale_review` is set if any
    /// active artifact's review is stale.
    pub fn status_summary_with_bases(
        &self, bases: &BTreeMap<ScopeName, PlanHash>, degraded: Option<String>,
    ) -> StatusSummary {
        let active = self.active.as_ref();
        let source = self.active_source();
        let policy_error = self.artifacts.policy_error.as_ref().map(|(_, message)| message.clone());
        let degraded = [degraded, policy_error, self.resources_degraded()]
            .into_iter()
            .flatten()
            .reduce(|reported, more| format!("{}; {}", reported, more));
        let mut scopes = bases.keys().collect::<BTreeSet<_>>();
        scopes.extend(source.iter().flat_map(|source| source.artifacts.keys()));
        let scopes = scopes
            .into_iter()
            .map(|scope| {
                let status = ScopeStatus {
                    default_program_hash: bases.get(scope).copied(),
                    active_artifact: source
                        .and_then(|source| source.artifacts.get(scope))
                        .map(|artifact| artifact.active()),
                    stale_base: source.is_some_and(|source| source.stale_base.contains(scope)),
                };
                (scope.clone(), status)
            })
            .collect::<BTreeMap<_, _>>();
        let policy_scope = scopes.iter().find(|(scope, _)| scope.as_str() == POLICY_SCOPE);
        StatusSummary {
            default_program_hash: policy_scope.and_then(|(_, status)| status.default_program_hash),
            revision_high_water: self.high_water,
            active_generation: active.map(|active| active.generation.id().get()),
            active_plan_hash: active.map(|active| active.plan_hash),
            active_artifact: policy_scope.and_then(|(_, status)| status.active_artifact.clone()),
            stale_base: policy_scope.is_some_and(|(_, status)| status.stale_base),
            stale_review: source.is_some_and(|source| !source.stale_review.is_empty()),
            degraded,
            live_generations: u64::try_from(self.live.len()).unwrap_or(u64::MAX),
            scopes,
            resources: self.resources_status(),
        }
    }

    /// Returns everything the host reports through the channel: the status summary and every config's acknowledgment,
    /// given the base program hash of the default program's policy scope.
    pub fn apply_states(&self, default_program_hash: Option<PlanHash>, degraded: Option<String>) -> ApplyStates {
        ApplyStates {
            status: self.status_summary(default_program_hash, degraded),
            config_states: self.config_states(),
        }
    }

    /// Returns everything the host reports through the channel, given the base program hash of each scope of the
    /// default program: the status summary, with every scope's status, and every config's acknowledgment.
    pub fn apply_states_with_bases(
        &self, bases: &BTreeMap<ScopeName, PlanHash>, degraded: Option<String>,
    ) -> ApplyStates {
        ApplyStates {
            status: self.status_summary_with_bases(bases, degraded),
            config_states: self.config_states(),
        }
    }

    /// Handles the configs of a non-empty policy snapshot.
    ///
    /// Configs held unchanged since the latest snapshot keep their acknowledgment. Every other config is read; those
    /// read are grouped by scope, and a scope with more than one rejects them all. Then the scopes the snapshot no
    /// longer delivers are revoked, new revisions shared by several configs are rejected, and each scope's one config
    /// is handled; the artifacts accepted are installed with the policy in effect for the other scopes.
    fn receive_all(&mut self, configs: &[ArtifactConfig], composer: &dyn Composer, actions: &mut Vec<Action>) {
        let previous = self
            .artifacts
            .last_snapshot
            .as_ref()
            .map(|snapshot| snapshot.configs.clone())
            .unwrap_or_default();
        let mut unread = false;
        let mut present = BTreeSet::new();
        let mut by_scope: BTreeMap<ScopeName, Vec<Delivery<'_>>> = BTreeMap::new();
        let mut unchanged: BTreeMap<ScopeName, Vec<&ArtifactConfig>> = BTreeMap::new();
        // Which configs to handle anew is decided as the snapshot arrives, so that reading one config cannot change
        // how another of the same snapshot is handled, even one under the same ID.
        let reread = self
            .artifacts
            .configs
            .iter()
            .filter(|(_, tracked)| tracked.reread)
            .map(|(id, _)| id.clone())
            .collect::<BTreeSet<_>>();
        for config in configs {
            // A config held unchanged keeps its acknowledgment, unless it was rejected for what the other configs of
            // its snapshot held: then it is handled anew.
            if previous.contains(config) && !reread.contains(&config.id) {
                match self
                    .artifacts
                    .configs
                    .get(&config.id)
                    .and_then(|tracked| tracked.scope.clone())
                {
                    Some(scope) => {
                        present.insert(scope.clone());
                        unchanged.entry(scope).or_default().push(config);
                    }
                    None => unread = true,
                }
                continue;
            }
            match self.read(config, actions) {
                Some(delivery) => {
                    let scope = delivery.received.artifact.scope.clone();
                    present.insert(scope.clone());
                    by_scope.entry(scope).or_default().push(delivery);
                }
                None => unread = true,
            }
        }

        // A scope no config of the snapshot names is revoked, unless a config it holds could not be read: the config
        // that delivered the scope's artifact no longer delivers it once it names another scope.
        let absent = self
            .artifacts
            .policy
            .keys()
            .filter(|scope| !present.contains(*scope))
            .cloned()
            .collect::<BTreeSet<_>>();
        let revocation = (!unread && !absent.is_empty()).then(|| self.revoke_scopes(Some(&absent), actions));
        let changed = revocation.is_some_and(|revocation| revocation.changed());
        // An artifact that waited for an output in a scope the snapshot no longer names is no longer delivered.
        let gone = self
            .artifacts
            .waiting
            .keys()
            .filter(|scope| !present.contains(*scope))
            .cloned()
            .collect::<BTreeSet<_>>();
        if !unread && !gone.is_empty() {
            self.drop_waiting(Some(&gone), REVOKED);
        }

        // A scope with one config is handled; one with more, held unchanged or not, rejects them all. Every config of
        // the snapshot is ordered against the mark as the snapshot found it, whatever scope comes first.
        let mark = self.high_water;
        let mut lone = Vec::new();
        for (scope, deliveries) in by_scope {
            let kept = unchanged.remove(&scope).unwrap_or_default();
            if deliveries.len() + kept.len() > 1 {
                let all = deliveries
                    .iter()
                    .map(|delivery| delivery.config)
                    .chain(kept)
                    .collect::<Vec<_>>();
                self.reject_scope(&scope, &all);
                continue;
            }
            lone.extend(deliveries);
        }
        for (scope, kept) in unchanged.into_iter().filter(|(_, kept)| kept.len() > 1) {
            self.reject_scope(&scope, &kept);
        }
        let mut scopes = Vec::new();
        let mut accepted = Vec::new();
        for delivery in self.reject_shared_revisions(lone, mark, actions) {
            scopes.push((delivery.config.id.clone(), delivery.received.artifact.scope.clone()));
            accepted.extend(self.receive(delivery, mark, actions));
        }
        let installed = self.accept(accepted, composer, actions);
        for (id, scope) in &scopes {
            self.set_scope(id, scope);
        }
        match installed {
            true => {}
            false if changed && !self.artifacts.policy.is_empty() => {
                // What the revoked scopes leave in effect is installed again without them.
                let policy = self.artifacts.policy.clone();
                match compose_policy(composer, &policy, &policy) {
                    Ok(composed) => {
                        self.install(&policy, composed, Recomposition::No, actions);
                    }
                    Err(error) => {
                        let message = format!(
                            "{} cannot be composed without the scopes the channel revoked, and {} as {} composed \
                             before: {}",
                            describe_revisions(&policy),
                            if policy.len() == 1 { "stays" } else { "stay" },
                            if policy.len() == 1 { "it was" } else { "they were" },
                            error
                        );
                        self.artifacts.policy_error = Some((policy, message));
                    }
                }
            }
            false => {
                if let Some(revocation) = revocation {
                    self.ask_after_revocation(revocation.changed() || revocation.artifact_running, actions);
                }
            }
        }
    }

    /// Rejects the configs of a snapshot that share a revision above the high-water mark the snapshot found, and
    /// returns the others.
    ///
    /// Revisions strictly increase across scopes, so none of several new artifacts under one revision is newer than
    /// the others: all are rejected, their revision is not seen, and each is handled anew in the next snapshot, where
    /// it may have a revision of its own. A delivery of the policy in effect under its own revision is not new,
    /// whatever the mark.
    fn reject_shared_revisions<'a>(
        &mut self, deliveries: Vec<Delivery<'a>>, mark: Option<u64>, actions: &mut Vec<Action>,
    ) -> Vec<Delivery<'a>> {
        let mut by_revision = BTreeMap::<u64, Vec<&ArtifactConfigId>>::new();
        for delivery in &deliveries {
            if mark.is_none_or(|mark| delivery.revision > mark) {
                by_revision
                    .entry(delivery.revision)
                    .or_default()
                    .push(&delivery.config.id);
            }
        }
        let shared = by_revision
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(revision, mut ids)| {
                ids.sort_unstable();
                let ids = ids.iter().map(|id| format!("`{}`", id)).collect::<Vec<_>>();
                (revision, ids.join(", "))
            })
            .collect::<BTreeMap<_, _>>();
        let mut kept = Vec::new();
        for delivery in deliveries {
            let Some(ids) = shared.get(&delivery.revision) else {
                kept.push(delivery);
                continue;
            };
            let message = format!(
                "revision {} is the revision of every config of ({}) in one snapshot, but revisions increase across \
                 scopes: none of them is newer than the others, so each is rejected until it has a revision of its own",
                delivery.revision, ids
            );
            let config = delivery.config;
            self.refuse_reported(config, delivery.declared(), Rejection::Refused { message }, actions);
            if let Some(tracked) = self.artifacts.configs.get_mut(&config.id) {
                tracked.scope = Some(delivery.received.artifact.scope.clone());
                tracked.reread = true;
            }
        }
        kept
    }

    /// Installs the artifacts a snapshot accepted with the policy in effect for the other scopes, and returns `true` if
    /// it proposed a candidate.
    ///
    /// Artifacts of several scopes are installed together. When the plan they compose to is rejected, each is checked
    /// alone with the policy in effect, so that each config is acknowledged with its own artifact's outcome: those
    /// rejected alone are rejected with their own reasons, and the others are installed together. Artifacts that each
    /// install alone, but not together, are rejected together.
    ///
    /// An artifact whose base records an output the host does not declare waits for it ([`Waiting`]) instead of being
    /// rejected: whatever runs keeps running, and it is composed again at every lowering.
    fn accept(&mut self, acceptances: Vec<Acceptance>, composer: &dyn Composer, actions: &mut Vec<Action>) -> bool {
        if acceptances.is_empty() {
            return false;
        }
        let policy = with_accepted(&self.artifacts.policy, &acceptances);
        let composed = compose_policy(composer, &policy, &self.artifacts.policy);
        if let (Err(error), [acceptance]) = (&composed, acceptances.as_slice()) {
            if let Some(output) = pending_output(error) {
                let reason = waiting_reason(output);
                self.wait_for_output(acceptance.clone(), reason);
                return false;
            }
        }
        // One artifact is checked when it is prepared; several are checked together first.
        let rejection = match &composed {
            Ok(composed) if acceptances.len() > 1 => self.check_composed(composed, &policy).err(),
            Ok(_) => None,
            Err(error) => Some(Rejection::Refused {
                message: error.to_string(),
            }),
        };
        if let Some(rejection) = rejection {
            if acceptances.len() > 1 {
                // Each artifact alone with the policy in effect, so that one rejected alone does not take the others
                // down with it.
                let mut passing = Vec::new();
                let mut failing = false;
                for acceptance in acceptances {
                    let alone = with_accepted(&self.artifacts.policy, std::slice::from_ref(&acceptance));
                    let checked = match compose_policy(composer, &alone, &self.artifacts.policy) {
                        Ok(composed) => self.check_composed(&composed, &alone),
                        Err(error) => match pending_output(&error) {
                            Some(output) => {
                                failing = true;
                                let reason = waiting_reason(output);
                                self.wait_for_output(acceptance, reason);
                                continue;
                            }
                            None => Err(Rejection::Refused {
                                message: error.to_string(),
                            }),
                        },
                    };
                    match checked {
                        Ok(()) => passing.push(acceptance),
                        Err(own) => {
                            failing = true;
                            self.refuse_reported(&acceptance.config, acceptance.declared, own, actions);
                        }
                    }
                }
                if failing {
                    return self.accept(passing, composer, actions);
                }
                if composed.is_ok() {
                    // Each installs alone, but not with the others: they are installed, and rejected, together.
                    return self.install_accepted(passing, policy, composed, actions);
                }
                // Each composes alone, but not with the others.
                for acceptance in &passing {
                    self.refuse_reported(&acceptance.config, acceptance.declared, rejection.clone(), actions);
                }
            } else {
                for acceptance in &acceptances {
                    self.refuse_reported(&acceptance.config, acceptance.declared, rejection.clone(), actions);
                }
            }
            return false;
        }
        self.install_accepted(acceptances, policy, composed, actions)
    }

    /// Makes an accepted artifact wait for an output its base records: its config is acknowledged `pending` with the
    /// reason, and it replaces any artifact of its scope that waited before it. Its revision counts as seen.
    fn wait_for_output(&mut self, acceptance: Acceptance, reason: String) {
        let scope = acceptance.artifact.scope.clone();
        self.supersede_waiting(&scope);
        self.track(
            &acceptance.config,
            Some(acceptance.declared),
            ApplyState::Pending,
            Some(reason.clone()),
            None,
        );
        self.set_scope(&acceptance.config.id, &scope);
        self.artifacts.waiting.insert(scope, Waiting { acceptance, reason });
    }

    /// Removes the artifact of a scope that waits for an output, if any, because a newer artifact for the scope
    /// replaces it: its config is acknowledged `rejected`, unless it delivers the newer one.
    fn supersede_waiting(&mut self, scope: &ScopeName) {
        let Some(waiting) = self.artifacts.waiting.remove(scope) else {
            return;
        };
        let config = &waiting.acceptance.config;
        let replaced = self
            .artifacts
            .configs
            .get(&config.id)
            .is_some_and(|tracked| tracked.ack.revision == Some(waiting.acceptance.declared.0));
        if replaced {
            self.refuse(config, Some(waiting.acceptance.declared), SUPERSEDED.to_string());
        }
    }

    /// Removes the artifacts that wait for an output in the given scopes, or in every scope: the channel revoked them,
    /// or no longer delivers them. Each one's config is acknowledged `rejected` with `message`.
    fn drop_waiting(&mut self, scopes: Option<&BTreeSet<ScopeName>>, message: &str) {
        let dropped = self
            .artifacts
            .waiting
            .keys()
            .filter(|scope| scopes.is_none_or(|scopes| scopes.contains(*scope)))
            .cloned()
            .collect::<Vec<_>>();
        for scope in dropped {
            let Some(waiting) = self.artifacts.waiting.remove(&scope) else {
                continue;
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
    }

    /// Composes again every artifact that waits for an output, after a lowering: one that now composes proceeds as a
    /// new installation, one that still waits keeps waiting, and one that now fails otherwise is rejected.
    fn compose_waiting(&mut self, composer: &dyn Composer, actions: &mut Vec<Action>) {
        if self.artifacts.waiting.is_empty() {
            return;
        }
        let waiting = std::mem::take(&mut self.artifacts.waiting);
        let mut ready = Vec::new();
        for (scope, waiting) in waiting {
            let alone = with_accepted(&self.artifacts.policy, std::slice::from_ref(&waiting.acceptance));
            match compose_policy(composer, &alone, &self.artifacts.policy) {
                Ok(_) => ready.push(waiting.acceptance),
                Err(error) => match pending_output(&error) {
                    Some(output) => {
                        let reason = waiting_reason(output);
                        if reason != waiting.reason {
                            let acceptance = waiting.acceptance.clone();
                            self.track(
                                &acceptance.config,
                                Some(acceptance.declared),
                                ApplyState::Pending,
                                Some(reason.clone()),
                                None,
                            );
                        }
                        self.artifacts.waiting.insert(
                            scope,
                            Waiting {
                                acceptance: waiting.acceptance,
                                reason,
                            },
                        );
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
        self.accept(ready, composer, actions);
    }

    /// Makes accepted artifacts, composed with the policy in effect for the other scopes into `policy`, the policy in
    /// effect, and installs their composition, returning `true` if there was one to install.
    fn install_accepted(
        &mut self, acceptances: Vec<Acceptance>, policy: Policy, composed: Result<Composed, ComposeError>,
        actions: &mut Vec<Action>,
    ) -> bool {
        let Ok(composed) = composed else { return false };
        for acceptance in acceptances {
            // A newer artifact for a scope replaces the one that waited for an output there.
            self.supersede_waiting(&acceptance.artifact.scope);
            self.follow(&acceptance.config, acceptance.declared, acceptance.artifact);
        }
        // The artifacts are the policy in effect for their scopes from now on, unless they are rejected: settling then
        // makes the policy what the schedule holds again. A re-composition failure of the artifacts running stays
        // reported until these replace them.
        self.artifacts.policy = policy.clone();
        self.install(&policy, composed, Recomposition::No, actions);
        true
    }

    /// Checks a composed policy as its preparation would check it, without preparing it: see [`InstallerCore::check`].
    fn check_composed(&self, composed: &Composed, policy: &Policy) -> Result<(), Rejection> {
        self.check(Candidate {
            plan: Arc::new(composed.plan.clone()),
            report: composed.report.clone(),
            origin: origin_of(policy),
        })
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

    /// Handles the one config of a scope that was read: acknowledges it as a delivery of the policy in effect, refuses
    /// it, or returns it accepted as a new artifact for its scope, for [`accept`][Self::accept] to compose with the
    /// policy in effect for the other scopes and install.
    ///
    /// The config is ordered against `mark`, the high-water mark when its snapshot arrived.
    fn receive(&mut self, delivery: Delivery<'_>, mark: Option<u64>, actions: &mut Vec<Action>) -> Option<Acceptance> {
        let declared = delivery.declared();
        let Delivery {
            config,
            revision,
            hash: artifact_hash,
            approved_against,
            received,
        } = delivery;
        let scope = received.artifact.scope.clone();

        // The policy in effect for the scope, delivered again under its own revision, or under a newer revision,
        // changes nothing. Under its own revision it is the scope's current state whatever the mark, which another
        // scope's newer artifact may have moved since; under any other revision at or below the mark it is stale.
        let newer = mark.is_none_or(|high_water| revision > high_water);
        if let Some(policy) = self
            .artifacts
            .policy
            .get(&scope)
            .cloned()
            .filter(|policy| policy.hash == artifact_hash && (newer || policy.revision == revision))
        {
            self.see_revision(revision);
            // The scope's one config delivers the policy in effect: an artifact that waited there is no longer
            // delivered.
            self.supersede_waiting(&scope);
            let accepted = self.reapprove(&policy, config, revision, approved_against, received);
            self.follow(config, declared, accepted);
            return None;
        }
        // The artifact that waits for an output there, delivered again by its config under its own revision, such as
        // under a newer config version, keeps waiting.
        if let Some(waiting) = self.artifacts.waiting.get_mut(&scope).filter(|waiting| {
            waiting.acceptance.config.id == config.id && waiting.acceptance.declared == (revision, artifact_hash)
        }) {
            waiting.acceptance.config = config.into();
            let reason = waiting.reason.clone();
            self.track(config, Some(declared), ApplyState::Pending, Some(reason), None);
            self.set_scope(&config.id, &scope);
            return None;
        }
        if let Some(high_water) = mark.filter(|high_water| revision <= *high_water) {
            self.refuse_reported(config, declared, Rejection::Stale { revision, high_water }, actions);
            return None;
        }
        self.see_revision(revision);
        // A newer artifact for the scope replaces the one that waited for an output there, whatever becomes of it:
        // installed, waiting itself, or rejected, as one that cannot be composed is.
        self.supersede_waiting(&scope);

        let composed_against = self.active.as_ref().map(|active| active.plan_hash);
        let artifact = Arc::new(Accepted {
            config: config.id.clone(),
            scope,
            revision,
            hash: artifact_hash,
            artifact: received,
            approved_against,
            composed_against,
            epoch: self.artifacts.revocations,
            upgraded: Mutex::default(),
        });
        Some(Acceptance {
            config: config.into(),
            declared,
            artifact,
        })
    }

    /// Makes a delivery of the policy in effect for its scope, its own redelivery or an approval of the same document
    /// under a newer revision or in another config, the policy in effect for the scope, and returns it.
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
            scope: policy.scope.clone(),
            revision,
            hash: policy.hash,
            artifact,
            approved_against,
            composed_against,
            epoch: self.artifacts.revocations,
            // The same document: what its compositions accepted holds for it.
            upgraded: Mutex::new(policy.upgraded.lock().unwrap_or_else(PoisonError::into_inner).clone()),
        });
        let stale_review = approved_against.is_some_and(|approved| composed_against != Some(approved));
        for source in self.artifacts.sources.values_mut() {
            for (scope, served) in source.artifacts.iter_mut() {
                if Arc::ptr_eq(served, policy) {
                    *served = Arc::clone(&accepted);
                    if stale_review {
                        source.stale_review.insert(scope.clone());
                    } else {
                        source.stale_review.remove(scope);
                    }
                }
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
            for served in failed.values_mut() {
                if Arc::ptr_eq(served, policy) {
                    *served = Arc::clone(&accepted);
                }
            }
        }
        self.artifacts
            .policy
            .insert(policy.scope.clone(), Arc::clone(&accepted));
        accepted
    }

    /// Records that an authentic delivery saw a revision: the high-water mark moves to it if it is higher.
    fn see_revision(&mut self, revision: u64) {
        if self.high_water.is_none_or(|high_water| revision > high_water) {
            self.high_water = Some(revision);
        }
    }

    /// Proposes a composed policy, returning its candidate's identifier.
    ///
    /// A new acceptance cancels a preparation in flight; a re-composition waits for it.
    fn install(
        &mut self, policy: &Policy, composed: Composed, recomposition: Recomposition, actions: &mut Vec<Action>,
    ) -> CandidateId {
        let Composed {
            plan,
            report,
            stale_base,
            stale_review,
            scopes,
            ..
        } = composed;
        // A composer that composes one artifact may report its flags only once, for every scope.
        let flagged = |flag: fn(&crate::compose::ScopeComposition) -> bool, all: bool| {
            policy
                .keys()
                .filter(|scope| scopes.get(*scope).map_or(all, flag))
                .cloned()
                .collect::<BTreeSet<_>>()
        };
        let id = self.candidate_id();
        let (arrival, retried) = match recomposition {
            Recomposition::No => (Arrival::Cancel, false),
            Recomposition::Of { running } => (Arrival::Queue, running),
        };
        let notes = scopes
            .iter()
            .filter(|(scope, _)| policy.contains_key(*scope))
            .filter_map(|(scope, composition)| Some((scope.clone(), composition.notes()?)))
            .collect();
        let source = Source {
            artifacts: policy.clone(),
            stale_base: flagged(|scope| scope.stale_base, stale_base),
            stale_review: flagged(|scope| scope.stale_review, stale_review),
            retried,
            attributed: false,
            notes,
        };
        self.artifacts.sources.insert(id, source);
        let origin = origin_of(policy);
        let candidate = Candidate {
            plan: Arc::new(plan),
            report,
            origin,
        };
        if self.submit(id, candidate, arrival, actions) {
            // The candidate equals what the schedule will hold: the generation that holds it serves the policy.
            if let Some(target) = self.target_candidate() {
                self.serve_unchanged(id, origin, target);
            }
        }
        id
    }

    /// Makes the candidate `target`, whose plan a candidate reported unchanged equals, serve what that candidate was
    /// created for: the artifacts it serves, or, for a default program while no artifact is in effect, the default
    /// program, even if `target` was created for a revoked artifact that composed to the same plan; and the resources
    /// revision it was lowered with.
    pub(super) fn serve_unchanged(&mut self, candidate: CandidateId, origin: Origin, target: CandidateId) {
        // The resources revision the candidate was lowered with is installed by the generation that holds its plan.
        self.declared.serve(target, candidate);
        let Some(source) = self.artifacts.sources.get(&candidate).cloned() else {
            if matches!(origin, Origin::DefaultProgram { .. }) && self.artifacts.policy.is_empty() {
                self.artifacts.sources.remove(&target);
            }
            return;
        };
        match self.artifacts.sources.get_mut(&target) {
            // The same documents: the generation that holds their composition serves their latest acceptance, such as
            // an approval under a newer revision, or a delivery after a revocation.
            Some(existing) if existing.documents() == source.documents() => {
                existing.artifacts = source.artifacts;
                existing.stale_base = source.stale_base;
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

    /// Revokes every artifact accepted so far: the default program runs.
    fn revoke_all(&mut self, actions: &mut Vec<Action>) {
        let revocation = self.revoke_scopes(None, actions);
        self.ask_after_revocation(revocation.changed() || revocation.artifact_running, actions);
    }

    /// Asks for the default program after a revocation that leaves no artifact in effect, if the revocation changed
    /// anything and nothing in flight installs the default program already.
    fn ask_after_revocation(&mut self, changed: bool, actions: &mut Vec<Action>) {
        let cancelling = self
            .cut
            .as_ref()
            .is_some_and(|cut| cut.after_cancel == Some(AfterCancel::InstallDefaultProgram));
        if changed && !self.default_in_flight() && !cancelling {
            actions.push(Action::InstallDefaultProgram);
        }
    }

    /// Revokes the artifacts of the given scopes, or of every scope, accepted so far: they leave effect, their
    /// installations in flight are dropped, a generation serving them is no longer kept to roll back to, and a cut
    /// installing them is aborted or cancelled, or, once irrevocable, waits for its finalization.
    fn revoke_scopes(&mut self, scopes: Option<&BTreeSet<ScopeName>>, actions: &mut Vec<Action>) -> Revocation {
        self.drop_waiting(scopes, REVOKED);
        self.artifacts.revocations += 1;
        let at = self.artifacts.revocations;
        let revoked = match scopes {
            None => {
                self.artifacts.revoked_all_at = at;
                std::mem::take(&mut self.artifacts.policy)
            }
            Some(scopes) => {
                for scope in scopes {
                    self.artifacts.revoked_at.insert(scope.clone(), at);
                }
                let (revoked, kept) = std::mem::take(&mut self.artifacts.policy)
                    .into_iter()
                    .partition(|(scope, _)| scopes.contains(scope));
                self.artifacts.policy = kept;
                revoked
            }
        };
        let full = scopes.is_none();
        let revoked_artifacts = |core: &Self, source: &Source| source.artifacts.values().any(|a| core.is_revoked(a));
        let attributions = self
            .artifacts
            .sources
            .iter()
            .filter(|(_, source)| source.attributed && revoked_artifacts(self, source))
            .map(|(candidate, _)| *candidate)
            .collect::<Vec<_>>();
        for candidate in attributions {
            self.artifacts.sources.remove(&candidate);
        }
        let serving_revoked = self
            .artifacts
            .sources
            .iter()
            .filter(|(_, source)| revoked_artifacts(self, source))
            .map(|(candidate, _)| *candidate)
            .collect::<BTreeSet<_>>();
        // A full revocation drops every artifact in flight, whether or not it came from the channel.
        let drops = |candidate: CandidateId, origin: &Origin| {
            (full && origin.can_roll_back()) || serving_revoked.contains(&candidate)
        };
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
        if self
            .rollback
            .as_ref()
            .is_some_and(|rollback| full || serving_revoked.contains(&rollback.from))
        {
            self.rollback = None;
        }
        self.try_start(actions);
        Revocation {
            revoked: !revoked.is_empty(),
            dropped,
            artifact_running: self
                .active
                .as_ref()
                .is_some_and(|active| active.origin.can_roll_back() && drops(active.candidate, &active.origin)),
        }
    }

    /// Rejects every config of a scope when a snapshot holds more than one for it. Their revisions are not seen.
    fn reject_scope(&mut self, scope: &ScopeName, configs: &[&ArtifactConfig]) {
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
            "the policy snapshot holds {} configs ({}) for scope `{}`, but one artifact replaces a scope: every config \
             for it is rejected until the snapshot holds one",
            configs.len(),
            ids.join(", "),
            scope
        );
        for config in configs {
            self.refuse(*config, None, message.clone());
            if let Some(tracked) = self.artifacts.configs.get_mut(&config.id) {
                tracked.scope = Some(scope.clone());
                tracked.reread = true;
            }
        }
        // What runs for the scope keeps running, but nothing new is installed for it: the artifact that waited for an
        // output there is no longer delivered on its own.
        self.drop_waiting(Some(&BTreeSet::from([scope.clone()])), &message);
    }

    /// Takes the artifacts of a failing active generation out of effect, before the failure is handled: their
    /// compositions in flight, under any revision that approved the same documents, are dropped rather than prepared
    /// again against the generation rolled back to, and no other generation serves them any more. A delivery after the
    /// failure is accepted anew.
    ///
    /// A cut in flight that serves one of them is dropped as a preparation is: aborted if it is not armed yet, and, if
    /// it is armed, rejected rather than prepared again once the cancellation the failure asks for succeeds. One that
    /// is irrevocable runs already.
    ///
    /// A generation composing several artifacts fails as a whole: each of its artifacts is taken out of effect.
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
        let Some(artifacts) = self
            .artifacts
            .sources
            .get(&failing)
            .map(|source| source.artifacts.clone())
        else {
            return;
        };
        let documents = artifacts
            .values()
            .map(|artifact| artifact.hash)
            .collect::<BTreeSet<_>>();
        let serves_document = |source: &Source| source.artifacts.values().any(|a| documents.contains(&a.hash));
        // The failure is the documents', whichever revisions approved them: candidates of the same artifacts accepted
        // under another revision are dropped too. The failing generation keeps its own source, even an attribution, so
        // that its failure is acknowledged.
        self.artifacts
            .sources
            .retain(|candidate, source| *candidate == failing || !(source.attributed && serves_document(source)));
        let message = format!(
            "{} failed under supervision: {}",
            describe_revisions(&artifacts),
            reason
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

    /// Returns `true` if two candidates serve the same artifact documents, scope by scope, whichever revisions approved
    /// them.
    pub(super) fn serve_same_artifact(&self, first: CandidateId, second: CandidateId) -> bool {
        match (self.artifacts.sources.get(&first), self.artifacts.sources.get(&second)) {
            (Some(first), Some(second)) => first.documents() == second.documents(),
            _ => false,
        }
    }

    /// Returns `true` if a candidate serves an artifact that a revocation revoked.
    pub(super) fn serves_revoked(&self, candidate: CandidateId) -> bool {
        self.artifacts
            .sources
            .get(&candidate)
            .is_some_and(|source| source.artifacts.values().any(|artifact| self.is_revoked(artifact)))
    }

    /// Returns `true` if a revocation of every scope, or of the artifact's own, came after the artifact was accepted.
    fn is_revoked(&self, artifact: &Accepted) -> bool {
        let scope = self.artifacts.revoked_at.get(&artifact.scope).copied().unwrap_or(0);
        artifact.epoch < self.artifacts.revoked_all_at.max(scope)
    }

    /// Returns `true` if a candidate's failed preparation is tried again although it is an artifact's: it composes
    /// again artifacts that were running when the configuration changed, or it is a rollback.
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
    /// policy's installation ended, forgets a re-composition failure whose artifacts neither are in effect nor run,
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
        if !self.artifacts.policy.is_empty() {
            let serving = self.serving();
            if !same_policy(&serving, &self.artifacts.policy) {
                self.artifacts.policy = serving;
                reverted = true;
            }
        }
        if let Some((failed, _)) = &self.artifacts.policy_error {
            let in_effect = failed
                .values()
                .any(|failed| self.artifacts.policy.values().any(|policy| Arc::ptr_eq(policy, failed)));
            let runs = self
                .active_source()
                .is_some_and(|source| failed.values().any(|failed| source.serves(failed)));
            if !in_effect && !runs {
                self.artifacts.policy_error = None;
            }
        }
        if let Settle::AfterFailure { rolled_back } = event {
            self.ask_for_default_program(reverted || rolled_back, &mut actions);
        }

        self.refresh_acknowledgments();
        self.prune_configs();
        self.refresh_resources_acknowledgments();
        actions.extend(self.take_released_credits());
        actions
    }

    /// Records how the installation of the artifacts a report concerns ended, if the report ends it.
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
        let artifacts = source.artifacts.values().cloned().collect::<Vec<_>>();
        let followers = self
            .artifacts
            .configs
            .values_mut()
            .flat_map(|tracked| [tracked.latest.as_mut(), tracked.earlier.as_mut()])
            .flatten()
            .filter(|followed| {
                artifacts
                    .iter()
                    .any(|artifact| Arc::ptr_eq(&followed.artifact, artifact))
            });
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

    /// Makes a rollback candidate serve the artifacts of the candidate whose plan it installs again, if that one serves
    /// any. Its preparation is tried again after a failure, so that the rollback installs eventually.
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
    /// or the policy's installation ended without running), it predates the latest configuration change.
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
            // degraded, with its emits to an output that was removed or rebound dropped, and what to rebase for.
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
        let (scope, _) = source
            .artifacts
            .iter()
            .find(|(_, served)| Arc::ptr_eq(served, artifact))?;
        source.notes.get(scope).cloned()
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

    /// Returns the artifacts the schedule will hold, without the revoked ones: a revoked artifact's generation runs
    /// only until it is replaced, and the default program's region is in effect for its scope meanwhile.
    fn serving(&self) -> Policy {
        self.target_candidate()
            .and_then(|candidate| self.artifacts.sources.get(&candidate))
            .map(|source| {
                source
                    .artifacts
                    .iter()
                    .filter(|(_, artifact)| !self.is_revoked(artifact))
                    .map(|(scope, artifact)| (scope.clone(), Arc::clone(artifact)))
                    .collect()
            })
            .unwrap_or_default()
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

    /// Records the scope of a config's latest delivery.
    fn set_scope(&mut self, config: &ArtifactConfigId, scope: &ScopeName) {
        if let Some(tracked) = self.artifacts.configs.get_mut(config) {
            tracked.scope = Some(scope.clone());
        }
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
        let scope = follows.as_ref().map(|artifact| artifact.scope.clone());
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
                scope,
                reread: false,
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
    /// Whether an artifact was in effect for a revoked scope.
    revoked: bool,

    /// Whether an installation in flight was dropped.
    dropped: bool,

    /// Whether the active generation serves a revoked artifact.
    artifact_running: bool,
}

impl Revocation {
    /// Returns `true` if the revocation took an artifact out of effect, or dropped an installation in flight.
    fn changed(&self) -> bool {
        self.revoked || self.dropped
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

/// Returns the policy `base` with the accepted artifacts in effect for their scopes.
fn with_accepted(base: &Policy, acceptances: &[Acceptance]) -> Policy {
    let mut policy = base.clone();
    for acceptance in acceptances {
        policy.insert(acceptance.artifact.scope.clone(), Arc::clone(&acceptance.artifact));
    }
    policy
}

/// Composes a policy through the host's composer, every artifact with its own approval: an artifact of the policy in
/// effect, `in_effect`, composes as the one in effect for its scope, any other as a new one.
///
/// Every artifact a successful composition accepted version 1 fingerprints for keeps them for its later compositions.
fn compose_policy(composer: &dyn Composer, policy: &Policy, in_effect: &Policy) -> Result<Composed, ComposeError> {
    let approvals = policy
        .iter()
        .map(|(scope, artifact)| {
            let in_effect = in_effect
                .get(scope)
                .is_some_and(|effective| Arc::ptr_eq(effective, artifact));
            artifact.approval(in_effect)
        })
        .collect::<Vec<_>>();
    let artifacts = policy
        .values()
        .zip(&approvals)
        .map(|(artifact, approval)| (&artifact.artifact, approval))
        .collect::<Vec<_>>();
    let composed = composer.compose_all(&artifacts)?;
    for (scope, artifact) in policy {
        if let Some(composition) = composed.scopes.get(scope) {
            artifact.upgrade(composition);
        }
    }
    Ok(composed)
}

/// Returns the bases of a host that reports only the policy scope's.
fn policy_scope_base(default_program_hash: Option<PlanHash>) -> BTreeMap<ScopeName, PlanHash> {
    ScopeName::new(POLICY_SCOPE)
        .ok()
        .zip(default_program_hash)
        .into_iter()
        .collect()
}

#[cfg(test)]
impl InstallerCore {
    /// Returns the policy in effect: of the artifacts the core runs or installs instead of the default program's
    /// regions, the one with the newest revision, if any.
    ///
    /// While one is in effect, [`on_default_program`][Self::on_default_program] composes the policy again rather than
    /// proposing the default program. The status summary reports the artifacts the active generation serves instead.
    pub(in crate::install) fn policy_in_effect(&self) -> Option<ActiveArtifact> {
        self.artifacts
            .policy
            .values()
            .max_by_key(|artifact| artifact.revision)
            .map(|artifact| artifact.active())
    }

    /// Returns the revision and hash of the artifact with the newest revision a generation the core remembers serves,
    /// if it serves one.
    pub(in crate::install) fn served_by(&self, generation: GenerationId) -> Option<(u64, PlanHash)> {
        let candidate = self.records.get(&generation)?.candidate;
        self.artifacts
            .sources
            .get(&candidate)
            .and_then(|source| source.artifacts.values().max_by_key(|artifact| artifact.revision))
            .map(|artifact| (artifact.revision, artifact.hash))
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
