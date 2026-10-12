//! Artifacts of two scopes through the installer core: one config per scope, the policy in effect per scope, one
//! revision high-water mark across scopes, and per-scope status, with the states expected after each step written by
//! hand.
//!
//! The test composer composes an artifact by its scope and marker: a `post_enrich` artifact's marker becomes the
//! listener filterlist, and a `dogstatsd_pre_aggregation` artifact's the metric name of the tag filterlist's rule, so
//! that a plan shows which artifacts it composes.

use std::{cell::Cell, collections::BTreeMap};

use saluki_core::execution::{GenerationId, GenerationRef};
use serde_json::{json, Value};

use super::{
    artifact_tests::{program, BOOT_POST_AGGREGATION},
    tests::{candidate, describe, node_mut, Harness},
    *,
};
use crate::{
    canonical::PlanHash,
    channel::{ArtifactConfig, ArtifactConfigId, ArtifactProduct, ArtifactSnapshot, POLICY_SCOPE},
    compile::SlotId,
    compose::{Approval, ComposeError, Composed, Composer, ScopeComposition},
    ir::{Plan, ReceivedArtifact, ScopeName},
    operators::OperatorRegistry,
    testing::default_program_catalog,
    validate::Validator,
};

const PRE: &str = "dogstatsd_pre_aggregation";
const POST: &str = POLICY_SCOPE;

/// The listener filterlist of the boot default program.
const BOOT_LISTENER: &str = "tenant.raw.blocked";

/// The metric name of the boot default program's tag filterlist rule.
const BOOT_FILTERLIST: &str = "tenant.checkout.latency";

const PANICS: &str = "3 panics in 60 seconds";

fn scope(name: &str) -> ScopeName {
    name.parse().unwrap()
}

/// An artifact document for `scope` that carries `marker`, which the test composer reads.
fn document(scope: &str, marker: &str) -> Value {
    json!({
        "ir_version": 1,
        "scope": scope,
        "base": {"program_hash": format!("sha256:{}", "0".repeat(64))},
        "nodes": [{"id": "marker", "op": "split", "params": {"predicate": {"name": {"prefix": marker}}}}],
        "edges": [{"from": "input.metrics", "to": "marker.in"}]
    })
}

fn hash_of(scope: &str, marker: &str) -> PlanHash {
    ReceivedArtifact::from_value(document(scope, marker)).unwrap().hash
}

fn marker_of(artifact: &ReceivedArtifact) -> String {
    let document = serde_json::to_value(&artifact.artifact).unwrap();
    document
        .pointer("/nodes/0/params/predicate/name/prefix")
        .and_then(Value::as_str)
        .unwrap()
        .to_string()
}

/// The default program with the listener filterlist and the tag filterlist rule's metric name set.
fn plan(listener: &str, filterlist: &str) -> Plan {
    let mut plan = serde_json::to_value(program(listener, BOOT_POST_AGGREGATION)).unwrap();
    node_mut(&mut plan, "dsd_tag_filterlist")["params"]["rules"]["resolved"][0]["metric_name"] = json!(filterlist);
    Plan::from_value(plan).unwrap()
}

/// Composes each scope's artifact into the default program, by its marker: an `invalid` marker composes to a plan that
/// does not validate, and `uncomposable` cannot be composed.
struct TwoScopes {
    /// Whether every composition has a stale base.
    stale: Cell<bool>,
}

