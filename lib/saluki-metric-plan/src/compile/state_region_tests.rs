//! Isolation slots in frames, placement, and programs, with expected results written by hand from the stage 6 design.
//!
//! Plans run under the native registry, whose `aggregate` declares an isolation fence.

use std::time::Duration;

use saluki_core::execution::{AdmissionPointId, EntryId};
use serde_json::{json, Value};

use super::*;
use crate::{
    ir::{NativeComponentId, NodeId, OutputId, Plan, RecipientId},
    model::PortType,
    testing::{default_program_catalog, DEFAULT_PROGRAM_JSON},
    validate::{CatalogOutput, MemoryCatalog, Validator},
};

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    crate::testing::default_program_slot_name(nodes)
}

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

fn point(id: u32) -> AdmissionPointId {
    AdmissionPointId::new(id)
}

fn default_program() -> Plan {
    Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap()
}

fn frame_of(plan: &Plan) -> Result<Frame, FrameError> {
    Frame::compute(plan, &OperatorRegistry::native(), &namer)
}

fn slot_named<'f>(frame: &'f Frame, name: &str) -> &'f Slot {
    frame
        .slots()
        .iter()
        .find(|slot| slot.name.as_str() == name)
        .unwrap_or_else(|| panic!("no slot `{}`", name))
}

