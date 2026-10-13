//! Frames on lanes: the lane layout `Frame::compute_with` computes from a lane policy, with expected results written by
//! hand from the stage 9 design (lanes per slot, lane points, per-lane output points, and the routing of every edge);
//! placement and programs on lanes (an entry for every lane's point, state owners on state lanes only when their state
//! is partitioned); and, over random plans and lane policies, that slots follow fences only, decisions stay whole, and
//! every lane of an admission point has exactly one path to every lane of each state owner it feeds.

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use proptest::{collection::vec, prelude::*, strategy::ValueTree as _};
use saluki_core::execution::{AdmissionPointId, EntryId};
use serde_json::{json, Value};

use super::*;
use crate::{
    canonical::PlanHash,
    ir::{DecodedParams, NativeComponentId, NodeId, OutputId, ParamSpec, Plan, RecipientId},
    model::PortType,
    operators::{Aggregate, EffectContract, KernelSpec, OperatorDef, OperatorRegistry, Ports},
    testing::{default_program_catalog, DEFAULT_PROGRAM_JSON},
    validate::{CatalogOutput, MemoryCatalog, ValidationReport, Validator},
};

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    crate::testing::default_program_slot_name(nodes)
}

fn point(id: u32) -> AdmissionPointId {
    AdmissionPointId::new(id)
}

fn default_program() -> Plan {
    Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap()
}

fn frame_on(lanes: LanePolicy, pipelining: bool) -> Result<Frame, FrameError> {
    Frame::compute_with(
        &default_program(),
        &OperatorRegistry::native(),
        &namer,
        &FrameHosts::new(),
        &lanes,
        FrameOptions { pipelining },
    )
}

fn slot_named<'f>(frame: &'f Frame, name: &str) -> &'f Slot {
    frame
        .slots()
        .iter()
        .find(|slot| slot.name.as_str() == name)
        .unwrap_or_else(|| panic!("no slot `{}`", name))
}

fn exit_to<'s>(frame: &Frame, slot: &'s Slot, target: &str) -> &'s Exit {
    let target = slot_named(frame, target).id;
    slot.exit_to(&FrameTarget::Slot(target))
        .unwrap_or_else(|| panic!("`{}` has no exit to slot {}", slot.name, target))
}

#[test]
fn a_frame_on_one_lane_with_the_default_options_is_the_frame_without_lanes() {
    let without = Frame::compute(&default_program(), &OperatorRegistry::native(), &namer).unwrap();
    let on_one = frame_on(LanePolicy::ONE, true).unwrap();
    assert_eq!(on_one, without);
    assert_eq!(on_one.lane_policy(), &LanePolicy::ONE);
    assert_eq!(on_one.options(), FrameOptions::default());
    assert!(on_one.slots().iter().all(|slot| slot.lanes == 1));
    assert!(on_one
        .slots()
        .iter()
        .flat_map(|slot| &slot.exits)
        .all(|exit| exit.routing == ExitRouting::One));
    assert!(on_one
        .admission_points()
        .iter()
        .all(|point| point.lanes == 1 && point.routing == PointRouting::One));
    assert_eq!(
        on_one.admission_point_ids().collect::<Vec<_>>(),
        (0..5).map(point).collect::<Vec<_>>()
    );
}

#[test]
fn without_pipelining_fusion_is_maximal_between_fences() {
    let frame = frame_on(LanePolicy::ONE, false).unwrap();
    let names = frame
        .slots()
        .iter()
        .map(|slot| slot.name.to_string())
        .collect::<Vec<_>>();
    // The pre-aggregation chain is one slot, aggregation its isolation slot, and everything after it, the
    // post-aggregation filter included, one slot fed by aggregation, the passthrough, and the other sources.
    assert_eq!(names, vec!["dsd_pre_aggregation", "dsd_agg", "metrics_enrich"]);
    assert!(frame
        .fusion()
        .iter()
        .all(|decision| !matches!(decision.verdict, FusionVerdict::Pipelined { .. })));
}

