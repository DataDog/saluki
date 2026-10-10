//! The installer core's artifact paths, driven by snapshots as the channel delivers them, with the acknowledgments and
//! status expected after each step written by hand.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    time::Duration,
};

use proptest::prelude::*;
use saluki_core::execution::{GenerationId, GenerationRef};
use serde_json::{json, Value};

use super::{
    tests::{boot_plan, candidate, describe, described, edited, node_mut, Harness},
    *,
};
use crate::{
    canonical::{PlanHash, ResolvedValues},
    channel::{ApplyState, ArtifactConfig, ArtifactConfigId, ArtifactProduct, ArtifactSnapshot, ConfigAcknowledgment},
    compile::SlotId,
    compose::{Approval, ComposeError, Composed, Composer, ScopeComposition},
    ir::{OutputId, Plan, ReceivedArtifact},
    operators::OperatorRegistry,
    runtime::PanicBudget,
    testing::default_program_catalog,
    validate::{MemoryCatalog, Validator},
};

/// The listener filterlist of the boot default program.
const BOOT_LISTENER: &str = "tenant.raw.blocked";

/// The post-aggregation filter of the boot default program.
pub(super) const BOOT_POST_AGGREGATION: &str = "tenant.checkout.latency.max";

/// Why the test host fails a generation under supervision.
const PANICS: &str = "3 panics in 60 seconds";

/// The default program with its listener filterlist and post-aggregation filter set to one value each. In these tests,
/// an artifact's policy is its listener filterlist, and configuration changes set the post-aggregation filter.
pub(super) fn program(listener: &str, post_aggregation: &str) -> Plan {
    edited(|value| {
        node_mut(value, "dsd_prefix_filter_blocklist")["params"]["predicate"]["intake_name"]["matches"]["values"]
            ["resolved"] = json!([listener]);
        node_mut(value, "dsd_post_agg_filter")["params"]["predicate"]["all"][1]["intake_name"]["matches"]["values"]
            ["resolved"] = json!([post_aggregation]);
    })
}

/// A policy artifact document that carries `marker`, which the test composer reads.
fn artifact_document(marker: &str) -> Value {
    json!({
        "ir_version": 1,
        "scope": "post_enrich",
        "base": {"program_hash": format!("sha256:{}", "0".repeat(64))},
        "nodes": [{"id": "metrics_policy", "op": "split", "params": {"predicate": {"name": {"prefix": marker}}}}],
        "edges": [{"from": "input.metrics", "to": "metrics_policy.in"}]
    })
}

fn artifact_hash(marker: &str) -> PlanHash {
    ReceivedArtifact::from_value(artifact_document(marker)).unwrap().hash
}

fn marker_of(artifact: &ReceivedArtifact) -> String {
    let document = serde_json::to_value(&artifact.artifact).unwrap();
    document
        .pointer("/nodes/0/params/predicate/name/prefix")
        .and_then(Value::as_str)
        .unwrap()
        .to_string()
}

/// Composes the test artifacts into the default program the host last lowered.
///
/// An artifact composes to the default program with its marker as the listener filterlist, except for these markers:
/// `unmodified` composes to the default program itself, as an unmodified export does; `invalid` composes to a plan
/// that does not validate; `uncomposable` cannot be composed; and `fragile` cannot be composed once the configuration
/// changed, as an artifact whose output was removed.
pub(super) struct TestComposer {
    /// The post-aggregation filter of the default program the host last lowered.
    pub(super) configuration: Cell<&'static str>,

    /// Whether the default program changed structurally since the artifacts' export.
    structural: Cell<bool>,
}

impl TestComposer {
    pub(super) fn new() -> Self {
        Self {
            configuration: Cell::new(BOOT_POST_AGGREGATION),
            structural: Cell::new(false),
        }
    }
}

impl Composer for TestComposer {
    fn compose_all(&self, artifacts: &[(&ReceivedArtifact, &Approval)]) -> Result<Composed, ComposeError> {
        let [(artifact, approval)] = artifacts else {
            return Err(ComposeError::Malformed {
                message: format!("the test composes one artifact, not {}", artifacts.len()),
            });
        };
        let configuration = self.configuration.get();
        let marker = marker_of(artifact);
        let (plan, catalog) = match marker.as_str() {
            "uncomposable" => {
                return Err(ComposeError::Malformed {
                    message: "the test refuses to compose it".to_string(),
                })
            }
            "fragile" if configuration != BOOT_POST_AGGREGATION => {
                return Err(ComposeError::RemovedOutput {
                    output: OutputId::new("af_metrics_encode").unwrap(),
                    declared: false,
                })
            }
            "unmodified" => (program(BOOT_LISTENER, configuration), default_program_catalog()),
            "invalid" => (program("invalid", configuration), MemoryCatalog::new()),
            listener => (program(listener, configuration), default_program_catalog()),
        };
        let report = Validator::new(&OperatorRegistry::native(), &catalog).validate(&plan);
        let stale_base = self.structural.get();
        let stale_review = approval
            .approved_against
            .is_some_and(|approved| Some(approved) != approval.active_plan_hash);
        let scope = ScopeComposition {
            correspondence: Correspondence::new(),
            stale_base,
            stale_review,
        };
        Ok(Composed {
            plan,
            report,
            correspondence: Correspondence::new(),
            stale_base,
            stale_review,
            scopes: BTreeMap::from([(artifact.artifact.scope.clone(), scope)]),
        })
    }
}

/// How a test config is damaged in transit or by its publisher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Damage {
    /// Its `sha256` is not the hash of its payload.
    TransportHash,

    /// Its payload is JSON, but not an envelope.
    NotAnEnvelope,

    /// Its envelope declares another artifact's hash.
    ArtifactHash,
}

/// A policy config, as a publisher builds it.
#[derive(Clone, Debug)]
pub(super) struct Policy {
    id: String,
    version: u64,
    revision: u64,
    marker: &'static str,
    approved_against: Option<PlanHash>,
    damage: Option<Damage>,
}

/// The `demo` config at version and revision `revision`, with the artifact `marker`.
pub(super) fn policy(revision: u64, marker: &'static str) -> Policy {
    Policy {
        id: "demo".to_string(),
        version: revision,
        revision,
        marker,
        approved_against: None,
        damage: None,
    }
}

impl Policy {
    fn id(self, id: impl Into<String>) -> Self {
        Self { id: id.into(), ..self }
    }

    fn version(self, version: u64) -> Self {
        Self { version, ..self }
    }

    fn approved_against(self, hash: PlanHash) -> Self {
        Self {
            approved_against: Some(hash),
            ..self
        }
    }

    fn damaged(self, damage: Damage) -> Self {
        Self {
            damage: Some(damage),
            ..self
        }
    }

    fn config(&self) -> ArtifactConfig {
        let declared = if self.damage == Some(Damage::ArtifactHash) {
            artifact_hash("another artifact")
        } else {
            artifact_hash(self.marker)
        };
        let mut envelope = json!({
            "revision": self.revision,
            "artifact_hash": declared,
            "artifact": artifact_document(self.marker),
        });
        if let Some(hash) = self.approved_against {
            envelope["approved_against"] = json!({ "active_plan_hash": hash });
        }
        if self.damage == Some(Damage::NotAnEnvelope) {
            envelope = json!({ "revision": self.revision });
        }
        let payload = envelope.to_string();
        let sha256 = if self.damage == Some(Damage::TransportHash) {
            ArtifactConfig::transport_hash("other bytes")
        } else {
            ArtifactConfig::transport_hash(&payload)
        };
        ArtifactConfig {
            id: ArtifactConfigId::new(self.id.as_str()),
            version: self.version,
            sha256,
            payload,
        }
    }
}