impl Composer for TwoScopes {
    fn compose_all(&self, artifacts: &[(&ReceivedArtifact, &Approval)]) -> Result<Composed, ComposeError> {
        let mut listener = BOOT_LISTENER.to_string();
        let mut filterlist = BOOT_FILTERLIST.to_string();
        let mut scopes = BTreeMap::new();
        for (artifact, approval) in artifacts {
            let marker = marker_of(artifact);
            if marker == "uncomposable" {
                return Err(ComposeError::Malformed {
                    message: "the test refuses to compose it".to_string(),
                });
            }
            match artifact.artifact.scope.as_str() {
                PRE => filterlist = marker,
                _ => listener = marker,
            }
            let composition = ScopeComposition {
                correspondence: Correspondence::new(),
                stale_base: self.stale.get(),
                stale_review: approval
                    .approved_against
                    .is_some_and(|approved| Some(approved) != approval.active_plan_hash),
                ..ScopeComposition::default()
            };
            scopes.insert(artifact.artifact.scope.clone(), composition);
        }
        let composed = plan(&listener, &filterlist);
        let catalog = if [&listener, &filterlist].iter().any(|marker| *marker == "invalid") {
            crate::validate::MemoryCatalog::new()
        } else {
            default_program_catalog()
        };
        let report = Validator::new(&OperatorRegistry::native(), &catalog).validate(&composed);
        Ok(Composed {
            plan: composed,
            report,
            correspondence: Correspondence::new(),
            stale_base: scopes.values().any(|scope| scope.stale_base),
            stale_review: scopes.values().any(|scope| scope.stale_review),
            scopes,
        })
    }
}

/// A config of `scope`, delivered in config `id` at version and revision `revision`, with the artifact `marker`.
fn config(id: &str, revision: u64, scope: &str, marker: &str) -> ArtifactConfig {
    let payload = json!({
        "revision": revision,
        "artifact_hash": hash_of(scope, marker),
        "artifact": document(scope, marker),
    })
    .to_string();
    ArtifactConfig {
        id: ArtifactConfigId::new(id),
        version: revision,
        sha256: ArtifactConfig::transport_hash(&payload),
        payload,
    }
}

/// The host around the core, as in the other installer tests, with the two-scope composer.
struct Host {
    harness: Harness,
    composer: TwoScopes,
    schedule: Option<GenerationRef>,
    changes: u64,
}

impl Host {
    fn new() -> Self {
        let mut harness = Harness::new(InstallerConfig::default());
        let schedule = harness.boot.take().map(|boot| boot.generation);
        Self {
            harness,
            composer: TwoScopes {
                stale: Cell::new(false),
            },
            schedule,
            changes: 0,
        }
    }

    fn core(&mut self) -> &mut InstallerCore {
        &mut self.harness.core
    }

    fn deliver(&mut self, configs: Vec<ArtifactConfig>) -> Vec<String> {
        let snapshot = ArtifactSnapshot::new(ArtifactProduct::Policy, configs);
        let actions = self.harness.core.on_snapshot(&snapshot, &self.composer);
        self.perform(actions)
    }

    fn acknowledge(&mut self) -> Vec<String> {
        let generation = self
            .core()
            .status()
            .preparing
            .expect("a generation is being prepared")
            .id;
        let mut actions = Vec::new();
        for slot in 0..self.core().frame().slots().len() as u16 {
            actions.extend(self.core().prepare_acknowledged(generation, SlotId::new(slot), Ok(())));
        }
        self.perform(actions)
    }

    fn lower(&mut self) -> Vec<Action> {
        self.changes += 1;
        let origin = Origin::DefaultProgram {
            config_change: self.changes,
        };
        let candidate = candidate(program(BOOT_LISTENER, BOOT_POST_AGGREGATION), origin);
        self.harness.core.on_default_program(candidate, &self.composer).1
    }

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