#[test]
fn the_default_program_on_lanes_splits_dogstatsd_into_lane_points_and_routes_each_edge_by_its_ends() {
    let lanes = LanePolicy {
        dogstatsd: 2,
        state: 3,
        post: 4,
    };
    let frame = frame_on(lanes, false).unwrap();
    assert_eq!(frame.lane_policy(), &lanes);

    // DogStatsD's metrics take two consecutive point IDs, one per lane, each feeding its own producer lane; every other
    // source keeps one point, numbered after them, spread over the post-aggregation lanes.
    let points = frame
        .admission_points()
        .iter()
        .map(|info| {
            (
                info.node.to_string(),
                info.id.get(),
                info.lanes,
                info.routing,
                info.feeds_state,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        points,
        vec![
            ("dsd_in".to_string(), 0, 2, PointRouting::Fixed, true),
            ("otlp_in".to_string(), 2, 1, PointRouting::Spread, false),
            ("checks_ipc_in".to_string(), 3, 1, PointRouting::Spread, false),
            ("liveness_in".to_string(), 4, 1, PointRouting::Spread, false),
            ("trace_sampler".to_string(), 5, 1, PointRouting::Spread, false),
        ]
    );
    let dogstatsd = &frame.admission_points()[0];
    assert_eq!(dogstatsd.lane_ids().collect::<Vec<_>>(), vec![point(0), point(1)]);
    assert_eq!(dogstatsd.lane_id(1), Some(point(1)));
    assert_eq!(dogstatsd.lane_id(2), None);
    assert_eq!(dogstatsd.lane_of(point(1)), Some(1));
    assert_eq!(dogstatsd.lane_of(point(2)), None);
    assert_eq!(frame.admission_point(point(1)).map(|info| info.id), Some(point(0)));
    assert_eq!(
        frame.admission_point_ids().collect::<Vec<_>>(),
        (0..6).map(point).collect::<Vec<_>>()
    );

    // Producer lanes, state lanes, and post-aggregation lanes.
    let pre = slot_named(&frame, "dsd_pre_aggregation");
    let agg = slot_named(&frame, "dsd_agg");
    let post = slot_named(&frame, "metrics_enrich");
    assert_eq!((pre.lanes, agg.lanes, post.lanes), (2, 3, 4));

    // Aggregation's watermark is made of both lane points, and each state lane stamps its output at a point of its own,
    // numbered after every point of the sources.
    assert_eq!(
        agg.kind,
        SlotKind::Isolation {
            output_points: vec![point(6), point(7), point(8)],
            upstream: vec![point(0), point(1)],
        }
    );
    assert_eq!(agg.output_point(2), Some(point(8)));
    assert_eq!(agg.output_point(3), None);
    assert_eq!(pre.output_point(0), None);
    assert_eq!(frame.affected_points(&[agg.id]), vec![point(0), point(1)]);
    let order = frame.cancel_lock_order();
    assert_eq!(order.len(), 6);
    assert_eq!(
        &order[4..],
        &[point(0), point(1)],
        "the points that feed state lock last"
    );

    // The exit into aggregation is the repartition edge, routing by key over three lanes, and forwards both points'
    // progress; the passthrough keeps each producer lane's buffers on its post-aggregation lane; aggregation's output
    // spreads over the post-aggregation lanes; exits to outputs have one lane.
    let keyed = exit_to(&frame, pre, "dsd_agg");
    let ExitRouting::Keyed(groups) = &keyed.routing else {
        panic!("the exit into aggregation is keyed: {:?}", keyed.routing);
    };
    assert_eq!(groups.lanes(), 3);
    assert_eq!(keyed.progress_points, vec![point(0), point(1)]);
    assert_eq!(exit_to(&frame, pre, "metrics_enrich").routing, ExitRouting::Affinity);
    assert_eq!(exit_to(&frame, agg, "metrics_enrich").routing, ExitRouting::Spread);
    assert!(pre
        .exits
        .iter()
        .chain(&post.exits)
        .filter(|exit| !matches!(exit.target, FrameTarget::Slot(_)))
        .all(|exit| exit.routing == ExitRouting::One));
}

#[test]
fn the_lanes_of_one_kind_of_slot_change_nothing_else() {
    let frame = frame_on(
        LanePolicy {
            dogstatsd: 1,
            state: 2,
            post: 1,
        },
        false,
    )
    .unwrap();
    let pre = slot_named(&frame, "dsd_pre_aggregation");
    let agg = slot_named(&frame, "dsd_agg");
    assert_eq!((pre.lanes, agg.lanes), (1, 2));
    assert!(matches!(exit_to(&frame, pre, "dsd_agg").routing, ExitRouting::Keyed(_)));
    // One producer lane into one post-aggregation lane: whole buffers.
    assert_eq!(exit_to(&frame, pre, "metrics_enrich").routing, ExitRouting::One);
    assert_eq!(frame.admission_points()[0].routing, PointRouting::One);
    assert_eq!(frame.admission_points()[1].id, point(1));
}

#[test]
fn one_producer_lane_keeps_its_passthrough_on_one_post_aggregation_lane() {
    // A producer lane's batches keep each stream connection's order, so its passthrough goes to one post-aggregation
    // lane, lane `d mod M`, at one producer lane as at several; aggregation's time-driven output and the other sources'
    // points spread.
    for policy in [lanes(1, 1, 4), lanes(1, 2, 4), lanes(3, 1, 4)] {
        let frame = frame_on(policy, false).unwrap();
        let pre = slot_named(&frame, "dsd_pre_aggregation");
        let agg = slot_named(&frame, "dsd_agg");
        let post = slot_named(&frame, "metrics_enrich");
        assert_eq!((pre.lanes, agg.lanes, post.lanes), (policy.dogstatsd, policy.state, 4));
        assert_eq!(
            exit_to(&frame, pre, "metrics_enrich").routing,
            ExitRouting::Affinity,
            "{:?}",
            policy
        );
        assert_eq!(exit_to(&frame, agg, "metrics_enrich").routing, ExitRouting::Spread);
        assert!(frame.admission_points()[1..]
            .iter()
            .all(|point| point.routing == PointRouting::Spread));
    }
}

#[test]
fn a_slot_with_one_lane_that_carries_the_producer_lanes_batches_keeps_them_on_one_lane_downstream() {
    // `joined` runs one lane, since a second aggregation is downstream, and carries the producer lanes' batches that
    // skip the first aggregation: its exit into the slot after the detour keeps them on one lane, as the producer
    // lanes' own exit would. That slot is also fed by the opaque host's readmitted point, which spreads.
    let any = json!({"kinds": "all", "timestamps": "mixed"});
    let plan = plan_of(
        json!([
            dogstatsd("src"),
            pick("pick"),
            aggregate("first"),
            node("joined", "merge", json!({"inputs": 2})),
            node("again", "split", json!({"predicate": {"name": {"prefix_in": ["y."]}}})),
            aggregate("second"),
            emit(),
            node("detour", "split", json!({"predicate": {"name": {"prefix_in": ["z."]}}})),
            node(
                "host",
                "opaque_host",
                json!({"component": "host", "input": any, "outputs": {"out": any}})
            ),
            node("rejoined", "merge", json!({"inputs": 2})),
            node("tail", "emit", json!({"output": "out"})),
        ]),
        &[
            ("src.metrics", "pick.in"),
            ("pick.true", "first.in"),
            ("pick.false", "joined.0"),
            ("first.out", "joined.1"),
            ("joined.out", "again.in"),
            ("again.true", "second.in"),
            ("second.out", "emit.in"),
            ("again.false", "detour.in"),
            ("detour.true", "host.in"),
            ("detour.false", "rejoined.0"),
            ("host.out", "rejoined.1"),
            ("rejoined.out", "tail.in"),
        ],
    );
    for dogstatsd in [1, 2] {
        let frame = laned(&plan, lanes(dogstatsd, 2, 3)).unwrap();
        let (pick, joined, rejoined) = (
            slot_named(&frame, "pick_region"),
            slot_named(&frame, "joined_region"),
            slot_named(&frame, "rejoined_region"),
        );
        assert_eq!((pick.lanes, joined.lanes, rejoined.lanes), (dogstatsd, 1, 3));
        assert_eq!(exit_to(&frame, pick, "joined_region").routing, ExitRouting::One);
        assert_eq!(
            exit_to(&frame, joined, "rejoined_region").routing,
            ExitRouting::Affinity
        );
        let host = frame
            .admission_point_at(&"host".parse().unwrap(), &"out".parse().unwrap())
            .unwrap();
        assert_eq!(host.routing, PointRouting::Spread);
        // The first aggregation's output, merged into `joined`, reaches the second aggregation by key; the second's
        // output spreads.
        let second = slot_named(&frame, "second_region");
        assert!(matches!(
            exit_to(&frame, joined, "second_region").routing,
            ExitRouting::Keyed(_)
        ));
        assert_eq!(exit_to(&frame, second, "emit_region").routing, ExitRouting::Spread);
    }
}

#[test]
fn lane_counts_are_bounded_and_need_maximal_fusion() {
    let lanes = |dogstatsd, state, post| LanePolicy { dogstatsd, state, post };
    for policy in [lanes(0, 1, 1), lanes(1, 65, 1), lanes(1, 1, 0)] {
        let error = frame_on(policy, false).unwrap_err().to_string();
        assert!(error.contains("1 to 64 lanes"), "{}", error);
    }
    assert!(frame_on(lanes(64, 64, 64), false).is_ok());
    let error = frame_on(lanes(2, 1, 1), true).unwrap_err().to_string();
    assert!(error.contains("pipelines slots"), "{}", error);
}

#[test]
fn key_groups_split_the_keys_into_contiguous_ranges_of_groups_per_lane() {
    let one = KeyGroups::new(1);
    assert!((0..256).all(|group| one.owner(group) == 0));
    assert_eq!(KeyGroups::new(0).lanes(), 1, "a table has at least one lane");

    let three = KeyGroups::new(3);
    assert_eq!(three.lanes(), 3);
    let owners = (0..256).map(|group| three.owner(group)).collect::<Vec<_>>();
    assert!(
        owners.windows(2).all(|pair| pair[0] <= pair[1]),
        "ranges are contiguous"
    );
    // Group g belongs to lane g * 3 / 256: lane 0 owns groups 0 to 85, lane 1 86 to 170, lane 2 171 to 255.
    assert_eq!((three.owner(85), three.owner(86)), (0, 1));
    assert_eq!((three.owner(170), three.owner(171)), (1, 2));
    // A key's group is its high byte.
    assert_eq!(KeyGroups::group_of(0x55ff_ffff_ffff_ffff), 0x55);
    assert_eq!(three.lane_of(0), 0);
    assert_eq!(three.lane_of(u64::MAX), 2);
    assert_eq!(three.lane_of(86 << 56), 1);
    // Each lane's groups are one range, and the ranges cover every group once.
    assert_eq!(
        (0..4).map(|lane| three.groups_of(lane)).collect::<Vec<_>>(),
        vec![0..86, 86..171, 171..256, 256..256]
    );
    assert_eq!(one.groups_of(0), 0..256);
    // Shared tables compare by content.
    assert_eq!(Arc::new(KeyGroups::new(3)), Arc::new(three));
}

// Placement and programs on lanes.

/// A plan of `nodes` and `edges`, written as `(from, to)` ports, with one output `out`.
fn plan_of(nodes: Value, edges: &[(&str, &str)]) -> Plan {
    Plan::from_value(json!({
        "ir_version": 1,
        "nodes": nodes,
        "edges": edges.iter().map(|(from, to)| json!({"from": from, "to": to})).collect::<Vec<_>>(),
        "outputs": [{"id": "out", "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": PlanHash::of_bytes(b"out").to_string(), "recipients": ["out/out"]}],
    }))
    .unwrap()
}

fn catalog() -> MemoryCatalog {
    MemoryCatalog::new().with_output(
        OutputId::new("out").unwrap(),
        CatalogOutput {
            accepts: PortType::ANY,
            fingerprint: PlanHash::of_bytes(b"out"),
            recipients: vec![RecipientId::new("out/out").unwrap()],
            legacy_fingerprint: None,
        },
    )
}

fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": id}})
}

fn dogstatsd(id: &str) -> Value {
    node(
        id,
        "source",
        json!({"binding": "dogstatsd", "timestamps": "untimestamped"}),
    )
}

fn otlp(id: &str) -> Value {
    node(id, "source", json!({"binding": "otlp"}))
}

fn aggregation_params() -> Value {
    json!({
        "window": "10s", "flush_interval": "15s", "context_limit": 100, "counter_expiry": "5m",
        "flush_open_windows": false, "histogram_aggregates": ["max"], "histogram_percentiles": [],
        "copy_to_distribution": false, "copy_to_distribution_prefix": "",
    })
}

fn aggregate(id: &str) -> Value {
    node(id, "aggregate", aggregation_params())
}

fn pick(id: &str) -> Value {
    node(id, "split", json!({"predicate": {"name": {"prefix_in": ["x."]}}}))
}

fn emit() -> Value {
    node("emit", "emit", json!({"output": "out"}))
}

/// The frame of `plan` on `lanes`, without pipelining, with the native registry and no delivery hosts.
fn laned(plan: &Plan, lanes: LanePolicy) -> Result<Frame, FrameError> {
    laned_with(plan, &OperatorRegistry::native(), lanes)
}

fn laned_with(plan: &Plan, registry: &OperatorRegistry, lanes: LanePolicy) -> Result<Frame, FrameError> {
    Frame::compute_with(
        plan,
        registry,
        &namer,
        &FrameHosts::new(),
        &lanes,
        FrameOptions { pipelining: false },
    )
}

fn validated(plan: &Plan, registry: &OperatorRegistry) -> ValidationReport {
    let report = Validator::new(registry, &catalog()).validate(plan);
    assert!(report.is_valid(), "{}", report);
    report
}

fn lanes(dogstatsd: u16, state: u16, post: u16) -> LanePolicy {
    LanePolicy { dogstatsd, state, post }
}

#[test]
fn every_lanes_point_has_an_entry_of_its_own_with_the_steps_of_the_first() {
    // The producer lanes run one program: each lane's batches carry its own lane's point, so the program has an entry
    // for each, and they run alike.
    let plan = default_program();
    let registry = OperatorRegistry::native();
    let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
    let frame = frame_on(lanes(3, 2, 2), false).unwrap();
    let compiled = compile(&plan, &report, &registry, &frame).unwrap();
    let pre = slot_named(&frame, "dsd_pre_aggregation");
    let placed = compiled.placement.slot(pre.id).unwrap();
    assert_eq!(
        placed
            .entries
            .iter()
            .map(|entry| (entry.key, entry.source, entry.from.to_string(), entry.input.to_string()))
            .collect::<Vec<_>>(),
        (0..3)
            .map(|lane| (
                EntryKey::Admission(point(lane)),
                FrameSource::Admission(point(lane)),
                "dsd_in.metrics".to_string(),
                "dsd_enrich.in".to_string()
            ))
            .collect::<Vec<_>>()
    );
    // The edge and the slot's inputs name the point by its first lane's ID.
    assert_eq!(pre.inputs, vec![FrameSource::Admission(point(0))]);

    let program = &compiled.programs[pre.id.get()];
    let entries = program.entries();
    assert_eq!(entries.len(), 3);
    for lane in 0..3 {
        let entry = program.entry(EntryKey::Admission(point(lane))).unwrap();
        assert_eq!(
            entry.steps, entries[0].steps,
            "lane {} runs the first lane's steps",
            lane
        );
        assert_eq!(entry.registers, entries[0].registers);
        let header = saluki_core::execution::BatchHeader {
            admission_point: Some(point(lane)),
            ..Default::default()
        };
        assert_eq!(program.entry_for(&header).map(|entry| entry.key), Some(entry.key));
    }
    // A point of another source keeps its one entry, where its edge leads.
    let post = slot_named(&frame, "metrics_enrich");
    let keys = compiled
        .placement
        .slot(post.id)
        .unwrap()
        .entries
        .iter()
        .map(|entry| entry.key);
    assert_eq!(
        keys.filter(|key| matches!(key, EntryKey::Admission(_)))
            .collect::<Vec<_>>(),
        (3..7).map(|id| EntryKey::Admission(point(id))).collect::<Vec<_>>()
    );
}

#[test]
fn a_state_owner_fed_straight_by_dogstatsd_has_no_frame_on_several_producer_lanes_at_any_number_of_state_lanes() {
    // Each producer lane stamps its own point in the lane of the slot it feeds. An isolation slot cannot be that slot:
    // on several lanes it holds shares of its state by key, which a point does not route by, and on one lane it would
    // stamp every lane's point.
    let straight = plan_of(
        json!([dogstatsd("src"), aggregate("dsd_agg"), emit()]),
        &[("src.metrics", "dsd_agg.in"), ("dsd_agg.out", "emit.in")],
    );
    for policy in [lanes(3, 4, 2), lanes(3, 2, 1), lanes(2, 1, 1), lanes(8, 8, 8)] {
        let error = laned(&straight, policy).unwrap_err();
        let FrameError::Lanes { message } = &error else {
            panic!("expected a lane error on {:?}, not {:?}", policy, error);
        };
        let names = format!(
            "the {} producer lanes of `src.metrics` feed state owner `dsd_agg` straight",
            policy.dogstatsd
        );
        assert!(
            message.contains(&names)
                && message.contains("Put a stateless node between them, or lay the frame out on one producer lane."),
            "{}",
            message
        );
    }

    // With a stateless node between them, even one that changes nothing (a `tee` with one port), the point feeds that
    // node's slot lane by lane, and its exit routes by key.
    let between = plan_of(
        json!([
            dogstatsd("src"),
            node("pass", "tee", json!({"ports": 1})),
            aggregate("dsd_agg"),
            emit()
        ]),
        &[
            ("src.metrics", "pass.in"),
            ("pass.0", "dsd_agg.in"),
            ("dsd_agg.out", "emit.in"),
        ],
    );
    let registry = OperatorRegistry::native();
    validated(&between, &registry);
    let frame = laned(&between, lanes(3, 2, 2)).unwrap();
    let (pass, agg) = (slot_named(&frame, "pass_region"), slot_named(&frame, "dsd_agg"));
    assert_eq!((pass.lanes, agg.lanes), (3, 2));
    assert_eq!(frame.admission_points()[0].routing, PointRouting::Fixed);
    let keyed = exit_to(&frame, pass, "dsd_agg");
    assert!(matches!(keyed.routing, ExitRouting::Keyed(ref groups) if groups.lanes() == 2));
    assert_eq!(keyed.progress_points, vec![point(0), point(1), point(2)]);
}

#[test]
fn a_state_owner_fed_straight_by_a_point_with_one_lane_keeps_its_state_on_one_lane() {
    // No exit leads to aggregation, so nothing could route its events by key: it runs on one lane, however many state
    // lanes the policy asks for, whether one producer lane or another source's point feeds it.
    let registry = OperatorRegistry::native();
    for source in [dogstatsd("src"), otlp("src")] {
        let plan = plan_of(
            json!([source, aggregate("dsd_agg"), emit()]),
            &[("src.metrics", "dsd_agg.in"), ("dsd_agg.out", "emit.in")],
        );
        let frame = laned(&plan, lanes(1, 4, 2)).unwrap();
        let agg = slot_named(&frame, "dsd_agg");
        assert_eq!(agg.lanes, 1);
        assert_eq!(
            agg.kind,
            SlotKind::Isolation {
                output_points: vec![point(1)],
                upstream: vec![point(0)],
            }
        );
        let point_info = &frame.admission_points()[0];
        assert_eq!((point_info.lanes, point_info.routing), (1, PointRouting::One));

        let compiled = compile(&plan, &validated(&plan, &registry), &registry, &frame).unwrap();
        let program = &compiled.programs[agg.id.get()];
        let keys = program.entries().iter().map(|entry| entry.key).collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                EntryKey::Admission(point(0)),
                EntryKey::StateOutput(KernelIndex::new(0))
            ]
        );
        let state = vec![Step::State {
            kernel: KernelIndex::new(0),
            register: RegisterId::new(0),
            params: StateParamsIndex::new(0),
        }];
        assert_eq!(program.entries()[0].steps, state);
    }
}

