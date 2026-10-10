//! Diff, lifecycle, and installer fixtures, with expected results written by hand.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use saluki_core::execution::{GenerationId, RetireNotifier};
use serde_json::{json, Value};

use super::{diff::NodeChange, diff::OutputChange, *};
use crate::{
    compile::{Frame, MissingCapability, SlotId},
    extension::TransitionClass,
    ir::{NativeComponentId, NodeId, Plan},
    operators::OperatorRegistry,
    testing::{default_program_catalog, with_opaque_gateways, DEFAULT_PROGRAM_JSON},
    validate::Validator,
};

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    crate::testing::default_program_slot_name(nodes)
}

fn id(id: &str) -> NodeId {
    id.parse().unwrap()
}

/// The stage 3 default program, edited.
fn edited(edit: impl FnOnce(&mut Value)) -> Plan {
    let mut value = with_opaque_gateways(DEFAULT_PROGRAM_JSON);
    edit(&mut value);
    Plan::from_value(value).unwrap()
}

fn boot_plan() -> Plan {
    edited(|_| {})
}

fn node_mut<'a>(value: &'a mut Value, id: &str) -> &'a mut Value {
    value["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == id)
        .unwrap()
}

/// The default program with the listener filterlist set to `values`, a live change.
fn filtered(values: &[&str]) -> Plan {
    edited(|value| {
        node_mut(value, "dsd_prefix_filter_blocklist")["params"]["predicate"]["intake_name"]["matches"]["values"]
            ["resolved"] = json!(values);
    })
}

/// The default program without the autoscaling failover branch.
fn without_autoscaling() -> Plan {
    edited(|value| {
        value["nodes"]
            .as_array_mut()
            .unwrap()
            .retain(|node| node["id"] != "af_metrics_gateway" && node["id"] != "af_metrics_gateway_out");
        value["edges"]
            .as_array_mut()
            .unwrap()
            .retain(|edge| edge["to"] != "af_metrics_gateway.in" && edge["from"] != "af_metrics_gateway.out");
        node_mut(value, "metrics_policy")["params"]["ports"] = json!(4);
        value["outputs"].as_array_mut().unwrap().pop();
    })
}

// Lifecycle.

#[test]
fn generation_states_follow_the_lifecycle_table() {
    use GenerationState::*;
    use LifecycleEvent as E;

    let events = [
        E::BarrierPassed,
        E::Committed,
        E::Superseded {
            kept_as_last_known_good: false,
        },
        E::Superseded {
            kept_as_last_known_good: true,
        },
        E::Released,
        E::Retired,
        E::Rejected,
        E::Failed,
    ];
    // Every allowed transition; everything else is an error.
    let allowed = [
        (Pending, E::BarrierPassed, Prepared),
        (Pending, E::Rejected, Rejected),
        (Pending, E::Failed, Failed),
        (Prepared, E::Committed, Active),
        (Prepared, E::Released, Draining),
        (Prepared, E::Rejected, Rejected),
        (Prepared, E::Failed, Failed),
        (Active, events[2], Draining),
        (Active, events[3], Prepared),
        (Active, E::Failed, Failed),
        (Draining, E::Retired, Retired),
        (Draining, E::Failed, Failed),
        (Rejected, E::Retired, Rejected),
        (Failed, E::Retired, Failed),
    ];
    for state in [Pending, Prepared, Active, Draining, Retired, Rejected, Failed] {
        for event in events {
            let expected = allowed
                .iter()
                .find(|(from, on, _)| *from == state && *on == event)
                .map(|(_, _, to)| *to);
            match expected {
                Some(to) => assert_eq!(state.apply(event), Ok(to), "{} on {:?}", state, event),
                None => assert_eq!(
                    state.apply(event),
                    Err(InvalidTransition { from: state, event }),
                    "{} on {:?}",
                    state,
                    event
                ),
            }
        }
    }
    assert!(Retired.is_final() && Rejected.is_final() && Failed.is_final());
    assert!(!Draining.is_final());
    assert_eq!(Draining.to_string(), "draining");
}

// Diffs.