pub(super) fn snapshot(policies: &[Policy]) -> ArtifactSnapshot {
    ArtifactSnapshot::new(ArtifactProduct::Policy, policies.iter().map(Policy::config).collect())
}

fn boot_hash() -> PlanHash {
    boot_plan().hash(ResolvedValues::Include).unwrap()
}

/// Returns the hash of the plan an artifact `marker` composes to, before any configuration change.
fn plan_hash(marker: &str) -> PlanHash {
    program(marker, BOOT_POST_AGGREGATION)
        .hash(ResolvedValues::Include)
        .unwrap()
}

/// One thing that happens to the host.
#[derive(Clone, Debug)]
enum Step {
    /// The channel delivers a snapshot of the policy product with these configs.
    Deliver(Vec<Policy>),

    /// The channel delivers an empty snapshot.
    Revoke,

    /// The channel's latest snapshot is handled again, as a restarted installer worker does.
    Redeliver,

    /// The channel delivers again the snapshot it delivered this many deliveries before the latest one (modulo the
    /// number of deliveries), as a publisher that reverts to an earlier snapshot does.
    RedeliverEarlier(usize),

    /// The configuration changes the post-aggregation filter.
    Configure(&'static str),

    /// The configuration changes the default program structurally.
    Restructure,

    /// Every region acknowledges the preparation.
    Acknowledge,

    /// A region fails to prepare.
    RefusePrepare,

    /// The preparation times out.
    TimeOut,

    /// Supervision fails the active generation.
    Panic,

    /// Supervision fails a generation.
    PanicIn(u64),

    /// The retry delay passes.
    Retry,

    /// Batches stamped with the active generation stay in flight.
    Hold,

    /// Every batch held finishes.
    Release,

    /// The process restarts: a new core boots with the default program, and nothing else survives but the channel's
    /// snapshot and the configuration.
    Restart,
}

/// The host around the core: it performs the core's actions as the ADP installer worker does, with regions that
/// acknowledge on request and drop what they are told to, a schedule that holds the generation committed last, and a
/// composer over the configuration.
struct Host {
    harness: Harness,
    composer: TestComposer,
    /// The channel's latest snapshot.
    channel: Option<ArtifactSnapshot>,
    /// Every snapshot the channel delivered, oldest first.
    history: Vec<Vec<Policy>>,
    /// The snapshot the core handled last.
    handled: Option<ArtifactSnapshot>,
    schedule: Option<GenerationRef>,
    held: Vec<GenerationRef>,
    changes: u64,
}

impl Host {
    fn new() -> Self {
        let mut harness = Harness::new(InstallerConfig::default());
        let schedule = harness.boot.take().map(|boot| boot.generation);
        Self {
            harness,
            composer: TestComposer::new(),
            channel: None,
            history: Vec::new(),
            handled: None,
            schedule,
            held: Vec::new(),
            changes: 0,
        }
    }

    /// Returns the configs a step delivers, if it delivers a snapshot.
    fn delivery(&self, step: &Step) -> Option<Vec<Policy>> {
        match step {
            Step::Deliver(policies) => Some(policies.clone()),
            Step::Revoke => Some(Vec::new()),
            Step::Redeliver => self.history.last().cloned(),
            Step::RedeliverEarlier(back) => (!self.history.is_empty())
                .then(|| self.history[self.history.len() - 1 - back % self.history.len()].clone()),
            _ => None,
        }
    }

    fn core(&self) -> &InstallerCore {
        &self.harness.core
    }

    fn preparing(&self) -> GenerationId {
        self.core()
            .status()
            .preparing
            .expect("a generation is being prepared")
            .id
    }

    /// Performs a step, returning a description of every action performed.
    fn step(&mut self, step: &Step) -> Vec<String> {
        let actions = match step {
            Step::Deliver(_) | Step::Revoke | Step::RedeliverEarlier(_) => match self.delivery(step) {
                Some(policies) => self.deliver(policies),
                None => Vec::new(),
            },
            Step::Redeliver => match self.channel.clone() {
                Some(snapshot) => {
                    let actions = self.harness.core.on_snapshot(&snapshot, &self.composer);
                    self.handled = Some(snapshot);
                    actions
                }
                None => Vec::new(),
            },
            Step::Configure(post_aggregation) => {
                self.composer.configuration.set(*post_aggregation);
                self.lower()
            }
            Step::Restructure => {
                self.composer.structural.set(true);
                self.lower()
            }
            Step::Acknowledge => {
                let generation = self.preparing();
                let mut actions = Vec::new();
                for slot in 0..self.core().frame().slots().len() as u16 {
                    actions.extend(
                        self.harness
                            .core
                            .prepare_acknowledged(generation, SlotId::new(slot), Ok(())),
                    );
                }
                actions
            }
            Step::RefusePrepare => {
                let generation = self.preparing();
                self.harness.core.prepare_acknowledged(
                    generation,
                    SlotId::new(0),
                    Err("the region's mailbox is closed".to_string()),
                )
            }
            Step::TimeOut => {
                let generation = self.preparing();
                self.harness.core.prepare_timed_out(generation)
            }
            Step::Panic => {
                let active = self.core().status().active.expect("a generation is active").id;
                self.harness.core.generation_failed(active, PANICS)
            }
            Step::PanicIn(generation) => self
                .harness
                .core
                .generation_failed(GenerationId::new(*generation), PANICS),
            Step::Retry => self.harness.core.retry(),
            Step::Hold => {
                self.held.extend(self.schedule.clone());
                Vec::new()
            }
            Step::Release => {
                self.held.clear();
                Vec::new()
            }
            Step::Restart => {
                let channel = self.channel.take();
                let history = std::mem::take(&mut self.history);
                let composer = std::mem::replace(&mut self.composer, TestComposer::new());
                *self = Self::new();
                self.channel = channel;
                self.history = history;
                self.composer = composer;
                Vec::new()
            }
        };
        self.perform(actions)
    }

    /// The channel delivers a snapshot of these configs.
    fn deliver(&mut self, policies: Vec<Policy>) -> Vec<Action> {
        let snapshot = snapshot(&policies);
        let actions = self.harness.core.on_snapshot(&snapshot, &self.composer);
        self.channel = Some(snapshot.clone());
        self.handled = Some(snapshot);
        self.history.push(policies);
        actions
    }

    /// Lowers the configuration into the default program and hands it to the core.
    fn lower(&mut self) -> Vec<Action> {
        self.changes += 1;
        let plan = program(BOOT_LISTENER, self.composer.configuration.get());
        let origin = Origin::DefaultProgram {
            config_change: self.changes,
        };
        self.harness
            .core
            .on_default_program(candidate(plan, origin), &self.composer)
            .1
    }

    /// Performs actions, and those that performing them and delivering the retirements they cause leads to.
    fn perform(&mut self, mut actions: Vec<Action>) -> Vec<String> {
        let mut performed = Vec::new();
        loop {
            let mut next = Vec::new();
            for action in actions {
                performed.push(describe(&action));
                match action {
                    Action::InstallDefaultProgram => next.extend(self.lower()),
                    Action::Commit { generation } => self.schedule = Some(generation),
                    _ => {}
                }
            }
            next.extend(self.harness.deliver_retirements());
            if next.is_empty() {
                return performed;
            }
            actions = next;
        }
    }