#[test]
fn a_candidate_passes_every_lanes_batches_through_the_keyed_exit_to_the_state_lanes() {
    // At boot, the source's batches reach aggregation through the split's region, on two producer lanes into two state
    // lanes. A candidate without the split feeds aggregation straight from the source: each producer lane keeps its
    // point's entry and passes its batches, unchanged, by the keyed exit to the one entry of the isolation slot.
    let boot = plan_of(
        json!([
            dogstatsd("src"),
            pick("pick"),
            node("pick_false", "drop", json!({})),
            aggregate("dsd_agg"),
            emit()
        ]),
        &[
            ("src.metrics", "pick.in"),
            ("pick.true", "dsd_agg.in"),
            ("pick.false", "pick_false.in"),
            ("dsd_agg.out", "emit.in"),
        ],
    );
    let registry = OperatorRegistry::native();
    let frame = laned(&boot, lanes(2, 2, 1)).unwrap();
    let passing = slot_named(&frame, "pick_region");
    let agg = slot_named(&frame, "dsd_agg");
    assert_eq!((passing.lanes, agg.lanes), (2, 2));
    let to_agg = exit_to(&frame, passing, "dsd_agg");
    assert!(matches!(to_agg.routing, ExitRouting::Keyed(_)));

    let straight = plan_of(
        json!([dogstatsd("src"), aggregate("dsd_agg"), emit()]),
        &[("src.metrics", "dsd_agg.in"), ("dsd_agg.out", "emit.in")],
    );
    let compiled = compile(&straight, &validated(&straight, &registry), &registry, &frame).unwrap();
    let placed = compiled.placement.slot(passing.id).unwrap();
    assert!(placed.nodes.is_empty());
    assert_eq!(
        placed.entries.iter().map(|entry| entry.key).collect::<Vec<_>>(),
        vec![EntryKey::Admission(point(0)), EntryKey::Admission(point(1))]
    );
    assert_eq!(placed.exits_used, vec![to_agg.index]);
    let into_agg = &compiled.placement.slot(agg.id).unwrap().entries;
    assert_eq!(
        into_agg
            .iter()
            .map(|entry| (entry.key, entry.source))
            .collect::<Vec<_>>(),
        vec![(
            EntryKey::Region(EntryId::new(0)),
            FrameSource::Exit {
                slot: passing.id,
                exit: to_agg.index
            }
        )]
    );
    let pass = vec![Step::Exit {
        register: RegisterId::new(0),
        exit: to_agg.index,
        entry: EntrySource::Region(EntryId::new(0)),
    }];
    let program = &compiled.programs[passing.id.get()];
    assert_eq!(program.entries().len(), 2);
    assert!(program.entries().iter().all(|entry| entry.steps == pass));
}