    /// Describes the active generation's plan, by the markers it composes, what is in flight, the high-water mark, and
    /// every config's acknowledgment.
    fn summary(&mut self) -> String {
        let status = self.core().status();
        let active = status.active.as_ref().unwrap();
        let plan = self.core().active_plan().unwrap().clone();
        let filterlist = plan
            .node(&"dsd_tag_filterlist".parse().unwrap())
            .and_then(|node| node.params.get("rules"))
            .and_then(|rules| rules.pointer("/resolved/0/metric_name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let listener = plan
            .node(&"dsd_prefix_filter_blocklist".parse().unwrap())
            .and_then(|node| node.params.get("predicate"))
            .and_then(|predicate| predicate.pointer("/intake_name/matches/values/resolved/0"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let marker = |value: &str, boot: &str| {
            if value == boot {
                "-".to_string()
            } else {
                value.to_string()
            }
        };
        let mut line = format!(
            "g{} post={} pre={}",
            active.id,
            marker(&listener, BOOT_LISTENER),
            marker(&filterlist, BOOT_FILTERLIST)
        );
        if active.state == GenerationState::Failed {
            line.push('!');
        }
        match status.revision_high_water {
            Some(high_water) => line.push_str(&format!(" hw{}", high_water)),
            None => line.push_str(" hw-"),
        }
        if let Some(preparing) = &status.preparing {
            line.push_str(&format!(" preparing g{}", preparing.id));
        }
        for ack in self.core().config_states() {
            let generation = ack
                .generation
                .map_or_else(|| "-".to_string(), |generation| generation.to_string());
            line.push_str(&format!(
                " | {} r{} {} g{}",
                ack.id,
                ack.revision.unwrap_or(0),
                ack.state,
                generation
            ));
            if let Some(error) = &ack.error {
                let kind = if error.contains("configs (") {
                    "configs"
                } else if error.contains("is stale") {
                    "stale"
                } else if error.contains("failed under supervision") || error.contains("panics") {
                    "failed"
                } else if error.contains("is not valid") {
                    "invalid"
                } else if error.contains("transport hash mismatch") {
                    "transport"
                } else if error.contains("is the revision of every config") {
                    "shared"
                } else {
                    error
                };
                line.push_str(&format!(" ({})", kind));
            }
        }
        line
    }
}

#[test]
fn artifacts_of_two_scopes_install_together_and_each_config_follows_its_own() {
    let mut host = Host::new();
    host.deliver(vec![config("post", 1, POST, "a"), config("pre", 2, PRE, "b")]);
    assert_eq!(
        host.summary(),
        "g1 post=- pre=- hw2 preparing g2 | post r1 pending g2 | pre r2 pending g2"
    );
    host.acknowledge();
    assert_eq!(
        host.summary(),
        "g2 post=a pre=b hw2 | post r1 active g2 | pre r2 active g2"
    );

    // The status names each scope's artifact; the top-level members describe the policy scope.
    let summary = host.core().status_summary(None, None);
    let active = |artifact: Option<&crate::channel::ActiveArtifact>| artifact.map(|artifact| artifact.revision);
    assert_eq!(active(summary.active_artifact.as_ref()), Some(1));
    let scopes = summary
        .scopes
        .iter()
        .map(|(scope, status)| (scope.to_string(), active(status.active_artifact.as_ref())))
        .collect::<Vec<_>>();
    assert_eq!(scopes, [(PRE.to_string(), Some(2)), (POST.to_string(), Some(1))]);
    // The generation's origin is the newest revision among the artifacts it composes.
    assert_eq!(
        host.core().status().active.unwrap().origin,
        Origin::Artifact {
            revision: 2,
            hash: hash_of(PRE, "b")
        }
    );
}

#[test]
fn a_new_revision_for_one_scope_keeps_the_others_artifact_under_one_high_water_mark() {
    let mut host = Host::new();
    host.deliver(vec![config("post", 1, POST, "a"), config("pre", 2, PRE, "b")]);
    host.acknowledge();

    // The pre-aggregation scope changes; the policy scope's config is held unchanged and keeps its acknowledgment.
    host.deliver(vec![config("post", 1, POST, "a"), config("pre", 3, PRE, "c")]);
    assert_eq!(
        host.summary(),
        "g2 post=a pre=b hw3 preparing g3 | post r1 active g2 | pre r3 pending g3"
    );
    host.acknowledge();
    assert_eq!(
        host.summary(),
        "g3 post=a pre=c hw3 | post r1 active g3 | pre r3 active g3"
    );

    // A new revision for either scope must exceed every revision seen, whichever scope saw it.
    host.deliver(vec![config("post", 2, POST, "d"), config("pre", 3, PRE, "c")]);
    assert_eq!(
        host.summary(),
        "g3 post=a pre=c hw3 | post r2 rejected g- (stale) | pre r3 active g3"
    );
    host.deliver(vec![config("post", 4, POST, "d"), config("pre", 3, PRE, "c")]);
    host.acknowledge();
    assert_eq!(
        host.summary(),
        "g4 post=d pre=c hw4 | post r4 active g4 | pre r3 active g4"
    );
}

#[test]
fn two_configs_for_one_scope_are_rejected_while_the_other_scope_installs() {
    let mut host = Host::new();
    host.deliver(vec![
        config("post", 1, POST, "a"),
        config("again", 2, POST, "b"),
        config("pre", 3, PRE, "c"),
    ]);
    // Their revisions are not seen, as for any refusal on receipt.
    assert_eq!(
        host.summary(),
        "g1 post=- pre=- hw3 preparing g2 | again r0 rejected g- (configs) | post r0 rejected g- (configs) | pre r3 \
         pending g2"
    );
    host.acknowledge();
    assert_eq!(
        host.summary(),
        "g2 post=- pre=c hw3 | again r0 rejected g- (configs) | post r0 rejected g- (configs) | pre r3 active g2"
    );
    // Alone again, the policy scope's config is handled anew, against the one mark.
    host.deliver(vec![config("post", 1, POST, "a"), config("pre", 3, PRE, "c")]);
    assert_eq!(
        host.summary(),
        "g2 post=- pre=c hw3 | again r0 rejected g- (configs) | post r1 rejected g- (stale) | pre r3 active g2"
    );
}

#[test]
fn the_lower_revision_scopes_artifact_delivered_again_acknowledges_its_state_whatever_the_mark() {
    let start = || {
        let mut host = Host::new();
        host.deliver(vec![config("post", 3, POST, "a"), config("pre", 2, PRE, "b")]);
        host.acknowledge();
        assert_eq!(
            host.summary(),
            "g2 post=a pre=b hw3 | post r3 active g2 | pre r2 active g2"
        );
        host
    };

    // The scope is crowded for one snapshot, which changes nothing for it: alone again, its config is read anew, and
    // is the scope's artifact in effect under its own revision, although the other scope's set the mark.
    let mut host = start();
    host.deliver(vec![
        config("post", 3, POST, "a"),
        config("pre", 2, PRE, "b"),
        config("other", 4, PRE, "c"),
    ]);
    assert_eq!(
        host.summary(),
        "g2 post=a pre=b hw3 | other r0 rejected g- (configs) | post r3 active g2 | pre r0 rejected g- (configs)"
    );
    assert!(host
        .deliver(vec![config("post", 3, POST, "a"), config("pre", 2, PRE, "b")])
        .is_empty());
    assert_eq!(
        host.summary(),
        "g2 post=a pre=b hw3 | other r0 rejected g- (configs) | post r3 active g2 | pre r2 active g2"
    );

    // The scope's config is replaced by one that cannot be read, which revokes nothing, and then comes back.
    let mut host = start();
    let mut unreadable = config("junk", 9, PRE, "z");
    unreadable.payload.push(' ');
    host.deliver(vec![config("post", 3, POST, "a"), unreadable]);
    assert!(host
        .deliver(vec![config("post", 3, POST, "a"), config("pre", 2, PRE, "b")])
        .is_empty());
    assert_eq!(
        host.summary(),
        "g2 post=a pre=b hw3 | junk r0 rejected g- (transport) | post r3 active g2 | pre r2 active g2"
    );

    // The same artifact under another revision at or below the mark is still stale.
    host.deliver(vec![config("post", 3, POST, "a"), config("pre", 1, PRE, "b")]);
    assert_eq!(
        host.summary(),
        "g2 post=a pre=b hw3 | junk r0 rejected g- (transport) | post r3 active g2 | pre r1 rejected g- (stale)"
    );
}

#[test]
fn an_artifact_rejected_alone_does_not_take_the_other_scopes_new_artifact_down_with_it() {
    let mut host = Host::new();
    // As after a restart, the channel delivers both scopes' artifacts at once, and one of them no longer validates:
    // each is checked alone, and the valid one installs.
    host.deliver(vec![config("post", 1, POST, "a"), config("pre", 2, PRE, "invalid")]);
    assert_eq!(
        host.summary(),
        "g1 post=- pre=- hw2 preparing g2 | post r1 pending g2 | pre r2 rejected g- (invalid)"
    );
    host.acknowledge();
    // Once the other is fixed, the valid one, held unchanged, keeps running and installs with it.
    host.deliver(vec![config("post", 1, POST, "a"), config("pre", 3, PRE, "b")]);
    host.acknowledge();
    assert_eq!(
        host.summary(),
        "g3 post=a pre=b hw3 | post r1 active g3 | pre r3 active g3"
    );

    // Artifacts that are both rejected alone are each rejected with their own reasons.
    let mut host = Host::new();
    host.deliver(vec![
        config("post", 1, POST, "invalid"),
        config("pre", 2, PRE, "invalid"),
    ]);
    assert_eq!(
        host.summary(),
        "g1 post=- pre=- hw2 | post r1 rejected g- (invalid) | pre r2 rejected g- (invalid)"
    );
}

#[test]
fn new_artifacts_of_two_scopes_under_one_revision_are_both_rejected_until_one_has_its_own() {
    let mut host = Host::new();
    host.deliver(vec![config("post", 2, POST, "a"), config("pre", 2, PRE, "b")]);
    // Neither is newer than the other, and their revision is not seen.
    assert_eq!(
        host.summary(),
        "g1 post=- pre=- hw- | post r2 rejected g- (shared) | pre r2 rejected g- (shared)"
    );
    let error = host.core().config_states()[0].error.clone().unwrap();
    assert_eq!(
        error,
        "revision 2 is the revision of every config of (`post`, `pre`) in one snapshot, but revisions increase across \
         scopes: none of them is newer than the others, so each is rejected until it has a revision of its own"
    );
    // One of them gets a revision of its own: the other, held unchanged, is handled anew, and both install.
    host.deliver(vec![config("post", 2, POST, "a"), config("pre", 3, PRE, "b")]);
    assert_eq!(
        host.summary(),
        "g1 post=- pre=- hw3 preparing g2 | post r2 pending g2 | pre r3 pending g2"
    );
}

#[test]
fn a_config_that_moves_to_another_scope_revokes_the_scope_it_left() {
    let mut host = Host::new();
    host.deliver(vec![config("demo", 1, POST, "a")]);
    host.acknowledge();
    // The snapshot is complete: no config delivers the policy scope's artifact any more.
    host.deliver(vec![config("demo", 2, PRE, "b")]);
    assert_eq!(host.summary(), "g2 post=a pre=- hw2 preparing g3 | demo r2 pending g3");
    host.acknowledge();
    assert_eq!(host.summary(), "g3 post=- pre=b hw2 | demo r2 active g3");
}

/// Returns the metric name of a plan's tag filterlist rule: the pre-aggregation artifact's marker.
fn filterlist(plan: &Plan) -> String {
    plan.node(&"dsd_tag_filterlist".parse().unwrap())
        .and_then(|node| node.params.get("rules"))
        .and_then(|rules| rules.pointer("/resolved/0/metric_name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[test]
fn a_cut_serving_an_artifact_of_a_failed_generation_is_dropped_as_a_preparation_is() {
    // A pre-aggregation artifact whose marker starts with `cut` is declared a cut installation.
    let run = |phase: &str| {
        let mut host = Host::new();
        host.core().classify_cuts_with(|active: &Plan, candidate: &Plan| {
            let (old, new) = (filterlist(active), filterlist(candidate));
            if old == new || !new.starts_with("cut") {
                return None;
            }
            let plan = crate::install::CutPlan {
                owners: vec!["dsd_agg".parse().unwrap()],
                widths: vec![std::time::Duration::from_secs(10)],
                unknown_widths: Vec::new(),
            };
            Some((plan, vec![saluki_core::execution::AdmissionPointId::new(0)]))
        });
        host.deliver(vec![config("post", 1, POST, "a")]);
        host.acknowledge();
        host.deliver(vec![config("post", 1, POST, "a"), config("pre", 2, PRE, "b")]);
        host.acknowledge();
        // Generation 4 composes the same policy scope artifact with a newer pre-aggregation one, through a cut.
        host.deliver(vec![config("post", 1, POST, "a"), config("pre", 3, PRE, "cut-c")]);
        host.acknowledge();
        if phase == "armed" {
            let actions = host.core().cut_armed(
                GenerationId::new(4),
                saluki_core::execution::AdmissionTime::from_unix_secs(100),
            );
            host.perform(actions);
        }
        host
    };
    let failed =
        "candidate 4 generation 4: rejected (artifact revisions 2 and 1 failed under supervision: 3 panics in \
                  60 seconds)";

    // Not armed yet: the cut is aborted, and the failed generation rolls back to the one composing the policy scope's
    // artifact alone. The failure, reported after the abort, is the pre-aggregation config's latest news.
    let mut host = run("barrier");
    let actions = host.core().generation_failed(GenerationId::new(3), PANICS);
    let performed = host.perform(actions);
    assert_eq!(performed[..2], strings(&["abort 4 in 6 slots", failed]));
    assert!(performed.contains(&"commit 2".to_string()), "{:?}", performed);
    assert!(host.core().status().preparing.is_none());
    assert_eq!(
        host.summary(),
        "g2 post=a pre=- hw3 | post r1 active g2 | pre r2 failed g3 (failed)"
    );

    // Armed: the cancellation the failure asks for rejects the cut instead of preparing it again. The rejection comes
    // last, so it is the pre-aggregation config's latest news.
    let mut host = run("armed");
    let actions = host.core().generation_failed(GenerationId::new(3), PANICS);
    assert_eq!(
        host.perform(actions),
        strings(&["candidate 3 generation 3: failed", "cancel the cut of 4"])
    );
    let actions = host.core().cut_cancelled(GenerationId::new(4));
    let performed = host.perform(actions);
    assert_eq!(performed[..2], strings(&["abort 4 in 6 slots", failed]));
    assert!(performed.contains(&"commit 2".to_string()), "{:?}", performed);
    assert!(host.core().status().preparing.is_none());
    assert_eq!(
        host.summary(),
        "g2 post=a pre=- hw3 | post r1 active g2 | pre r3 rejected g4 (failed)"
    );
}

#[test]
fn a_scope_the_snapshot_no_longer_delivers_is_revoked_and_the_other_installs_again_without_it() {
    let mut host = Host::new();
    host.deliver(vec![config("post", 1, POST, "a"), config("pre", 2, PRE, "b")]);
    host.acknowledge();
    assert_eq!(
        host.deliver(vec![config("post", 1, POST, "a")]),
        strings(&[
            "prepare 3 [0:unchanged reuse 1 build 0, 1:unchanged reuse 1 build 0, 2:changed reuse 1 build 1, \
             3:unchanged reuse 1 build 0, 4:unchanged reuse 0 build 0, 5:unchanged reuse 2 build 0]",
            "candidate 3 generation 3: pending",
        ])
    );
    host.acknowledge();
    // The revoked artifact's generation retired: nothing runs it any more.
    assert_eq!(
        host.summary(),
        "g3 post=a pre=- hw2 | post r1 active g3 | pre r2 retired g2"
    );

    // An empty snapshot revokes the rest: the default program runs.
    host.deliver(Vec::new());
    host.acknowledge();
    assert_eq!(
        host.summary(),
        "g4 post=- pre=- hw2 | post r1 retired g3 | pre r2 retired g2"
    );
}

#[test]
fn a_configuration_change_composes_every_artifact_in_effect_again() {
    let mut host = Host::new();
    host.deliver(vec![config("post", 1, POST, "a"), config("pre", 2, PRE, "b")]);
    host.acknowledge();
    let actions = host.lower();
    host.perform(actions);
    // The default program changed nothing the composer reads, so the re-composition is the plan running.
    assert_eq!(
        host.summary(),
        "g2 post=a pre=b hw2 | post r1 active g2 | pre r2 active g2"
    );
    assert!(host.core().status().preparing.is_none());
}

#[test]
fn a_generation_composing_artifacts_of_two_scopes_fails_as_a_whole() {
    let mut host = Host::new();
    host.deliver(vec![config("post", 1, POST, "a")]);
    host.acknowledge();
    host.deliver(vec![config("post", 1, POST, "a"), config("pre", 2, PRE, "b")]);
    host.acknowledge();
    assert_eq!(
        host.summary(),
        "g3 post=a pre=b hw2 | post r1 active g3 | pre r2 active g3"
    );
    let actions = host.core().generation_failed(GenerationId::new(3), PANICS);
    host.perform(actions);
    // Both artifacts the generation composed are out of effect: it rolled back to the generation composing the policy
    // scope's artifact alone, which the policy scope's config follows again.
    assert_eq!(
        host.summary(),
        "g2 post=a pre=- hw2 | post r1 active g2 | pre r2 failed g3 (failed)"
    );
}

#[test]
fn every_scopes_base_and_stale_flags_are_reported_given_their_bases() {
    let mut host = Host::new();
    host.composer.stale.set(true);
    host.deliver(vec![config("pre", 1, PRE, "b")]);
    host.acknowledge();
    let bases = BTreeMap::from([
        (scope(POST), PlanHash::of_bytes(b"post")),
        (scope(PRE), PlanHash::of_bytes(b"pre")),
    ]);
    let summary = host.core().status_summary_with_bases(&bases, None);
    assert_eq!(summary.default_program_hash, Some(PlanHash::of_bytes(b"post")));
    assert_eq!(summary.active_artifact, None, "no artifact replaces the policy scope");
    assert!(!summary.stale_base);
    let scopes = summary
        .scopes
        .iter()
        .map(|(scope, status)| {
            (
                scope.to_string(),
                status.default_program_hash,
                status.active_artifact.as_ref().map(|artifact| artifact.revision),
                status.stale_base,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        scopes,
        [
            (PRE.to_string(), Some(PlanHash::of_bytes(b"pre")), Some(1), true),
            (POST.to_string(), Some(PlanHash::of_bytes(b"post")), None, false),
        ]
    );

    // A host that reports only the policy scope's base writes the scopes while another scope has an artifact active,
    // and not once none has.
    let document = serde_json::to_value(host.core().status_summary(Some(PlanHash::of_bytes(b"post")), None)).unwrap();
    assert_eq!(
        document["scopes"],
        json!({
            PRE: {"default_program_hash": null, "active_artifact": {"id": "pre", "revision": 1,
                  "artifact_hash": hash_of(PRE, "b")}, "stale_base": true},
            POST: {"default_program_hash": PlanHash::of_bytes(b"post"), "active_artifact": null, "stale_base": false}
        })
    );
    host.deliver(Vec::new());
    host.acknowledge();
    let document = serde_json::to_value(host.core().status_summary(Some(PlanHash::of_bytes(b"post")), None)).unwrap();
    assert!(document.get("scopes").is_none(), "{}", document);
    assert_eq!(host.summary(), "g3 post=- pre=- hw1 | pre r1 retired g2");
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|item| item.to_string()).collect()
}