    /// Describes the host's state in one line: the active generation (`!` when it failed) and the artifact it serves,
    /// the high-water mark, what is in flight and kept, the status flags, and every config's acknowledgment, with the
    /// kind of its error.
    fn summary(&self) -> String {
        let status = self.core().status();
        let summary = self.core().status_summary(None, None);
        let active = status.active.as_ref().expect("a generation is active");
        let mut line = format!("g{}", active.id);
        if active.state == GenerationState::Failed {
            line.push('!');
        }
        match &summary.active_artifact {
            Some(artifact) => write!(line, " {}@{}", artifact.id, artifact.revision).unwrap(),
            None => line.push_str(" default"),
        }
        match summary.revision_high_water {
            Some(high_water) => write!(line, " hw{}", high_water).unwrap(),
            None => line.push_str(" hw-"),
        }
        if let Some(preparing) = &status.preparing {
            write!(line, " preparing g{}", preparing.id).unwrap();
        }
        if status.pending.is_some() {
            line.push_str(" pending");
        }
        if let Some(kept) = &status.last_known_good {
            write!(line, " kept g{}", kept.id).unwrap();
        }
        for (flag, name) in [
            (summary.stale_base, " stale-base"),
            (summary.stale_review, " stale-review"),
            (summary.degraded.is_some(), " degraded"),
        ] {
            if flag {
                line.push_str(name);
            }
        }
        for ack in self.core().config_states() {
            let optional = |value: Option<u64>| value.map_or_else(|| "-".to_string(), |value| value.to_string());
            write!(
                line,
                " | {} v{} r{} {} g{}",
                ack.id,
                ack.version,
                optional(ack.revision),
                ack.state,
                optional(ack.generation)
            )
            .unwrap();
            if let Some(error) = &ack.error {
                write!(line, " ({})", error_kind(error)).unwrap();
            }
        }
        line
    }
}

/// Returns the kind of an acknowledgment's error, or the error itself if it is of no known kind.
fn error_kind(error: &str) -> &str {
    const KINDS: &[(&str, &str)] = &[
        ("is stale", "stale"),
        ("transport hash mismatch", "transport"),
        ("artifact hash mismatch", "hash"),
        ("not an artifact envelope", "envelope"),
        ("configs (", "configs"),
        ("cancelled the preparation", "cancelled"),
        ("revoked the policy", "revoked"),
        ("failed under supervision", "supervision"),
        ("panics", "panic"),
        ("failed to prepare", "prepare"),
        ("did not acknowledge", "timeout"),
        ("is not valid", "invalid"),
        ("test refuses", "compose"),
        ("no longer declares", "removed"),
        ("replaced it before", "superseded"),
    ];
    KINDS
        .iter()
        .find(|(needle, _)| error.contains(needle))
        .map_or(error, |(_, kind)| kind)
}

use Step::*;

/// A lifecycle case: steps, each with the summary expected after it.
struct Case {
    name: &'static str,
    steps: Vec<(Step, &'static str)>,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "an accepted artifact is pending while it is prepared, and active once it commits, keeping the \
                   generation it replaced to roll back to",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
            ],
        },
        Case {
            name: "a revision at or below the high-water mark is stale, and a newer one replaces the active artifact",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(7, "b")]),
                    "g2 demo@7 hw7 kept g1 | demo v7 r7 rejected g- (stale)",
                ),
                (
                    Deliver(vec![policy(5, "c")]),
                    "g2 demo@7 hw7 kept g1 | demo v5 r5 rejected g- (stale)",
                ),
                (
                    Deliver(vec![policy(8, "d")]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3",
                ),
                (Acknowledge, "g3 demo@8 hw8 kept g2 | demo v8 r8 active g3"),
            ],
        },
        Case {
            name: "a repeated delivery, or the artifact in effect under a new version or revision, acknowledges the \
                   current state",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Redeliver, "g1 default hw7 preparing g2 | demo v7 r7 pending g2"),
                (
                    Deliver(vec![policy(7, "a").version(8)]),
                    "g1 default hw7 preparing g2 | demo v8 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v8 r7 active g2"),
                // The approval under a newer revision becomes the artifact in effect: the status reports its revision,
                // without a generation for it.
                (
                    Deliver(vec![policy(9, "a")]),
                    "g2 demo@9 hw9 kept g1 | demo v9 r9 active g2",
                ),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@9 hw9 kept g1 | demo v8 r8 rejected g- (stale)",
                ),
            ],
        },
        Case {
            name: "a redelivery of the active artifact while a newer one is prepared is stale, and the newer one \
                   installs",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3",
                ),
                (
                    Deliver(vec![policy(7, "a").version(10)]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v10 r7 rejected g- (stale)",
                ),
                // The stale delivery does not hide what becomes of the artifact the config's earlier delivery installs.
                (Acknowledge, "g3 demo@8 hw8 kept g2 | demo v8 r8 active g3"),
            ],
        },
        Case {
            name: "a newer artifact cancels a preparation that has not committed, whichever config it arrives in",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g1 default hw8 preparing g3 | demo v8 r8 pending g3",
                ),
                (Acknowledge, "g3 demo@8 hw8 kept g1 | demo v8 r8 active g3"),
                (
                    Deliver(vec![policy(9, "c").id("other")]),
                    "g3 demo@8 hw9 preparing g4 kept g1 | demo v8 r8 active g3 | other v9 r9 pending g4",
                ),
                (
                    Deliver(vec![policy(10, "d")]),
                    "g3 demo@8 hw10 preparing g5 kept g1 | demo v10 r10 pending g5 | other v9 r9 rejected g4 \
                     (cancelled)",
                ),
                (
                    Acknowledge,
                    "g5 demo@10 hw10 kept g3 | demo v10 r10 active g5 | other v9 r9 rejected g4 (cancelled)",
                ),
            ],
        },
        Case {
            name: "an artifact whose preparation fails or times out is rejected, and its revision stays seen",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (RefusePrepare, "g1 default hw7 | demo v7 r7 rejected g2 (prepare)"),
                (
                    Deliver(vec![policy(8, "a")]),
                    "g1 default hw8 preparing g3 | demo v8 r8 pending g3",
                ),
                (TimeOut, "g1 default hw8 | demo v8 r8 rejected g3 (timeout)"),
                (
                    Deliver(vec![policy(8, "a").version(9)]),
                    "g1 default hw8 | demo v9 r8 rejected g- (stale)",
                ),
            ],
        },
        Case {
            name: "supervision failures roll back to last-known-good and then to the default program, and never move \
                   the high-water mark",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3",
                ),
                (Acknowledge, "g3 demo@8 hw8 kept g2 | demo v8 r8 active g3"),
                (Panic, "g2 demo@7 hw8 | demo v8 r8 failed g3 (panic)"),
                (Redeliver, "g2 demo@7 hw8 | demo v8 r8 failed g3 (panic)"),
                (Panic, "g2! default hw8 preparing g4 | demo v8 r8 failed g3 (panic)"),
                (Acknowledge, "g4 default hw8 | demo v8 r8 failed g3 (panic)"),
                (
                    Deliver(vec![policy(8, "b").version(9)]),
                    "g4 default hw8 | demo v9 r8 rejected g- (stale)",
                ),
                (
                    Deliver(vec![policy(9, "b")]),
                    "g4 default hw9 preparing g5 | demo v9 r9 pending g5",
                ),
            ],
        },
        Case {
            name: "an artifact in flight when the active artifact fails replaces it, and the default program does if \
                   it is rejected",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (PanicIn(1), "g2 demo@7 hw7 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g3 | demo v8 r8 pending g3",
                ),
                // The failure of the artifact running is the config's latest news, although a newer delivery of it is
                // being prepared.
                (Panic, "g2! default hw8 preparing g3 | demo v7 r7 failed g2 (panic)"),
                (
                    RefusePrepare,
                    "g2! default hw8 preparing g4 | demo v8 r8 rejected g3 (prepare)",
                ),
                (Acknowledge, "g4 default hw8 | demo v8 r8 rejected g3 (prepare)"),
            ],
        },
        Case {
            name: "a last-known-good generation that fails is no longer kept, so the next failure falls back to the \
                   default program",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3",
                ),
                (Acknowledge, "g3 demo@8 hw8 kept g2 | demo v8 r8 active g3"),
                (PanicIn(2), "g3 demo@8 hw8 | demo v8 r8 active g3"),
                (Panic, "g3! default hw8 preparing g4 | demo v8 r8 failed g3 (panic)"),
            ],
        },
        Case {
            name: "revoking installs the default program, and the revoked artifact stays active until its generation \
                   drains, then retires",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (Hold, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (Revoke, "g2 demo@7 hw7 preparing g3 kept g1 | demo v7 r7 active g2"),
                (Acknowledge, "g3 default hw7 | demo v7 r7 active g2"),
                (Release, "g3 default hw7 | demo v7 r7 retired g2"),
                (Revoke, "g3 default hw7 | demo v7 r7 retired g2"),
                (
                    Deliver(vec![policy(7, "a")]),
                    "g3 default hw7 | demo v7 r7 rejected g- (stale)",
                ),
            ],
        },
        Case {
            name: "revoking while an artifact is prepared drops the preparation, and the default program stays",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Revoke, "g1 default hw7 | demo v7 r7 rejected g2 (revoked)"),
            ],
        },
        Case {
            name: "a draining artifact generation that fails is acknowledged failed and changes nothing else",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (Hold, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (Revoke, "g2 demo@7 hw7 preparing g3 kept g1 | demo v7 r7 active g2"),
                (Acknowledge, "g3 default hw7 | demo v7 r7 active g2"),
                (PanicIn(2), "g3 default hw7 | demo v7 r7 failed g2 (panic)"),
                (Release, "g3 default hw7 | demo v7 r7 failed g2 (panic)"),
            ],
        },
        Case {
            name: "a policy snapshot with more than one config rejects all of them and changes nothing else",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b"), policy(9, "c").id("other")]),
                    "g2 demo@7 hw7 kept g1 | demo v8 r- rejected g- (configs) | other v9 r- rejected g- (configs)",
                ),
                (
                    Deliver(vec![policy(7, "a")]),
                    "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2 | other v9 r- rejected g- (configs)",
                ),
            ],
        },
        Case {
            name: "a config that fails its integrity checks is rejected, and its revision is not seen",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a").damaged(Damage::TransportHash)]),
                    "g1 default hw- | demo v7 r- rejected g- (transport)",
                ),
                (
                    Deliver(vec![policy(7, "a").damaged(Damage::NotAnEnvelope)]),
                    "g1 default hw- | demo v7 r- rejected g- (envelope)",
                ),
                (
                    Deliver(vec![policy(9, "a").damaged(Damage::ArtifactHash)]),
                    "g1 default hw- | demo v9 r9 rejected g- (hash)",
                ),
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
            ],
        },
        Case {
            name: "an artifact that cannot be composed, or composes to an invalid plan, is rejected, and its revision \
                   is seen",
            steps: vec![
                (
                    Deliver(vec![policy(7, "uncomposable")]),
                    "g1 default hw7 | demo v7 r7 rejected g- (compose)",
                ),
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 | demo v7 r7 rejected g- (stale)",
                ),
                (
                    Deliver(vec![policy(8, "invalid")]),
                    "g1 default hw8 | demo v8 r8 rejected g- (invalid)",
                ),
            ],
        },
        Case {
            name: "an unmodified export is served by the default program's generation, composed again when the \
                   configuration changes, and retired when revoked",
            steps: vec![
                (
                    Deliver(vec![policy(1, "unmodified")]),
                    "g1 demo@1 hw1 | demo v1 r1 active g1",
                ),
                (Configure("other"), "g1 demo@1 hw1 preparing g2 | demo v1 r1 active g1"),
                (Acknowledge, "g2 demo@1 hw1 | demo v1 r1 active g2"),
                // The default program takes the generation over: the export stays acknowledged active in it until it
                // retires.
                (Revoke, "g2 default hw1 | demo v1 r1 active g2"),
                (
                    Deliver(vec![policy(1, "unmodified")]),
                    "g2 default hw1 | demo v1 r1 rejected g- (stale)",
                ),
                (
                    Configure(BOOT_POST_AGGREGATION),
                    "g2 default hw1 preparing g3 | demo v1 r1 rejected g- (stale)",
                ),
                (Acknowledge, "g3 default hw1 | demo v1 r1 retired g2"),
            ],
        },
        Case {
            name: "a configuration change composes the artifact again under its revision, retries a failed \
                   preparation of it, and keeps the generation kept before the artifact",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Configure("other"),
                    "g2 demo@7 hw7 preparing g3 kept g1 | demo v7 r7 active g2",
                ),
                (RefusePrepare, "g2 demo@7 hw7 pending kept g1 | demo v7 r7 active g2"),
                (Retry, "g2 demo@7 hw7 preparing g4 kept g1 | demo v7 r7 active g2"),
                (Acknowledge, "g4 demo@7 hw7 kept g1 | demo v7 r7 active g4"),
                (Panic, "g1 default hw7 preparing g5 | demo v7 r7 failed g4 (panic)"),
                (Acknowledge, "g5 default hw7 | demo v7 r7 failed g4 (panic)"),
            ],
        },
        Case {
            name: "configuration changes while the artifact in effect is composed again wait for its preparation, and \
                   the latest commits without an abort",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Configure("x"),
                    "g2 demo@7 hw7 preparing g3 kept g1 | demo v7 r7 active g2",
                ),
                (
                    Configure("y"),
                    "g2 demo@7 hw7 preparing g3 pending kept g1 | demo v7 r7 active g2",
                ),
                (
                    Configure("z"),
                    "g2 demo@7 hw7 preparing g3 pending kept g1 | demo v7 r7 active g2",
                ),
                (Acknowledge, "g3 demo@7 hw7 preparing g4 kept g1 | demo v7 r7 active g3"),
                (Acknowledge, "g4 demo@7 hw7 kept g1 | demo v7 r7 active g4"),
            ],
        },
        Case {
            name: "a newer artifact does not wait out the retry delay of a re-composition that failed to prepare",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Configure("other"),
                    "g2 demo@7 hw7 preparing g3 kept g1 | demo v7 r7 active g2",
                ),
                (RefusePrepare, "g2 demo@7 hw7 pending kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g4 kept g1 | demo v8 r8 pending g4",
                ),
                (Acknowledge, "g4 demo@8 hw8 kept g2 | demo v8 r8 active g4"),
                // The delay of the replaced candidate passes, and starts nothing.
                (Retry, "g4 demo@8 hw8 kept g2 | demo v8 r8 active g4"),
            ],
        },
        Case {
            name: "an approval of the artifact in effect under a newer revision or in another config becomes the \
                   artifact in effect, with its review checked against the active plan",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a").approved_against(plan_hash("x"))]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 stale-review | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "a").approved_against(plan_hash("a"))]),
                    "g2 demo@8 hw8 kept g1 | demo v8 r8 active g2",
                ),
                // An acknowledgment that followed the artifact keeps following it, under its own delivery.
                (
                    Deliver(vec![policy(9, "a").id("other").approved_against(plan_hash("a"))]),
                    "g2 other@9 hw9 kept g1 | demo v8 r8 active g2 | other v9 r9 active g2",
                ),
                (
                    Configure("other"),
                    "g2 other@9 hw9 preparing g3 kept g1 | demo v8 r8 active g2 | other v9 r9 active g2",
                ),
                (
                    Acknowledge,
                    "g3 other@9 hw9 kept g1 | demo v8 r8 active g3 | other v9 r9 active g3",
                ),
                (
                    Deliver(vec![policy(10, "a").id("other").approved_against(plan_hash("x"))]),
                    "g3 other@10 hw10 kept g1 stale-review | demo v8 r8 active g3 | other v10 r10 active g3",
                ),
            ],
        },
        Case {
            name: "an artifact that fails while it is composed again is not prepared again",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Configure("other"),
                    "g2 demo@7 hw7 preparing g3 kept g1 | demo v7 r7 active g2",
                ),
                (Panic, "g1 default hw7 preparing g4 | demo v7 r7 failed g2 (panic)"),
                (Acknowledge, "g4 default hw7 | demo v7 r7 failed g2 (panic)"),
            ],
        },
        Case {
            name: "an artifact that cannot be composed again keeps running, and the host is degraded until it can",
            steps: vec![
                (
                    Deliver(vec![policy(7, "fragile")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Configure("other"),
                    "g2 demo@7 hw7 kept g1 degraded | demo v7 r7 active g2",
                ),
                (
                    Configure(BOOT_POST_AGGREGATION),
                    "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2",
                ),
            ],
        },
        Case {
            name: "a stale review and a stale base are reported in status, and the artifact still installs",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a").approved_against(boot_hash())]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b").approved_against(boot_hash())]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3",
                ),
                (Acknowledge, "g3 demo@8 hw8 kept g2 stale-review | demo v8 r8 active g3"),
                (
                    Restructure,
                    "g3 demo@8 hw8 kept g2 stale-base stale-review | demo v8 r8 active g3",
                ),
            ],
        },
        Case {
            name:
                "a worker restart handles the same snapshot again and changes nothing; a process restart forgets the \
                   high-water mark, and the channel's redelivery installs the artifact again",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3",
                ),
                (Redeliver, "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3"),
                (Restart, "g1 default hw-"),
                (Redeliver, "g1 default hw8 preparing g2 | demo v8 r8 pending g2"),
                (
                    Deliver(vec![policy(5, "c")]),
                    "g1 default hw8 preparing g2 | demo v5 r5 rejected g- (stale)",
                ),
            ],
        },
        Case {
            name: "after a revocation, a newer artifact that fails to prepare leaves the default program in effect: \
                   the revoked artifact is never composed again",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (Revoke, "g2 demo@7 hw7 preparing g3 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g4 kept g1 | demo v8 r8 pending g4",
                ),
                (
                    RefusePrepare,
                    "g2 demo@7 hw8 preparing g5 kept g1 | demo v8 r8 rejected g4 (prepare)",
                ),
                (
                    Configure("other"),
                    "g2 demo@7 hw8 preparing g5 pending kept g1 | demo v8 r8 rejected g4 (prepare)",
                ),
                (Acknowledge, "g5 default hw8 preparing g6 | demo v7 r7 retired g2"),
                (Acknowledge, "g6 default hw8 | demo v7 r7 retired g2"),
            ],
        },
        Case {
            name: "after a revocation, a newer artifact that commits and fails rolls back to the default program, \
                   never to the revoked artifact",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (Revoke, "g2 demo@7 hw7 preparing g3 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g4 kept g1 | demo v8 r8 pending g4",
                ),
                (Acknowledge, "g4 demo@8 hw8 kept g1 | demo v8 r8 active g4"),
                (Panic, "g1 default hw8 | demo v8 r8 failed g4 (panic)"),
            ],
        },
        Case {
            name: "a revocation releases an artifact kept to roll back to, so a failure before the default program \
                   commits waits for it",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3",
                ),
                (Acknowledge, "g3 demo@8 hw8 kept g2 | demo v8 r8 active g3"),
                (Revoke, "g3 demo@8 hw8 preparing g4 | demo v8 r8 active g3"),
                (Panic, "g3! default hw8 preparing g4 | demo v8 r8 failed g3 (panic)"),
                (Acknowledge, "g4 default hw8 | demo v8 r8 failed g3 (panic)"),
            ],
        },
        Case {
            name: "an artifact rejected while the policy cannot be composed again leaves the host degraded, until a \
                   newer artifact replaces the policy",
            steps: vec![
                (
                    Deliver(vec![policy(7, "fragile")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Configure("other"),
                    "g2 demo@7 hw7 kept g1 degraded | demo v7 r7 active g2",
                ),
                (
                    Deliver(vec![policy(8, "invalid")]),
                    "g2 demo@7 hw8 kept g1 degraded | demo v8 r8 rejected g- (invalid)",
                ),
                (
                    Deliver(vec![policy(9, "b")]),
                    "g2 demo@7 hw9 preparing g3 kept g1 degraded | demo v9 r9 pending g3",
                ),
                (Acknowledge, "g3 demo@9 hw9 kept g2 | demo v9 r9 active g3"),
            ],
        },
        Case {
            name: "an artifact equal to the active plan that cancels a preparation creates no generation",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (
                    Deliver(vec![policy(8, "unmodified")]),
                    "g1 demo@8 hw8 | demo v8 r8 active g1",
                ),
            ],
        },
        Case {
            name: "the active artifact approved again while another is prepared keeps its generation, which is never \
                   kept to roll back to",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3",
                ),
                (
                    Deliver(vec![policy(9, "a")]),
                    "g2 demo@9 hw9 kept g1 | demo v9 r9 active g2",
                ),
                (Panic, "g1 default hw9 | demo v9 r9 failed g2 (panic)"),
            ],
        },
        Case {
            name: "a composition of the same artifact under a newer revision replaces the earlier one without keeping \
                   it",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3",
                ),
                // The re-composition waits for the preparation of the artifact it composes again.
                (
                    Configure("other"),
                    "g2 demo@7 hw8 preparing g3 pending kept g1 | demo v8 r8 pending g3",
                ),
                (
                    Deliver(vec![policy(9, "a")]),
                    "g2 demo@7 hw9 preparing g4 kept g1 | demo v9 r9 pending g4",
                ),
                (Acknowledge, "g4 demo@9 hw9 kept g1 | demo v9 r9 active g4"),
                (Panic, "g1 default hw9 preparing g5 | demo v9 r9 failed g4 (panic)"),
            ],
        },
        Case {
            name: "a failure takes the artifact out of effect under every revision that approved it",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Configure("other"),
                    "g2 demo@7 hw7 preparing g3 kept g1 | demo v7 r7 active g2",
                ),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g4 kept g1 | demo v8 r8 pending g4",
                ),
                (
                    Deliver(vec![policy(9, "a")]),
                    "g2 demo@7 hw9 preparing g5 kept g1 | demo v9 r9 pending g5",
                ),
                // The composition of revision 9 is dropped, and the default program catches up with the configuration.
                (Panic, "g1 default hw9 preparing g6 | demo v7 r7 failed g2 (panic)"),
                (Acknowledge, "g6 default hw9 | demo v7 r7 failed g2 (panic)"),
                (
                    Deliver(vec![policy(10, "a")]),
                    "g6 default hw10 preparing g7 | demo v10 r10 pending g7",
                ),
            ],
        },
        Case {
            name: "the active artifact delivered under a revision below the high-water mark, or equal to one seen for \
                   another artifact, is stale",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(5, "a")]),
                    "g2 demo@7 hw7 kept g1 | demo v5 r5 rejected g- (stale)",
                ),
                (
                    Deliver(vec![policy(8, "invalid")]),
                    "g2 demo@7 hw8 kept g1 | demo v8 r8 rejected g- (invalid)",
                ),
                (
                    Deliver(vec![policy(8, "a").version(9)]),
                    "g2 demo@7 hw8 kept g1 | demo v9 r8 rejected g- (stale)",
                ),
                (
                    Deliver(vec![policy(9, "a").version(10)]),
                    "g2 demo@9 hw9 kept g1 | demo v10 r9 active g2",
                ),
            ],
        },
        Case {
            name:
                "the artifact in effect delivered again under its own revision acknowledges the current state, after \
                   a newer artifact was rejected too",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "invalid")]),
                    "g2 demo@7 hw8 kept g1 | demo v8 r8 rejected g- (invalid)",
                ),
                (
                    Deliver(vec![policy(7, "a")]),
                    "g2 demo@7 hw8 kept g1 | demo v7 r7 active g2",
                ),
            ],
        },
        Case {
            name: "a released last-known-good artifact stays prepared until its generation retires",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (Hold, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b").id("other")]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v7 r7 active g2 | other v8 r8 pending g3",
                ),
                (
                    Acknowledge,
                    "g3 other@8 hw8 kept g2 | demo v7 r7 prepared g2 | other v8 r8 active g3",
                ),
                (
                    Deliver(vec![policy(9, "c").id("third")]),
                    "g3 other@8 hw9 preparing g4 kept g2 | demo v7 r7 prepared g2 | other v8 r8 active g3 | third v9 \
                     r9 pending g4",
                ),
                (
                    Acknowledge,
                    "g4 third@9 hw9 kept g3 | demo v7 r7 prepared g2 | other v8 r8 prepared g3 | third v9 r9 active g4",
                ),
                (
                    Release,
                    "g4 third@9 hw9 kept g3 | demo v7 r7 retired g2 | other v8 r8 prepared g3 | third v9 r9 active g4",
                ),
            ],
        },
        Case {
            name: "a refused delivery under the running artifact's config still reports what becomes of the artifact",
            steps: vec![
                (
                    Deliver(vec![policy(7, "a")]),
                    "g1 default hw7 preparing g2 | demo v7 r7 pending g2",
                ),
                (Acknowledge, "g2 demo@7 hw7 kept g1 | demo v7 r7 active g2"),
                (
                    Deliver(vec![policy(8, "b")]),
                    "g2 demo@7 hw8 preparing g3 kept g1 | demo v8 r8 pending g3",
                ),
                (Acknowledge, "g3 demo@8 hw8 kept g2 | demo v8 r8 active g3"),
                (
                    Deliver(vec![policy(9, "c"), policy(10, "d").id("other")]),
                    "g3 demo@8 hw8 kept g2 | demo v9 r- rejected g- (configs) | other v10 r- rejected g- (configs)",
                ),
                (
                    Panic,
                    "g2 demo@7 hw8 | demo v8 r8 failed g3 (panic) | other v10 r- rejected g- (configs)",
                ),
            ],
        },
    ]
}