#[test]
fn without_pipelining_aggregation_fed_straight_by_dogstatsd_places_through_the_one_pre_aggregation_slot() {
    // A candidate that drops the whole pre-aggregation chain feeds aggregation straight from DogStatsD. R1's pipelined
    // frame cannot run it: the point's slot has no exit to aggregation, only to the next stage of its pipeline. With
    // fusion maximal, the point's slot is the one that leads to aggregation, so it passes each producer lane's batches
    // through its keyed exit.
    let mut value = serde_json::from_str::<Value>(DEFAULT_PROGRAM_JSON).unwrap();
    let chain = [
        "dsd_enrich",
        "dsd_prefix_filter",
        "dsd_prefix_filter_blocklist",
        "dsd_prefix_filter_blocklist_true",
        "dsd_no_agg_split",
        "dsd_tag_filterlist",
    ];
    let in_chain = |port: &Value| {
        chain
            .iter()
            .any(|node| port.as_str().unwrap().starts_with(&format!("{}.", node)))
    };
    value["nodes"]
        .as_array_mut()
        .unwrap()
        .retain(|node| !chain.contains(&node["id"].as_str().unwrap()));
    let edges = value["edges"].as_array_mut().unwrap();
    edges.retain(|edge| !in_chain(&edge["from"]) && !in_chain(&edge["to"]));
    edges.push(json!({"from": "dsd_in.metrics", "to": "dsd_agg.in"}));
    for node in value["nodes"].as_array_mut().unwrap() {
        if node["id"] == "dsd_post_agg_filter_in" {
            node["params"]["inputs"] = json!(1);
        }
    }
    let straight = Plan::from_value(value).unwrap();
    let registry = OperatorRegistry::native();

    let pipelined = Frame::compute(&default_program(), &registry, &namer).unwrap();
    let error = place_unvalidated(&straight, &registry, &pipelined).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no such isolation slot for `dsd_pre_aggregation`"),
        "{}",
        error
    );

    let frame = frame_on(lanes(2, 2, 2), false).unwrap();
    let compiled = compile_unvalidated(&straight, &registry, &frame).unwrap();
    let pre = slot_named(&frame, "dsd_pre_aggregation");
    let to_agg = exit_to(&frame, pre, "dsd_agg");
    assert!(compiled.placement.slot(pre.id).unwrap().nodes.is_empty());
    let program = &compiled.programs[pre.id.get()];
    let pass = vec![Step::Exit {
        register: RegisterId::new(0),
        exit: to_agg.index,
        entry: EntrySource::Region(EntryId::new(0)),
    }];
    assert_eq!(
        program
            .entries()
            .iter()
            .map(|entry| (entry.key, entry.steps.clone()))
            .collect::<Vec<_>>(),
        vec![
            (EntryKey::Admission(point(0)), pass.clone()),
            (EntryKey::Admission(point(1)), pass)
        ]
    );
}

/// The name of [`UnpartitionedAggregate`].
const UNPARTITIONED: &str = "test_unpartitioned_aggregate";

/// Aggregation behind an isolation fence, whose state the operator does not say how to partition: every rule is the
/// native operator's, but it has no partition key.
struct UnpartitionedAggregate;

impl OperatorDef for UnpartitionedAggregate {
    fn name(&self) -> &'static str {
        UNPARTITIONED
    }

    fn summary(&self) -> &'static str {
        "Aggregates natively, with no partition key."
    }

    fn params(&self) -> &'static [ParamSpec] {
        Aggregate.params()
    }

    fn check_params(&self, params: &DecodedParams) -> Vec<crate::ir::ParamError> {
        Aggregate.check_params(params)
    }

    fn ports(&self, params: &DecodedParams) -> Ports {
        Aggregate.ports(params)
    }

    fn contract(&self, params: &DecodedParams) -> EffectContract {
        Aggregate.contract(params)
    }

    fn output_types(&self, params: &DecodedParams, ports: &Ports, inputs: &[PortType]) -> Vec<PortType> {
        Aggregate.output_types(params, ports, inputs)
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        Aggregate.kernel(params)
    }

    fn lineage_params(&self, params: &DecodedParams) -> crate::ir::Params {
        Aggregate.lineage_params(params)
    }

    fn step_params(
        &self, params: &DecodedParams, filter: Option<Arc<dyn saluki_metric_kernels::aggregate::EmissionFilter>>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        Aggregate.step_params(params, filter)
    }

    fn flush_interval(&self, params: &DecodedParams) -> Option<Duration> {
        Aggregate.flush_interval(params)
    }
}

static UNPARTITIONED_AGGREGATE: UnpartitionedAggregate = UnpartitionedAggregate;

/// The native registry, with [`UnpartitionedAggregate`].
fn unpartitioned_registry() -> OperatorRegistry {
    static OPERATORS: std::sync::OnceLock<Vec<&'static dyn OperatorDef>> = std::sync::OnceLock::new();
    let operators = OPERATORS.get_or_init(|| {
        let mut operators = OperatorRegistry::native().iter().collect::<Vec<_>>();
        operators.push(&UNPARTITIONED_AGGREGATE);
        operators
    });
    OperatorRegistry::with_operators(operators)
}

/// The default program with `dsd_agg`'s operator replaced by [`UnpartitionedAggregate`], written by native
/// configuration or, with `artifact`, by an artifact.
fn unpartitioned_default_program(artifact: bool) -> Plan {
    let mut value = serde_json::from_str::<Value>(DEFAULT_PROGRAM_JSON).unwrap();
    let hash = format!("sha256:{}", "a".repeat(64));
    if artifact {
        value["provenance"] = json!({"artifacts": [{"revision": 1, "hash": hash}]});
    }
    for node in value["nodes"].as_array_mut().unwrap() {
        if node["id"] == "dsd_agg" {
            node["op"] = json!(UNPARTITIONED);
            if artifact {
                node["provenance"] = json!({"artifact": {"revision": 1, "hash": hash}});
            }
        }
    }
    Plan::from_value(value).unwrap()
}

#[test]
fn a_state_owner_without_a_partition_key_runs_on_one_lane_and_is_placed_on_state_lanes_by_no_candidate() {
    let registry = unpartitioned_registry();

    // At boot, an isolation slot whose state owner cannot partition its state runs on one lane, whatever the policy:
    // its exit is no repartition edge.
    let boot = unpartitioned_default_program(false);
    let frame = laned_with(&boot, &registry, lanes(2, 3, 2)).unwrap();
    let agg = slot_named(&frame, "dsd_agg");
    assert_eq!(agg.lanes, 1);
    let pre = slot_named(&frame, "dsd_pre_aggregation");
    assert_eq!(exit_to(&frame, pre, "dsd_agg").routing, ExitRouting::One);
    assert_eq!(agg.output_point(0), Some(point(6)));

    // A frame whose state lanes hold a partitioned state owner's state cannot run one that keeps its state whole: the
    // restart that native configuration asks for lays the slot out on one lane; an artifact cannot have it at all.
    let frame = laned_with(&default_program(), &registry, lanes(2, 3, 2)).unwrap();
    assert_eq!(slot_named(&frame, "dsd_agg").lanes, 3);
    for (artifact, capability) in [
        (false, MissingCapability::Restart),
        (true, MissingCapability::FrameChanges),
    ] {
        let candidate = unpartitioned_default_program(artifact);
        let error = place_unvalidated(&candidate, &registry, &frame).unwrap_err();
        let PlacementError::Unplaceable { problems } = &error else {
            panic!("expected an unplaceable candidate, not {:?}", error);
        };
        let [problem] = problems.as_slice() else {
            panic!("one problem, at the state owner: {}", error);
        };
        assert_eq!(problem.node, Some("dsd_agg".parse().unwrap()));
        assert_eq!(problem.capability, Some(capability));
        assert!(
            problem.message.contains("cannot be partitioned by context key")
                && problem.message.contains("`dsd_agg` on 3 lanes")
                && problem.message.contains("running the isolation slot on one lane"),
            "{}",
            problem.message
        );
    }

    // On one state lane, the same candidate places.
    let frame = laned_with(&default_program(), &registry, lanes(2, 1, 2)).unwrap();
    assert!(place_unvalidated(&unpartitioned_default_program(true), &registry, &frame).is_ok());
}

