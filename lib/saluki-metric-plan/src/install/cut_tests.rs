//! Cut installations through the installer core, with the actions and states expected after each step written by hand.
//!
//! No registry of this stage places a state owner in an isolation slot, so the core is told which candidates are cut
//! installations ([`InstallerCore::classify_cuts_with`]): a candidate whose listener filterlist differs from the active
//! plan's, where either is `cut-<widths>` (such as `cut-10-30`), changes the state owner `dsd_agg` between the widths
//! the two name, and is armed over admission point 0; a marker with `unknown` in it names a width that is not known.
//! Every other part of the core runs as in production: compilation into the stage 3 frame, preparation, the artifact
//! paths, and the lifecycle of every generation.

use std::{collections::BTreeSet, time::Duration};

use saluki_core::execution::{AdmissionPointId, AdmissionTime, GenerationId, GenerationRef};
use serde_json::{json, Value};

use super::{
    artifact_tests::{policy, program, snapshot, Policy, TestComposer, BOOT_POST_AGGREGATION},
    diff::bind_lineages,
    tests::{boot_plan, candidate, describe, edited, node_mut, Harness},
    *,
};
use crate::{
    canonical::PlanHash,
    channel::ApplyState,
    compile::{Frame, LineageKey, MissingCapability, PlacementError, SlotId},
    extension::TransitionClass,
    ir::{NodeId, Plan},
    limits::values::{CUT_ARM_ATTEMPTS, CUT_ARM_RETRY, CUT_MARGIN},
    operators::OperatorRegistry,
    testing::default_program_slot_name,
};

/// The listener filterlist of the boot default program.
const BOOT_LISTENER: &str = "tenant.raw.blocked";

/// Why the test host fails a generation under supervision.
const PANICS: &str = "3 panics in 60 seconds";

/// The cut time the tests arm cuts at, in seconds since the Unix epoch.
const T: u64 = 1_700_000_100;

fn id(id: &str) -> NodeId {
    id.parse().unwrap()
}

fn g(generation: u64) -> GenerationId {
    GenerationId::new(generation)
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|item| item.to_string()).collect()
}