#[test]
fn the_default_program_runs_aggregation_alone_in_its_slots_main_region_fed_by_dogstatsd_alone() {
    let frame = frame_of(&default_program()).unwrap();
    let names = frame
        .slots()
        .iter()
        .map(|slot| slot.name.to_string())
        .collect::<Vec<_>>();
    // The pre-aggregation chain keeps its three pipelined slots, aggregation runs in `dsd_agg`, then the
    // post-aggregation filter and enrichment, as with aggregation in its host component.
    assert_eq!(
        names,
        [
            "dsd_pre_aggregation",
            "dsd_prefix_filter_region",
            "dsd_no_agg_split_region",
            "dsd_agg",
            "dsd_post_agg_filter_region",
            "metrics_enrich",
        ]
    );
    let agg = slot_named(&frame, "dsd_agg");
    assert_eq!(agg.nodes, vec![id("dsd_agg")]);
    assert_eq!(agg.cost, 0, "R1 prices no state owner");
    // Aggregation's output point is numbered after the five admission points of the sources, and other state owners'
    // derived points after it.
    assert_eq!(frame.boot_output_points(&id("dsd_agg")), Some(&[point(5)][..]));
    assert_eq!(frame.derived_point_base(), 6);
    assert!(
        frame.fixed().iter().all(|fixed| fixed.node != id("dsd_agg")),
        "aggregation is no host"
    );
    // Its exits are those of any slot: its boot edge's target, then every output downstream of it, idle.
    let exits = agg.exits.iter().map(|exit| exit.name.to_string()).collect::<Vec<_>>();
    assert_eq!(exits[0], "dsd_post_agg_filter_region");
    assert!(exits.len() > 1, "{:?}", exits);
    assert!(agg
        .exits
        .iter()
        .skip(1)
        .all(|exit| matches!(exit.target, FrameTarget::Destination(_))));

    // Placed, aggregation runs in its slot's main region, alone, over DogStatsD's point.
    let plan = default_program();
    let registry = OperatorRegistry::native();
    let compiled = compile_unvalidated(
        &plan,
        &registry,
        &frame,
        &DerivedPoints::for_plan(&frame, &plan, &registry),
    )
    .unwrap();
    let placed = compiled.placement.slot(agg.id).unwrap();
    assert_eq!(placed.regions.len(), 1);
    let main = &placed.regions[0];
    assert_eq!(main.key, RegionKey::Main);
    assert_eq!(main.state_owner, Some(id("dsd_agg")));
    assert_eq!(main.output_points, vec![point(5)]);
    assert_eq!(
        main.upstream,
        vec![UpstreamPoint {
            id: point(0),
            clock: ClockId::Source(id("dsd_in")),
            derived: false,
        }]
    );
    assert!(compiled.sub_programs.is_empty());
    assert!(compiled
        .placement
        .slots()
        .iter()
        .all(|placed| placed.regions.len() == 1));
    assert_eq!(
        compiled.placement.region_of(&id("dsd_agg")),
        Some((agg.id, RegionKey::Main))
    );
    assert_eq!(compiled.placement.admission_upstream(&id("dsd_agg")), [point(0)].into());
    let program = &compiled.programs[agg.id.get()];
    assert_eq!(program.region(), &RegionKey::Main);
    assert_eq!(program.output_points(), &[point(5)]);
    assert_eq!(program.state_upstream(), main.upstream.as_slice());

    // Every exit on the path from DogStatsD's point to aggregation forwards its progress, and no other.
    let progress = compiled
        .placement
        .slots()
        .iter()
        .flat_map(|placed| {
            let name = frame.slot(placed.slot).unwrap().name.to_string();
            placed.regions.iter().flat_map(move |region| {
                let name = name.clone();
                region
                    .progress
                    .iter()
                    .map(move |(exit, points)| (name.clone(), exit.clone(), points.clone()))
            })
        })
        .collect::<Vec<_>>();
    let exit_to = |from: &str, to: &str| {
        let to = slot_named(&frame, to).id;
        StepExit::Slot(slot_named(&frame, from).exit_to(&FrameTarget::Slot(to)).unwrap().index)
    };
    assert_eq!(
        progress,
        vec![
            (
                "dsd_pre_aggregation".to_string(),
                exit_to("dsd_pre_aggregation", "dsd_prefix_filter_region"),
                vec![point(0)]
            ),
            (
                "dsd_prefix_filter_region".to_string(),
                exit_to("dsd_prefix_filter_region", "dsd_no_agg_split_region"),
                vec![point(0)]
            ),
            (
                "dsd_no_agg_split_region".to_string(),
                exit_to("dsd_no_agg_split_region", "dsd_agg"),
                vec![point(0)]
            ),
        ]
    );
    assert_eq!(
        compiled.programs[slot_named(&frame, "dsd_no_agg_split_region").id.get()].progress(),
        &[(exit_to("dsd_no_agg_split_region", "dsd_agg"), vec![point(0)])]
    );

    // The edges into and out of aggregation are isolation decisions.
    let isolation = frame
        .fusion()
        .iter()
        .filter(|decision| matches!(decision.verdict, FusionVerdict::Isolation { slot } if slot == agg.id))
        .map(|decision| (decision.from.to_string(), decision.to.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        isolation,
        vec![
            ("dsd_tag_filterlist.out".to_string(), "dsd_agg.in".to_string()),
            ("dsd_agg.out".to_string(), "dsd_post_agg_filter_in.0".to_string()),
        ]
    );
}

/// A plan for frame errors: `nodes` and `edges`, written as `(from, to)` ports, with one output `out`.
fn plan_of(nodes: Value, edges: &[(&str, &str)]) -> Plan {
    Plan::from_value(json!({
        "ir_version": 1,
        "nodes": nodes,
        "edges": edges.iter().map(|(from, to)| json!({"from": from, "to": to})).collect::<Vec<_>>(),
        "outputs": [{"id": "out", "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "recipients": ["out/out"]}],
    }))
    .unwrap()
}

fn catalog() -> MemoryCatalog {
    MemoryCatalog::new().with_output(
        OutputId::new("out").unwrap(),
        CatalogOutput {
            accepts: PortType::ANY,
            recipients: vec![RecipientId::new("out/out").unwrap()],
        },
    )
}

fn source(id: &str) -> Value {
    source_of(id, json!({"binding": "dogstatsd", "timestamps": "untimestamped"}))
}

fn source_of(id: &str, params: Value) -> Value {
    json!({"id": id, "op": "source", "params": params})
}

fn aggregate(id: &str, window: &str) -> Value {
    json!({"id": id, "op": "aggregate", "params": {
        "window": window, "flush_interval": "15s", "context_limit": 100, "counter_expiry": "5m",
        "flush_open_windows": false, "histogram_aggregates": ["max"], "histogram_percentiles": [],
        "copy_to_distribution": false, "copy_to_distribution_prefix": ""}})
}

fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params})
}

fn emit() -> Value {
    node("emit", "emit", json!({"output": "out"}))
}