#[test]
fn a_state_owner_fed_by_another_state_owner_and_a_point_on_lanes_has_no_frame_and_several_sources_are_named_first() {
    // A spread exit on the way to a state owner would give one point several paths to a state lane: here the first
    // aggregation's time-driven output would spread over the producer lanes of the slot that feeds the second.
    let plan = plan_of(
        json!([
            dogstatsd("src"),
            otlp("other"),
            node(
                "other_ns",
                "namespace_prefix",
                json!({"namespace": "o", "exclusions": []})
            ),
            aggregate("first"),
            node("joined", "merge", json!({"inputs": 2})),
            node(
                "joined_ns",
                "namespace_prefix",
                json!({"namespace": "j", "exclusions": []})
            ),
            aggregate("second"),
            emit()
        ]),
        &[
            ("other.metrics", "other_ns.in"),
            ("other_ns.out", "first.in"),
            ("src.metrics", "joined.0"),
            ("first.out", "joined.1"),
            ("joined.out", "joined_ns.in"),
            ("joined_ns.out", "second.in"),
            ("second.out", "emit.in"),
        ],
    );
    assert!(laned(&plan, LanePolicy::ONE).is_ok());
    assert!(
        laned(&plan, lanes(1, 2, 2)).is_ok(),
        "one producer lane spreads nothing"
    );
    let error = laned(&plan, lanes(2, 1, 1)).unwrap_err();
    assert!(
        matches!(&error, FrameError::Lanes { message } if message.contains("would spread its buffers")),
        "{}",
        error
    );

    // A point of another source merged before aggregation is a second point upstream of it, which is what the frame
    // says, on lanes as on one.
    let merged = plan_of(
        json!([
            dogstatsd("src"),
            otlp("other"),
            node("joined", "merge", json!({"inputs": 2})),
            node(
                "joined_ns",
                "namespace_prefix",
                json!({"namespace": "j", "exclusions": []})
            ),
            aggregate("dsd_agg"),
            emit()
        ]),
        &[
            ("src.metrics", "joined.0"),
            ("other.metrics", "joined.1"),
            ("joined.out", "joined_ns.in"),
            ("joined_ns.out", "dsd_agg.in"),
            ("dsd_agg.out", "emit.in"),
        ],
    );
    for policy in [LanePolicy::ONE, lanes(2, 2, 2)] {
        let error = laned(&merged, policy).unwrap_err();
        assert!(matches!(error, FrameError::StateOwnerPoints { .. }), "{}", error);
    }
}

#[test]
fn the_slot_dogstatsd_feeds_runs_the_producer_lanes_even_on_one_and_never_the_post_aggregation_lanes() {
    // Each producer lane stamps its own point, so the slot DogStatsD feeds runs one lane per point, with no state
    // downstream as with it, and one producer lane is never spread over the post-aggregation lanes.
    let plan = plan_of(
        json!([
            dogstatsd("src"),
            node("ns", "namespace_prefix", json!({"namespace": "a", "exclusions": []})),
            emit()
        ]),
        &[("src.metrics", "ns.in"), ("ns.out", "emit.in")],
    );
    for (dogstatsd, routing) in [(1, PointRouting::One), (3, PointRouting::Fixed)] {
        let frame = laned(&plan, lanes(dogstatsd, 2, 4)).unwrap();
        assert_eq!(slot_named(&frame, "ns_region").lanes, dogstatsd);
        assert_eq!(frame.admission_points()[0].routing, routing);
    }
    // Another source's slot with no state downstream runs the post-aggregation lanes, spread.
    let other = plan_of(
        json!([
            otlp("src"),
            node("ns", "namespace_prefix", json!({"namespace": "a", "exclusions": []})),
            emit()
        ]),
        &[("src.metrics", "ns.in"), ("ns.out", "emit.in")],
    );
    let frame = laned(&other, lanes(1, 2, 4)).unwrap();
    assert_eq!(slot_named(&frame, "ns_region").lanes, 4);
    assert_eq!(frame.admission_points()[0].routing, PointRouting::Spread);
}

#[test]
fn a_fence_on_the_way_to_aggregation_leaves_the_producer_lanes_before_it_and_one_lane_after_it() {
    // An opaque host readmits: the slot after it is fed by its one point, so it runs one lane, and its exit into
    // aggregation is the repartition edge; the producer lanes before it end at the host.
    let plan = plan_of(
        json!([
            dogstatsd("src"),
            node(
                "before",
                "namespace_prefix",
                json!({"namespace": "b", "exclusions": []})
            ),
            node(
                "host",
                "opaque_host",
                json!({"component": "host", "input": {"kinds": "all", "timestamps": "mixed"},
                       "outputs": {"out": {"kinds": "all", "timestamps": "mixed"}}})
            ),
            node("after", "namespace_prefix", json!({"namespace": "a", "exclusions": []})),
            aggregate("dsd_agg"),
            emit()
        ]),
        &[
            ("src.metrics", "before.in"),
            ("before.out", "host.in"),
            ("host.out", "after.in"),
            ("after.out", "dsd_agg.in"),
            ("dsd_agg.out", "emit.in"),
        ],
    );
    let frame = laned(&plan, lanes(4, 3, 2)).unwrap();
    let (before, after, agg) = (
        slot_named(&frame, "before_region"),
        slot_named(&frame, "after_region"),
        slot_named(&frame, "dsd_agg"),
    );
    assert_eq!((before.lanes, after.lanes, agg.lanes), (4, 1, 3));
    assert_eq!(before.exits[0].target, FrameTarget::Fixed("host".parse().unwrap()));
    assert_eq!(before.exits[0].routing, ExitRouting::One);
    assert!(matches!(exit_to(&frame, after, "dsd_agg").routing, ExitRouting::Keyed(ref groups) if groups.lanes() == 3));
    let host_point = frame
        .admission_point_at(&"host".parse().unwrap(), &"out".parse().unwrap())
        .unwrap();
    assert_eq!(
        (host_point.id, host_point.lanes, host_point.routing),
        (point(4), 1, PointRouting::One)
    );
    assert_eq!(frame.affected_points(&[agg.id]), vec![point(4)]);
    // The emit after aggregation runs on the post-aggregation lanes, fed by a spread exit.
    let tail = slot_named(&frame, "emit_region");
    assert_eq!(tail.lanes, 2);
    assert_eq!(exit_to(&frame, agg, "emit_region").routing, ExitRouting::Spread);
}

// Random plans on random lanes.

/// What follows a port below the DogStatsD source, before aggregation. Every path leads to at most one state owner,
/// and every state owner is fed by one admission point over one path, as a frame requires.
#[derive(Clone, Debug)]
enum Pre {
    /// An `aggregate`, whose output joins the post-aggregation merge.
    Aggregate,
    /// An `emit` to an output of its own.
    Emit,
    /// A `drop`.
    Drop,
    /// An edge to the post-aggregation merge, as the no-aggregation passthrough has.
    Passthrough,
    /// A stateless kernel, then the rest.
    Kernel(Box<Pre>),
    /// A `split`, with a shape on each port.
    Split(Box<Pre>, Box<Pre>),
    /// A `tee`, with a shape on each port.
    Tee(Vec<Pre>),
    /// A `split` whose ports join again in a `merge`, then the rest: two logical paths in one slot.
    Rejoin(Box<Pre>),
    /// An opaque host, which readmits, then the rest.
    Fence(Box<Pre>),
}

/// What follows the post-aggregation merge: stateless.
#[derive(Clone, Debug)]
enum Post {
    Emit,
    Drop,
    Kernel(Box<Post>),
    Split(Box<Post>, Box<Post>),
    Tee(Vec<Post>),
    Fence(Box<Post>),
    /// A `split` whose `true` port goes through an opaque host into a `merge` its `false` port joins directly, then
    /// the rest: the merge is one fence deeper than the split, so they run in different slots.
    Detour(Box<Post>),
}