/// Returns the listener filterlist of a plan made by [`program`], or of the boot plan.
fn listener(plan: &Plan) -> String {
    plan.node(&id("dsd_prefix_filter_blocklist"))
        .and_then(|node| node.params.get("predicate"))
        .and_then(|predicate| predicate.pointer("/intake_name/matches/values/resolved/0"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The bucket widths a `cut-<widths>` marker names.
fn widths(marker: &str) -> Vec<Duration> {
    if !marker.starts_with("cut") {
        return Vec::new();
    }
    marker
        .split('-')
        .skip(1)
        .filter_map(|secs| secs.parse().ok())
        .map(Duration::from_secs)
        .collect()
}

/// Classifies a candidate as the module docs say.
fn classify(active: &Plan, candidate: &Plan) -> Option<(CutPlan, Vec<AdmissionPointId>)> {
    let (old, new) = (listener(active), listener(candidate));
    if old == new || !(old.starts_with("cut") || new.starts_with("cut")) {
        return None;
    }
    let mut widths = [widths(&old), widths(&new)].concat();
    widths.sort_unstable();
    widths.dedup();
    let unknown_widths = if new.contains("unknown") {
        vec![id("dsd_agg")]
    } else {
        Vec::new()
    };
    let plan = CutPlan {
        owners: vec![id("dsd_agg")],
        widths,
        unknown_widths,
    };
    Some((plan, vec![AdmissionPointId::new(0)]))
}

fn artifact(revision: u64) -> Origin {
    Origin::Artifact {
        revision,
        hash: format!("sha256:{}", "a".repeat(64)).parse().unwrap(),
    }
}

fn at(secs: u64) -> AdmissionTime {
    AdmissionTime::from_unix_secs(secs)
}

/// What the schedule of the topology holds, as the host performs the core's actions.
enum Schedule {
    Steady(GenerationRef),
    Cut { old: GenerationRef, new: GenerationRef },
}

impl Schedule {
    fn describe(&self) -> String {
        match self {
            Self::Steady(generation) => format!("steady {}", generation.id()),
            Self::Cut { old, new } => format!("cut {} -> {}", old.id(), new.id()),
        }
    }
}

/// The host around the core: it performs the core's actions as a host's installer worker does, with a schedule that
/// holds what the commits, cuts, and cancellations stored, a composer over the configuration, and regions that report
/// what the tests say the cut actions did.
struct Host {
    harness: Harness,
    composer: TestComposer,
    schedule: Schedule,
    /// The latest arming asked for, until its generation is aborted.
    arming: Option<ArmCut>,
    /// References that keep generations alive, as a retiring lineage keeps the generation it served.
    held: Vec<GenerationRef>,
    changes: u64,
    /// The listener filterlist of the default program the host lowers when the core asks for it.
    default_marker: &'static str,
}

impl Host {
    fn new() -> Self {
        Self::with_config(InstallerConfig::default())
    }

    fn with_config(config: InstallerConfig) -> Self {
        let mut harness = Harness::new(config);
        harness.core.classify_cuts_with(classify);
        let boot = harness.boot.take().expect("the harness boots").generation;
        Self {
            harness,
            composer: TestComposer::new(),
            schedule: Schedule::Steady(boot),
            arming: None,
            held: Vec::new(),
            changes: 0,
            default_marker: BOOT_LISTENER,
        }
    }

    fn core(&mut self) -> &mut InstallerCore {
        &mut self.harness.core
    }

    /// Hands the core the default program lowered after a configuration change, with the listener filterlist
    /// `marker`.
    fn lower(&mut self, marker: &str) -> Vec<String> {
        let actions = self.lowered(marker);
        self.perform(actions)
    }

    fn lowered(&mut self, marker: &str) -> Vec<Action> {
        self.changes += 1;
        let origin = Origin::DefaultProgram {
            config_change: self.changes,
        };
        let plan = program(marker, self.composer.configuration.get());
        self.harness
            .core
            .on_default_program(candidate(plan, origin), &self.composer)
            .1
    }

    /// The configuration changes the post-aggregation filter, which composes the policy in effect again.
    fn configure(&mut self, post_aggregation: &'static str) -> Vec<String> {
        self.composer.configuration.set(post_aggregation);
        self.lower(BOOT_LISTENER)
    }

    /// Proposes an artifact of revision `revision` whose plan has the listener filterlist `marker`, directly.
    fn propose(&mut self, marker: &str, revision: u64) -> Vec<String> {
        let plan = program(marker, self.composer.configuration.get());
        let (_, actions) = self.core().propose(candidate(plan, artifact(revision)));
        self.perform(actions)
    }

    /// The channel delivers a snapshot of these configs.
    fn deliver(&mut self, policies: &[Policy]) -> Vec<String> {
        let actions = self.harness.core.on_snapshot(&snapshot(policies), &self.composer);
        self.perform(actions)
    }

    /// Every region acknowledges the generation being prepared.
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

    /// The latest arming asked for publishes the cut at `secs`. The host keeps no reference to the generations but the
    /// schedule's, as a host's installer worker does once it performed the arming.
    fn arm(&mut self, secs: u64) -> Vec<String> {
        let arming = self.arming.take().expect("an arming was asked for");
        self.schedule = Schedule::Cut {
            old: arming.old.clone(),
            new: arming.generation.clone(),
        };
        let actions = self.core().cut_armed(arming.generation.id(), at(secs));
        self.perform(actions)
    }

    /// The latest arming asked for is put off.
    fn defer(&mut self, reason: CutDeferReason) -> Vec<String> {
        let generation = self.arming.as_ref().expect("an arming was asked for").generation.id();
        let actions = self.core().cut_deferred(generation, reason);
        self.perform(actions)
    }

    /// A poll of the admission points says whether some point stamped the cut generation, and whether the affected
    /// point passed the cut.
    fn poll(&mut self, stamped_new: bool, passed: bool) -> Vec<String> {
        let generation = self.cut_generation();
        let progress = CutProgress {
            stamped_new,
            passed: usize::from(passed),
            affected: 1,
        };
        let actions = self.core().cut_progress(generation, progress);
        self.perform(actions)
    }

    /// The cancellation asked for succeeds, or is refused.
    fn cancel(&mut self, succeeds: bool) -> Vec<String> {
        let generation = self.cut_generation();
        let actions = if succeeds {
            if let Schedule::Cut { old, .. } = &self.schedule {
                self.schedule = Schedule::Steady(old.clone());
            }
            self.core().cut_cancelled(generation)
        } else {
            self.core().cut_cancel_refused(generation)
        };
        self.perform(actions)
    }

    fn fail(&mut self, generation: u64) -> Vec<String> {
        let actions = self.core().generation_failed(g(generation), PANICS);
        self.perform(actions)
    }

    fn retry(&mut self) -> Vec<String> {
        let actions = self.core().retry();
        self.perform(actions)
    }

    /// The generations stay alive while something other than the core and the schedule references them.
    fn hold(&mut self) {
        if let Schedule::Cut { old, .. } = &self.schedule {
            self.held.push(old.clone());
        }
    }

    fn release(&mut self) -> Vec<String> {
        self.held.clear();
        self.perform(Vec::new())
    }

    fn cut_generation(&mut self) -> GenerationId {
        self.core()
            .cut_in_flight()
            .expect("a cut is in flight")
            .generation()
            .id()
    }

    /// Performs actions, and those that performing them and delivering the retirements they cause leads to, returning
    /// a description of every action performed.
    fn perform(&mut self, mut actions: Vec<Action>) -> Vec<String> {
        let mut performed = Vec::new();
        loop {
            let mut next = Vec::new();
            for action in actions {
                performed.push(describe(&action));
                match action {
                    Action::Commit { generation } | Action::FinalizeCut { generation } => {
                        self.schedule = Schedule::Steady(generation);
                    }
                    Action::ArmCut(arming) => self.arming = Some(arming),
                    Action::Abort { generation, .. } => {
                        // Regions drop an aborted generation, as the host does its arming.
                        if self
                            .arming
                            .as_ref()
                            .is_some_and(|arming| arming.generation.id() == generation)
                        {
                            self.arming = None;
                        }
                    }
                    Action::InstallDefaultProgram => next.extend(self.lowered(self.default_marker)),
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

    /// Describes the core's state in one line: the active generation (`!` when it failed), what is in flight and kept,
    /// the cut's phase, the plan a failed cut generation rolls back to, and the schedule.
    fn summary(&mut self) -> String {
        let status = self.core().status();
        let state = |record: &GenerationRecord| {
            let failed = if record.state == GenerationState::Failed {
                "!"
            } else {
                ""
            };
            format!("g{}{}", record.id, failed)
        };
        let mut line = status.active.as_ref().map_or_else(|| "none".to_string(), state);
        if let Some(preparing) = &status.preparing {
            line.push_str(&format!(" preparing g{}", preparing.id));
        }
        if let Some((candidate, _)) = status.pending {
            line.push_str(&format!(" pending c{}", candidate));
        }
        if let Some(kept) = &status.last_known_good {
            line.push_str(&format!(" kept g{}", kept.id));
        }
        if let Some(cut) = &status.cut {
            let phase = match cut.phase {
                CutPhase::Barrier => "barrier".to_string(),
                CutPhase::Armed { at } => format!("armed at {}", at.as_unix_secs()),
                CutPhase::Irrevocable { at } => format!("irrevocable at {}", at.as_unix_secs()),
            };
            line.push_str(&format!(" cut g{} {}", cut.generation, phase));
        }
        if let Some(rollback) = self.core().rollback_candidate() {
            line.push_str(&format!(" rollback {}", listener(&rollback.plan)));
        }
        line.push_str(&format!(" | {}", self.schedule.describe()));
        line
    }

    /// Returns the acknowledgment of the `demo` config: its state, generation, and cut time in seconds.
    fn demo(&mut self) -> (ApplyState, Option<u64>, Option<u64>) {
        let ack = self
            .core()
            .config_states()
            .into_iter()
            .find(|ack| ack.id.as_str() == "demo")
            .expect("the demo config is acknowledged");
        (ack.state, ack.generation, ack.cut_at.map(|millis| millis / 1_000))
    }
}

/// Describes the preparation of `generation` by a candidate that changes only the listener filterlist.
fn prepare(generation: u64) -> String {
    format!(
        "prepare {} [0:unchanged reuse 1 build 0, 1:changed reuse 1 build 0, 2:unchanged reuse 2 build 0, 3:unchanged \
         reuse 1 build 0, 4:unchanged reuse 0 build 0, 5:unchanged reuse 2 build 0]",
        generation
    )
}

fn arm(generation: u64) -> String {
    format!("arm the cut of {} over [AdmissionPointId(0)]", generation)
}

fn watch(generation: u64) -> String {
    format!("watch the cut of {} at {}", generation, at(T))
}

fn abort(generation: u64) -> String {
    format!("abort {} in 6 slots", generation)
}

/// Why the core rejects a generation a newer candidate cancelled.
const CANCELLED: &str = "rejected (a newer candidate cancelled the preparation)";

/// Why the core rejects a generation the channel's revocation dropped.
const REVOKED: &str = "rejected (the channel revoked the policy before the artifact was installed)";

// The classifier and the state bindings.

#[test]
fn state_owners_are_classified_with_their_lineages_and_widths() {
    let registry = OperatorRegistry::native();
    let window =
        |window: &str| edited(|value| node_mut(value, "dsd_agg")["params"]["window"]["resolved"] = json!(window));
    let owner = |diff: &PlanDiff, node: &str| {
        diff.state_owners
            .iter()
            .find(|owner| owner.node == id(node))
            .cloned()
            .unwrap()
    };
    let secs = |secs| Some(Duration::from_secs(secs));

    // A width change is a cut of a kept owner between two lineages.
    let changes = diff(&boot_plan(), &window("30s"), &registry);
    let aggregation = owner(&changes, "dsd_agg");
    assert_eq!(
        (aggregation.class, aggregation.change, aggregation.widths),
        (TransitionClass::Cut, StateOwnerChange::Kept, (secs(10), secs(30)))
    );
    assert!(aggregation.lineage.0.is_some() && aggregation.lineage.1.is_some());
    assert_ne!(aggregation.lineage.0, aggregation.lineage.1);
    let cut = changes.cut().unwrap();
    assert_eq!(cut.owners, vec![id("dsd_agg")]);
    assert_eq!(cut.widths, vec![Duration::from_secs(10), Duration::from_secs(30)]);
    assert!(cut.unknown_widths.is_empty());

    // Another flush interval keeps the state: the time contract is the window.
    let flushed = edited(|value| node_mut(value, "dsd_agg")["params"]["flush_interval"]["resolved"] = json!("30s"));
    let changes = diff(&boot_plan(), &flushed, &registry);
    let aggregation = owner(&changes, "dsd_agg");
    assert_eq!(
        (aggregation.class, aggregation.change),
        (TransitionClass::Retain, StateOwnerChange::Kept)
    );
    assert_eq!(changes.cut(), None);

    // An added aggregation is a cut: its lineage starts at the cut.
    let added = edited(|value| {
        let mut aggregation = node_mut(value, "dsd_agg").clone();
        aggregation["id"] = json!("policy/agg");
        value["nodes"].as_array_mut().unwrap().push(aggregation);
    });
    let changes = diff(&boot_plan(), &added, &registry);
    let aggregation = owner(&changes, "policy/agg");
    assert_eq!(
        (aggregation.class, aggregation.change, aggregation.widths),
        (TransitionClass::Cut, StateOwnerChange::Added, (None, secs(10)))
    );
    assert!(aggregation.lineage.0.is_none() && aggregation.lineage.1.is_some());
    assert_eq!(owner(&changes, "dsd_agg").class, TransitionClass::Retain);
    assert_eq!(changes.cut().unwrap().owners, vec![id("policy/agg")]);

    // A removed one is a cut too, listed after the candidate's: its lineage ends at the cut.
    let changes = diff(&added, &boot_plan(), &registry);
    let aggregation = changes.state_owners.last().unwrap();
    assert_eq!(aggregation.node, id("policy/agg"));
    assert_eq!(
        (aggregation.class, aggregation.change, aggregation.widths),
        (TransitionClass::Cut, StateOwnerChange::Removed, (secs(10), None))
    );

    // Another operator cannot take the state over, even one without state.
    let replaced = edited(|value| {
        let aggregation = node_mut(value, "dsd_agg");
        aggregation["op"] = json!("drop");
        aggregation.as_object_mut().unwrap().remove("params");
    });
    let changes = diff(&boot_plan(), &replaced, &registry);
    assert_eq!(owner(&changes, "dsd_agg").class, TransitionClass::Reject);
    assert_eq!(changes.cut(), None, "a rejected owner leaves no cut to make");

    // A width the plan does not resolve is not known.
    let unresolved = edited(|value| {
        node_mut(value, "dsd_agg")["params"]["window"]
            .as_object_mut()
            .unwrap()
            .remove("resolved");
    });
    let changes = diff(&boot_plan(), &unresolved, &registry);
    let aggregation = owner(&changes, "dsd_agg");
    assert_eq!(
        (aggregation.class, aggregation.widths),
        (TransitionClass::Cut, (secs(10), None))
    );
    let cut = changes.cut().unwrap();
    assert_eq!(
        (cut.widths, cut.unknown_widths),
        (vec![Duration::from_secs(10)], vec![id("dsd_agg")])
    );
}

#[test]
fn a_frame_that_readmits_records_cannot_be_cut_over() {
    // The stage 3 frame readmits the records of its opaque hosts: aggregation and the gateways.
    let frame = Frame::compute(&boot_plan(), &OperatorRegistry::native(), &default_program_slot_name).unwrap();
    let cut = CutPlan {
        owners: vec![id("dsd_agg")],
        widths: vec![Duration::from_secs(10)],
        unknown_widths: Vec::new(),
    };
    let Err(PlacementError::Unplaceable { problems }) = cut.check_frame(&frame) else {
        panic!("the frame readmits records")
    };
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].node, Some(id("dsd_agg")));
    assert_eq!(problems[0].capability, Some(MissingCapability::State));
    assert!(
        problems[0].message.contains("readmits the records of host component"),
        "{}",
        problems[0].message
    );
}

fn key(node: &str, contract: &str) -> LineageKey {
    LineageKey {
        node: id(node),
        contract: PlanHash::of_bytes(contract.as_bytes()),
    }
}

#[test]
fn state_owners_bind_the_active_instance_a_new_one_or_retire_the_old_one() {
    let retain = |node: &str, contract: &str, from: u64| StateBinding::Retain {
        node: id(node),
        key: key(node, contract),
        from: g(from),
    };
    let new = |node: &str, contract: &str| StateBinding::New {
        node: id(node),
        key: key(node, contract),
    };
    let retire = |node: &str, contract: &str, from: u64| StateBinding::Retire {
        node: id(node),
        key: key(node, contract),
        from: g(from),
    };
    let cut = |nodes: &[&str]| nodes.iter().map(|node| id(node)).collect::<BTreeSet<_>>();
    // (what, active generation and lineages, candidate lineages, affected owners, bindings)
    type Case = (
        &'static str,
        Option<(u64, Vec<LineageKey>)>,
        Vec<LineageKey>,
        BTreeSet<NodeId>,
        Vec<StateBinding>,
    );
    let cases: Vec<Case> = vec![
        (
            "nothing is active",
            None,
            vec![key("agg", "10s")],
            cut(&[]),
            vec![new("agg", "10s")],
        ),
        (
            "a retained lineage",
            Some((1, vec![key("agg", "10s")])),
            vec![key("agg", "10s")],
            cut(&[]),
            vec![retain("agg", "10s", 1)],
        ),
        (
            "a width change",
            Some((1, vec![key("agg", "10s")])),
            vec![key("agg", "30s")],
            cut(&["agg"]),
            vec![new("agg", "30s"), retire("agg", "10s", 1)],
        ),
        (
            "an added owner",
            Some((2, vec![key("agg", "10s")])),
            vec![key("agg", "10s"), key("policy/agg", "10s")],
            cut(&["policy/agg"]),
            vec![retain("agg", "10s", 2), new("policy/agg", "10s")],
        ),
        (
            "a removed owner",
            Some((2, vec![key("agg", "10s"), key("policy/agg", "10s")])),
            vec![key("agg", "10s")],
            cut(&["policy/agg"]),
            vec![retain("agg", "10s", 2), retire("policy/agg", "10s", 2)],
        ),
        (
            // A width restored while the lineage of that width still retires: the new lineage has the same key as the
            // retiring one, and binds a new instance, while the lineage of the replaced width retires.
            "a width round trip",
            Some((3, vec![key("agg", "5s")])),
            vec![key("agg", "10s")],
            cut(&["agg"]),
            vec![new("agg", "10s"), retire("agg", "5s", 3)],
        ),
        (
            "a lineage whose key changed without a cut",
            Some((1, vec![key("agg", "10s")])),
            vec![key("agg", "other")],
            cut(&[]),
            vec![new("agg", "other")],
        ),
    ];
    for (what, active, candidate, cut, expected) in cases {
        let active = active.map(|(generation, keys)| (g(generation), keys));
        assert_eq!(bind_lineages(active, &candidate, &cut), expected, "{}", what);
    }
}

// The cut protocol.

#[test]
fn a_cut_installation_is_armed_at_its_barrier_irrevocable_at_the_first_new_stamp_and_finalized_once_every_point_passed()
{
    let mut host = Host::new();
    assert_eq!(
        host.lower("cut-10-30"),
        strings(&[&prepare(2), "candidate 2 generation 2: pending"])
    );

    // At the barrier the generation is prepared, and arming is asked for instead of a commit. The arming locks the
    // affected point, and names the generation it replaces, the widths, and the margin.
    assert_eq!(
        host.acknowledge(),
        strings(&["candidate 2 generation 2: prepared", &arm(2)])
    );
    let arming = host.arming.as_ref().unwrap();
    assert_eq!(arming.old.id(), g(1));
    assert_eq!(arming.widths, vec![Duration::from_secs(10), Duration::from_secs(30)]);
    assert_eq!(arming.affected, vec![AdmissionPointId::new(0)]);
    assert_eq!(arming.margin, CUT_MARGIN);
    assert_eq!(host.summary(), "g1 cut g2 barrier | steady 1");
    // The preparation is over: its timeout no longer applies.
    assert!(host.core().prepare_timed_out(g(2)).is_empty());
    assert_eq!(host.core().status().preparing, None);

    // Armed: the old generation stays active, and the caller watches the cut.
    assert_eq!(host.arm(T), strings(&[&watch(2)]));
    assert_eq!(host.summary(), format!("g1 cut g2 armed at {} | cut 1 -> 2", T));

    // A poll in which nothing stamped the generation changes nothing.
    assert!(host.poll(false, false).is_empty());

    // The first stamp of the generation makes the cut irrevocable: the generation is active, and the one it replaced
    // drains until the cut is finalized: the schedule still references it.
    assert_eq!(
        host.poll(true, false),
        strings(&["candidate 2 generation 2: active", "candidate 1 generation 1: draining"])
    );
    assert_eq!(host.summary(), format!("g2 cut g2 irrevocable at {} | cut 1 -> 2", T));
    let status = host.core().cut_in_flight().unwrap().status();
    assert_eq!((status.stamped_new, status.passed, status.total), (true, 0, 1));

    // Every affected point passed the cut: finalization stores the generation alone, which retires the old one.
    assert_eq!(
        host.poll(true, true),
        strings(&["finalize the cut of 2", "candidate 1 generation 1: retired"])
    );
    assert_eq!(host.summary(), "g2 | steady 2");
    assert_eq!(host.core().status().live, vec![g(2)]);
    // A default program's generation was replaced: there is no plan to roll back to.
    assert!(host.core().rollback_candidate().is_none());
}

#[test]
fn arming_is_put_off_and_retried_and_a_clock_ahead_is_not_an_attempt() {
    let mut host = Host::new();
    host.lower("cut-10");
    host.acknowledge();
    let deferred = |reason: &str, attempts: u32| {
        strings(&[
            &format!(
                "candidate 2 generation 2: pending (CutDeferred {{ reason: {}, attempts: {} }})",
                reason, attempts
            ),
            &format!("retry after {:?}", CUT_ARM_RETRY),
        ])
    };
    assert_eq!(host.defer(CutDeferReason::Timeout), deferred("Timeout", 1));
    assert_eq!(
        host.core().status().cut.unwrap().deferred,
        Some((CutDeferReason::Timeout, 1))
    );
    // The retry arms again; another event before it does not.
    assert_eq!(host.lower("cut-10"), strings(&["candidate 3: unchanged"]));
    assert_eq!(host.retry(), strings(&[&arm(2)]));
    assert!(host.retry().is_empty(), "the arming is asked for once per deferral");

    let ahead = CutDeferReason::ClockAhead {
        by: Duration::from_secs(5),
    };
    assert_eq!(host.defer(ahead), deferred("ClockAhead { by: 5s }", 1));

    // A default program is never given up on: native configuration installs eventually.
    for attempts in 2..=u32::try_from(CUT_ARM_ATTEMPTS).unwrap() + 1 {
        host.retry();
        assert_eq!(host.defer(CutDeferReason::Timeout), deferred("Timeout", attempts));
    }
    host.retry();
    assert_eq!(host.arm(T), strings(&[&watch(2)]));
    assert_eq!(host.core().status().cut.unwrap().deferred, None);
}

#[test]
fn an_artifact_whose_cut_cannot_be_armed_in_ten_attempts_is_rejected() {
    let mut host = Host::new();
    host.propose("cut-10", 1);
    host.acknowledge();
    for _ in 1..CUT_ARM_ATTEMPTS {
        host.defer(CutDeferReason::Timeout);
        assert_eq!(host.retry(), strings(&[&arm(2)]));
    }
    assert_eq!(
        host.defer(CutDeferReason::Timeout),
        strings(&[
            &abort(2),
            "candidate 2 generation 2: rejected (the cut could not be armed: the admission points upstream of the \
             changed state owners stayed busy through 10 attempts in a row; propose the change again when intake is \
             lighter)",
        ])
    );
    assert_eq!(host.summary(), "g1 | steady 1");
    assert!(host.retry().is_empty());
}

#[test]
fn widths_that_line_up_too_rarely_or_are_not_known_are_rejected_before_preparing() {
    let mut host = Host::new();
    // 120 s and 180 s line up every 360 s, more than a cut may wait for.
    let actions = host.lower("cut-120-180");
    assert_eq!(actions.len(), 1);
    assert!(
        actions[0].starts_with("candidate 2: rejected (Bucket widths ")
            && actions[0].contains("line up only every 360 seconds, more than the 300 seconds a cut may wait for")
            && actions[0].contains("Change through an intermediate width of at most 60 seconds."),
        "{}",
        actions[0]
    );
    assert_eq!(
        host.lower("cut-unknown"),
        strings(&[
            "candidate 3: rejected (state owner `dsd_agg` switches its state at a cut, but its bucket width is not \
             known, so no cut time lines up with it: resolve the binding of its window)"
        ])
    );
    // 10 s and 15 s line up every 30 s.
    assert_eq!(
        host.lower("cut-10-15"),
        strings(&[&prepare(2), "candidate 4 generation 2: pending"])
    );
    assert_eq!(host.core().status().live, vec![g(1), g(2)]);
}

#[test]
fn an_irrevocable_cut_releases_the_last_known_good_generation_and_keeps_its_plan_to_roll_back_to() {
    let mut host = Host::new();
    host.propose("a", 1);
    host.acknowledge();
    assert_eq!(host.summary(), "g2 kept g1 | steady 2");
    host.propose("cut-10-30", 2);
    host.acknowledge();
    host.arm(T);
    // While the cut can be cancelled, the generation kept stays kept.
    assert_eq!(host.summary(), format!("g2 kept g1 cut g3 armed at {} | cut 2 -> 3", T));

    assert_eq!(
        host.poll(true, false),
        strings(&[
            "candidate 3 generation 3: active",
            "candidate 2 generation 2: draining",
            "candidate 1 generation 1: draining",
            "candidate 1 generation 1: retired",
        ])
    );
    assert_eq!(
        host.summary(),
        format!("g3 cut g3 irrevocable at {} rollback a | cut 2 -> 3", T)
    );
    let rollback = host.core().rollback_candidate().unwrap();
    assert_eq!(rollback.origin, artifact(1));
    assert!(rollback.report.is_valid());
    assert_eq!(
        host.poll(true, true),
        strings(&["finalize the cut of 3", "candidate 2 generation 2: retired"])
    );

    // The finalized cut generation fails: its replaced plan is installed again, under its own revision, with a cut of
    // its own, since its lineages differ.
    assert_eq!(
        host.fail(3),
        strings(&[
            "candidate 3 generation 3: failed",
            &prepare(4),
            "candidate 4 generation 4: pending"
        ])
    );
    assert_eq!(host.core().status().preparing.unwrap().origin, artifact(1));
    assert_eq!(host.core().status().revision_high_water, Some(2));
    assert!(host.core().rollback_candidate().is_none());
    assert_eq!(
        host.acknowledge(),
        strings(&["candidate 4 generation 4: prepared", &arm(4)])
    );

    // A rollback is never given up on, although it is an artifact's: its cut is armed again however often it is put
    // off.
    for attempts in 1..=u32::try_from(CUT_ARM_ATTEMPTS).unwrap() + 1 {
        let deferred = host.defer(CutDeferReason::Timeout);
        assert_eq!(
            deferred[0],
            format!(
                "candidate 4 generation 4: pending (CutDeferred {{ reason: Timeout, attempts: {} }})",
                attempts
            )
        );
        assert_eq!(host.retry(), strings(&[&arm(4)]));
    }
    assert_eq!(
        host.arm(T + 20),
        strings(&[&format!("watch the cut of 4 at {}", at(T + 20))])
    );
}

#[test]
fn a_newer_artifact_cancels_an_armed_cut_or_waits_for_it_when_the_cancellation_is_refused() {
    for succeeds in [true, false] {
        let mut host = Host::new();
        host.lower("cut-10");
        host.acknowledge();
        host.arm(T);
        assert_eq!(
            host.propose("b", 1),
            strings(&["cancel the cut of 2", "candidate 3: pending (CutInProgress)"])
        );
        // The cancellation is asked for once.
        assert_eq!(
            host.propose("c", 2),
            strings(&["candidate 3: superseded", "candidate 4: pending (CutInProgress)"])
        );
        if succeeds {
            assert_eq!(
                host.cancel(true),
                strings(&[
                    &abort(2),
                    &format!("candidate 2 generation 2: {}", CANCELLED),
                    &prepare(3),
                    "candidate 4 generation 3: pending",
                ])
            );
            assert_eq!(host.summary(), "g1 preparing g3 | steady 1");
        } else {
            assert_eq!(
                host.cancel(false),
                strings(&["candidate 2 generation 2: active", "candidate 1 generation 1: draining"])
            );
            assert_eq!(
                host.summary(),
                format!("g2 pending c4 cut g2 irrevocable at {} | cut 1 -> 2", T)
            );
            // The newer artifact is prepared once the cut is finalized, against the cut generation.
            assert_eq!(
                host.poll(true, true),
                strings(&[
                    "finalize the cut of 2",
                    &prepare(3),
                    "candidate 4 generation 3: pending",
                    "candidate 1 generation 1: retired",
                ])
            );
        }
    }
}

#[test]
fn a_candidate_equal_to_the_active_plan_that_cancels_an_armed_cut_creates_no_generation() {
    // Proposed directly: once the cut is cancelled, the schedule holds the plan the candidate equals.
    let mut host = Host::new();
    host.propose("cut-10", 1);
    host.acknowledge();
    host.arm(T);
    assert_eq!(
        host.propose(BOOT_LISTENER, 2),
        strings(&["cancel the cut of 2", "candidate 3: pending (CutInProgress)"])
    );
    assert_eq!(
        host.cancel(true),
        strings(&[
            &abort(2),
            &format!("candidate 2 generation 2: {}", CANCELLED),
            "candidate 3: unchanged",
        ])
    );
    assert_eq!(host.summary(), "g1 | steady 1");
    assert_eq!(host.core().status().live, vec![g(1)]);

    // Through the channel: the active generation serves the artifact, as it does one that cancels a preparation.
    let mut host = Host::new();
    host.deliver(&[policy(1, "cut-10")]);
    host.acknowledge();
    host.arm(T);
    host.deliver(&[policy(2, "unmodified")]);
    assert_eq!(host.demo(), (ApplyState::Pending, None, None));
    host.cancel(true);
    assert_eq!(host.summary(), "g1 | steady 1");
    assert_eq!(host.demo(), (ApplyState::Active, Some(1), None));
}

#[test]
fn a_revoked_artifact_whose_replacement_waits_for_the_cut_says_it_is_active_until_replaced() {
    let error = |host: &mut Host| host.core().config_states().into_iter().find_map(|ack| ack.error);
    let mut host = Host::new();
    host.deliver(&[policy(1, "cut-10")]);
    host.acknowledge();
    host.arm(T);
    host.poll(true, false);
    assert_eq!(error(&mut host), None);

    // The default program waits for the finalization, up to the whole wait of the cut: the artifact says why it runs.
    host.deliver(&[]);
    assert_eq!(host.demo(), (ApplyState::Active, Some(2), Some(T)));
    assert_eq!(error(&mut host).as_deref(), Some("revoked; active until replaced"));
    // It keeps saying so while it stays active, and until its generation retires.
    host.poll(true, true);
    assert_eq!(host.demo(), (ApplyState::Active, Some(2), None));
    assert_eq!(error(&mut host).as_deref(), Some("revoked; active until replaced"));
    host.acknowledge();
    host.arm(T + 10);
    host.poll(true, false);
    assert_eq!(error(&mut host).as_deref(), Some("revoked; active until replaced"));
    host.poll(true, true);
    assert_eq!(host.demo(), (ApplyState::Retired, Some(2), None));
    assert_eq!(error(&mut host), None);

    // A cancellation refused after the revocation leaves it active for the cut's wait too.
    let mut host = Host::new();
    host.deliver(&[policy(1, "cut-10")]);
    host.acknowledge();
    host.arm(T);
    host.deliver(&[]);
    host.cancel(false);
    assert_eq!(host.demo(), (ApplyState::Active, Some(2), Some(T)));
    assert_eq!(error(&mut host).as_deref(), Some("revoked; active until replaced"));
}

#[test]
fn a_newer_artifact_aborts_a_cut_that_is_not_armed_yet() {
    let mut host = Host::new();
    host.lower("cut-10");
    host.acknowledge();
    host.defer(CutDeferReason::Timeout);
    assert_eq!(
        host.propose("b", 1),
        strings(&[
            &abort(2),
            &format!("candidate 2 generation 2: {}", CANCELLED),
            &prepare(3),
            "candidate 3 generation 3: pending",
        ])
    );
    // The deferred arming is not asked for again.
    assert!(host.retry().is_empty());
    assert_eq!(host.summary(), "g1 preparing g3 | steady 1");
}

#[test]
fn every_other_candidate_waits_for_the_cut_in_every_phase_and_one_cut_runs_at_a_time() {
    let mut host = Host::new();
    host.lower("cut-10");
    host.acknowledge();
    assert_eq!(host.lower("x"), strings(&["candidate 3: pending (CutInProgress)"]));
    host.arm(T);
    assert_eq!(
        host.lower("y"),
        strings(&["candidate 3: superseded", "candidate 4: pending (CutInProgress)"])
    );
    host.poll(true, false);
    // Another cut waits too.
    assert_eq!(
        host.lower("cut-30"),
        strings(&["candidate 4: superseded", "candidate 5: pending (CutInProgress)"])
    );
    assert_eq!(
        host.poll(true, true),
        strings(&[
            "finalize the cut of 2",
            &prepare(3),
            "candidate 5 generation 3: pending",
            "candidate 1 generation 1: retired",
        ])
    );
    assert_eq!(
        host.acknowledge(),
        strings(&["candidate 5 generation 3: prepared", &arm(3)])
    );
    assert_eq!(
        host.arming.as_ref().unwrap().widths,
        vec![Duration::from_secs(10), Duration::from_secs(30)]
    );
}

#[test]
fn the_replaced_generation_counts_against_the_overlap_budget_until_its_last_lineage_retires() {
    let mut host = Host::with_config(InstallerConfig {
        overlap_budget: 2,
        ..InstallerConfig::default()
    });
    host.lower("cut-10");
    host.acknowledge();
    host.arm(T);
    host.poll(true, false);
    // The retiring lineage holds the generation it served until its final flush.
    host.hold();
    assert_eq!(host.poll(true, true), strings(&["finalize the cut of 2"]));
    assert_eq!(
        host.lower("x"),
        strings(&["candidate 3: pending (OverlapBudget { live: 2, budget: 2 })"])
    );
    assert_eq!(
        host.release(),
        strings(&[
            "candidate 1 generation 1: retired",
            &prepare(3),
            "candidate 3 generation 3: pending"
        ])
    );
}

#[test]
fn the_cut_generation_failing_aborts_cancels_or_rolls_back_by_phase() {
    // Before the cut is armed, it is aborted.
    let mut host = Host::new();
    host.propose("cut-10", 1);
    host.acknowledge();
    assert_eq!(host.fail(2), strings(&[&abort(2), "candidate 2 generation 2: failed"]));
    assert_eq!(host.summary(), "g1 | steady 1");

    // Armed, it is cancelled, and failed once the cancellation succeeds.
    let mut host = Host::new();
    host.propose("cut-10", 1);
    host.acknowledge();
    host.arm(T);
    assert_eq!(host.fail(2), strings(&["cancel the cut of 2"]));
    assert_eq!(
        host.cancel(true),
        strings(&[&abort(2), "candidate 2 generation 2: failed"])
    );
    assert_eq!(host.summary(), "g1 | steady 1");

    // A refused cancellation makes it active and failed; it keeps running until the cut is finalized, and the default
    // program it replaced is then asked for.
    let mut host = Host::new();
    host.propose("cut-10", 1);
    host.acknowledge();
    host.arm(T);
    host.fail(2);
    assert_eq!(
        host.cancel(false),
        strings(&[
            "candidate 2 generation 2: active",
            "candidate 1 generation 1: draining",
            "candidate 2 generation 2: failed",
        ])
    );
    assert_eq!(host.summary(), format!("g2! cut g2 irrevocable at {} | cut 1 -> 2", T));
    assert_eq!(
        host.poll(true, true),
        strings(&[
            "finalize the cut of 2",
            "install the default program",
            &prepare(3),
            "candidate 3 generation 3: pending",
            "candidate 1 generation 1: retired",
        ])
    );

    // Irrevocable, it keeps processing its input, and the plan the cut replaced waits to be installed again.
    let mut host = Host::new();
    host.propose("a", 1);
    host.acknowledge();
    host.propose("cut-10", 2);
    host.acknowledge();
    host.arm(T);
    host.poll(true, false);
    assert_eq!(
        host.fail(3),
        strings(&[
            "candidate 3 generation 3: failed",
            "candidate 4: pending (CutInProgress)"
        ])
    );
    assert_eq!(
        host.summary(),
        format!("g3! pending c4 cut g3 irrevocable at {} | cut 2 -> 3", T)
    );
    assert_eq!(
        host.poll(true, true),
        strings(&[
            "finalize the cut of 3",
            &prepare(4),
            "candidate 4 generation 4: pending",
            "candidate 2 generation 2: retired",
        ])
    );
    assert_eq!(host.core().status().preparing.unwrap().origin, artifact(1));
}

#[test]
fn the_replaced_generation_failing_rolls_back_and_prepares_the_cut_again_or_is_recorded_once_irrevocable() {
    let start = || {
        let mut host = Host::new();
        host.propose("a", 1);
        host.acknowledge();
        host.propose("cut-10", 2);
        host.acknowledge();
        host
    };
    // Before the cut is armed: the cut is aborted, the failed generation rolls back, and the cut is prepared again
    // against the generation rolled back to.
    let mut host = start();
    host.defer(CutDeferReason::Timeout);
    assert_eq!(
        host.fail(2),
        strings(&[
            &abort(3),
            "candidate 3 generation 3: rejected",
            "candidate 2 generation 2: failed",
            "commit 1",
            "candidate 1 generation 1: active",
            &prepare(4),
            "candidate 3 generation 4: pending",
        ])
    );

    // Armed: the cut is cancelled first; the rollback follows its cancellation.
    let mut host = start();
    host.arm(T);
    assert_eq!(
        host.fail(2),
        strings(&["candidate 2 generation 2: failed", "cancel the cut of 3"])
    );
    assert_eq!(
        host.cancel(true),
        strings(&[
            &abort(3),
            "candidate 3 generation 3: rejected",
            "commit 1",
            "candidate 1 generation 1: active",
            &prepare(4),
            "candidate 3 generation 4: pending",
        ])
    );
    assert_eq!(host.summary(), "g1 preparing g4 | steady 1");

    // Irrevocable: the replaced generation drains, and its failure is only recorded.
    let mut host = start();
    host.arm(T);
    host.poll(true, false);
    assert_eq!(host.fail(2), strings(&["candidate 2 generation 2: failed"]));
    assert_eq!(
        host.summary(),
        format!("g3 cut g3 irrevocable at {} rollback a | cut 2 -> 3", T)
    );
}

#[test]
fn a_revocation_aborts_cancels_or_waits_for_an_artifacts_cut_by_phase() {
    let start = || {
        let mut host = Host::new();
        host.deliver(&[policy(1, "cut-10")]);
        host.acknowledge();
        host
    };
    // Before the cut is armed, it is aborted, and the default program, which is active, changes nothing.
    let mut host = start();
    assert_eq!(host.demo(), (ApplyState::Prepared, Some(2), None));
    assert_eq!(
        host.deliver(&[]),
        strings(&[
            &abort(2),
            &format!("candidate 2 generation 2: {}", REVOKED),
            "install the default program",
            "candidate 3: unchanged",
        ])
    );
    assert_eq!(host.demo().0, ApplyState::Rejected);

    // Armed, it is cancelled, after which the default program is asked for.
    let mut host = start();
    host.arm(T);
    assert_eq!(host.demo(), (ApplyState::Prepared, Some(2), Some(T)));
    assert_eq!(host.deliver(&[]), strings(&["cancel the cut of 2"]));
    assert_eq!(
        host.cancel(true),
        strings(&[
            &abort(2),
            &format!("candidate 2 generation 2: {}", REVOKED),
            "install the default program",
            "candidate 3: unchanged",
        ])
    );
    assert_eq!(host.summary(), "g1 | steady 1");

    // A refused cancellation leaves the artifact running until its cut is finalized; the default program follows.
    let mut host = start();
    host.arm(T);
    host.deliver(&[]);
    assert_eq!(
        host.cancel(false),
        strings(&[
            "candidate 2 generation 2: active",
            "candidate 1 generation 1: draining",
            "install the default program",
            "candidate 3: pending (CutInProgress)",
        ])
    );
    assert_eq!(host.demo(), (ApplyState::Active, Some(2), Some(T)));

    // Irrevocable, the default program waits for the finalization, and is then installed with a cut of its own.
    let mut host = start();
    host.arm(T);
    host.poll(true, false);
    assert_eq!(
        host.deliver(&[]),
        strings(&["install the default program", "candidate 3: pending (CutInProgress)"])
    );
    assert_eq!(
        host.poll(true, true),
        strings(&[
            "finalize the cut of 2",
            &prepare(3),
            "candidate 3 generation 3: pending",
            "candidate 1 generation 1: retired",
        ])
    );
    assert_eq!(host.demo(), (ApplyState::Active, Some(2), None));
    assert_eq!(
        host.acknowledge(),
        strings(&["candidate 3 generation 3: prepared", &arm(3)])
    );
}

#[test]
fn re_compositions_of_the_cuts_artifact_replace_it_before_arming_and_wait_for_it_afterwards() {
    // Before arming, the re-composition replaces the cut: nothing has read its cut time.
    let mut host = Host::new();
    host.deliver(&[policy(1, "cut-10")]);
    host.acknowledge();
    host.defer(CutDeferReason::Timeout);
    assert_eq!(
        host.configure("tenant.other.max"),
        strings(&[
            &abort(2),
            &format!("candidate 2 generation 2: {}", CANCELLED),
            // R5b pushed the post-aggregation filter into aggregation, so the filter's change reaches the aggregation
            // slot's per-call parameters, on the same instance.
            "prepare 3 [0:unchanged reuse 1 build 0, 1:changed reuse 1 build 0, 2:unchanged reuse 2 build 0, \
             3:changed reuse 1 build 0, 4:changed reuse 0 build 0, 5:unchanged reuse 2 build 0]",
            "candidate 3 generation 3: pending",
        ])
    );

    // Armed: three configuration changes compose the artifact again; the cut keeps its time, and the latest
    // re-composition commits right after the finalization, keeping the cut's lineages.
    let mut host = Host::new();
    host.deliver(&[policy(1, "cut-10")]);
    host.acknowledge();
    host.arm(T);
    for (configuration, candidate) in [("tenant.one.max", 3), ("tenant.two.max", 4), ("tenant.three.max", 5)] {
        let mut expected = Vec::new();
        if candidate > 3 {
            expected.push(format!("candidate {}: superseded", candidate - 1));
        }
        expected.push(format!("candidate {}: pending (CutInProgress)", candidate));
        assert_eq!(host.configure(configuration), expected);
    }
    assert_eq!(
        host.core().cut_in_flight().unwrap().phase(),
        CutPhase::Armed { at: at(T) }
    );
    host.poll(true, false);
    assert_eq!(
        host.poll(true, true),
        strings(&[
            "finalize the cut of 2",
            "prepare 3 [0:unchanged reuse 1 build 0, 1:unchanged reuse 1 build 0, 2:unchanged reuse 2 build 0, \
             3:changed reuse 1 build 0, 4:changed reuse 0 build 0, 5:unchanged reuse 2 build 0]",
            "candidate 5 generation 3: pending",
            "candidate 1 generation 1: retired",
        ])
    );
    assert_eq!(
        host.acknowledge(),
        strings(&[
            "commit 3",
            "candidate 5 generation 3: active",
            "candidate 2 generation 2: draining",
            "candidate 2 generation 2: retired",
        ])
    );
    assert_eq!(host.demo(), (ApplyState::Active, Some(3), None));
}

#[test]
fn a_restarted_caller_is_asked_for_the_cut_action_in_flight_again() {
    let mut host = Host::new();
    let resumed = |host: &mut Host| crate::install::tests::described(&host.core().resume_actions());
    assert!(resumed(&mut host).is_empty());
    host.lower("cut-10");
    host.acknowledge();
    assert_eq!(resumed(&mut host), strings(&[&arm(2)]));
    host.arm(T);
    // A cut armed again keeps its first cut time.
    assert_eq!(
        crate::install::tests::described(&host.core().cut_armed(g(2), at(T + 10))),
        strings(&[&watch(2)])
    );
    assert_eq!(resumed(&mut host), strings(&[&watch(2)]));
    host.propose("b", 1);
    assert_eq!(resumed(&mut host), strings(&[&watch(2), "cancel the cut of 2"]));
    host.cancel(false);
    assert_eq!(resumed(&mut host), strings(&[&watch(2)]));
    host.poll(true, true);
    // The finalization is asked for again until anything else changes the schedule: the newer artifact's own cut.
    assert_eq!(resumed(&mut host), strings(&["finalize the cut of 2"]));
    host.acknowledge();
    assert_eq!(resumed(&mut host), strings(&[&arm(3)]));
    host.arm(T);
    host.poll(true, true);
    assert_eq!(resumed(&mut host), strings(&["finalize the cut of 3"]));
    // Or a commit.
    host.lower("x");
    host.acknowledge();
    assert!(resumed(&mut host).is_empty());
}

#[test]
fn a_cut_boundary_recorded_but_never_published_prepares_the_candidate_again_as_a_new_generation() {
    let mut host = Host::new();
    host.lower("cut-10");
    host.acknowledge();
    let actions = host.core().cut_arm_abandoned(g(2));
    assert_eq!(
        host.perform(actions),
        strings(&[
            &abort(2),
            "candidate 2 generation 2: rejected",
            &prepare(3),
            "candidate 2 generation 3: pending",
        ])
    );
    // Events about the abandoned generation change nothing.
    assert!(host.core().cut_armed(g(2), at(T)).is_empty());
    assert!(host.core().cut_cancelled(g(2)).is_empty());
}

#[test]
fn the_events_of_another_generation_or_phase_change_nothing() {
    let mut host = Host::new();
    host.lower("cut-10");
    host.acknowledge();
    let progress = CutProgress {
        stamped_new: true,
        passed: 1,
        affected: 1,
    };
    // Not armed yet: no progress, cancellation, or refusal applies.
    assert!(host.core().cut_progress(g(2), progress).is_empty());
    assert!(host.core().cut_cancelled(g(2)).is_empty());
    assert!(host.core().cut_cancel_refused(g(2)).is_empty());
    // Another generation's events.
    assert!(host.core().cut_armed(g(7), at(T)).is_empty());
    assert!(host.core().cut_deferred(g(7), CutDeferReason::Timeout).is_empty());
    host.arm(T);
    assert!(
        host.core().cut_deferred(g(2), CutDeferReason::Timeout).is_empty(),
        "armed: nothing to put off"
    );
    assert!(host.core().cut_arm_abandoned(g(2)).is_empty(), "armed: published");
    assert_eq!(host.summary(), format!("g1 cut g2 armed at {} | cut 1 -> 2", T));
}

#[test]
fn a_late_post_aggregation_change_is_not_a_cut() {
    // The classifier decides what is a cut; any other change through the same core commits at once.
    let mut host = Host::new();
    assert_eq!(
        host.configure(BOOT_POST_AGGREGATION),
        strings(&["candidate 2: unchanged"])
    );
    host.lower("plain");
    assert_eq!(
        host.acknowledge(),
        strings(&[
            "commit 2",
            "candidate 3 generation 2: active",
            "candidate 1 generation 1: draining",
            "candidate 1 generation 1: retired",
        ])
    );
}

// Widths across a chain of installations.

/// Installs the cut the latest action asked to arm, at `secs`, and finalizes it.
fn install_cut(host: &mut Host, secs: u64) {
    host.acknowledge();
    host.arm(secs);
    host.poll(true, true);
}

/// Returns the error of the `demo` config's acknowledgment.
fn demo_error(host: &mut Host) -> Option<String> {
    host.core()
        .config_states()
        .into_iter()
        .find(|ack| ack.id.as_str() == "demo")
        .and_then(|ack| ack.error)
}

#[test]
fn an_artifact_whose_widths_the_default_programs_line_up_with_too_rarely_is_rejected_so_a_revocation_installs_it() {
    let mut host = Host::new();
    host.default_marker = "cut-120";
    // The default program aggregates over 120 s.
    host.lower("cut-120");
    install_cut(&mut host, T);
    assert_eq!(host.summary(), "g2 | steady 2");

    // An artifact narrows the window to 60 s, which lines up with 120 s every 120 s.
    host.deliver(&[policy(1, "cut-60")]);
    install_cut(&mut host, T + 120);
    assert_eq!(host.summary(), "g3 | steady 3");

    // A newer one widens it to 180 s. That lines up with the active 60 s every 180 s, but with the default program's
    // 120 s only every 360 s: the default program could not replace it, so it is rejected before it is prepared.
    let actions = host.deliver(&[policy(2, "cut-180")]);
    assert_eq!(actions.len(), 1, "{:?}", actions);
    assert_eq!(
        actions[0],
        "candidate 4: rejected (The bucket widths of the artifact's state owners and of the default program's, 120 s, \
         180 s, line up only every 360 seconds, more than the 300 seconds a cut may wait for its boundary, so the \
         default program could not replace the artifact, as it does when the channel revokes it. Use widths that line \
         up with the default program's within 300 seconds, such as divisors of 60 seconds.)"
    );
    assert_eq!(host.summary(), "g3 | steady 3");
    assert_eq!(host.demo(), (ApplyState::Rejected, None, None));

    // The revocation takes the policy out of effect: the default program replaces revision 1 at a cut of 120 s.
    assert_eq!(
        host.deliver(&[]),
        strings(&[
            "install the default program",
            &prepare(4),
            "candidate 5 generation 4: pending"
        ])
    );
    install_cut(&mut host, T + 240);
    assert_eq!(host.summary(), "g4 | steady 4");
}

#[test]
fn a_newer_artifact_that_cannot_follow_the_active_plan_never_cancels_a_cut_and_waits_for_one_it_can_follow() {
    // The active artifact aggregates over 120 s, and a newer one at 60 s is armed at a cut.
    let start = || {
        let mut host = Host::new();
        host.deliver(&[policy(1, "cut-120")]);
        install_cut(&mut host, T);
        host.deliver(&[policy(2, "cut-60")]);
        host.acknowledge();
        host.arm(T + 120);
        assert_eq!(host.summary(), format!("g2 cut g3 armed at {} | cut 2 -> 3", T + 120));
        host
    };

    // A third at 180 s lines up with the cut's 60 s every 180 s but with the active 120 s only every 360 s: it does not
    // cancel the cut, which it can follow, and waits for its finalization, whichever way the cut would have answered a
    // cancellation.
    let mut host = start();
    assert_eq!(
        host.deliver(&[policy(3, "cut-180")]),
        strings(&["candidate 4: pending (CutInProgress)"])
    );
    assert_eq!(host.demo(), (ApplyState::Pending, None, None));
    assert_eq!(
        host.poll(true, true),
        strings(&[
            "candidate 3 generation 3: active",
            "candidate 2 generation 2: draining",
            "finalize the cut of 3",
            &prepare(4),
            "candidate 4 generation 4: pending",
            "candidate 2 generation 2: retired",
        ])
    );
    install_cut(&mut host, T + 360);
    assert_eq!(host.summary(), "g4 rollback cut-60 | steady 4");
    assert_eq!(host.demo(), (ApplyState::Active, Some(4), None));

    // One that lines up with neither is rejected at once: the cut proceeds as armed.
    let mut host = start();
    let actions = host.deliver(&[policy(3, "cut-120-180")]);
    assert_eq!(actions.len(), 1, "{:?}", actions);
    assert!(
        actions[0].starts_with("candidate 4: rejected (Bucket widths 120 s, 180 s line up only every 360 seconds"),
        "{}",
        actions[0]
    );
    assert_eq!(host.summary(), format!("g2 cut g3 armed at {} | cut 2 -> 3", T + 120));
    assert_eq!(
        host.poll(true, true),
        strings(&[
            "candidate 3 generation 3: active",
            "candidate 2 generation 2: draining",
            "finalize the cut of 3",
            "candidate 2 generation 2: retired",
        ])
    );
    assert_eq!(host.summary(), "g3 rollback cut-120 | steady 3");
}

#[test]
fn a_revoked_artifact_the_default_program_cannot_replace_says_it_stays_active_and_why() {
    // The default program aggregates over 60 s, and an artifact at 180 s, which lines up with it every 180 s, runs.
    let mut host = Host::new();
    host.default_marker = "cut-60";
    host.lower("cut-60");
    install_cut(&mut host, T);
    host.deliver(&[policy(1, "cut-180")]);
    install_cut(&mut host, T + 180);
    assert_eq!(host.summary(), "g3 | steady 3");

    // The default program's width changes to 120 s, which the artifact's 180 s lines up with only every 360 s. The
    // artifact's composition is unchanged, so it keeps running.
    host.default_marker = "cut-120";
    assert_eq!(host.lower("cut-120"), strings(&["candidate 4: unchanged"]));

    // Revoked, it cannot be replaced at a cut: it stays active, and its acknowledgment says so and why.
    let actions = host.deliver(&[]);
    assert_eq!(actions.len(), 2, "{:?}", actions);
    assert_eq!(actions[0], "install the default program");
    assert!(
        actions[1].starts_with("candidate 5: rejected (Bucket widths 120 s, 180 s line up only every 360 seconds"),
        "{}",
        actions[1]
    );
    assert_eq!(host.summary(), "g3 | steady 3");
    assert_eq!(host.demo(), (ApplyState::Active, Some(3), None));
    let error = demo_error(&mut host).expect("the acknowledgment has an error");
    assert!(
        error.starts_with(
            "revoked; active until replaced: the default program was rejected: Bucket widths 120 s, 180 s line up only \
             every 360 seconds"
        ),
        "{}",
        error
    );

    // A default program that lines up with it replaces it, and the error goes with it.
    host.default_marker = "cut-60";
    host.lower("cut-60");
    install_cut(&mut host, T + 360);
    assert_eq!(host.summary(), "g4 | steady 4");
    assert_eq!(host.demo().0, ApplyState::Retired);
    assert_eq!(demo_error(&mut host), None);
}