#[test]
fn a_state_owner_fed_by_several_points_or_by_another_places_and_one_point_over_two_paths_is_refused() {
    let registry = OperatorRegistry::native();
    let compiled = |plan: &Plan, frame: &Frame| {
        compile_unvalidated(plan, &registry, frame, &DerivedPoints::for_plan(frame, plan, &registry))
    };
    // Two sources merged before aggregation: two upstream points.
    let two_points = plan_of(
        json!([
            source("a"),
            source_of("b", json!({"binding": "otlp"})),
            node("join", "merge", json!({"inputs": 2})),
            aggregate("agg", "10s"),
            emit()
        ]),
        &[
            ("a.metrics", "join.0"),
            ("b.metrics", "join.1"),
            ("join.out", "agg.in"),
            ("agg.out", "emit.in"),
        ],
    );
    // Its watermark is the minimum over both points, each of its own clock.
    let frame = frame_of(&two_points).unwrap();
    let placed = compiled(&two_points, &frame).unwrap().placement;
    let (slot, region) = placed.region_of(&id("agg")).unwrap();
    assert_eq!(region, RegionKey::Main);
    assert_eq!(
        placed.slot(slot).unwrap().regions[0].upstream,
        vec![
            UpstreamPoint {
                id: point(0),
                clock: ClockId::Source(id("a")),
                derived: false
            },
            UpstreamPoint {
                id: point(1),
                clock: ClockId::Source(id("b")),
                derived: false
            },
        ]
    );

    // A state owner fed only by another's time-driven output: its upstream is the other's derived point, through which
    // the source's point is upstream of it.
    let chained = plan_of(
        json!([
            source("a"),
            aggregate("first", "10s"),
            aggregate("second", "1m"),
            emit()
        ]),
        &[
            ("a.metrics", "first.in"),
            ("first.out", "second.in"),
            ("second.out", "emit.in"),
        ],
    );
    let frame = frame_of(&chained).unwrap();
    let first = frame.boot_output_points(&id("first")).unwrap().to_vec();
    assert_eq!(first, vec![point(1)]);
    let placed = compiled(&chained, &frame).unwrap().placement;
    let (slot, _) = placed.region_of(&id("second")).unwrap();
    assert_eq!(
        placed.slot(slot).unwrap().regions[0].upstream,
        vec![UpstreamPoint {
            id: point(1),
            clock: ClockId::Derived(id("first")),
            derived: true
        }]
    );
    assert_eq!(placed.admission_upstream(&id("second")), [point(0)].into());

    // One point over two paths: a chain of mappers that R1 pipelines, one per slot since each alone costs the whole
    // budget, beside the split's direct edge to the merge, which runs with the last of them: the split's slot reaches
    // the merge's slot directly and through the slots between.
    let mapper = |id: &str| {
        node(
            id,
            "dogstatsd_mapper",
            json!({
                "profiles": [{"name": "test", "prefix": "app.", "mappings": [{
                    "match": "app.*", "match_type": "wildcard", "name": "mapped", "tags": []
                }]}],
                "cache_size": 0,
                "string_interner_bytes": 65_536
            }),
        )
    };
    let two_paths = plan_of(
        json!([
            source("a"),
            node("pick", "split", json!({"predicate": {"name": {"prefix_in": ["x."]}}})),
            mapper("first"),
            mapper("second"),
            mapper("third"),
            node("join", "merge", json!({"inputs": 2})),
            aggregate("agg", "10s"),
            emit()
        ]),
        &[
            ("a.metrics", "pick.in"),
            ("pick.true", "first.in"),
            ("first.out", "second.in"),
            ("second.out", "third.in"),
            ("third.out", "join.0"),
            ("pick.false", "join.1"),
            ("join.out", "agg.in"),
            ("agg.out", "emit.in"),
        ],
    );
    let without_agg = {
        let mut value = serde_json::to_value(&two_paths).unwrap();
        value["nodes"]
            .as_array_mut()
            .unwrap()
            .retain(|node| node["id"] != "agg");
        let edges = value["edges"].as_array_mut().unwrap();
        edges.retain(|edge| edge["to"] != "agg.in" && edge["from"] != "agg.out");
        edges.push(json!({"from": "join.out", "to": "emit.in"}));
        Plan::from_value(value).unwrap()
    };
    let stateless = Frame::compute(&without_agg, &OperatorRegistry::native(), &namer).unwrap();
    assert!(
        stateless
            .fusion()
            .iter()
            .filter(|decision| matches!(decision.verdict, FusionVerdict::Pipelined { .. }))
            .count()
            >= 2,
        "R1 pipelines the chain over three slots: {:?}",
        stateless.fusion()
    );
    // The boot layout has a frame; placing the plan finds the point's two paths, along which its batches could
    // overtake each other.
    let frame = frame_of(&two_paths).unwrap();
    let Err(CompileError::Placement {
        source: PlacementError::Unplaceable { problems },
    }) = compiled(&two_paths, &frame)
    else {
        panic!("expected the plan to be refused");
    };
    let [problem] = problems.as_slice() else {
        panic!("expected one problem: {:?}", problems);
    };
    assert_eq!(problem.node, Some(id("agg")));
    assert_eq!(problem.capability, None);
    assert!(
        problem
            .message
            .contains("admission point `a.metrics` reaches this state owner over more than one path"),
        "{}",
        problem.message
    );
}