fn arb_pre() -> impl Strategy<Value = Pre> {
    let leaf = prop_oneof![
        3 => Just(Pre::Aggregate),
        2 => Just(Pre::Emit),
        1 => Just(Pre::Drop),
        2 => Just(Pre::Passthrough),
    ];
    leaf.prop_recursive(5, 24, 3, |inner| {
        prop_oneof![
            inner.clone().prop_map(|rest| Pre::Kernel(Box::new(rest))),
            (inner.clone(), inner.clone()).prop_map(|(yes, no)| Pre::Split(Box::new(yes), Box::new(no))),
            vec(inner.clone(), 1..4).prop_map(Pre::Tee),
            inner.clone().prop_map(|rest| Pre::Rejoin(Box::new(rest))),
            inner.prop_map(|rest| Pre::Fence(Box::new(rest))),
        ]
    })
}

/// What follows the DogStatsD source: now and then an aggregation its points feed straight, which has no frame on
/// several producer lanes, and otherwise any shape (where that is rare).
fn arb_root_pre() -> impl Strategy<Value = Pre> {
    prop_oneof![1 => Just(Pre::Aggregate), 7 => arb_pre()]
}

fn arb_post() -> impl Strategy<Value = Post> {
    let leaf = prop_oneof![3 => Just(Post::Emit), 1 => Just(Post::Drop)];
    leaf.prop_recursive(4, 16, 3, |inner| {
        prop_oneof![
            inner.clone().prop_map(|rest| Post::Kernel(Box::new(rest))),
            (inner.clone(), inner.clone()).prop_map(|(yes, no)| Post::Split(Box::new(yes), Box::new(no))),
            vec(inner.clone(), 1..4).prop_map(Post::Tee),
            inner.clone().prop_map(|rest| Post::Fence(Box::new(rest))),
            inner.prop_map(|rest| Post::Detour(Box::new(rest))),
        ]
    })
}

fn arb_policy() -> impl Strategy<Value = LanePolicy> {
    let count = || prop_oneof![4 => 1u16..=4, 1 => Just(8u16)];
    // One producer lane with lanes elsewhere is a layout of its own (a single decoder), so it comes up more often.
    let producers = prop_oneof![1 => Just(1u16), 2 => count()];
    (producers, count(), count()).prop_map(|(dogstatsd, state, post)| LanePolicy { dogstatsd, state, post })
}

/// Lowers random shapes into a plan, numbering nodes and outputs as it goes.
#[derive(Default)]
struct RandomPlan {
    nodes: Vec<Value>,
    edges: Vec<Value>,
    outputs: Vec<String>,
    /// The ports that feed the post-aggregation merge.
    post_inputs: Vec<String>,
}

impl RandomPlan {
    fn next_id(&self) -> String {
        format!("n{}", self.nodes.len())
    }

    fn connect(&mut self, from: &str, to: String) {
        self.edges.push(json!({"from": from, "to": to}));
    }

    fn add(&mut self, op: &str, params: Value, from: &str) -> String {
        let id = self.next_id();
        self.nodes.push(node(&id, op, params));
        self.connect(from, format!("{}.in", id));
        id
    }

    fn emit(&mut self, from: &str) {
        let output = format!("o{}", self.outputs.len());
        self.add("emit", json!({"output": output}), from);
        self.outputs.push(output);
    }

    fn kernel(&mut self, from: &str) -> String {
        // Alternate between two kernels, so that slots hold several operators.
        if self.nodes.len().is_multiple_of(2) {
            self.add("namespace_prefix", json!({"namespace": "ns", "exclusions": []}), from)
        } else {
            self.add("host_enrichment", json!({}), from)
        }
    }

    fn split(&mut self, from: &str) -> String {
        self.add("split", json!({"predicate": {"name": {"prefix_in": ["a."]}}}), from)
    }

    fn fence(&mut self, from: &str) -> String {
        let any = json!({"kinds": "all", "timestamps": "mixed"});
        let id = self.next_id();
        self.add(
            "opaque_host",
            json!({"component": id, "input": any, "outputs": {"out": any}}),
            from,
        )
    }

    fn pre(&mut self, shape: &Pre, from: &str) {
        match shape {
            Pre::Aggregate => {
                let id = self.add("aggregate", aggregation_params(), from);
                self.post_inputs.push(format!("{}.out", id));
            }
            Pre::Emit => self.emit(from),
            Pre::Drop => {
                self.add("drop", json!({}), from);
            }
            Pre::Passthrough => self.post_inputs.push(from.to_string()),
            Pre::Kernel(rest) => {
                let id = self.kernel(from);
                self.pre(rest, &format!("{}.out", id));
            }
            Pre::Split(yes, no) => {
                let id = self.split(from);
                self.pre(yes, &format!("{}.true", id));
                self.pre(no, &format!("{}.false", id));
            }
            Pre::Tee(branches) => {
                let id = self.add("tee", json!({"ports": branches.len()}), from);
                for (port, branch) in branches.iter().enumerate() {
                    self.pre(branch, &format!("{}.{}", id, port));
                }
            }
            Pre::Rejoin(rest) => {
                let id = self.split(from);
                let merge = self.next_id();
                self.nodes.push(node(&merge, "merge", json!({"inputs": 2})));
                self.connect(&format!("{}.true", id), format!("{}.0", merge));
                self.connect(&format!("{}.false", id), format!("{}.1", merge));
                self.pre(rest, &format!("{}.out", merge));
            }
            Pre::Fence(rest) => {
                let id = self.fence(from);
                self.pre(rest, &format!("{}.out", id));
            }
        }
    }

    fn post(&mut self, shape: &Post, from: &str) {
        match shape {
            Post::Emit => self.emit(from),
            Post::Drop => {
                self.add("drop", json!({}), from);
            }
            Post::Kernel(rest) => {
                let id = self.kernel(from);
                self.post(rest, &format!("{}.out", id));
            }
            Post::Split(yes, no) => {
                let id = self.split(from);
                self.post(yes, &format!("{}.true", id));
                self.post(no, &format!("{}.false", id));
            }
            Post::Tee(branches) => {
                let id = self.add("tee", json!({"ports": branches.len()}), from);
                for (port, branch) in branches.iter().enumerate() {
                    self.post(branch, &format!("{}.{}", id, port));
                }
            }
            Post::Fence(rest) => {
                let id = self.fence(from);
                self.post(rest, &format!("{}.out", id));
            }
            Post::Detour(rest) => {
                let id = self.split(from);
                let host = self.fence(&format!("{}.true", id));
                let merge = self.next_id();
                self.nodes.push(node(&merge, "merge", json!({"inputs": 2})));
                self.connect(&format!("{}.out", host), format!("{}.0", merge));
                self.connect(&format!("{}.false", id), format!("{}.1", merge));
                self.post(rest, &format!("{}.out", merge));
            }
        }
    }

    /// Builds the plan: the DogStatsD source, `pre` below it, and, when anything feeds it, the post-aggregation merge
    /// with `post` below it, fed by every aggregation, every passthrough, and, with `other_source`, an OTLP source.
    fn build(pre: &Pre, post: &Post, other_source: bool) -> Plan {
        let mut plan = RandomPlan::default();
        plan.nodes.push(dogstatsd("dsd"));
        plan.pre(pre, "dsd.metrics");
        if other_source {
            plan.nodes.push(otlp("otlp"));
            plan.post_inputs.push("otlp.metrics".to_string());
        }
        if !plan.post_inputs.is_empty() {
            let merge = "post_in".to_string();
            plan.nodes
                .push(node(&merge, "merge", json!({"inputs": plan.post_inputs.len()})));
            for (port, from) in std::mem::take(&mut plan.post_inputs).iter().enumerate() {
                plan.connect(from, format!("{}.{}", merge, port));
            }
            plan.post(post, &format!("{}.out", merge));
        }
        let outputs = plan
            .outputs
            .iter()
            .map(|output| {
                json!({"id": output, "accepts": {"kinds": "all", "timestamps": "mixed"},
                       "fingerprint": PlanHash::of_bytes(output.as_bytes()).to_string(),
                       "recipients": [format!("out/{}", output)]})
            })
            .collect::<Vec<_>>();
        Plan::from_value(json!({"ir_version": 1, "nodes": plan.nodes, "edges": plan.edges, "outputs": outputs}))
            .unwrap()
    }
}

/// Labels each of `nodes` with the smallest node it is joined to by edges of `plan` between two of `nodes` that `joins`
/// accepts, so that the nodes with one label are one connected set.
fn components(
    plan: &Plan, nodes: impl Iterator<Item = NodeId>, joins: impl Fn(&NodeId, &NodeId) -> bool,
) -> BTreeMap<NodeId, NodeId> {
    let mut labels = nodes.map(|id| (id.clone(), id)).collect::<BTreeMap<_, _>>();
    loop {
        let mut changed = false;
        for edge in &plan.edges {
            let (from, to) = (&edge.from.node, &edge.to.node);
            let (Some(a), Some(b)) = (labels.get(from).cloned(), labels.get(to).cloned()) else {
                continue;
            };
            if a != b && joins(from, to) {
                let smallest = a.clone().min(b.clone());
                for label in labels.values_mut() {
                    if *label == a || *label == b {
                        *label = smallest.clone();
                    }
                }
                changed = true;
            }
        }
        if !changed {
            return labels;
        }
    }
}