#[test]
fn an_identical_plan_diffs_as_no_change() {
    let plan = boot_plan();
    let changes = diff(&plan, &plan, &OperatorRegistry::native());
    assert!(changes.is_empty());
    assert_eq!(changes.nodes.len(), plan.nodes.len());
    // Aggregation and the four gateways keep state in both plans; nothing about them changed.
    let owners = changes
        .state_owners
        .iter()
        .map(|owner| (owner.node.to_string(), owner.class))
        .collect::<Vec<_>>();
    assert_eq!(
        owners,
        vec![
            ("dsd_agg".to_string(), TransitionClass::Retain),
            ("metrics_routing_filter_0".to_string(), TransitionClass::Retain),
            ("metrics_routing_filter_1".to_string(), TransitionClass::Retain),
            ("mrf_metrics_gateway".to_string(), TransitionClass::Retain),
            ("af_metrics_gateway".to_string(), TransitionClass::Retain),
        ]
    );
    assert!(changes
        .outputs
        .iter()
        .all(|output| output.change == OutputChange::Reused));
}

#[test]
fn a_live_change_diffs_as_one_changed_node() {
    let changes = diff(&boot_plan(), &filtered(&["other"]), &OperatorRegistry::native());
    let changed = changes
        .changes()
        .map(|node| (node.id.to_string(), node.change.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        changed,
        vec![(
            "dsd_prefix_filter_blocklist".to_string(),
            NodeChange::Changed {
                operator: false,
                params: true,
                provenance: false,
                edges: false,
            }
        )]
    );
    assert!(changes.edges_added.is_empty() && changes.edges_removed.is_empty());
    assert_eq!(changes.strictest_transition(), Some(TransitionClass::Retain));
}

#[test]
fn removing_a_branch_diffs_as_removed_nodes_edges_and_output() {
    let changes = diff(&boot_plan(), &without_autoscaling(), &OperatorRegistry::native());
    let changed = changes
        .changes()
        .map(|node| (node.id.to_string(), node.change.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        changed,
        vec![
            (
                "metrics_policy".to_string(),
                NodeChange::Changed {
                    operator: false,
                    params: true,
                    provenance: false,
                    edges: true,
                }
            ),
            ("af_metrics_gateway".to_string(), NodeChange::Removed),
            ("af_metrics_gateway_out".to_string(), NodeChange::Removed),
        ]
    );
    let removed = changes
        .edges_removed
        .iter()
        .map(|edge| format!("{} -> {}", edge.from, edge.to))
        .collect::<Vec<_>>();
    assert_eq!(
        removed,
        vec![
            "metrics_policy.4 -> af_metrics_gateway.in".to_string(),
            "af_metrics_gateway.out -> af_metrics_gateway_out.in".to_string(),
        ]
    );
    assert!(changes.edges_added.is_empty());
    let outputs = changes
        .outputs
        .iter()
        .filter(|output| output.change != OutputChange::Reused)
        .map(|output| (output.id.to_string(), output.change))
        .collect::<Vec<_>>();
    assert_eq!(outputs, vec![("af_metrics_encode".to_string(), OutputChange::Removed)]);

    // Adding it back diffs the other way.
    let back = diff(&without_autoscaling(), &boot_plan(), &OperatorRegistry::native());
    let added = back
        .changes()
        .filter(|node| node.change == NodeChange::Added)
        .map(|node| node.id.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        added,
        vec!["af_metrics_gateway".to_string(), "af_metrics_gateway_out".to_string()]
    );
    assert_eq!(back.edges_added.len(), 2);
}

#[test]
fn state_owner_and_output_changes_are_classified() {
    let registry = OperatorRegistry::native();

    // A different aggregation width changes the state owner's time contract: a cut.
    let wider = edited(|value| {
        node_mut(value, "dsd_agg")["params"]["window"]["resolved"] = json!("20s");
    });
    let changes = diff(&boot_plan(), &wider, &registry);
    let aggregation = changes
        .state_owners
        .iter()
        .find(|owner| owner.node == id("dsd_agg"))
        .unwrap();
    assert_eq!(aggregation.class, TransitionClass::Cut);
    assert_eq!(changes.strictest_transition(), Some(TransitionClass::Cut));

    // An opaque host refuses any change.
    let renamed = edited(|value| {
        node_mut(value, "mrf_metrics_gateway")["params"]["component"] = json!("other");
    });
    let changes = diff(&boot_plan(), &renamed, &registry);
    assert_eq!(changes.strictest_transition(), Some(TransitionClass::Reject));

    // A different fingerprint rebinds the output.
    let rebound = edited(|value| {
        value["outputs"][1]["fingerprint"] = json!(format!("sha256:{}", "9".repeat(64)));
    });
    let changes = diff(&boot_plan(), &rebound, &registry);
    let outputs = changes
        .outputs
        .iter()
        .filter(|output| output.change != OutputChange::Reused)
        .map(|output| (output.id.to_string(), output.change))
        .collect::<Vec<_>>();
    assert_eq!(outputs, vec![("dd_metrics_encode".to_string(), OutputChange::Rebound)]);
}

fn frame() -> Arc<Frame> {
    Arc::new(Frame::compute(&boot_plan(), &OperatorRegistry::native(), &namer).unwrap())
}

fn compiled(plan: &Plan, frame: &Frame) -> crate::compile::CompiledPlan {
    let registry = OperatorRegistry::native();
    let report = Validator::new(&registry, &default_program_catalog()).validate(plan);
    crate::compile::compile(plan, &report, &registry, frame).unwrap()
}

#[test]
fn kernel_reuse_keeps_every_kernel_whose_key_is_unchanged() {
    let frame = frame();
    let boot = compiled(&boot_plan(), &frame);
    let projected = edited(|value| {
        node_mut(value, "dsd_tag_filterlist")["params"]["rules"]["resolved"] = json!([]);
    });
    let candidate = compiled(&projected, &frame);
    let slots = kernel_reuse(Some(&boot.programs), &candidate.programs);
    let nodes = |keys: &[crate::compile::KernelKey]| keys.iter().map(|key| key.node.to_string()).collect::<Vec<_>>();

    // The tag filterlist runs in the third region of the pre-aggregation pipeline, with the timestamp split.
    assert_eq!(slots[2].slot, SlotId::new(2));
    assert!(!slots[2].unchanged);
    assert_eq!(nodes(&slots[2].reuse), vec!["dsd_no_agg_split"]);
    assert_eq!(nodes(&slots[2].build), vec!["dsd_tag_filterlist"]);
    assert_eq!(nodes(&slots[2].release), vec!["dsd_tag_filterlist"]);
    assert_ne!(slots[2].build, slots[2].release, "the rebuilt kernel has a new key");

    // Every other region is unchanged, and keeps its kernels: the mapper, the namespace prefix, none in the
    // post-aggregation filter's region, and host enrichment with host tags.
    let unchanged = [
        vec!["dsd_enrich"],
        vec!["dsd_prefix_filter"],
        Vec::new(),
        vec!["metrics_enrich", "metrics_enrich_host_tags"],
    ];
    for (slot, reused) in [0, 1, 3, 4].into_iter().zip(unchanged) {
        assert!(slots[slot].unchanged, "slot {}", slot);
        assert_eq!(nodes(&slots[slot].reuse), reused, "slot {}", slot);
        assert!(slots[slot].build.is_empty() && slots[slot].release.is_empty());
    }

    // Without an active generation, everything is built.
    let fresh = kernel_reuse(None, &candidate.programs);
    assert!(fresh.iter().all(|slot| slot.reuse.is_empty() && !slot.unchanged));
    assert_eq!(
        fresh.iter().map(|slot| slot.build.len()).collect::<Vec<_>>(),
        vec![1, 1, 2, 0, 2]
    );
}

// The installer core.

/// An installer over the stage 3 frame, booted with the default program, and the retirements its tokens reported.
struct Harness {
    core: InstallerCore,
    boot: Option<Boot>,
    retired: Arc<Mutex<Vec<GenerationId>>>,
    next_change: u64,
}

impl Harness {
    fn new(config: InstallerConfig) -> Self {
        let retired = Arc::new(Mutex::new(Vec::new()));
        let notified = Arc::clone(&retired);
        let notifier = move |_: GenerationId| {
            let notified = Arc::clone(&notified);
            RetireNotifier::new(move |id| notified.lock().unwrap().push(id))
        };
        let (core, boot) = InstallerCore::new(
            frame(),
            OperatorRegistry::native(),
            config,
            notifier,
            candidate(boot_plan(), Origin::DefaultProgram { config_change: 0 }),
        )
        .unwrap();
        Self {
            core,
            boot: Some(boot),
            retired,
            next_change: 1,
        }
    }

    fn propose(&mut self, plan: Plan) -> (CandidateId, Vec<Action>) {
        let origin = Origin::DefaultProgram {
            config_change: self.next_change,
        };
        self.next_change += 1;
        self.core.propose(candidate(plan, origin))
    }

    /// Acknowledges every slot of the generation being prepared, returning what the last acknowledgment did.
    fn acknowledge_all(&mut self, generation: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        for slot in 0..self.core.frame().slots().len() as u16 {
            actions = self
                .core
                .prepare_acknowledged(GenerationId::new(generation), SlotId::new(slot), Ok(()));
        }
        actions
    }

    /// Delivers the retirements the tokens reported since the last call.
    fn deliver_retirements(&mut self) -> Vec<Action> {
        let retired = std::mem::take(&mut *self.retired.lock().unwrap());
        retired.into_iter().flat_map(|id| self.core.retired(id)).collect()
    }
}

fn candidate(plan: Plan, origin: Origin) -> Candidate {
    let report = Validator::new(&OperatorRegistry::native(), &default_program_catalog()).validate(&plan);
    Candidate {
        plan: Arc::new(plan),
        report,
        origin,
    }
}

/// A compact description of an action, for comparing action lists.
fn describe(action: &Action) -> String {
    match action {
        Action::Prepare(prepare) => format!(
            "prepare {} [{}]",
            prepare.generation.id(),
            prepare
                .slots
                .iter()
                .map(|slot| format!(
                    "{}:{} reuse {} build {}",
                    slot.slot,
                    if slot.unchanged { "unchanged" } else { "changed" },
                    slot.reuse.len(),
                    slot.build.len()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Action::Abort { generation, slots } => format!("abort {} in {} slots", generation, slots.len()),
        Action::Commit { generation } => format!("commit {}", generation.id()),
        Action::RetryAfter(delay) => format!("retry after {:?}", delay),
        Action::InstallDefaultProgram => "install the default program".to_string(),
        Action::Report(report) => {
            let generation = report
                .generation
                .map_or_else(String::new, |id| format!(" generation {}", id));
            let outcome = match &report.outcome {
                Outcome::Generation(state) => state.to_string(),
                Outcome::Pending(reason) => format!("pending ({:?})", reason),
                Outcome::Rejected(rejection) => format!("rejected ({})", rejection),
                other => format!("{:?}", other).to_lowercase(),
            };
            format!("candidate {}{}: {}", report.candidate, generation, outcome)
        }
    }
}

fn described(actions: &[Action]) -> Vec<String> {
    actions.iter().map(describe).collect()
}

/// What every region prepares for a change of the listener filterlist, as [`describe`] writes it: only the region
/// running the filterlist, the second of the pre-aggregation pipeline, changes, and it builds nothing, since the
/// filterlist is a decision; every region reuses its kernels.
const FILTERLIST_CHANGED: &str =
    "0:unchanged reuse 1 build 0, 1:changed reuse 1 build 0, 2:unchanged reuse 2 build 0, \
                                  3:unchanged reuse 0 build 0, 4:unchanged reuse 2 build 0";

/// What every region prepares for a plan equal to the active one.
const NOTHING_CHANGED: &str = "0:unchanged reuse 1 build 0, 1:unchanged reuse 1 build 0, 2:unchanged reuse 2 build 0, \
                               3:unchanged reuse 0 build 0, 4:unchanged reuse 2 build 0";

/// Describes the preparation of `generation` with what each region prepares, as [`describe`] writes it.
fn prepared(generation: u64, slots: &str) -> String {
    let slots = slots.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("prepare {} [{}]", generation, slots)
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|item| item.to_string()).collect()
}

#[test]
fn the_boot_generation_is_active_at_once() {
    let harness = Harness::new(InstallerConfig::default());
    let boot = harness.boot.as_ref().unwrap();
    assert_eq!(boot.generation.id(), GenerationId::new(1));
    assert_eq!(
        boot.programs.len(),
        5,
        "three pre-aggregation regions, and two after aggregation"
    );
    let status = harness.core.status();
    assert_eq!(
        status.active.as_ref().map(|record| record.state),
        Some(GenerationState::Active)
    );
    assert_eq!(status.live, vec![GenerationId::new(1)]);
    assert_eq!(status.preparing, None);
    assert_eq!(status.last_known_good, None);
    assert_eq!(harness.core.active_programs().unwrap().len(), 5);
    // No logical rewrite applies to the boot plan yet, and no decision records one.
    assert_eq!(harness.core.active_rewrites(), Some(&[][..]));
}

#[test]
fn an_unchanged_plan_creates_no_generation() {
    let mut harness = Harness::new(InstallerConfig::default());
    let (_, actions) = harness.propose(boot_plan());
    assert_eq!(described(&actions), strings(&["candidate 2: unchanged"]));
    assert_eq!(harness.core.status().live, vec![GenerationId::new(1)]);
}

#[test]
fn a_live_change_prepares_every_region_then_commits_with_one_store() {
    let mut harness = Harness::new(InstallerConfig::default());
    let (_, actions) = harness.propose(filtered(&["other"]));
    // Only the program of the region running the listener filterlist changed, and it builds nothing new: the
    // filterlist is a decision.
    assert_eq!(
        described(&actions),
        strings(&[&prepared(2, FILTERLIST_CHANGED), "candidate 2 generation 2: pending",])
    );
    let Action::Prepare(prepare) = &actions[0] else {
        panic!("prepare")
    };
    let active = harness.core.active_programs().unwrap();
    for slot in [0, 2, 3, 4] {
        assert!(
            Arc::ptr_eq(&prepare.slots[slot].program, &active[slot]),
            "unchanged region {} keeps its program",
            slot
        );
    }
    assert!(!Arc::ptr_eq(&prepare.slots[1].program, &active[1]));
    assert_eq!(prepare.timeout, Duration::from_secs(10));

    // The barrier waits for every region.
    for slot in 0..4 {
        let actions = harness
            .core
            .prepare_acknowledged(GenerationId::new(2), SlotId::new(slot), Ok(()));
        assert!(actions.is_empty(), "slot {}", slot);
    }
    let actions = harness
        .core
        .prepare_acknowledged(GenerationId::new(2), SlotId::new(4), Ok(()));
    assert_eq!(
        described(&actions),
        strings(&[
            "commit 2",
            "candidate 2 generation 2: active",
            "candidate 1 generation 1: draining",
        ])
    );
    let status = harness.core.status();
    assert_eq!(
        status.active.as_ref().map(|record| record.id),
        Some(GenerationId::new(2))
    );
    assert_eq!(
        status.last_known_good, None,
        "default-program generations keep no last-known-good"
    );
    assert_eq!(status.live, vec![GenerationId::new(1), GenerationId::new(2)]);

    // The boot generation retires once nothing holds it: here, the topology's boot handle.
    harness.boot = None;
    let actions = harness.deliver_retirements();
    assert_eq!(described(&actions), strings(&["candidate 1 generation 1: retired"]));
    assert_eq!(harness.core.status().live, vec![GenerationId::new(2)]);

    // A late acknowledgment of an old generation changes nothing.
    assert!(harness
        .core
        .prepare_acknowledged(GenerationId::new(2), SlotId::new(0), Ok(()))
        .is_empty());
}

#[test]
fn only_the_latest_waiting_candidate_is_kept() {
    let mut harness = Harness::new(InstallerConfig::default());
    harness.propose(filtered(&["a"]));
    let (_, actions) = harness.propose(filtered(&["b"]));
    assert_eq!(described(&actions), strings(&["candidate 3: pending (Preparing)"]));
    let (_, actions) = harness.propose(filtered(&["c"]));
    assert_eq!(
        described(&actions),
        strings(&["candidate 3: superseded", "candidate 4: pending (Preparing)"])
    );

    // When the first commits, the latest starts at once.
    let actions = harness.acknowledge_all(2);
    assert_eq!(
        described(&actions),
        strings(&[
            "commit 2",
            "candidate 2 generation 2: active",
            "candidate 1 generation 1: draining",
            &prepared(3, FILTERLIST_CHANGED),
            "candidate 4 generation 3: pending",
        ])
    );

    // A candidate equal to what is being prepared changes nothing.
    let (_, actions) = harness.propose(filtered(&["c"]));
    assert_eq!(described(&actions), strings(&["candidate 5: unchanged"]));
}

#[test]
fn a_failed_preparation_of_the_default_program_is_retried_with_backoff() {
    let mut harness = Harness::new(InstallerConfig::default());
    harness.propose(filtered(&["a"]));
    let actions =
        harness
            .core
            .prepare_acknowledged(GenerationId::new(2), SlotId::new(4), Err("mailbox closed".to_string()));
    assert_eq!(
        described(&actions),
        strings(&[
            "abort 2 in 5 slots",
            "candidate 2 generation 2: rejected",
            "retry after 1s",
            "candidate 2: pending (Retry { attempt: 2 })",
        ])
    );
    let record = harness
        .core
        .status()
        .generations
        .into_iter()
        .find(|record| record.id == GenerationId::new(2));
    assert_eq!(
        record.and_then(|record| record.reason),
        Some("region `metrics_enrich` failed to prepare: mailbox closed".to_string())
    );

    // The retry prepares a new generation; a timeout fails it again, and the delay doubles.
    let actions = harness.core.retry();
    assert_eq!(described(&actions)[0], prepared(3, FILTERLIST_CHANGED));
    let actions = harness.core.prepare_timed_out(GenerationId::new(3));
    assert_eq!(
        described(&actions),
        strings(&[
            "abort 3 in 5 slots",
            "candidate 2 generation 3: rejected",
            "retry after 2s",
            "candidate 2: pending (Retry { attempt: 3 })",
        ])
    );

    // A newer candidate replaces the one being retried.
    let (_, actions) = harness.propose(filtered(&["b"]));
    assert_eq!(
        described(&actions),
        strings(&["candidate 2: superseded", "candidate 3: pending (Backoff)"])
    );
    let actions = harness.core.retry();
    assert_eq!(described(&actions)[0], prepared(4, FILTERLIST_CHANGED));
}

#[test]
fn the_retry_delay_doubles_up_to_its_maximum() {
    let config = InstallerConfig::default();
    let delays = (1..=8)
        .map(|attempt| config.retry_delay(attempt).as_secs())
        .collect::<Vec<_>>();
    assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30, 30]);
    assert_eq!(config.retry_delay(u32::MAX), Duration::from_secs(30));
}

#[test]
fn a_full_overlap_budget_keeps_the_latest_candidate_pending_until_a_generation_retires() {
    let mut harness = Harness::new(InstallerConfig {
        overlap_budget: 2,
        ..InstallerConfig::default()
    });
    harness.propose(filtered(&["a"]));
    harness.acknowledge_all(2);
    // Generation 1 still drains (the boot handle holds it) and generation 2 is active: the budget is full.
    let (_, actions) = harness.propose(filtered(&["b"]));
    assert_eq!(
        described(&actions),
        strings(&["candidate 3: pending (OverlapBudget { live: 2, budget: 2 })"])
    );
    let (_, actions) = harness.propose(filtered(&["c"]));
    assert_eq!(
        described(&actions),
        strings(&[
            "candidate 3: superseded",
            "candidate 4: pending (OverlapBudget { live: 2, budget: 2 })",
        ])
    );

    // Once generation 1 retires, the latest candidate is prepared.
    harness.boot = None;
    let actions = harness.deliver_retirements();
    assert_eq!(
        described(&actions),
        strings(&[
            "candidate 1 generation 1: retired",
            &prepared(3, FILTERLIST_CHANGED),
            "candidate 4 generation 3: pending",
        ])
    );
}

#[test]
fn invalid_and_unplaceable_candidates_are_rejected_and_the_active_generation_stays() {
    let mut harness = Harness::new(InstallerConfig::default());
    // Validated against an empty catalog, the plan names bindings and outputs the catalog lacks.
    let invalid = |origin| Candidate {
        plan: Arc::new(filtered(&["a"])),
        report: Validator::new(&OperatorRegistry::native(), &crate::validate::MemoryCatalog::new())
            .validate(&filtered(&["a"])),
        origin,
    };
    let (_, actions) = harness.core.propose(invalid(artifact(1)));
    assert!(matches!(
        &actions[..],
        [Action::Report(Report {
            outcome: Outcome::Rejected(Rejection::Invalid { .. }),
            ..
        })]
    ));

    let wider = edited(|value| {
        node_mut(value, "dsd_agg")["params"]["flush_interval"]["resolved"] = json!("30s");
    });
    let (_, actions) = harness.propose(wider);
    let [Action::Report(Report {
        outcome: Outcome::Rejected(Rejection::Unplaceable(error)),
        ..
    })] = &actions[..]
    else {
        panic!("{:?}", described(&actions))
    };
    assert_eq!(error.capabilities(), vec![MissingCapability::State]);
    assert_eq!(harness.core.status().live, vec![GenerationId::new(1)]);
}

#[test]
fn a_default_program_with_validation_errors_is_installed_as_long_as_it_fits_the_frame() {
    let mut harness = Harness::new(InstallerConfig::default());
    // Validated against an empty catalog, the plan names bindings and outputs the catalog lacks; it is still the
    // default program, which runs degraded rather than not at all.
    let report =
        Validator::new(&OperatorRegistry::native(), &crate::validate::MemoryCatalog::new()).validate(&filtered(&["a"]));
    assert!(!report.is_valid());
    let (_, actions) = harness.core.propose(Candidate {
        plan: Arc::new(filtered(&["a"])),
        report,
        origin: Origin::DefaultProgram { config_change: 1 },
    });
    assert_eq!(
        described(&actions),
        strings(&[&prepared(2, FILTERLIST_CHANGED), "candidate 2 generation 2: pending",])
    );

    // A default program that is not structurally sound still cannot run.
    let unknown = edited(|value| {
        node_mut(value, "dsd_tag_filterlist")["op"] = json!("no_such_operator");
    });
    let report = Validator::new(&OperatorRegistry::native(), &default_program_catalog()).validate(&unknown);
    let (_, actions) = harness.core.propose(Candidate {
        plan: Arc::new(unknown),
        report,
        origin: Origin::DefaultProgram { config_change: 2 },
    });
    assert!(
        described(&actions).iter().any(|action| action.contains("rejected")),
        "{:?}",
        described(&actions)
    );
}

fn artifact(revision: u64) -> Origin {
    Origin::Artifact {
        revision,
        hash: format!("sha256:{}", "a".repeat(64)).parse().unwrap(),
    }
}

#[test]
fn artifact_revisions_must_increase_and_failures_roll_back_to_last_known_good() {
    let mut harness = Harness::new(InstallerConfig::default());
    let (_, actions) = harness.core.propose(candidate(filtered(&["a"]), artifact(5)));
    assert_eq!(described(&actions)[0], prepared(2, FILTERLIST_CHANGED));
    let actions = harness.acknowledge_all(2);
    // An artifact generation keeps the one it replaced prepared, to roll back to.
    assert_eq!(
        described(&actions),
        strings(&[
            "commit 2",
            "candidate 2 generation 2: active",
            "candidate 1 generation 1: prepared",
        ])
    );
    assert_eq!(
        harness.core.status().last_known_good.map(|record| record.id),
        Some(GenerationId::new(1))
    );

    for revision in [5, 3] {
        let (_, actions) = harness.core.propose(candidate(filtered(&["b"]), artifact(revision)));
        assert_eq!(
            described(&actions),
            vec![format!(
                "candidate {}: rejected (revision {} is stale: revision 5 has already been seen, and revisions must \
                 increase)",
                if revision == 5 { 3 } else { 4 },
                revision
            )]
        );
    }

    // Supervision fails the active artifact generation: one store rolls back.
    let actions = harness
        .core
        .generation_failed(GenerationId::new(2), "3 panics in 60 seconds");
    assert_eq!(
        described(&actions),
        strings(&[
            "candidate 2 generation 2: failed",
            "commit 1",
            "candidate 1 generation 1: active",
        ])
    );
    let status = harness.core.status();
    assert_eq!(status.active.map(|record| record.id), Some(GenerationId::new(1)));
    assert_eq!(status.last_known_good, None);
    assert_eq!(
        status.revision_high_water,
        Some(5),
        "a failure does not change the high-water mark"
    );
}

#[test]
fn a_newer_artifact_cancels_an_uncommitted_preparation() {
    let mut harness = Harness::new(InstallerConfig::default());
    harness.core.propose(candidate(filtered(&["a"]), artifact(1)));
    let (_, actions) = harness.core.propose(candidate(filtered(&["b"]), artifact(2)));
    assert_eq!(
        described(&actions),
        strings(&[
            "abort 2 in 5 slots",
            "candidate 2 generation 2: rejected (a newer candidate cancelled the preparation)",
            &prepared(3, FILTERLIST_CHANGED),
            "candidate 3 generation 3: pending",
        ])
    );
    // A failed artifact generation with nothing kept asks for the default program.
    harness.acknowledge_all(3);
    let actions = harness.core.propose(candidate(filtered(&["c"]), artifact(3))).1;
    assert_eq!(described(&actions)[0], prepared(4, FILTERLIST_CHANGED));
    harness.acknowledge_all(4);
    harness.core.generation_failed(GenerationId::new(4), "panicked");
    let actions = harness.core.generation_failed(GenerationId::new(3), "panicked");
    assert_eq!(
        described(&actions),
        strings(&["candidate 3 generation 3: failed", "install the default program"])
    );
}

#[test]
fn a_failed_default_program_generation_does_not_roll_back() {
    let mut harness = Harness::new(InstallerConfig::default());
    let actions = harness.core.generation_failed(GenerationId::new(1), "panicked");
    assert_eq!(described(&actions), strings(&["candidate 1 generation 1: failed"]));
}

/// Boots, commits artifact revisions 1 and 2 (generations 2 and 3), and fails both: generation 3 rolls back to 2, and
/// generation 2, with nothing kept, asks for the default program.
fn harness_with_both_artifacts_failed() -> Harness {
    let mut harness = Harness::new(InstallerConfig::default());
    for (revision, values) in [(1, "a"), (2, "b")] {
        harness.core.propose(candidate(filtered(&[values]), artifact(revision)));
        harness.acknowledge_all(revision + 1);
    }
    let actions = harness.core.generation_failed(GenerationId::new(3), "panicked");
    assert_eq!(
        described(&actions),
        strings(&[
            "candidate 3 generation 3: failed",
            "commit 2",
            "candidate 2 generation 2: active",
        ])
    );
    let actions = harness.core.generation_failed(GenerationId::new(2), "panicked");
    assert_eq!(
        described(&actions),
        strings(&["candidate 2 generation 2: failed", "install the default program"])
    );
    // Generations 1 and 3 retire once nothing holds them.
    harness.boot = None;
    harness.deliver_retirements();
    assert_eq!(harness.core.status().live, vec![GenerationId::new(2)]);
    harness
}

#[test]
fn a_generation_that_replaces_a_failed_one_leaves_it_failed() {
    let mut harness = harness_with_both_artifacts_failed();
    // The default program the failure asked for commits over the failed generation, which stays failed.
    let (_, actions) = harness.propose(boot_plan());
    assert_eq!(described(&actions)[0], prepared(4, FILTERLIST_CHANGED));
    let actions = harness.acknowledge_all(4);
    assert_eq!(
        described(&actions),
        strings(&["commit 4", "candidate 4 generation 4: active"])
    );
    let status = harness.core.status();
    assert_eq!(status.active.map(|record| record.id), Some(GenerationId::new(4)));
    assert_eq!(status.last_known_good, None);
    let failed = status
        .generations
        .iter()
        .find(|record| record.id == GenerationId::new(2))
        .unwrap();
    assert_eq!(failed.state, GenerationState::Failed);
    assert_eq!(failed.reason.as_deref(), Some("panicked"));
}

#[test]
fn a_failed_generation_is_never_kept_to_roll_back_to() {
    let mut harness = harness_with_both_artifacts_failed();
    // A newer artifact cancels the default program's preparation and commits over the failed generation: nothing is
    // kept below it, since the generation it replaces failed.
    harness.propose(boot_plan());
    harness.core.propose(candidate(filtered(&["c"]), artifact(3)));
    harness.deliver_retirements();
    let actions = harness.acknowledge_all(5);
    assert_eq!(
        described(&actions),
        strings(&["commit 5", "candidate 5 generation 5: active"])
    );
    assert_eq!(harness.core.status().last_known_good, None);
    // Its failure then has nothing to roll back to, rather than rolling back to the failed generation.
    let actions = harness.core.generation_failed(GenerationId::new(5), "panicked");
    assert_eq!(
        described(&actions),
        strings(&["candidate 5 generation 5: failed", "install the default program"])
    );
    // A failure reported twice changes nothing.
    assert!(harness
        .core
        .generation_failed(GenerationId::new(5), "panicked")
        .is_empty());
}

#[test]
fn a_candidate_equal_to_a_failed_generation_is_installed_anew() {
    let mut harness = harness_with_both_artifacts_failed();
    // Revision 3 carries the plan of revision 1, whose generation failed: it gets a generation of its own.
    let (_, actions) = harness.core.propose(candidate(filtered(&["a"]), artifact(3)));
    assert_eq!(
        described(&actions),
        strings(&[&prepared(4, NOTHING_CHANGED), "candidate 4 generation 4: pending",])
    );
}

#[test]
fn rolling_back_prepares_the_preparation_in_flight_again_against_the_generation_rolled_back_to() {
    let mut harness = Harness::new(InstallerConfig::default());
    harness.core.propose(candidate(filtered(&["a"]), artifact(1)));
    harness.acknowledge_all(2);
    // Revision 2 is being prepared against generation 2, reusing its kernels, when generation 2 fails.
    harness.core.propose(candidate(filtered(&["b"]), artifact(2)));
    let actions = harness.core.generation_failed(GenerationId::new(2), "panicked");
    assert_eq!(
        described(&actions),
        strings(&[
            "candidate 2 generation 2: failed",
            "abort 3 in 5 slots",
            "candidate 3 generation 3: rejected",
            "commit 1",
            "candidate 1 generation 1: active",
            &prepared(4, FILTERLIST_CHANGED),
            "candidate 3 generation 4: pending",
        ])
    );
    let status = harness.core.status();
    assert_eq!(status.preparing.map(|record| record.id), Some(GenerationId::new(4)));
    assert_eq!(status.active.map(|record| record.id), Some(GenerationId::new(1)));
    // Acknowledgments of the cancelled preparation are ignored; the new one commits over the generation rolled back to,
    // which it keeps.
    assert!(harness.acknowledge_all(3).is_empty());
    let actions = harness.acknowledge_all(4);
    assert_eq!(
        described(&actions),
        strings(&[
            "commit 4",
            "candidate 3 generation 4: active",
            "candidate 1 generation 1: prepared",
        ])
    );
}