#[test]
fn a_candidate_places_a_state_owner_by_its_inputs_in_a_sub_region_after_the_nodes_before_it() {
    let boot = plan_of(
        json!([
            source("a"),
            node("pick", "split", json!({"predicate": {"name": {"prefix_in": ["x."]}}})),
            node("pick_false", "drop", json!({})),
            aggregate("dsd_agg", "10s"),
            emit()
        ]),
        &[
            ("a.metrics", "pick.in"),
            ("pick.true", "dsd_agg.in"),
            ("pick.false", "pick_false.in"),
            ("dsd_agg.out", "emit.in"),
        ],
    );
    let registry = OperatorRegistry::native();
    let frame = frame_of(&boot).unwrap();
    let validated = |plan: &Plan| {
        let report = Validator::new(&registry, &catalog()).validate(plan);
        assert!(report.is_valid(), "{}", report);
        report
    };

    let compiled = |plan: &Plan| {
        compile(
            plan,
            &validated(plan),
            &registry,
            &frame,
            &DerivedPoints::for_plan(&frame, plan, &registry),
        )
        .unwrap()
    };
    let agg = slot_named(&frame, "dsd_agg");
    let pick = slot_named(&frame, "pick_region");

    // A second state owner on the other branch runs in the split's slot, in a sub-region the split's region feeds
    // through an internal exit.
    let branched = plan_of(
        json!([
            source("a"),
            node("pick", "split", json!({"predicate": {"name": {"prefix_in": ["x."]}}})),
            aggregate("dsd_agg", "10s"),
            aggregate("second", "1m"),
            node("join", "merge", json!({"inputs": 2})),
            emit()
        ]),
        &[
            ("a.metrics", "pick.in"),
            ("pick.true", "dsd_agg.in"),
            ("pick.false", "second.in"),
            ("dsd_agg.out", "join.0"),
            ("second.out", "join.1"),
            ("join.out", "emit.in"),
        ],
    );
    let branched = compiled(&branched);
    let second = RegionKey::State(id("second"));
    assert_eq!(
        branched.placement.region_of(&id("dsd_agg")),
        Some((agg.id, RegionKey::Main))
    );
    assert_eq!(
        branched.placement.region_of(&id("second")),
        Some((pick.id, second.clone()))
    );
    let regions = &branched.placement.slot(pick.id).unwrap().regions;
    assert_eq!(
        regions.iter().map(|region| region.key.clone()).collect::<Vec<_>>(),
        vec![RegionKey::Main, second.clone()]
    );
    assert_eq!(regions[0].internal, vec![(second.clone(), ExitRouting::One)]);
    assert_eq!(regions[1].state_owner, Some(id("second")));
    assert_eq!(regions[1].output_points, vec![point(frame.derived_point_base())]);
    assert_eq!(
        regions[1].upstream,
        vec![UpstreamPoint {
            id: point(0),
            clock: ClockId::Source(id("a")),
            derived: false
        }]
    );
    // The split's region forwards the point's progress on both branches: to aggregation's slot and to the sub-region.
    let to_agg = StepExit::Slot(pick.exit_to(&FrameTarget::Slot(agg.id)).unwrap().index);
    assert_eq!(
        regions[0].progress,
        vec![
            (to_agg, vec![point(0)]),
            (StepExit::Region(second.clone()), vec![point(0)])
        ]
    );
    let [sub] = branched.sub_programs.as_slice() else {
        panic!("one sub-region");
    };
    assert_eq!((sub.slot(), sub.region()), (pick.id, &second));
    assert_eq!(sub.output_points(), regions[1].output_points.as_slice());
    assert_eq!(
        sub.entries().iter().map(|entry| entry.key).collect::<Vec<_>>(),
        vec![
            EntryKey::Region(EntryId::new(0)),
            EntryKey::StateOutput(KernelIndex::new(0))
        ]
    );
    let decides = branched.programs[pick.id.get()].entries()[0]
        .steps
        .iter()
        .filter_map(|step| match step {
            Step::Exit { exit, entry, .. } => Some((exit.clone(), entry.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        decides.contains(&(StepExit::Region(second.clone()), EntrySource::Region(EntryId::new(0)))),
        "{:?}",
        decides
    );
    assert_ne!(
        sub.fingerprint(),
        branched.programs[pick.id.get()].fingerprint(),
        "a sub-region's program is told apart from its slot's"
    );

    // A state owner after the one in aggregation's slot runs there, in a sub-region its time-driven output feeds,
    // over its derived point.
    let chained = plan_of(
        json!([
            source("a"),
            node("pick", "split", json!({"predicate": {"name": {"prefix_in": ["x."]}}})),
            node("pick_false", "drop", json!({})),
            aggregate("dsd_agg", "10s"),
            aggregate("again", "1m"),
            emit()
        ]),
        &[
            ("a.metrics", "pick.in"),
            ("pick.true", "dsd_agg.in"),
            ("pick.false", "pick_false.in"),
            ("dsd_agg.out", "again.in"),
            ("again.out", "emit.in"),
        ],
    );
    let chained = compiled(&chained);
    let again = RegionKey::State(id("again"));
    assert_eq!(chained.placement.region_of(&id("again")), Some((agg.id, again.clone())));
    let regions = &chained.placement.slot(agg.id).unwrap().regions;
    let dsd_agg = frame.boot_output_points(&id("dsd_agg")).unwrap().to_vec();
    assert_eq!(regions[0].output_points, dsd_agg);
    assert_eq!(regions[0].internal, vec![(again.clone(), ExitRouting::One)]);
    assert_eq!(
        regions[0].progress,
        vec![(StepExit::Region(again.clone()), dsd_agg.clone())]
    );
    assert_eq!(
        regions[1].upstream,
        vec![UpstreamPoint {
            id: dsd_agg[0],
            clock: ClockId::Derived(id("dsd_agg")),
            derived: true
        }]
    );
    assert_eq!(chained.placement.admission_upstream(&id("again")), [point(0)].into());
    // Aggregation's time-driven output enters the sub-region; the sub-region's leaves by the slot's exit.
    let output = |program: &RegionProgram| {
        program
            .entries()
            .iter()
            .find(|entry| matches!(entry.key, EntryKey::StateOutput(_)))
            .unwrap()
            .steps
            .clone()
    };
    assert_eq!(
        output(&chained.programs[agg.id.get()]),
        vec![Step::Exit {
            register: RegisterId::new(0),
            exit: StepExit::Region(again.clone()),
            entry: EntrySource::Region(EntryId::new(0)),
        }]
    );
    let emit_slot = slot_named(&frame, "emit_region");
    assert_eq!(
        output(&chained.sub_programs[0]),
        vec![Step::Exit {
            register: RegisterId::new(0),
            exit: StepExit::Slot(agg.exit_to(&FrameTarget::Slot(emit_slot.id)).unwrap().index),
            entry: EntrySource::Region(EntryId::new(0)),
        }]
    );
}

#[test]
fn a_boot_state_owner_fed_straight_by_an_admission_point_keeps_its_slot_which_the_points_slot_passes_batches_to() {
    // At boot, the source's batches reach aggregation through the split's region. A candidate without the split feeds
    // aggregation straight from the source: the split's region keeps the point's entry, and passes its batches,
    // unchanged, by its exit to the isolation slot, where they enter aggregation.
    let boot = plan_of(
        json!([
            source("a"),
            node("pick", "split", json!({"predicate": {"name": {"prefix_in": ["x."]}}})),
            node("pick_false", "drop", json!({})),
            aggregate("dsd_agg", "10s"),
            emit()
        ]),
        &[
            ("a.metrics", "pick.in"),
            ("pick.true", "dsd_agg.in"),
            ("pick.false", "pick_false.in"),
            ("dsd_agg.out", "emit.in"),
        ],
    );
    let registry = OperatorRegistry::native();
    let frame = frame_of(&boot).unwrap();
    let validated = |plan: &Plan| {
        let report = Validator::new(&registry, &catalog()).validate(plan);
        assert!(report.is_valid(), "{}", report);
        report
    };
    let straight = plan_of(
        json!([source("a"), aggregate("dsd_agg", "10s"), emit()]),
        &[("a.metrics", "dsd_agg.in"), ("dsd_agg.out", "emit.in")],
    );
    let placement = place(
        &straight,
        &validated(&straight),
        &registry,
        &frame,
        &DerivedPoints::for_plan(&frame, &straight, &registry),
    )
    .unwrap();
    let pick = slot_named(&frame, "pick_region");
    let agg = slot_named(&frame, "dsd_agg");
    let to_agg = pick.exit_to(&FrameTarget::Slot(agg.id)).unwrap().index;
    assert_eq!(placement.site(&id("dsd_agg")), Some(Site::Slot(agg.id)));
    let passing = placement.slot(pick.id).unwrap();
    assert!(passing.nodes.is_empty());
    assert_eq!(
        passing
            .entries
            .iter()
            .map(|entry| (entry.key, entry.source, entry.input.to_string()))
            .collect::<Vec<_>>(),
        vec![(
            EntryKey::Admission(point(0)),
            FrameSource::Admission(point(0)),
            "dsd_agg.in".to_string()
        )]
    );
    assert_eq!(passing.exits_used, vec![to_agg]);
    assert_eq!(
        placement
            .slot(agg.id)
            .unwrap()
            .entries
            .iter()
            .map(|entry| (entry.key, entry.source, entry.input.to_string()))
            .collect::<Vec<_>>(),
        vec![(
            EntryKey::Region(EntryId::new(0)),
            FrameSource::Exit {
                slot: pick.id,
                exit: to_agg
            },
            "dsd_agg.in".to_string()
        )]
    );

    // The split's region's program passes the point's batches to the isolation slot's entry; aggregation takes them
    // there.
    let compiled = compile(
        &straight,
        &validated(&straight),
        &registry,
        &frame,
        &DerivedPoints::for_plan(&frame, &straight, &registry),
    )
    .unwrap();
    let steps = |slot: SlotId| {
        compiled.programs[slot.get()]
            .entries()
            .iter()
            .map(|entry| (entry.key, entry.steps.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        steps(pick.id),
        vec![(
            EntryKey::Admission(point(0)),
            vec![Step::Exit {
                register: RegisterId::new(0),
                exit: StepExit::Slot(to_agg),
                entry: EntrySource::Region(EntryId::new(0)),
            }]
        )]
    );
    assert_eq!(
        steps(agg.id)[0],
        (
            EntryKey::Region(EntryId::new(0)),
            vec![Step::State {
                kernel: KernelIndex::new(0),
                register: RegisterId::new(0),
                params: StateParamsIndex::new(0),
            }]
        )
    );

    // In a topology without a boot state owner, the state owner runs in the main region of the slot the point leads
    // to.
    let stateless = plan_of(
        json!([
            source("a"),
            node("pick", "split", json!({"predicate": {"name": {"prefix_in": ["x."]}}})),
            node("pick_false", "drop", json!({})),
            emit()
        ]),
        &[
            ("a.metrics", "pick.in"),
            ("pick.true", "emit.in"),
            ("pick.false", "pick_false.in"),
        ],
    );
    let frame = frame_of(&stateless).unwrap();
    let placement = place(
        &straight,
        &validated(&straight),
        &registry,
        &frame,
        &DerivedPoints::for_plan(&frame, &straight, &registry),
    )
    .unwrap();
    let pick = slot_named(&frame, "pick_region");
    assert_eq!(placement.region_of(&id("dsd_agg")), Some((pick.id, RegionKey::Main)));
    assert_eq!(placement.admission_upstream(&id("dsd_agg")), [point(0)].into());
}

#[test]
fn a_state_owners_kernel_is_keyed_by_its_lineage_and_its_per_call_parameters_by_their_own_hash() {
    let plan = default_program();
    let registry = OperatorRegistry::native();
    let frame = frame_of(&plan).unwrap();
    let catalog = default_program_catalog();
    let compiled_with = |plan: &Plan| {
        let report = Validator::new(&registry, &catalog).validate(plan);
        assert!(report.is_valid(), "{}", report);
        compile(
            plan,
            &report,
            &registry,
            &frame,
            &DerivedPoints::for_plan(&frame, plan, &registry),
        )
        .unwrap()
    };
    let compiled = compiled_with(&plan);
    let slot = slot_named(&frame, "dsd_agg").id;
    let program = &compiled.programs[slot.get()];
    let [kernel] = program.kernels() else {
        panic!("aggregation's region runs one kernel");
    };
    let lineage = kernel.lineage.as_ref().expect("a state owner's kernel has a lineage");
    assert_eq!(lineage.flush_interval, Duration::from_secs(15));
    assert_eq!(kernel.key.params_hash, lineage.key.contract, "keyed by its lineage");
    assert_eq!(program.state_params().len(), 1);
    let entries = program
        .entries()
        .iter()
        .map(|entry| (entry.key, entry.steps.clone()))
        .collect::<Vec<_>>();
    let post = slot_named(&frame, "dsd_post_agg_filter_region");
    let to_post = frame.slots()[slot.get()]
        .exit_to(&FrameTarget::Slot(post.id))
        .unwrap()
        .index;
    assert_eq!(
        entries,
        vec![
            (
                EntryKey::Region(EntryId::new(0)),
                vec![Step::State {
                    kernel: KernelIndex::new(0),
                    register: RegisterId::new(0),
                    params: StateParamsIndex::new(0),
                }]
            ),
            (
                EntryKey::StateOutput(KernelIndex::new(0)),
                vec![Step::Exit {
                    register: RegisterId::new(0),
                    exit: StepExit::Slot(to_post),
                    entry: EntrySource::Region(EntryId::new(0)),
                }]
            ),
        ]
    );

    // (parameter, value, same lineage): only the window is the lineage's; every change changes the fingerprint.
    let cases = [
        ("context_limit", json!(5), true),
        ("counter_expiry", json!("1m"), true),
        ("flush_interval", json!("20s"), true),
        ("window", json!("20s"), false),
    ];
    for (parameter, value, same_lineage) in cases {
        let mut changed = serde_json::to_value(&plan).unwrap();
        let nodes = changed["nodes"].as_array_mut().unwrap();
        // The timestamp split's window is the aggregation's, and follows it.
        for node in nodes.iter_mut() {
            let follows = parameter == "window" && node["id"] == "dsd_no_agg_split";
            if node["id"] == "dsd_agg" || follows {
                node["params"][parameter] = value.clone();
            }
        }
        let changed = Plan::from_value(changed).unwrap();
        let other = compiled_with(&changed);
        let other_program = &other.programs[slot.get()];
        let other_kernel = &other_program.kernels()[0];
        assert_eq!(
            other_kernel.key == kernel.key,
            same_lineage,
            "{} {}: kernel key",
            parameter,
            value
        );
        assert_eq!(
            other_kernel.lineage.as_ref().unwrap().key == lineage.key,
            same_lineage,
            "{} {}: lineage",
            parameter,
            value
        );
        assert_ne!(
            other_program.fingerprint(),
            program.fingerprint(),
            "{} {}: fingerprint",
            parameter,
            value
        );
        assert_ne!(
            other_program.state_params()[0].hash,
            program.state_params()[0].hash,
            "{} {}: state parameters",
            parameter,
            value
        );
    }
}

/// A filter pushed into a state owner, for tests: it drops every record.
struct DropsEverything;

impl saluki_metric_kernels::aggregate::EmissionFilter for DropsEverything {
    fn drops(
        &self, _context: &saluki_core::data_model::event::metric::context::Context,
        _kind: saluki_metric_kernels::aggregate::OutputKind,
        _metadata: &saluki_core::data_model::event::metric::MetricMetadata,
    ) -> bool {
        true
    }

    fn site(&self) -> saluki_metric_kernels::aggregate::SiteIndex {
        saluki_metric_kernels::aggregate::SiteIndex::new(0)
    }
}

#[test]
fn a_pushed_filter_reaches_its_state_owners_parameters_and_fingerprint_and_a_site_without_one_is_recorded_skipped() {
    let plan = default_program();
    let registry = OperatorRegistry::native();
    let frame = frame_of(&plan).unwrap();
    let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
    assert!(report.is_valid(), "{}", report);
    let placement = place(
        &plan,
        &report,
        &registry,
        &frame,
        &DerivedPoints::for_plan(&frame, &plan, &registry),
    )
    .unwrap();
    let slot = slot_named(&frame, "dsd_agg").id;
    let test_site = |owner: &str, filter: &str| PushedFilterSite {
        owner: id(owner),
        filter: id(filter),
        drop_port: "true".parse().unwrap(),
        drop: id(&format!("{}_true", filter)),
        predicate: crate::predicate::Predicate::Const(true),
        drops_matching: true,
        emitted: crate::model::KindSet::of(&[crate::model::Kind::Rate]),
        histograms: false,
    };
    let site = test_site("dsd_agg", "dsd_post_agg_filter");
    let filter: std::sync::Arc<dyn saluki_metric_kernels::aggregate::EmissionFilter> =
        std::sync::Arc::new(DropsEverything);
    let programs = |pushed: &[program::SiteFilter]| {
        program::compile_programs_with(&plan, &registry, &frame, &placement, pushed, DecisionRewrites::ALL)
            .unwrap()
            .0
    };
    let plain = programs(&[]);
    let pushed = programs(&[program::SiteFilter {
        site: site.clone(),
        filter: std::sync::Arc::clone(&filter),
    }]);

    // The state owner's per-call parameters carry the filter, and the site's identity is in their hash, so in the
    // program's fingerprint; no other program changes.
    let carried = |programs: &[RegionProgram]| {
        programs[slot.get()].state_params()[0]
            .params
            .downcast_ref::<saluki_metric_kernels::aggregate::AggregateStepParams>()
            .expect("aggregation's per-call parameters")
            .flush
            .filter
            .clone()
    };
    assert!(carried(&plain).is_none());
    assert!(carried(&pushed).is_some_and(|carried| std::sync::Arc::ptr_eq(&carried, &filter)));
    assert_ne!(
        pushed[slot.get()].state_params()[0].hash,
        plain[slot.get()].state_params()[0].hash
    );
    assert_ne!(pushed[slot.get()].fingerprint(), plain[slot.get()].fingerprint());
    for (index, (pushed, plain)) in pushed.iter().zip(&plain).enumerate() {
        if index != slot.get() {
            assert_eq!(pushed.fingerprint(), plain.fingerprint(), "slot {}", index);
        }
    }

    // A site R5b found but whose state owner is given no filter suppresses nothing: its record is skipped, with no
    // telemetry effect, and the site is not kept. Every other record stays as it was.
    let applied = |nodes: Vec<NodeId>| RewriteRecord {
        id: RewriteId::R5b,
        outcome: RewriteOutcome::Applied,
        nodes,
        justification: "the filter commutes with aggregation's emission".to_string(),
        telemetry: vec![TelemetryEffect {
            node: id("dsd_agg"),
            change: TelemetryChange::Changed,
            detail: "it sends fewer records downstream".to_string(),
        }],
        capacity_rule: false,
    };
    let other_site = test_site("second", "second_filter");
    let unrelated = RewriteRecord {
        id: RewriteId::R3,
        ..applied(vec![id("dsd_prefix_filter_namespace_prefix")])
    };
    let records = vec![
        applied(vec![id("dsd_agg"), id("dsd_post_agg_filter")]),
        applied(vec![id("second"), id("second_filter")]),
        unrelated.clone(),
    ];
    let filtered = [program::SiteFilter {
        site: other_site.clone(),
        filter,
    }];
    let (sites, records) = super::reconcile_pushed(vec![site, other_site.clone()], records, &filtered);
    assert_eq!(sites, vec![other_site]);
    assert_eq!(
        records[0].outcome,
        RewriteOutcome::Skipped {
            reason: super::NO_SITE_FILTER.to_string()
        }
    );
    assert!(records[0].telemetry.is_empty());
    assert_eq!(records[1], applied(vec![id("second"), id("second_filter")]));
    assert_eq!(records[2], unrelated);
}