/// The fence depth of every synchronous node of `plan`, and its connected set of synchronous nodes, computed from the
/// definition by relaxation to a fixed point: the largest number of fences (fixed components and state owners behind
/// isolation fences) on any path to the node from a node of its own set.
fn fence_depths(plan: &Plan) -> BTreeMap<NodeId, (NodeId, u32)> {
    let op = |id: &NodeId| plan.node(id).map(|node| node.op.as_str()).unwrap_or_default();
    let fixed = |id: &NodeId| matches!(op(id), "source" | "opaque_host");
    let fence = |id: &NodeId| fixed(id) || op(id) == "aggregate";
    let input_of = |id: &NodeId| {
        plan.edges
            .iter()
            .find(|edge| &edge.to.node == id)
            .map(|edge| &edge.from.node)
    };
    // An `emit` or `drop` fed straight by a fixed component runs in no slot.
    let terminal = |id: &NodeId| matches!(op(id), "emit" | "drop") && input_of(id).is_some_and(&fixed);
    let synchronous = |id: &NodeId| !fence(id) && !terminal(id);
    let ids = plan.nodes.iter().map(|node| node.id.clone()).collect::<Vec<_>>();

    // Connected sets of synchronous nodes.
    let set = components(plan, ids.iter().cloned(), |from, to| {
        synchronous(from) && synchronous(to)
    });

    let mut depths = BTreeMap::new();
    for root in ids
        .iter()
        .filter(|id| synchronous(id))
        .map(|id| &set[id])
        .collect::<BTreeSet<_>>()
    {
        let mut reach: BTreeMap<&NodeId, u32> = ids
            .iter()
            .filter(|id| synchronous(id) && &set[*id] == root)
            .map(|id| (id, 0))
            .collect();
        loop {
            let mut changed = false;
            for edge in &plan.edges {
                let Some(&at) = reach.get(&edge.from.node) else {
                    continue;
                };
                let value = at + u32::from(fence(&edge.from.node));
                if reach.get(&edge.to.node).is_none_or(|&known| value > known) {
                    reach.insert(&edge.to.node, value);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for id in ids.iter().filter(|id| synchronous(id) && &set[*id] == root) {
            depths.insert(id.clone(), (root.clone(), reach[id]));
        }
    }
    depths
}

/// Returns, for a slot, whether a state owner is downstream of it through slots' exits.
fn state_downstream(frame: &Frame, slot: SlotId) -> bool {
    frame.slot(slot).unwrap().exits.iter().any(|exit| match exit.target {
        FrameTarget::Slot(next) => frame.slot(next).unwrap().is_isolation() || state_downstream(frame, next),
        _ => false,
    })
}

/// Counts the paths from lane `lane` of slot `from` to lane `to.1` of the isolation slot `to.0`, through the lanes of
/// fused slots, as the routing of each exit sends: one lane, the sender's lane modulo the target's, or every lane.
fn lane_paths(frame: &Frame, from: SlotId, lane: u16, to: (SlotId, u16)) -> u64 {
    if (from, lane) == to {
        return 1;
    }
    let slot = frame.slot(from).unwrap();
    if slot.is_isolation() {
        return 0;
    }
    let mut paths = 0;
    for exit in &slot.exits {
        let FrameTarget::Slot(next) = exit.target else {
            continue;
        };
        let lanes = frame.slot(next).unwrap().lanes;
        let reached = match exit.routing {
            ExitRouting::One => vec![0],
            ExitRouting::Affinity => vec![lane % lanes],
            ExitRouting::Keyed(_) | ExitRouting::Spread => (0..lanes).collect(),
        };
        paths += reached
            .into_iter()
            .map(|next_lane| lane_paths(frame, next, next_lane, to))
            .sum::<u64>();
    }
    paths
}

/// Counts the paths from lane `lane` of an admission point to lane `to.1` of the isolation slot `to.0`.
fn point_lane_paths(frame: &Frame, point: &AdmissionPointInfo, lane: u16, to: (SlotId, u16)) -> u64 {
    let Some(FrameTarget::Slot(first)) = point.route else {
        return 0;
    };
    let lanes = frame.slot(first).unwrap().lanes;
    let reached = match point.routing {
        PointRouting::One => vec![0],
        PointRouting::Fixed => vec![lane],
        PointRouting::Spread => (0..lanes).collect(),
    };
    reached
        .into_iter()
        .map(|first_lane| lane_paths(frame, first, first_lane, to))
        .sum()
}

/// The fused slots that the DogStatsD points (the frame's first admission point) reach through fused slots only: the
/// slots whose batches keep the producer lanes' order.
fn producer_ordered(frame: &Frame) -> BTreeSet<SlotId> {
    let mut reached = BTreeSet::new();
    let mut pending = match frame.admission_points()[0].route {
        Some(FrameTarget::Slot(slot)) => vec![slot],
        _ => Vec::new(),
    };
    while let Some(slot) = pending.pop() {
        let info = frame.slot(slot).unwrap();
        if info.is_isolation() || !reached.insert(slot) {
            continue;
        }
        pending.extend(info.exits.iter().filter_map(|exit| match exit.target {
            FrameTarget::Slot(next) => Some(next),
            _ => None,
        }));
    }
    reached
}

/// Checks a frame on lanes against the plan it was laid out from and against the same plan on one lane, as
/// `property_test_frames_fuse_maximally_between_fences` states, and returns it.
///
/// A plan whose DogStatsD points feed a state owner straight has no frame on several producer lanes; it is then
/// checked on one producer lane and the policy's other lanes, and the result says it was refused.
fn check_frame_on_lanes(plan: &Plan, asked: LanePolicy) -> Result<(Frame, bool), TestCaseError> {
    let registry = OperatorRegistry::native();
    let op = |id: &NodeId| plan.node(id).map(|node| node.op.as_str()).unwrap_or_default();

    // Each producer lane stamps its point in the lane of the slot it feeds, which no isolation slot can give it.
    let straight = plan
        .edges
        .iter()
        .any(|edge| edge.from.node.to_string() == "dsd" && op(&edge.to.node) == "aggregate");
    let refused = straight && asked.dogstatsd > 1;
    let policy = if refused {
        let error = laned(plan, asked).err();
        prop_assert!(
            matches!(&error, Some(FrameError::Lanes { message }) if message.contains("feed state owner")),
            "the DogStatsD points feed a state owner straight on {:?}: {:?}",
            asked,
            error
        );
        LanePolicy { dogstatsd: 1, ..asked }
    } else {
        asked
    };
    let frame =
        laned(plan, policy).map_err(|error| TestCaseError::fail(format!("no frame on {:?}: {}", policy, error)))?;
    let one = laned(plan, LanePolicy::ONE).map_err(|error| TestCaseError::fail(format!("no frame: {}", error)))?;

    // Lanes change neither the slots nor the fusion decisions, and nothing is pipelined.
    let nodes_of = |frame: &Frame| frame.slots().iter().map(|slot| slot.nodes.clone()).collect::<Vec<_>>();
    prop_assert_eq!(nodes_of(&frame), nodes_of(&one));
    prop_assert_eq!(frame.fusion(), one.fusion());
    let pipelined = frame
        .fusion()
        .iter()
        .any(|decision| matches!(decision.verdict, FusionVerdict::Pipelined { .. }));
    prop_assert!(!pipelined, "R1's cost model cut a slot: {:?}", frame.fusion());

    // Slots follow fences only: two synchronous nodes share a slot exactly when they are joined by edges between
    // synchronous nodes of one connected set and one fence depth, and every state owner runs alone.
    let depths = fence_depths(plan);
    let groups = components(plan, depths.keys().cloned(), |from, to| depths[from] == depths[to]);
    let mut expected: BTreeSet<BTreeSet<NodeId>> = BTreeSet::new();
    for group in groups.values().collect::<BTreeSet<_>>() {
        expected.insert(
            groups
                .iter()
                .filter(|(_, root)| *root == group)
                .map(|(id, _)| id.clone())
                .collect(),
        );
    }
    for node in plan.nodes.iter().filter(|node| node.op.as_str() == "aggregate") {
        expected.insert(BTreeSet::from([node.id.clone()]));
    }
    let actual = frame
        .slots()
        .iter()
        .map(|slot| slot.nodes.iter().cloned().collect::<BTreeSet<_>>())
        .collect::<BTreeSet<_>>();
    prop_assert_eq!(&actual, &expected);

    // Decisions stay whole: `split`s and `tee`s joined by an edge share a slot.
    let decides = |id: &NodeId| matches!(op(id), "split" | "tee");
    for edge in &plan.edges {
        if decides(&edge.from.node) && decides(&edge.to.node) {
            prop_assert_eq!(frame.site(&edge.from.node), frame.site(&edge.to.node), "{}", edge.from);
        }
    }

    // Lanes per slot, by the policy's rules.
    let dogstatsd = &frame.admission_points()[0];
    prop_assert_eq!(dogstatsd.lanes, policy.dogstatsd);
    prop_assert_eq!(
        frame.admission_point_ids().count(),
        frame.admission_points().len() + usize::from(policy.dogstatsd) - 1
    );
    for slot in frame.slots() {
        let fed_by_dogstatsd = slot.inputs.contains(&FrameSource::Admission(dogstatsd.id));
        let feeding_points = slot
            .inputs
            .iter()
            .filter_map(|input| match input {
                FrameSource::Admission(id) => frame.admission_point(*id),
                FrameSource::Exit { .. } => None,
            })
            .collect::<Vec<_>>();
        let expected = if slot.is_isolation() {
            // A state owner fed straight by a point routes nothing by key, and only a point with one lane can feed it
            // straight.
            if feeding_points.is_empty() {
                policy.state
            } else {
                prop_assert!(
                    feeding_points.iter().all(|point| point.lanes == 1),
                    "slot `{}`",
                    slot.name
                );
                1
            }
        } else if fed_by_dogstatsd {
            policy.dogstatsd
        } else if state_downstream(&frame, slot.id) {
            1
        } else {
            policy.post
        };
        prop_assert_eq!(slot.lanes, expected, "slot `{}`", slot.name);
        if let SlotKind::Isolation { output_points, .. } = &slot.kind {
            prop_assert_eq!(output_points.len(), usize::from(slot.lanes));
        }
    }

    // Routing of every edge into a slot, from the lanes of its ends: a slot that carries the producer lanes' batches
    // keeps each sender lane's batches on one lane of a stateless target, at one lane as at several.
    let ordered = producer_ordered(&frame);
    for slot in frame.slots() {
        let keeps_order = ordered.contains(&slot.id);
        for exit in &slot.exits {
            let FrameTarget::Slot(next) = exit.target else {
                prop_assert_eq!(&exit.routing, &ExitRouting::One);
                continue;
            };
            let target = frame.slot(next).unwrap();
            match &exit.routing {
                ExitRouting::One => prop_assert_eq!(target.lanes, 1),
                ExitRouting::Keyed(groups) => {
                    prop_assert!(target.is_isolation() && target.lanes > 1);
                    prop_assert_eq!(groups.lanes(), target.lanes);
                }
                ExitRouting::Affinity => {
                    prop_assert!(!target.is_isolation() && target.lanes > 1);
                    prop_assert!(!slot.is_isolation() && (slot.lanes > 1 || keeps_order));
                }
                ExitRouting::Spread => {
                    prop_assert!(!target.is_isolation() && target.lanes > 1);
                    prop_assert!(
                        slot.is_isolation() || (slot.lanes == 1 && !keeps_order),
                        "`{}` spreads the producer lanes' batches",
                        slot.name
                    );
                    prop_assert!(
                        !state_downstream(&frame, next),
                        "a spread toward state from `{}`",
                        slot.name
                    );
                }
            }
        }
    }
    for point in frame.admission_points() {
        let target = match point.route {
            Some(FrameTarget::Slot(next)) => frame.slot(next).unwrap(),
            _ => {
                prop_assert_eq!(point.routing, PointRouting::One);
                continue;
            }
        };
        match point.routing {
            PointRouting::One => prop_assert_eq!(target.lanes, 1),
            PointRouting::Fixed => prop_assert!(point.lanes == target.lanes && target.lanes > 1),
            PointRouting::Spread => {
                prop_assert!(point.lanes == 1 && target.lanes > 1);
                prop_assert!(!target.is_isolation() && !state_downstream(&frame, target.id));
            }
        }
    }

    // One path per admission point lane and state lane: every lane of each state owner's one upstream point reaches
    // every state lane over exactly one path, and no other point's lanes reach it.
    for slot in frame.slots() {
        let SlotKind::Isolation { upstream, .. } = &slot.kind else {
            continue;
        };
        let mut feeding = Vec::new();
        for point in frame.admission_points() {
            for lane in 0..point.lanes {
                let id = point.lane_id(lane).unwrap();
                for state_lane in 0..slot.lanes {
                    let paths = point_lane_paths(&frame, point, lane, (slot.id, state_lane));
                    let expected = u64::from(upstream.contains(&id));
                    prop_assert_eq!(
                        paths,
                        expected,
                        "paths from point {} to lane {} of `{}`",
                        id.get(),
                        state_lane,
                        slot.name
                    );
                }
            }
            if point.lane_ids().any(|id| upstream.contains(&id)) {
                prop_assert_eq!(&point.lane_ids().collect::<Vec<_>>(), upstream);
                prop_assert!(point.feeds_state);
                feeding.push(point.id);
            }
        }
        prop_assert_eq!(feeding.len(), 1, "one point feeds `{}`", slot.name);
    }

    // Placement and programs on lanes: the boot plan compiles into its frame, and every slot a point's edge leads to
    // has an entry for each of the point's lanes, all running the first lane's steps.
    let compiled = compile_unvalidated(plan, &registry, &frame)
        .map_err(|error| TestCaseError::fail(format!("the boot plan does not compile on lanes: {}", error)))?;
    for point in frame.admission_points() {
        let Some(FrameTarget::Slot(target)) = point.route else {
            continue;
        };
        let program = &compiled.programs[target.get()];
        let first = program
            .entry(EntryKey::Admission(point.id))
            .ok_or_else(|| TestCaseError::fail(format!("no entry for point {}", point.id.get())))?;
        for id in point.lane_ids() {
            let entry = program.entry(EntryKey::Admission(id));
            prop_assert!(
                entry.is_some_and(|entry| entry.steps == first.steps),
                "point {}",
                id.get()
            );
        }
    }
    Ok((frame, refused))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Without R1's pipelining, slots follow fences only, decisions stay whole, and lanes keep one path per admission
    /// point lane and state lane, over random plans with state owners behind fences and passthrough branches, and
    /// random lane policies (5.10 and 6.4 of the stage 9 design).
    #[test]
    fn property_test_frames_fuse_maximally_between_fences(
        pre in arb_root_pre(),
        post in arb_post(),
        other_source in any::<bool>(),
        policy in arb_policy(),
    ) {
        let plan = RandomPlan::build(&pre, &post, other_source);
        check_frame_on_lanes(&plan, policy)?;
    }
}

#[test]
fn random_plans_exercise_every_routing_and_state_lanes_fed_by_exits_and_by_points() {
    // The property would hold vacuously over plans without the cases it is about: count them over a fixed sample.
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let mut seen = BTreeMap::<&str, usize>::new();
    for _ in 0..256 {
        let shapes = (arb_root_pre(), arb_post(), any::<bool>(), arb_policy());
        let (pre, post, other_source, policy) = shapes.new_tree(&mut runner).unwrap().current();
        let plan = RandomPlan::build(&pre, &post, other_source);
        let (frame, refused) = check_frame_on_lanes(&plan, policy).unwrap();
        if refused {
            *seen.entry("refused").or_default() += 1;
        }
        let ordered = producer_ordered(&frame);
        for slot in frame.slots() {
            if slot.lanes == 1 && ordered.contains(&slot.id) {
                *seen.entry("producer order from one lane").or_default() += slot
                    .exits
                    .iter()
                    .filter(|exit| exit.routing == ExitRouting::Affinity)
                    .count();
            }
            if slot.is_isolation() {
                let fed_by_point = slot
                    .inputs
                    .iter()
                    .any(|input| matches!(input, FrameSource::Admission(_)));
                *seen
                    .entry(if fed_by_point {
                        "state fed by a point"
                    } else {
                        "state fed by exits"
                    })
                    .or_default() += 1;
            }
            for exit in &slot.exits {
                *seen.entry(exit.routing.name()).or_default() += 1;
            }
        }
        for point in frame.admission_points() {
            *seen.entry(point.routing.name()).or_default() += 1;
        }
        if frame
            .fusion()
            .iter()
            .any(|decision| matches!(decision.verdict, FusionVerdict::Separated { .. }))
        {
            *seen.entry("separated").or_default() += 1;
        }
    }
    for case in [
        "keyed",
        "affinity",
        "spread",
        "fixed",
        "separated",
        "state fed by a point",
        "state fed by exits",
        "refused",
        "producer order from one lane",
    ] {
        assert!(
            seen.get(case).copied().unwrap_or_default() >= 8,
            "{} in {:?}",
            case,
            seen
        );
    }
}