#[test]
fn the_artifact_lifecycle_follows_the_installation_rules() {
    // Every case runs, and each one's first unexpected step is reported.
    let mut failures = Vec::new();
    for case in cases() {
        let mut host = Host::new();
        assert_eq!(host.summary(), "g1 default hw-", "{}: boot", case.name);
        for (index, (step, expected)) in case.steps.iter().enumerate() {
            let performed = host.step(step);
            let summary = host.summary();
            if summary != *expected {
                failures.push(format!(
                    "{}\n  step {} ({:?})\n  expected {}\n  found    {}\n  performed {:?}",
                    case.name,
                    index + 1,
                    step,
                    expected,
                    summary,
                    performed
                ));
                break;
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn acknowledgments_name_the_config_revision_hash_generation_and_error() {
    let mut host = Host::new();
    host.step(&Deliver(vec![policy(7, "a")]));
    host.step(&Acknowledge);
    let active = ConfigAcknowledgment {
        product: ArtifactProduct::Policy,
        id: ArtifactConfigId::new("demo"),
        version: 7,
        revision: Some(7),
        artifact_hash: Some(artifact_hash("a")),
        state: ApplyState::Active,
        generation: Some(2),
        cut_at: None,
        error: None,
    };
    assert_eq!(host.core().config_states(), vec![active]);

    host.step(&Deliver(vec![policy(6, "b")]));
    let stale = ConfigAcknowledgment {
        product: ArtifactProduct::Policy,
        id: ArtifactConfigId::new("demo"),
        version: 6,
        revision: Some(6),
        artifact_hash: Some(artifact_hash("b")),
        state: ApplyState::Rejected,
        generation: None,
        cut_at: None,
        error: Some("revision 6 is stale: revision 7 has already been seen, and revisions must increase".to_string()),
    };
    assert_eq!(host.core().config_states(), vec![stale]);

    // The status summary names the artifact the active generation serves, and the host's own values.
    let states = host
        .core()
        .apply_states(Some(boot_hash()), Some("a host reason".to_string()));
    assert_eq!(states.status.default_program_hash, Some(boot_hash()));
    assert_eq!(states.status.revision_high_water, Some(7));
    assert_eq!(states.status.active_generation, Some(2));
    assert_eq!(
        states.status.active_plan_hash,
        Some(
            program("a", BOOT_POST_AGGREGATION)
                .hash(ResolvedValues::Include)
                .unwrap()
        )
    );
    let artifact = states.status.active_artifact.as_ref().unwrap();
    assert_eq!(
        (artifact.id.as_str(), artifact.revision, artifact.artifact_hash),
        ("demo", 7, artifact_hash("a"))
    );
    assert_eq!(states.status.degraded.as_deref(), Some("a host reason"));
    assert_eq!(
        states.status.live_generations, 2,
        "the active generation, and the one kept to roll back to"
    );
    assert_eq!(states.config_states, host.core().config_states());
}

#[test]
fn refusals_after_the_envelope_is_read_are_reported_like_rejected_candidates() {
    let mut host = Host::new();
    let snapshot = snapshot(&[policy(7, "uncomposable")]);
    let actions = host.harness.core.on_snapshot(&snapshot, &host.composer);
    let [Action::Report(Report {
        origin,
        generation: None,
        outcome: Outcome::Rejected(Rejection::Refused { message }),
        ..
    })] = &actions[..]
    else {
        panic!("{:?}", described(&actions))
    };
    assert_eq!(
        *origin,
        Origin::Artifact {
            revision: 7,
            hash: artifact_hash("uncomposable"),
        }
    );
    assert_eq!(
        message,
        "The artifact cannot be composed: the test refuses to compose it"
    );

    // A config refused before its envelope is read has no revision or hash to report under: it is acknowledged only.
    let snapshot = self::snapshot(&[policy(8, "a").damaged(Damage::TransportHash)]);
    assert!(host.harness.core.on_snapshot(&snapshot, &host.composer).is_empty());
    let error = host.core().config_states()[0].error.clone().unwrap();
    assert!(
        error.starts_with("transport hash mismatch: the config declares sha256"),
        "{}",
        error
    );
}

#[test]
fn a_revoked_policy_leaves_effect_at_once_while_its_generation_runs_until_replaced() {
    let mut host = Host::new();
    host.step(&Deliver(vec![policy(7, "a")]));
    let in_effect = host.core().policy_in_effect().unwrap();
    assert_eq!((in_effect.id.as_str(), in_effect.revision), ("demo", 7));
    host.step(&Acknowledge);

    let performed = host.step(&Revoke);
    assert_eq!(
        performed,
        vec![
            "install the default program".to_string(),
            "prepare 3 [0:unchanged reuse 1 build 0, 1:changed reuse 1 build 0, 2:unchanged reuse 2 build 0, \
             3:unchanged reuse 1 build 0, 4:unchanged reuse 0 build 0, 5:unchanged reuse 2 build 0]"
                .to_string(),
            "candidate 3 generation 3: pending".to_string(),
        ]
    );
    assert_eq!(host.core().policy_in_effect(), None);
    // A configuration change now proposes the default program, which waits behind the one being prepared: nothing
    // composes the revoked artifact.
    host.step(&Configure("other"));
    assert_eq!(
        host.summary(),
        "g2 demo@7 hw7 preparing g3 pending kept g1 | demo v7 r7 active g2"
    );
    host.step(&Acknowledge);
    assert_eq!(host.summary(), "g3 default hw7 preparing g4 | demo v7 r7 retired g2");
    host.step(&Acknowledge);
    assert_eq!(host.summary(), "g4 default hw7 | demo v7 r7 retired g2");
}

#[test]
fn a_revocation_before_any_artifact_changes_nothing() {
    let mut host = Host::new();
    assert!(host.step(&Revoke).is_empty());
    assert_eq!(host.summary(), "g1 default hw-");
}

#[test]
fn the_default_program_keeps_a_zero_panic_budget_and_artifacts_get_theirs() {
    assert_eq!(
        PanicBudget::for_origin(&Origin::DefaultProgram { config_change: 0 }),
        PanicBudget::Zero
    );
    assert_eq!(
        PanicBudget::for_origin(&Origin::Artifact {
            revision: 1,
            hash: artifact_hash("a"),
        }),
        PanicBudget::Limited {
            per_window: 3,
            window: Duration::from_secs(60),
            total: 5,
        }
    );

    // An unmodified export runs in the default program's generation, whose budget stays zero: every node in it is
    // native. Once composed again, it runs in an artifact generation.
    let mut host = Host::new();
    host.step(&Deliver(vec![policy(1, "unmodified")]));
    let origin = host.core().status().active.unwrap().origin;
    assert_eq!(origin, Origin::DefaultProgram { config_change: 0 });
    host.step(&Configure("other"));
    host.step(&Acknowledge);
    let origin = host.core().status().active.unwrap().origin;
    assert_eq!(
        origin,
        Origin::Artifact {
            revision: 1,
            hash: artifact_hash("unmodified"),
        }
    );
}

#[test]
fn a_configuration_change_without_an_artifact_proposes_the_default_program() {
    let mut host = Host::new();
    let performed = host.step(&Configure("other"));
    assert_eq!(performed[1], "candidate 2 generation 2: pending");
    host.step(&Acknowledge);
    assert_eq!(host.summary(), "g2 default hw-");
    // A failed default-program generation fails its component, as a native one does: the core asks for nothing.
    let performed = host.step(&Panic);
    assert_eq!(performed, vec!["candidate 2 generation 2: failed".to_string()]);
}

#[test]
fn the_acknowledgments_of_configs_absent_from_the_channel_are_forgotten_beyond_a_history() {
    let mut host = Host::new();
    for index in 0..40 {
        // None of the configs installs: the first cannot be composed, and the others are stale.
        let revision = if index == 0 { 100 } else { 1 };
        let config = policy(revision, "uncomposable").id(format!("config-{:02}", index));
        host.step(&Deliver(vec![config]));
    }
    let states = host.core().config_states();
    // The latest config, and the 16 most recent before it.
    assert_eq!(states.len(), 17);
    assert_eq!(states.first().unwrap().id.as_str(), "config-23");
    assert_eq!(states.last().unwrap().id.as_str(), "config-39");
}

#[test]
fn a_snapshot_with_many_configs_is_rejected_with_a_message_that_names_a_few() {
    let mut host = Host::new();
    let policies = (0..30)
        .map(|index| policy(7, "a").id(format!("config-{:02}", index)))
        .collect::<Vec<_>>();
    host.step(&Deliver(policies));
    let states = host.core().config_states();
    assert_eq!(states.len(), 30);
    let expected = "the policy snapshot holds 30 configs (`config-00`, `config-01`, `config-02`, `config-03`, \
                    `config-04`, `config-05`, `config-06`, `config-07`, `config-08`, `config-09`, and 20 more) for \
                    scope `post_enrich`, but one artifact replaces a scope: every config for it is rejected until the \
                    snapshot holds one";
    for ack in &states {
        assert_eq!(
            (ack.state, ack.error.as_deref()),
            (ApplyState::Rejected, Some(expected))
        );
    }
    assert_eq!(host.summary().split(" | ").next(), Some("g1 default hw-"));
}

/// One step of a random event sequence, whose deliveries' revisions are offsets from the high-water mark, so that
/// sequences keep delivering newer revisions as well as stale ones and redeliveries.
#[derive(Clone, Debug)]
struct Draft(Step);

impl Draft {
    /// Returns the step, with each delivered revision `offset` made `high_water + offset - 1`, and at least 1: an
    /// offset of 0 is below the mark, 1 is the mark, and above that is newer. The config's version is its revision.
    fn resolve(&self, high_water: Option<u64>) -> Step {
        match &self.0 {
            Deliver(policies) => Deliver(
                policies
                    .iter()
                    .map(|policy| {
                        let revision = (high_water.unwrap_or(0) + policy.revision).saturating_sub(1).max(1);
                        Policy {
                            version: revision,
                            revision,
                            ..policy.clone()
                        }
                    })
                    .collect(),
            ),
            step => step.clone(),
        }
    }
}

/// One step of a random event sequence.
fn arb_step() -> impl Strategy<Value = Draft> {
    // Artifacts that install are the most common, so that sequences run artifacts long enough to revoke, replace, and
    // fail them.
    let marker = prop_oneof![
        6 => prop::sample::select(vec!["a", "b", "c"]),
        1 => prop::sample::select(vec!["unmodified", "invalid", "uncomposable", "fragile"]),
    ];
    let damage = prop::option::weighted(
        0.1,
        prop::sample::select(vec![Damage::TransportHash, Damage::NotAnEnvelope, Damage::ArtifactHash]),
    );
    let config = (prop::sample::select(vec!["demo", "other"]), 0..5u64, marker, damage).prop_map(
        |(id, offset, marker, damage)| {
            let policy = policy(offset, marker).id(id);
            match damage {
                Some(damage) => policy.damaged(damage),
                None => policy,
            }
        },
    );
    prop_oneof![
        4 => config.clone().prop_map(|config| Deliver(vec![config])),
        1 => (config.clone(), config).prop_map(|(first, second)| Deliver(vec![first, second])),
        2 => Just(Revoke),
        1 => Just(Redeliver),
        1 => (0..6usize).prop_map(RedeliverEarlier),
        2 => prop::sample::select(vec!["x", "y", BOOT_POST_AGGREGATION]).prop_map(Configure),
        1 => Just(Restructure),
        8 => Just(Acknowledge),
        1 => Just(RefusePrepare),
        1 => Just(TimeOut),
        2 => Just(Panic),
        1 => (1..12u64).prop_map(PanicIn),
        1 => Just(Retry),
        1 => Just(Hold),
        1 => Just(Release),
    ]
    .prop_map(Draft)
}

/// An independent model of what the core accepts, revokes, and fails, from the steps alone.
#[derive(Default)]
struct Model {
    /// The newest revision of a single config that passed its integrity checks.
    high_water: Option<u64>,

    /// The revision and hash of every such config delivered above the high-water mark at the time: accepted, or
    /// acknowledged as the policy in effect.
    accepted: BTreeSet<(u64, PlanHash)>,

    /// The artifacts every revocation so far revoked, unless delivered above the high-water mark since.
    revoked: BTreeSet<PlanHash>,

    /// The generation active when the latest revocation came, while it stays active: a revoked artifact may run only
    /// in it.
    revoked_running: Option<GenerationId>,

    /// The artifacts whose active or kept generation supervision failed, unless delivered above the high-water mark
    /// since.
    failed: BTreeSet<PlanHash>,

    /// The generations supervision failed while they were active or kept.
    failed_generations: BTreeSet<GenerationId>,

    /// The configs of the snapshot the core handled last.
    last: Vec<Policy>,
}

impl Model {
    /// Records a delivery the core handles, while `active` is the active generation.
    ///
    /// Every test artifact replaces the policy scope. The configs that pass their integrity checks are counted for it:
    /// one is handled, unless it was handled alone in the last snapshot and is held unchanged, and more are all
    /// rejected.
    fn deliver(&mut self, policies: &[Policy], active: GenerationId) {
        let last = std::mem::replace(&mut self.last, policies.to_vec());
        let checked = |policies: &[Policy]| {
            policies
                .iter()
                .filter(|policy| policy.damage.is_none())
                .cloned()
                .collect::<Vec<_>>()
        };
        let crowded = checked(&last).len() > 1;
        match (policies, checked(policies).as_slice()) {
            ([], _) => {
                self.revoked.extend(self.accepted.iter().map(|(_, hash)| *hash));
                self.revoked_running = Some(active);
            }
            (_, [policy]) if crowded || !last.iter().any(|earlier| earlier.config() == policy.config()) => {
                let hash = artifact_hash(policy.marker);
                if self.high_water.is_none_or(|high_water| policy.revision > high_water) {
                    self.accepted.insert((policy.revision, hash));
                    self.revoked.remove(&hash);
                    self.failed.remove(&hash);
                }
                self.high_water = Some(
                    self.high_water
                        .map_or(policy.revision, |mark| mark.max(policy.revision)),
                );
            }
            _ => {}
        }
    }
}

/// Returns the hash of the artifact a generation serves, or, if it serves none, the hash it was created for.
fn artifact_of(core: &InstallerCore, record: &GenerationRecord) -> Option<PlanHash> {
    core.served_by(record.id).map(|(_, hash)| hash).or(match record.origin {
        Origin::Artifact { hash, .. } => Some(hash),
        Origin::DefaultProgram { .. } => None,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn property_test_the_high_water_mark_never_decreases_and_only_accepted_plans_run(
        drafts in prop::collection::vec(arb_step(), 1..40),
    ) {
        let mut host = Host::new();
        let mut model = Model::default();
        for draft in &drafts {
            let step = &draft.resolve(model.high_water);
            // Steps the host cannot take in the current state are skipped.
            let status = host.core().status();
            let skip = match step {
                Acknowledge | RefusePrepare | TimeOut => status.preparing.is_none(),
                // A default-program generation that fails ends the process.
                Panic => status.active.as_ref().is_some_and(|active| !active.origin.can_roll_back()),
                PanicIn(generation) => status
                    .generations
                    .iter()
                    .find(|record| record.id == GenerationId::new(*generation))
                    .is_none_or(|record| !record.origin.can_roll_back()),
                _ => false,
            };
            if skip {
                continue;
            }
            let active = status.active.as_ref().unwrap();
            // A snapshot equal to the one the core handled last changes nothing.
            if let Some(policies) = host
                .delivery(step)
                .filter(|policies| host.handled.as_ref() != Some(&snapshot(policies)))
            {
                model.deliver(&policies, active.id);
            }
            // An artifact whose active or kept generation supervision fails is out of effect until it is delivered
            // again: no other generation may serve it, or be created for it, and become active or kept. Failing a
            // generation again changes nothing.
            let targeted = match step {
                Panic => Some(active.id),
                PanicIn(generation) => Some(GenerationId::new(*generation)),
                _ => None,
            };
            let failing = targeted
                .filter(|target| {
                    *target == active.id || status.last_known_good.as_ref().is_some_and(|kept| kept.id == *target)
                })
                .and_then(|target| status.generations.iter().find(|record| record.id == target))
                .filter(|record| record.state != GenerationState::Failed);
            if let Some(hash) = failing.and_then(|record| artifact_of(host.core(), record)) {
                model.failed.insert(hash);
                model.failed_generations.extend(targeted);
            }
            let before = status.revision_high_water;
            let performed = host.step(step);

            let core = host.core();
            let status = core.status();
            let summary = core.status_summary(None, None);
            prop_assert!(status.revision_high_water >= before, "{:?} lowered the high-water mark", step);
            prop_assert_eq!(status.revision_high_water, model.high_water, "after {:?}", step);
            prop_assert!(
                performed.iter().filter(|action| *action == "install the default program").count() <= 1,
                "{:?} asked for the default program more than once: {:?}", step, performed
            );

            // The active generation runs the default program, or an artifact the core accepted: one it installed, or
            // last-known-good rolled back to.
            let active = status.active.as_ref().unwrap();
            if let Origin::Artifact { revision, hash } = active.origin {
                prop_assert!(
                    model.accepted.contains(&(revision, hash)),
                    "{:?} runs, but was never accepted", active.origin
                );
            }
            if let Some(served) = core.served_by(active.id) {
                prop_assert!(model.accepted.contains(&served), "generation {} serves {:?}", active.id, served);
            }
            let active_artifact = artifact_of(core, active);
            if let Some(hash) = active_artifact.filter(|hash| model.failed.contains(hash)) {
                prop_assert!(
                    model.failed_generations.contains(&active.id),
                    "{} failed under supervision, but generation {} runs it again", hash, active.id
                );
            }
            if let Some(artifact) = &summary.active_artifact {
                prop_assert!(model.accepted.contains(&(artifact.revision, artifact.artifact_hash)));
                prop_assert!(!model.failed.contains(&artifact.artifact_hash));
                prop_assert_ne!(active.state, GenerationState::Failed);
            }
            // A failed artifact generation is always being replaced.
            if active.state == GenerationState::Failed {
                prop_assert!(
                    status.preparing.is_some() || status.pending.is_some(),
                    "failed generation {} is not being replaced after {:?}", active.id, step
                );
            }
            // The generation kept to roll back to never serves the artifact the active one does, nor one that failed.
            if let Some(kept) = &status.last_known_good {
                prop_assert_ne!(kept.state, GenerationState::Failed);
                let kept_artifact = artifact_of(core, kept);
                if active.origin.can_roll_back() && kept_artifact.is_some() {
                    prop_assert_ne!(kept_artifact, active_artifact, "generations {} and {}", kept.id, active.id);
                }
                prop_assert!(kept_artifact.is_none_or(|hash| !model.failed.contains(&hash)));
            }

            // A revoked artifact is never in effect, prepared, or kept again, and runs only in the generation that was
            // active when it was revoked, until that one is replaced.
            if model.revoked_running != Some(active.id) {
                model.revoked_running = None;
            }
            if let Some(policy) = core.policy_in_effect() {
                prop_assert!(!model.revoked.contains(&policy.artifact_hash), "revoked {:?} is in effect", policy);
            }
            if let Some((_, hash)) = core.served_by(active.id).filter(|(_, hash)| model.revoked.contains(hash)) {
                prop_assert_eq!(
                    model.revoked_running, Some(active.id),
                    "generation {} serves revoked {} after {:?}", active.id, hash, step
                );
            }
            for record in [&status.preparing, &status.last_known_good].into_iter().flatten() {
                let served = core.served_by(record.id);
                prop_assert!(
                    served.is_none_or(|(_, hash)| !model.revoked.contains(&hash)),
                    "generation {} serves revoked {:?} after {:?}", record.id, served, step
                );
            }
            // Every config acknowledged active or prepared names a generation that is alive, one acknowledged retired
            // names none, and every config acknowledged as installing names a revision the core accepted, or a
            // redelivery of one.
            for ack in host.core().config_states() {
                let generation = ack.generation.map(GenerationId::new);
                if matches!(ack.state, ApplyState::Active | ApplyState::Prepared) {
                    prop_assert!(
                        generation.is_some_and(|generation| status.live.contains(&generation)),
                        "{:?} names no live generation", ack
                    );
                }
                if ack.state == ApplyState::Retired {
                    prop_assert!(
                        generation.is_none_or(|generation| !status.live.contains(&generation)),
                        "{:?} is retired, but its generation is alive", ack
                    );
                }
                if matches!(ack.state, ApplyState::Pending | ApplyState::Prepared | ApplyState::Active) {
                    let delivered = ack.revision.zip(ack.artifact_hash);
                    prop_assert!(
                        delivered.is_some_and(|delivered| model.accepted.contains(&delivered)),
                        "{:?} acknowledges a revision that was never accepted", ack
                    );
                }
            }
        }
    }
}
