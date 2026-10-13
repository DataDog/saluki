//! Frame, placement, and program fixtures, with expected results written by hand from the implementation design.

use saluki_core::execution::{AdmissionMode, AdmissionPointId, BatchHeader, EntryId};
use serde_json::{json, Value};

use super::{frame::REGION_COST_FLOOR, *};
use crate::{
    ir::{NativeComponentId, NodeId, OutputId, Plan, PortRef, Provenance},
    model::PortType,
    operators::Boundary,
    testing::{default_program_catalog, with_opaque_gateways, DEFAULT_PROGRAM_JSON, NO_AGGREGATION_PIPELINE_OFF_JSON},
    validate::{CatalogOutput, MemoryCatalog, Validator},
};

/// Names slots as the host does for the default program: the slot holding the DogStatsD mapper is the pre-aggregation
/// chain, and the slot holding host enrichment keeps the native `metrics_enrich` ID.
fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    crate::testing::default_program_slot_name(nodes)
}

/// Names every slot after its first node.
fn first_node_namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    Ok(NativeComponentId::new(format!("slot_{}", nodes[0].local())).unwrap())
}

fn id(id: &str) -> NodeId {
    id.parse().unwrap()
}

fn ids(list: &[&str]) -> Vec<NodeId> {
    list.iter().map(|node| id(node)).collect()
}

fn output(id: &str) -> OutputId {
    OutputId::new(id).unwrap()
}

fn port(port: &str) -> PortRef {
    port.parse().unwrap()
}

fn fixed(node: &str) -> FrameTarget {
    FrameTarget::Fixed(id(node))
}

fn to_slot(slot: u16) -> FrameTarget {
    FrameTarget::Slot(SlotId::new(slot))
}

fn to_output(id: &str) -> FrameTarget {
    FrameTarget::Destination(output(id))
}

fn point(id: u32) -> AdmissionPointId {
    AdmissionPointId::new(id)
}

fn exit_of(slot: u16, exit: u16) -> FrameSource {
    FrameSource::Exit {
        slot: SlotId::new(slot),
        exit: ExitIndex::new(exit),
    }
}

/// The default program as it is through stage 3: endpoint routing filters and failover gateways are opaque hosts.
fn stage3_default_program() -> Plan {
    Plan::from_value(with_opaque_gateways(DEFAULT_PROGRAM_JSON)).unwrap()
}

fn stage3_without_no_aggregation_pipeline() -> Plan {
    Plan::from_value(with_opaque_gateways(NO_AGGREGATION_PIPELINE_OFF_JSON)).unwrap()
}

fn frame_of(plan: &Plan) -> Frame {
    Frame::compute(plan, &OperatorRegistry::native(), &namer).unwrap()
}

fn validated(plan: &Plan, catalog: &MemoryCatalog) -> ValidationReport {
    let report = Validator::new(&OperatorRegistry::native(), catalog).validate(plan);
    assert!(report.is_valid(), "{}", report);
    report
}

fn placed(plan: &Plan, frame: &Frame) -> Result<Placement, PlacementError> {
    let report = validated(plan, &default_program_catalog());
    place(plan, &report, &OperatorRegistry::native(), frame)
}

fn exits(slot: &Slot) -> Vec<(String, FrameTarget)> {
    slot.exits
        .iter()
        .map(|exit| (exit.name.to_string(), exit.target.clone()))
        .collect()
}

fn named(list: &[(&str, FrameTarget)]) -> Vec<(String, FrameTarget)> {
    list.iter()
        .map(|(name, target)| (name.to_string(), target.clone()))
        .collect()
}

/// The outputs of the default-program fixtures, in declaration order.
const OUTPUTS: [&str; 6] = [
    "dsd_client_telemetry",
    "dd_metrics_encode",
    "metrics_routing_encode_0",
    "metrics_routing_encode_1",
    "mrf_metrics_encode",
    "af_metrics_encode",
];

/// The slots of the pre-aggregation chain, as R1's cost model cuts it, with their nodes. Estimated per event, the
/// mapper costs 65 ns, the namespace prefix with its listener filterlist natively 110 ns (prefixing builds a new name
/// for every metric, 90 ns, and the filterlist normalizes and matches the intake name, 15 ns, and routes, 5 ns), the
/// timestamp split 10 ns, and the tag filterlist 15 ns: the budget is the costliest native component's, 110 ns. In a
/// region the filterlist is a memoizable decision, priced as a memo lookup (10 ns) and routing (5 ns), so the prefix
/// and its filterlist cost 105 ns, and the timestamp split (10 ns) does not fit beside them: the mapper runs alone, the
/// namespace prefix with its filterlist alone, and the split with the tag filterlist.
const PRE_AGGREGATION: [(&str, &[&str]); 3] = [
    ("dsd_pre_aggregation", &["dsd_enrich"]),
    (
        "dsd_prefix_filter_region",
        &[
            "dsd_prefix_filter",
            "dsd_prefix_filter_blocklist",
            "dsd_prefix_filter_blocklist_true",
        ],
    ),
    ("dsd_no_agg_split_region", &["dsd_no_agg_split", "dsd_tag_filterlist"]),
];

/// The slots after aggregation, as the cost model cuts them: the post-aggregation filter and the client telemetry tap
/// are one decision, whose predicate reads the kind and the intake name (a memo lookup, 10 ns, and routing, 5 ns) and
/// whose tap makes one copy (15 ns), 30 ns, which fits within the budget, but not with enrichment, whose host tags
/// (30 ns) and five-port fan-out (four copies of 15 ns) cost 90 ns.
const POST_AGGREGATION: [(&str, &[&str]); 2] = [
    (
        "dsd_post_agg_filter_region",
        &[
            "dsd_post_agg_filter_in",
            "dsd_post_agg_filter",
            "dsd_post_agg_filter_true",
            "dsd_post_agg_filter_fanout",
            "dsd_post_agg_filter_fanout_0",
        ],
    ),
    (
        "metrics_enrich",
        &[
            "metrics_enrich_in",
            "metrics_enrich",
            "metrics_enrich_host_tags",
            "metrics_policy",
            "metrics_policy_0",
        ],
    ),
];

/// Aggregation's isolation slot: a declared isolation fence runs alone, in a slot named after its node, which costs
/// nothing in R1's model (it never fuses).
const AGGREGATION: (&str, &[&str]) = ("dsd_agg", &["dsd_agg"]);

/// The slots of the stage 3 default program, with their nodes, in slot order.
fn designed_slots() -> Vec<(String, Vec<NodeId>)> {
    PRE_AGGREGATION
        .iter()
        .chain([&AGGREGATION])
        .chain(&POST_AGGREGATION)
        .map(|(name, nodes)| (name.to_string(), ids(nodes)))
        .collect()
}

fn slots_of(frame: &Frame) -> Vec<(String, Vec<NodeId>)> {
    frame
        .slots()
        .iter()
        .map(|slot| (slot.name.to_string(), slot.nodes.clone()))
        .collect()
}

const FIXED: [&str; 9] = [
    "dsd_in",
    "otlp_in",
    "checks_ipc_in",
    "liveness_in",
    "trace_sampler",
    "metrics_routing_filter_0",
    "metrics_routing_filter_1",
    "mrf_metrics_gateway",
    "af_metrics_gateway",
];

/// Returns `targets` followed by an exit to each output of the default programs that is not one of them yet.
fn then_every_output(targets: &[(&str, FrameTarget)]) -> Vec<(String, FrameTarget)> {
    let mut exits = named(targets);
    for id in OUTPUTS {
        if !exits.iter().any(|(_, target)| *target == to_output(id)) {
            exits.push((id.to_string(), to_output(id)));
        }
    }
    exits
}

/// The exits of the post-aggregation filter's slot of either default program: the client telemetry tap and
/// enrichment, the edges leaving it in plan order, then every other output, all downstream of it.
fn post_aggregation_exits(enrich: u16) -> Vec<(String, FrameTarget)> {
    then_every_output(&[
        ("dsd_client_telemetry", to_output("dsd_client_telemetry")),
        ("metrics_enrich", to_slot(enrich)),
    ])
}

/// The exits of the `metrics_enrich` slot of either default program: the edges leaving it in plan order (the primary
/// output and the four gateways), then the outputs downstream of the gateways. The client telemetry tap is upstream.
fn enrich_exits() -> Vec<(String, FrameTarget)> {
    named(&[
        ("dd_metrics_encode", to_output("dd_metrics_encode")),
        ("metrics_routing_filter_0", fixed("metrics_routing_filter_0")),
        ("metrics_routing_filter_1", fixed("metrics_routing_filter_1")),
        ("mrf_metrics_gateway", fixed("mrf_metrics_gateway")),
        ("af_metrics_gateway", fixed("af_metrics_gateway")),
        ("metrics_routing_encode_0", to_output("metrics_routing_encode_0")),
        ("metrics_routing_encode_1", to_output("metrics_routing_encode_1")),
        ("mrf_metrics_encode", to_output("mrf_metrics_encode")),
        ("af_metrics_encode", to_output("af_metrics_encode")),
    ])
}

/// The admission points of either default program: every output port of every fence, in plan order, with where its
/// physical edge leads. Both programs have six slots: three before aggregation, aggregation's isolation slot, and the
/// same two after it. Aggregation runs in a region, so it has no admission point: its time-driven output is stamped
/// at its slot's output point, an ID after every fixed component's, in headers only.
fn expected_admission_points() -> Vec<(u32, &'static str, &'static str, AdmissionMode, Option<FrameTarget>)> {
    use AdmissionMode::{Admit, Readmit};
    vec![
        (0, "dsd_in", "metrics", Admit, Some(to_slot(0))),
        (1, "otlp_in", "metrics", Admit, Some(to_slot(5))),
        (2, "checks_ipc_in", "metrics", Admit, Some(to_slot(5))),
        (3, "liveness_in", "metrics", Admit, Some(to_slot(5))),
        (4, "trace_sampler", "metrics", Admit, Some(to_slot(5))),
        // The gateways' outputs connect straight to their encoders: their `emit`s run in no region.
        (
            5,
            "metrics_routing_filter_0",
            "out",
            Readmit,
            Some(to_output("metrics_routing_encode_0")),
        ),
        (
            6,
            "metrics_routing_filter_1",
            "out",
            Readmit,
            Some(to_output("metrics_routing_encode_1")),
        ),
        (
            7,
            "mrf_metrics_gateway",
            "out",
            Readmit,
            Some(to_output("mrf_metrics_encode")),
        ),
        (
            8,
            "af_metrics_gateway",
            "out",
            Readmit,
            Some(to_output("af_metrics_encode")),
        ),
    ]
}

/// The exit of aggregation's isolation slot in either default program: only its boot edge's target, the
/// post-aggregation filter's slot. An isolation slot has no idle exits to outputs.
fn aggregation_exits() -> Vec<(String, FrameTarget)> {
    named(&[("dsd_post_agg_filter_region", to_slot(4))])
}

fn admission_points(frame: &Frame) -> Vec<(u32, String, String, AdmissionMode, Option<FrameTarget>)> {
    frame
        .admission_points()
        .iter()
        .map(|point| {
            (
                point.id.get(),
                point.node.to_string(),
                point.port.to_string(),
                point.mode,
                point.route.clone(),
            )
        })
        .collect()
}

fn owned(
    list: Vec<(u32, &'static str, &'static str, AdmissionMode, Option<FrameTarget>)>,
) -> Vec<(u32, String, String, AdmissionMode, Option<FrameTarget>)> {
    list.into_iter()
        .map(|(id, node, port, mode, route)| (id, node.to_string(), port.to_string(), mode, route))
        .collect()
}

#[test]
fn the_default_programs_frame_has_the_designed_slots_and_fixed_components() {
    let frame = frame_of(&stage3_default_program());

    assert_eq!(slots_of(&frame), designed_slots());
    assert_eq!(frame.cost_budget(), 110);
    // The listener filterlist (one intake-name leaf, 20 ns) and the post-aggregation filter (the kind and one
    // intake-name leaf, 21 ns) cost less than `MEMO_COST_FLOOR`, so R1 prices them evaluated, as regions run them: the
    // prefix filter's slot costs 90 + 20 ns, and the post-aggregation filter's 21 ns beside its fan-out's copy (15 ns).
    // Aggregation's isolation slot is outside R1's model.
    let costs = frame.slots().iter().map(|slot| slot.cost).collect::<Vec<_>>();
    assert_eq!(costs, vec![65, 110, 25, 0, 36, 90]);
    let fixed_nodes = frame.fixed().iter().map(|fixed| fixed.node.clone()).collect::<Vec<_>>();
    assert_eq!(fixed_nodes, ids(&FIXED));
    let outputs = frame.outputs().iter().map(|decl| decl.id.clone()).collect::<Vec<_>>();
    assert_eq!(outputs, OUTPUTS.map(output).to_vec());

    // Sources are I/O hosts and the gateways opaque hosts; aggregation runs in its isolation slot, whose watermark is
    // the DogStatsD source's admission point's, the only point upstream of it, which therefore feeds state.
    let boundaries = frame
        .fixed()
        .iter()
        .map(|fixed| (fixed.node.to_string(), fixed.boundary, fixed.state_owner))
        .collect::<Vec<_>>();
    assert_eq!(
        frame.slots()[3].kind,
        SlotKind::Isolation {
            output_points: vec![point(9)],
            upstream: vec![point(0)],
        }
    );
    let feeds_state = frame
        .admission_points()
        .iter()
        .filter(|point| point.feeds_state)
        .map(|point| point.id)
        .collect::<Vec<_>>();
    assert_eq!(feeds_state, vec![point(0)]);
    assert_eq!(
        boundaries,
        vec![
            ("dsd_in".to_string(), Boundary::IoHost, false),
            ("otlp_in".to_string(), Boundary::IoHost, false),
            ("checks_ipc_in".to_string(), Boundary::IoHost, false),
            ("liveness_in".to_string(), Boundary::IoHost, false),
            ("trace_sampler".to_string(), Boundary::IoHost, false),
            ("metrics_routing_filter_0".to_string(), Boundary::OpaqueHost, true),
            ("metrics_routing_filter_1".to_string(), Boundary::OpaqueHost, true),
            ("mrf_metrics_gateway".to_string(), Boundary::OpaqueHost, true),
            ("af_metrics_gateway".to_string(), Boundary::OpaqueHost, true),
        ]
    );
    assert_eq!(admission_points(&frame), owned(expected_admission_points()));

    // What feeds each fixed component: the gateways are fed by exits of enrichment's slot.
    let inputs = frame
        .fixed()
        .iter()
        .map(|fixed| (fixed.node.to_string(), fixed.input))
        .collect::<Vec<_>>();
    assert_eq!(
        inputs,
        vec![
            ("dsd_in".to_string(), None),
            ("otlp_in".to_string(), None),
            ("checks_ipc_in".to_string(), None),
            ("liveness_in".to_string(), None),
            ("trace_sampler".to_string(), None),
            ("metrics_routing_filter_0".to_string(), Some(exit_of(5, 1))),
            ("metrics_routing_filter_1".to_string(), Some(exit_of(5, 2))),
            ("mrf_metrics_gateway".to_string(), Some(exit_of(5, 3))),
            ("af_metrics_gateway".to_string(), Some(exit_of(5, 4))),
        ]
    );
    for (index, fixed) in frame.fixed().iter().enumerate() {
        assert_eq!(fixed.admission_points, vec![point(index as u32)], "{}", fixed.node);
    }
}

#[test]
fn the_default_programs_slots_have_an_exit_for_every_edge_leaving_them_and_every_output_downstream() {
    let frame = frame_of(&stage3_default_program());
    let slots = frame.slots();
    assert_eq!(slots.len(), 6);

    // Each slot of the pre-aggregation pipeline leaves to the next, and its last to aggregation's isolation slot and,
    // for timestamped points, to the post-aggregation merge; every output is downstream of each, so each has an idle
    // exit to every output. Aggregation's isolation slot has only its boot edge's exit.
    let expected = [
        then_every_output(&[("dsd_prefix_filter_region", to_slot(1))]),
        then_every_output(&[("dsd_no_agg_split_region", to_slot(2))]),
        then_every_output(&[("dsd_agg", to_slot(3)), ("dsd_post_agg_filter_region", to_slot(4))]),
        aggregation_exits(),
        post_aggregation_exits(5),
        enrich_exits(),
    ];
    for (slot, exits_expected) in slots.iter().zip(&expected) {
        assert_eq!(&exits(slot), exits_expected, "{}", slot.name);
        for (index, exit) in slot.exits.iter().enumerate() {
            assert_eq!(exit.index, ExitIndex::new(index as u16));
        }
    }

    // The exits on the one path from the DogStatsD source's point to aggregation carry its progress: header-only
    // batches that move aggregation's watermark when a region forwards no data.
    let progress = slots
        .iter()
        .flat_map(|slot| {
            slot.exits
                .iter()
                .filter(|exit| !exit.progress_points.is_empty())
                .map(move |exit| (slot.id.get(), exit.index.get(), exit.progress_points.clone()))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        progress,
        vec![(0, 0, vec![point(0)]), (1, 0, vec![point(0)]), (2, 0, vec![point(0)])]
    );

    let inputs = slots.iter().map(|slot| slot.inputs.clone()).collect::<Vec<_>>();
    assert_eq!(
        inputs,
        vec![
            vec![FrameSource::Admission(point(0))],
            vec![exit_of(0, 0)],
            vec![exit_of(1, 0)],
            vec![exit_of(2, 0)],
            vec![exit_of(2, 1), exit_of(3, 0)],
            vec![
                FrameSource::Admission(point(1)),
                FrameSource::Admission(point(2)),
                FrameSource::Admission(point(3)),
                FrameSource::Admission(point(4)),
                exit_of(4, 1),
            ],
        ]
    );

    // The physical edges: each admission point's, then each slot's exits.
    let mut edges = expected_admission_points()
        .into_iter()
        .map(|(id, _, _, _, route)| FrameEdge {
            from: FrameSource::Admission(point(id)),
            to: route.unwrap(),
        })
        .collect::<Vec<_>>();
    for (slot, exits_expected) in expected.iter().enumerate() {
        for (index, (_, target)) in exits_expected.iter().enumerate() {
            edges.push(FrameEdge {
                from: exit_of(slot as u16, index as u16),
                to: target.clone(),
            });
        }
    }
    assert_eq!(frame.edges(), edges.as_slice());
    assert_eq!(
        frame.describe_source(&exit_of(2, 1)),
        "dsd_no_agg_split_region.dsd_post_agg_filter_region"
    );
    assert_eq!(
        frame.describe_source(&exit_of(3, 0)),
        "dsd_agg.dsd_post_agg_filter_region"
    );
    assert_eq!(
        frame.describe_source(&FrameSource::Admission(point(1))),
        "otlp_in.metrics"
    );
    assert_eq!(frame.describe_target(&to_slot(5)), "metrics_enrich");
}

#[test]
fn the_default_programs_fusion_decisions_name_each_edges_fence_or_slot() {
    let frame = frame_of(&stage3_default_program());
    let verdict = |from: &str, to: &str| {
        frame
            .fusion()
            .iter()
            .find(|decision| decision.from == port(from) && decision.to == port(to))
            .unwrap_or_else(|| panic!("no decision for {} -> {}", from, to))
            .verdict
            .clone()
    };
    let fence = |node: &str, boundary| FusionVerdict::Fence {
        node: id(node),
        boundary,
    };
    let fused = |slot| FusionVerdict::Fused {
        slot: SlotId::new(slot),
    };

    let pipelined = |from, to| FusionVerdict::Pipelined {
        from: SlotId::new(from),
        to: SlotId::new(to),
        budget: 110,
    };

    assert_eq!(frame.fusion().len(), 30, "one decision per edge");
    assert_eq!(
        verdict("dsd_in.metrics", "dsd_enrich.in"),
        fence("dsd_in", Boundary::IoHost)
    );
    // The cost model cuts the pre-aggregation chain between native components: the mapper (65 ns per event) and the
    // namespace prefix with its filterlist (110 ns) would cost more than the budget together.
    assert_eq!(verdict("dsd_enrich.out", "dsd_prefix_filter.in"), pipelined(0, 1));
    assert_eq!(
        verdict(
            "dsd_prefix_filter_blocklist.true",
            "dsd_prefix_filter_blocklist_true.in"
        ),
        fused(1)
    );
    assert_eq!(
        verdict("dsd_prefix_filter_blocklist.false", "dsd_no_agg_split.in"),
        pipelined(1, 2)
    );
    assert_eq!(verdict("dsd_no_agg_split.default", "dsd_tag_filterlist.in"), fused(2));
    assert_eq!(
        verdict("dsd_post_agg_filter_fanout.1", "metrics_enrich_in.0"),
        pipelined(4, 5)
    );
    // Both edges of aggregation cross its isolation fence, into and out of its isolation slot.
    let isolation = FusionVerdict::Isolation { slot: SlotId::new(3) };
    assert_eq!(verdict("dsd_tag_filterlist.out", "dsd_agg.in"), isolation);
    assert_eq!(verdict("dsd_agg.out", "dsd_post_agg_filter_in.0"), isolation);
    // The passthrough is between synchronous nodes, but aggregation's output also reaches the merge, so fusing them
    // would form a cycle through `dsd_agg`.
    assert_eq!(
        verdict("dsd_no_agg_split.passthrough", "dsd_post_agg_filter_in.1"),
        FusionVerdict::Separated {
            from: SlotId::new(2),
            to: SlotId::new(4),
            through: Some(id("dsd_agg")),
        }
    );
    assert_eq!(
        verdict("otlp_in.metrics", "metrics_enrich_in.1"),
        fence("otlp_in", Boundary::IoHost)
    );
    assert_eq!(verdict("metrics_policy.0", "metrics_policy_0.in"), fused(5));
    assert_eq!(
        verdict("metrics_policy.3", "mrf_metrics_gateway.in"),
        fence("mrf_metrics_gateway", Boundary::OpaqueHost)
    );
    assert_eq!(
        verdict("mrf_metrics_gateway.out", "mrf_metrics_gateway_out.in"),
        FusionVerdict::Direct {
            fence: id("mrf_metrics_gateway")
        }
    );

    assert_eq!(frame.site(&id("dsd_in")), Some(Site::Fixed));
    assert_eq!(frame.site(&id("dsd_tag_filterlist")), Some(Site::Slot(SlotId::new(2))));
    assert_eq!(frame.site(&id("dsd_agg")), Some(Site::Slot(SlotId::new(3))));
    assert_eq!(frame.site(&id("metrics_policy_0")), Some(Site::Slot(SlotId::new(5))));
    assert_eq!(frame.site(&id("af_metrics_gateway_out")), Some(Site::Direct));
    assert_eq!(frame.site(&id("nonexistent")), None);
}

#[test]
fn without_the_no_aggregation_pipeline_the_slots_are_joined_only_through_aggregation() {
    let frame = frame_of(&stage3_without_no_aggregation_pipeline());

    // Without the timestamp split, the tag filterlist runs alone after the namespace prefix, and the post-aggregation
    // filter has no merge in front of it.
    let mut expected = vec![
        ("dsd_pre_aggregation".to_string(), ids(&["dsd_enrich"])),
        ("dsd_prefix_filter_region".to_string(), ids(PRE_AGGREGATION[1].1)),
        ("dsd_tag_filterlist_region".to_string(), ids(&["dsd_tag_filterlist"])),
        (AGGREGATION.0.to_string(), ids(AGGREGATION.1)),
        (
            "dsd_post_agg_filter_region".to_string(),
            ids(&POST_AGGREGATION[0].1[1..]),
        ),
    ];
    expected.push(("metrics_enrich".to_string(), ids(POST_AGGREGATION[1].1)));
    assert_eq!(slots_of(&frame), expected);
    let fixed_nodes = frame.fixed().iter().map(|fixed| fixed.node.clone()).collect::<Vec<_>>();
    assert_eq!(fixed_nodes, ids(&FIXED));
    assert_eq!(admission_points(&frame), owned(expected_admission_points()));

    assert_eq!(exits(&frame.slots()[2]), then_every_output(&[("dsd_agg", to_slot(3))]));
    assert_eq!(exits(&frame.slots()[3]), aggregation_exits());
    assert_eq!(exits(&frame.slots()[4]), post_aggregation_exits(5));
    assert_eq!(exits(&frame.slots()[5]), enrich_exits());
    assert_eq!(frame.slots()[3].inputs, vec![exit_of(2, 0)]);
    assert_eq!(frame.slots()[4].inputs, vec![exit_of(3, 0)]);
}

#[test]
fn with_gateways_as_routing_operators_enrichment_runs_the_whole_policy_region_as_one_decision() {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let frame = frame_of(&plan);
    let fixed_nodes = frame.fixed().iter().map(|fixed| fixed.node.clone()).collect::<Vec<_>>();
    assert_eq!(fixed_nodes, ids(&FIXED[..5]));
    assert!(frame.edges().iter().all(|edge| !matches!(
        (edge.from, &edge.to),
        (FrameSource::Admission(_), FrameTarget::Destination(_))
    )));

    // The policy fan-out and the four gateways below it are one decision, which R1 never cuts and prices as it runs:
    // its predicates read only context fields and the kind, so one memo lookup (10 ns) and routing (5 ns) stand for
    // the gateways' matchers, beside the fan-out's four copies (60 ns). With host tags (30 ns), enrichment costs
    // 105 ns, within the budget (110 ns, the native prefix filter's), so it runs the whole policy region and feeds
    // every encoder directly; the post-aggregation filter (36 ns with its fan-out's copy) still does not fit beside it.
    let gateway = |node: &str| ids(&[node, &format!("{}_true", node), &format!("{}_false", node)]);
    let mut enrichment = ids(POST_AGGREGATION[1].1);
    for node in [
        "metrics_routing_filter_0",
        "metrics_routing_filter_1",
        "mrf_metrics_gateway",
        "af_metrics_gateway",
    ] {
        enrichment.extend(gateway(node));
    }
    let mut expected = designed_slots();
    expected[5].1 = enrichment;
    assert_eq!(slots_of(&frame), expected);
    assert_eq!(frame.cost_budget(), 110);
    let costs = frame.slots().iter().map(|slot| slot.cost).collect::<Vec<_>>();
    assert_eq!(costs, vec![65, 110, 25, 0, 36, 105]);

    let outputs = [
        "dd_metrics_encode",
        "metrics_routing_encode_0",
        "metrics_routing_encode_1",
        "mrf_metrics_encode",
        "af_metrics_encode",
    ];
    let expected = outputs.map(|output_id| (output_id, to_output(output_id)));
    assert_eq!(exits(&frame.slots()[5]), named(&expected));
}

/// A plan of sources, opaque hosts, `host_enrichment` maps, routing operators, and one output `out`, written as
/// `(id, op, params)` nodes and `(from, to)` edges.
fn synthetic(nodes: &[(&str, &str, Value)], edges: &[(&str, &str)]) -> Plan {
    let nodes = nodes
        .iter()
        .map(|(id, op, params)| json!({"id": id, "op": op, "params": params, "provenance": {"native": id}}))
        .collect::<Vec<_>>();
    let edges = edges
        .iter()
        .map(|(from, to)| json!({"from": from, "to": to}))
        .collect::<Vec<_>>();
    Plan::from_value(json!({
        "ir_version": 1,
        "nodes": nodes,
        "edges": edges,
        "outputs": [{"id": "out", "accepts": {"kinds": "all", "timestamps": "mixed"},
                     "fingerprint": format!("sha256:{}", "7".repeat(64))}],
    }))
    .unwrap()
}

fn synthetic_catalog() -> MemoryCatalog {
    MemoryCatalog::new().with_output(
        output("out"),
        CatalogOutput {
            accepts: PortType::ANY,
            fingerprint: format!("sha256:{}", "7".repeat(64)).parse().unwrap(),
            recipients: Vec::new(),
            legacy_fingerprint: None,
        },
    )
}

fn opaque() -> Value {
    let all = json!({"kinds": "all", "timestamps": "mixed"});
    json!({"component": "host", "input": all, "outputs": {"out": all}})
}

#[test]
fn synchronous_nodes_joined_downstream_of_an_unrelated_fence_share_a_slot() {
    // `a` and `w` both feed `z`; the fence `host` before `w` is not reachable from the slot, so nothing separates them.
    let plan = synthetic(
        &[
            ("otlp_in", "source", json!({"binding": "otlp"})),
            ("checks_ipc_in", "source", json!({"binding": "checks"})),
            ("a", "host_enrichment", json!({})),
            ("host", "opaque_host", opaque()),
            ("w", "host_enrichment", json!({})),
            ("z", "merge", json!({"inputs": 2})),
            ("z_out", "emit", json!({"output": "out"})),
        ],
        &[
            ("otlp_in.metrics", "a.in"),
            ("checks_ipc_in.metrics", "host.in"),
            ("host.out", "w.in"),
            ("a.out", "z.0"),
            ("w.out", "z.1"),
            ("z.out", "z_out.in"),
        ],
    );
    validated(&plan, &synthetic_catalog());
    let frame = Frame::compute(&plan, &OperatorRegistry::native(), &first_node_namer).unwrap();
    assert_eq!(frame.slots().len(), 1);
    assert_eq!(frame.slots()[0].nodes, ids(&["a", "w", "z", "z_out"]));
    assert_eq!(exits(&frame.slots()[0]), named(&[("out", to_output("out"))]));
    assert_eq!(
        frame.slots()[0].inputs,
        vec![FrameSource::Admission(point(0)), FrameSource::Admission(point(2))]
    );
    assert_eq!(
        frame.fixed_component(&id("host")).unwrap().input,
        Some(FrameSource::Admission(point(1)))
    );
}

#[test]
fn synchronous_nodes_that_a_fence_separates_get_their_own_slots() {
    // `t` copies to the host and to the merge `m`, which the host's output also reaches: one slot would be a cycle.
    let plan = synthetic(
        &[
            ("otlp_in", "source", json!({"binding": "otlp"})),
            ("a", "host_enrichment", json!({})),
            ("t", "tee", json!({"ports": 2})),
            ("host", "opaque_host", opaque()),
            ("m", "merge", json!({"inputs": 2})),
            ("m_out", "emit", json!({"output": "out"})),
        ],
        &[
            ("otlp_in.metrics", "a.in"),
            ("a.out", "t.in"),
            ("t.0", "host.in"),
            ("t.1", "m.1"),
            ("host.out", "m.0"),
            ("m.out", "m_out.in"),
        ],
    );
    let frame = Frame::compute(&plan, &OperatorRegistry::native(), &first_node_namer).unwrap();
    let slots = frame
        .slots()
        .iter()
        .map(|slot| (slot.name.to_string(), slot.nodes.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        slots,
        vec![
            ("slot_a".to_string(), ids(&["a", "t"])),
            ("slot_m".to_string(), ids(&["m", "m_out"])),
        ]
    );
    assert_eq!(
        exits(&frame.slots()[0]),
        named(&[
            ("host", fixed("host")),
            ("slot_m", to_slot(1)),
            ("out", to_output("out"))
        ])
    );
    assert_eq!(exits(&frame.slots()[1]), named(&[("out", to_output("out"))]));
    let separated = frame
        .fusion()
        .iter()
        .find(|decision| decision.from == port("t.1"))
        .unwrap();
    assert_eq!(
        separated.verdict,
        FusionVerdict::Separated {
            from: SlotId::new(0),
            to: SlotId::new(1),
            through: Some(id("host")),
        }
    );
}

/// A chain from `otlp_in` through `host_tags` nodes, each `(id, native component)`, to the output `out`.
fn host_tags_chain(nodes: &[(&str, &str)], operator: &str) -> Plan {
    let mut value = serde_json::to_value(synthetic(&[("otlp_in", "source", json!({"binding": "otlp"}))], &[])).unwrap();
    let mut previous = "otlp_in.metrics".to_string();
    for (node, component) in nodes {
        let params = if operator == "host_tags" {
            json!({"tags": ["team:core"], "deadline": "1700000300s"})
        } else {
            json!({})
        };
        value["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": node, "op": operator, "params": params, "provenance": {"native": component}}));
        value["edges"]
            .as_array_mut()
            .unwrap()
            .push(json!({"from": previous, "to": format!("{}.in", node)}));
        previous = format!("{}.out", node);
    }
    value["nodes"].as_array_mut().unwrap().push(
        json!({"id": "last_out", "op": "emit", "params": {"output": "out"}, "provenance": {"native": "last_out"}}),
    );
    value["edges"]
        .as_array_mut()
        .unwrap()
        .push(json!({"from": previous, "to": "last_out.in"}));
    Plan::from_value(value).unwrap()
}

#[test]
fn the_cost_model_pipelines_a_chain_between_native_components_and_within_its_budget() {
    // Host tags cost 30 ns per event each. The costliest component, `first`, costs 60 ns, less than the floor, so the
    // budget is the floor, 64 ns: `first` fills the first region, and `second` and `third` share the next.
    let nodes = [("t1", "first"), ("t2", "first"), ("t3", "second"), ("t4", "third")];
    let plan = host_tags_chain(&nodes, "host_tags");
    validated(&plan, &synthetic_catalog());
    let frame = Frame::compute(&plan, &OperatorRegistry::native(), &first_node_namer).unwrap();
    assert_eq!(frame.cost_budget(), REGION_COST_FLOOR);
    assert_eq!(
        slots_of(&frame),
        vec![
            ("slot_t1".to_string(), ids(&["t1", "t2"])),
            ("slot_t3".to_string(), ids(&["t3", "t4", "last_out"])),
        ]
    );
    assert_eq!(
        frame.slots().iter().map(|slot| slot.cost).collect::<Vec<_>>(),
        vec![60, 60]
    );
    assert_eq!(
        exits(&frame.slots()[0]),
        named(&[("slot_t3", to_slot(1)), ("out", to_output("out"))])
    );
    let verdict = |from: &str| {
        frame
            .fusion()
            .iter()
            .find(|decision| decision.from == port(from))
            .unwrap()
            .verdict
            .clone()
    };
    assert_eq!(verdict("t1.out"), FusionVerdict::Fused { slot: SlotId::new(0) });
    assert_eq!(
        verdict("t2.out"),
        FusionVerdict::Pipelined {
            from: SlotId::new(0),
            to: SlotId::new(1),
            budget: REGION_COST_FLOOR,
        }
    );
    assert_eq!(verdict("t3.out"), FusionVerdict::Fused { slot: SlotId::new(1) });

    // A component is never cut, even one costlier than the floor: it sets the budget, 90 ns, and runs alone.
    let nodes = [
        ("t1", "first"),
        ("t2", "first"),
        ("t3", "first"),
        ("t4", "second"),
        ("t5", "third"),
    ];
    let frame = Frame::compute(
        &host_tags_chain(&nodes, "host_tags"),
        &OperatorRegistry::native(),
        &first_node_namer,
    )
    .unwrap();
    assert_eq!(frame.cost_budget(), 90);
    assert_eq!(
        slots_of(&frame),
        vec![
            ("slot_t1".to_string(), ids(&["t1", "t2", "t3"])),
            ("slot_t4".to_string(), ids(&["t4", "t5", "last_out"])),
        ]
    );

    // Work within the floor is never cut: host enrichment costs nothing.
    let frame = Frame::compute(
        &host_tags_chain(&nodes, "host_enrichment"),
        &OperatorRegistry::native(),
        &first_node_namer,
    )
    .unwrap();
    assert_eq!(frame.slots().len(), 1);
    assert_eq!(frame.slots()[0].cost, 0);
}

/// Two branches, `x` and `y`, each a native component of two `host_tags` nodes (60 ns per event), either fanned out
/// from `otlp_in` and ending in terminals of their own (`x_out` emits, `y_out` drops), or, if `join`, fed by `otlp_in`
/// and `checks_ipc_in` and meeting in a merge `m` that `m_out` emits (a fan-out cannot feed both: its copies would
/// reach one output twice).
fn parallel_branches(join: bool) -> Plan {
    let tags = json!({"tags": ["team:core"], "deadline": "1700000300s"});
    let mut nodes = vec![json!(
        {"id": "otlp_in", "op": "source", "params": {"binding": "otlp"}, "provenance": {"native": "otlp_in"}}
    )];
    let mut edges = Vec::new();
    let inputs = if join {
        nodes.push(json!(
            {"id": "checks_ipc_in", "op": "source", "params": {"binding": "checks"},
             "provenance": {"native": "checks_ipc_in"}}
        ));
        ["otlp_in.metrics", "checks_ipc_in.metrics"]
    } else {
        nodes.push(json!({"id": "fanout", "op": "tee", "params": {"ports": 2}, "provenance": {"native": "fanout"}}));
        edges.push(json!({"from": "otlp_in.metrics", "to": "fanout.in"}));
        ["fanout.0", "fanout.1"]
    };
    for (input, branch) in inputs.into_iter().zip(["x", "y"]) {
        for step in 1..=2 {
            nodes.push(json!({
                "id": format!("{}{}", branch, step), "op": "host_tags", "params": tags,
                "provenance": {"native": branch},
            }));
        }
        edges.push(json!({"from": input, "to": format!("{}1.in", branch)}));
        edges.push(json!({"from": format!("{}1.out", branch), "to": format!("{}2.in", branch)}));
    }
    let terminals = if join {
        edges.push(json!({"from": "x2.out", "to": "m.0"}));
        edges.push(json!({"from": "y2.out", "to": "m.1"}));
        edges.push(json!({"from": "m.out", "to": "m_out.in"}));
        vec![
            ("m", "merge", json!({"inputs": 2})),
            ("m_out", "emit", json!({"output": "out"})),
        ]
    } else {
        edges.push(json!({"from": "x2.out", "to": "x_out.in"}));
        edges.push(json!({"from": "y2.out", "to": "y_out.in"}));
        vec![
            ("x_out", "emit", json!({"output": "out"})),
            ("y_out", "drop", json!({})),
        ]
    };
    for (id, op, params) in terminals {
        nodes.push(json!({"id": id, "op": op, "params": params, "provenance": {"native": id}}));
    }
    let mut value = serde_json::to_value(synthetic(&[], &[])).unwrap();
    value["nodes"] = Value::Array(nodes);
    value["edges"] = Value::Array(edges);
    Plan::from_value(value).unwrap()
}

#[test]
fn the_cost_model_sums_parallel_branches_only_where_a_node_joins_them() {
    // The budget is the floor, 64 ns. After a fan-out, neither branch (60 ns) fits beside the fan-out's copy (15 ns),
    // so each starts a region of its own on the next stage. That stage holds 120 ns of work, but in two regions, each
    // within the budget, so the terminals, which cost nothing, stay with their branches rather than starting regions
    // of their own. Fed by two sources, the branches run on the first stage, in two regions; a merge of the two, which
    // costs nothing either, would join both into one region of 120 ns, so it starts the next stage, with its emit.
    type Slots<'a> = &'a [(&'a str, &'a [&'a str])];
    let cases: [(bool, Slots, &[u32]); 2] = [
        (
            false,
            &[
                ("slot_fanout", &["fanout"]),
                ("slot_x1", &["x1", "x2", "x_out"]),
                ("slot_y1", &["y1", "y2", "y_out"]),
            ],
            &[15, 60, 60],
        ),
        (
            true,
            &[
                ("slot_x1", &["x1", "x2"]),
                ("slot_y1", &["y1", "y2"]),
                ("slot_m", &["m", "m_out"]),
            ],
            &[60, 60, 0],
        ),
    ];
    for (join, slots, costs) in cases {
        let plan = parallel_branches(join);
        validated(&plan, &synthetic_catalog());
        let frame = Frame::compute(&plan, &OperatorRegistry::native(), &first_node_namer).unwrap();
        assert_eq!(frame.cost_budget(), REGION_COST_FLOOR, "join: {}", join);
        let expected = slots
            .iter()
            .map(|(name, nodes)| (name.to_string(), ids(nodes)))
            .collect::<Vec<_>>();
        assert_eq!(slots_of(&frame), expected, "join: {}", join);
        assert_eq!(
            frame.slots().iter().map(|slot| slot.cost).collect::<Vec<_>>(),
            costs,
            "join: {}",
            join
        );
    }
}

#[test]
fn a_decision_is_never_cut_and_costs_what_its_function_does() {
    // Host tags (30 ns) and a three-way fan-out (two copies, 30 ns) are one native component, and the fan-out leads to
    // three splits, each matching the intake name four times (natively 35 ns: it normalizes the name once). The
    // budget is the floor, 64 ns. The fan-out and the splits are one decision, never cut, so the component and the
    // splits' components run in one region: the splits' predicates read only the name, so the decision costs one memo
    // lookup (10 ns) and routing (5 ns) beside the copies, and the region 75 ns, more than the budget, as a component
    // alone may.
    let matches = json!({"intake_name": {"matches": {"values": ["a"], "prefix": false}}});
    let split = json!({"predicate": {"any": [matches, matches, matches, matches]}});
    let names = [
        "s0", "s0_true", "s0_false", "s1", "s1_true", "s1_false", "s2", "s2_true", "s2_false",
    ];
    let mut nodes = vec![
        ("otlp_in", "source", json!({"binding": "otlp"})),
        (
            "tags",
            "host_tags",
            json!({"tags": ["team:core"], "deadline": "1700000300s"}),
        ),
        ("fanout", "tee", json!({"ports": 3})),
    ];
    for name in names {
        let (op, params) = match name {
            "s0_true" => ("emit", json!({"output": "out"})),
            _ if name.contains('_') => ("drop", json!({})),
            _ => ("split", split.clone()),
        };
        nodes.push((name, op, params));
    }
    let mut edges = vec![
        ("otlp_in.metrics".to_string(), "tags.in".to_string()),
        ("tags.out".to_string(), "fanout.in".to_string()),
    ];
    for index in 0..3 {
        edges.push((format!("fanout.{}", index), format!("s{}.in", index)));
        edges.push((format!("s{}.true", index), format!("s{}_true.in", index)));
        edges.push((format!("s{}.false", index), format!("s{}_false.in", index)));
    }
    let edges = edges
        .iter()
        .map(|(from, to)| (from.as_str(), to.as_str()))
        .collect::<Vec<_>>();
    let mut plan = synthetic(&nodes, &edges);
    let enrich = NativeComponentId::new("enrich").unwrap();
    for node in &mut plan.nodes {
        if ["tags", "fanout"].contains(&node.id.local().as_str()) {
            node.provenance = Some(Provenance::Native(enrich.clone()));
        }
    }
    validated(&plan, &synthetic_catalog());

    let frame = Frame::compute(&plan, &OperatorRegistry::native(), &first_node_namer).unwrap();
    assert_eq!(frame.cost_budget(), REGION_COST_FLOOR);
    let mut members = vec!["tags", "fanout"];
    members.extend(names);
    assert_eq!(slots_of(&frame), vec![("slot_tags".to_string(), ids(&members))]);
    assert_eq!(frame.slots()[0].cost, 75);
}

#[test]
fn a_candidate_runs_its_boot_nodes_in_their_boot_regions_and_new_nodes_where_their_inputs_run() {
    let nodes = [("t1", "first"), ("t2", "first"), ("t3", "second"), ("t4", "third")];
    let boot = host_tags_chain(&nodes, "host_tags");
    let frame = Frame::compute(&boot, &OperatorRegistry::native(), &first_node_namer).unwrap();

    // A new node between the two regions runs in the region its input runs in; `t3` stays in its boot region, which the
    // first region has an exit to.
    let mut value = serde_json::to_value(&boot).unwrap();
    value["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "between", "op": "host_enrichment", "params": {}, "provenance": {"native": "between"}}));
    for edge in value["edges"].as_array_mut().unwrap() {
        if edge["from"] == "t2.out" {
            edge["from"] = json!("between.out");
        }
    }
    value["edges"]
        .as_array_mut()
        .unwrap()
        .push(json!({"from": "t2.out", "to": "between.in"}));
    let candidate = Plan::from_value(value).unwrap();
    let report = validated(&candidate, &synthetic_catalog());
    let placement = place(&candidate, &report, &OperatorRegistry::native(), &frame).unwrap();
    let nodes = placement
        .slots()
        .iter()
        .map(|slot| slot.nodes.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        nodes,
        vec![ids(&["t1", "t2", "between"]), ids(&["t3", "t4", "last_out"])]
    );
    assert_eq!(placement.slots()[0].exits_used, vec![ExitIndex::new(0)]);
}

#[test]
fn an_emit_or_drop_right_after_a_fence_is_a_direct_edge_or_nothing() {
    let plan = synthetic(
        &[
            ("otlp_in", "source", json!({"binding": "otlp"})),
            ("otlp_in_metrics", "emit", json!({"output": "out"})),
            ("checks_ipc_in", "source", json!({"binding": "checks"})),
            ("checks_ipc_in_metrics", "drop", json!({})),
        ],
        &[
            ("otlp_in.metrics", "otlp_in_metrics.in"),
            ("checks_ipc_in.metrics", "checks_ipc_in_metrics.in"),
        ],
    );
    let frame = Frame::compute(&plan, &OperatorRegistry::native(), &first_node_namer).unwrap();
    assert!(frame.slots().is_empty());
    assert_eq!(
        frame.edges(),
        &[FrameEdge {
            from: FrameSource::Admission(point(0)),
            to: to_output("out"),
        }]
    );
    assert_eq!(frame.admission_points()[1].route, None);
    assert_eq!(frame.site(&id("checks_ipc_in_metrics")), Some(Site::Direct));
}

#[test]
fn frames_need_a_sound_plan_and_distinct_names() {
    let registry = OperatorRegistry::native();
    let plan = stage3_default_program();

    let refusing = |_: &[NodeId]| -> Result<NativeComponentId, String> { Err("no names today".to_string()) };
    let error = Frame::compute(&plan, &registry, &refusing).unwrap_err();
    assert!(matches!(error, FrameError::SlotName { .. }));
    assert!(error.to_string().ends_with(": no names today"), "{}", error);

    let constant = |_: &[NodeId]| Ok(NativeComponentId::new("region").unwrap());
    assert_eq!(
        Frame::compute(&plan, &registry, &constant).unwrap_err(),
        FrameError::DuplicateSlotName {
            name: NativeComponentId::new("region").unwrap()
        }
    );

    // A slot named like an output it has an exit to cannot name both exits.
    let clashing = |nodes: &[NodeId]| {
        if nodes.iter().any(|node| node.to_string() == "metrics_enrich") {
            Ok(NativeComponentId::new("dd_metrics_encode").unwrap())
        } else {
            namer(nodes)
        }
    };
    assert_eq!(
        Frame::compute(&plan, &registry, &clashing).unwrap_err(),
        FrameError::ExitNameCollision {
            slot: NativeComponentId::new("dsd_post_agg_filter_region").unwrap(),
            name: "dd_metrics_encode".to_string(),
        }
    );

    let mut broken = serde_json::to_value(&plan).unwrap();
    broken["edges"].as_array_mut().unwrap().remove(0);
    let error = Frame::compute(&Plan::from_value(broken).unwrap(), &registry, &namer).unwrap_err();
    assert!(matches!(error, FrameError::InvalidPlan { .. }), "{}", error);
}

// Placement.

#[test]
fn the_boot_plan_places_where_the_frame_put_it() {
    for plan in [stage3_default_program(), stage3_without_no_aggregation_pipeline()] {
        let frame = frame_of(&plan);
        let placement = placed(&plan, &frame).unwrap();
        for node in &plan.nodes {
            assert_eq!(placement.site(&node.id), frame.site(&node.id), "{}", node.id);
        }
        for (slot, placed) in frame.slots().iter().zip(placement.slots()) {
            assert_eq!(placed.nodes, slot.nodes);
        }
        assert!(placement.idle_fixed().is_empty());
    }
}

#[test]
fn the_default_programs_entries_and_exits_follow_its_edges() {
    let plan = stage3_default_program();
    let frame = frame_of(&plan);
    let placement = placed(&plan, &frame).unwrap();
    let entries = |slot: usize| {
        placement.slots()[slot]
            .entries
            .iter()
            .map(|entry| (entry.key, entry.source, entry.from.to_string(), entry.input.to_string()))
            .collect::<Vec<_>>()
    };
    let admission = |id| (EntryKey::Admission(point(id)), FrameSource::Admission(point(id)));
    let from_region_as = |entry, slot, exit, from: &str, input: &str| {
        (
            EntryKey::Region(EntryId::new(entry)),
            exit_of(slot, exit),
            from.to_string(),
            input.to_string(),
        )
    };
    let from_region = |slot, exit, from: &str, input: &str| from_region_as(0, slot, exit, from, input);

    let (key, source) = admission(0);
    assert_eq!(
        entries(0),
        vec![(key, source, "dsd_in.metrics".to_string(), "dsd_enrich.in".to_string())]
    );
    // Each region of the pre-aggregation pipeline enters the next as that one's entry 0.
    assert_eq!(
        entries(1),
        vec![from_region(0, 0, "dsd_enrich.out", "dsd_prefix_filter.in")]
    );
    assert_eq!(
        entries(2),
        vec![from_region(
            1,
            0,
            "dsd_prefix_filter_blocklist.false",
            "dsd_no_agg_split.in"
        )]
    );
    // Aggregation's isolation slot: from the pipeline's last region.
    assert_eq!(
        entries(3),
        vec![from_region(2, 0, "dsd_tag_filterlist.out", "dsd_agg.in")]
    );
    // The post-aggregation filter's region: from aggregation's isolation slot, and the passthrough from the pipeline's
    // last region, its entries 0 and 1.
    assert_eq!(
        entries(4),
        vec![
            from_region(3, 0, "dsd_agg.out", "dsd_post_agg_filter_in.0"),
            from_region_as(1, 2, 1, "dsd_no_agg_split.passthrough", "dsd_post_agg_filter_in.1"),
        ]
    );
    let mut expected = Vec::new();
    for (id, from, input) in [
        (1, "otlp_in.metrics", "metrics_enrich_in.1"),
        (2, "checks_ipc_in.metrics", "metrics_enrich_in.2"),
        (3, "liveness_in.metrics", "metrics_enrich_in.3"),
        (4, "trace_sampler.metrics", "metrics_enrich_in.4"),
    ] {
        let (key, source) = admission(id);
        expected.push((key, source, from.to_string(), input.to_string()));
    }
    expected.push(from_region(4, 1, "dsd_post_agg_filter_fanout.1", "metrics_enrich_in.0"));
    assert_eq!(entries(5), expected);

    // Each slot uses the exits its edges leave by: to the next region, to aggregation, from aggregation to the
    // post-aggregation filter, to the client telemetry tap, and from enrichment to the primary output and the four
    // gateways. Every other exit is idle.
    let used = |slot: usize| {
        placement.slots()[slot]
            .exits_used
            .iter()
            .map(|exit| exit.get())
            .collect::<Vec<_>>()
    };
    let all_used = (0..6).map(used).collect::<Vec<_>>();
    assert_eq!(
        all_used,
        vec![vec![0], vec![0], vec![0, 1], vec![0], vec![0, 1], vec![0, 1, 2, 3, 4]]
    );
    assert!(placement.is_idle(SlotId::new(0), ExitIndex::new(1)));
    assert!(!placement.is_idle(SlotId::new(5), ExitIndex::new(4)));
    assert!(placement.is_idle(SlotId::new(5), ExitIndex::new(8)));
}

/// Applies `edit` to the JSON of the stage 3 default program.
fn edited(edit: impl FnOnce(&mut Value)) -> Plan {
    let mut value = with_opaque_gateways(DEFAULT_PROGRAM_JSON);
    edit(&mut value);
    Plan::from_value(value).unwrap()
}

fn remove_nodes(value: &mut Value, removed: &[&str]) {
    value["nodes"]
        .as_array_mut()
        .unwrap()
        .retain(|node| !removed.iter().any(|id| node["id"] == *id));
    value["edges"].as_array_mut().unwrap().retain(|edge| {
        let ends = |end: &str| edge[end].as_str().unwrap().split('.').next().unwrap().to_string();
        !removed.iter().any(|id| ends("from") == *id || ends("to") == *id)
    });
}

fn node_mut<'a>(value: &'a mut Value, id: &str) -> &'a mut Value {
    value["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == id)
        .unwrap()
}

#[test]
fn removing_a_branch_leaves_its_exit_and_host_idle() {
    let frame = frame_of(&stage3_default_program());
    let plan = edited(|value| {
        remove_nodes(value, &["af_metrics_gateway", "af_metrics_gateway_out"]);
        node_mut(value, "metrics_policy")["params"]["ports"] = json!(4);
        value["outputs"].as_array_mut().unwrap().pop();
    });
    let placement = placed(&plan, &frame).unwrap();
    assert_eq!(placement.idle_fixed(), &[id("af_metrics_gateway")]);
    // Enrichment's exit 4 led to the autoscaling failover gateway.
    assert_eq!(
        placement.slots()[5]
            .exits_used
            .iter()
            .map(|exit| exit.get())
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert!(placement.is_idle(SlotId::new(5), ExitIndex::new(4)));
}

#[test]
fn removing_a_branch_of_routing_operators_leaves_its_output_exit_idle() {
    let boot = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let frame = frame_of(&boot);
    let mut value = serde_json::from_str::<Value>(DEFAULT_PROGRAM_JSON).unwrap();
    remove_nodes(
        &mut value,
        &[
            "mrf_metrics_gateway",
            "mrf_metrics_gateway_true",
            "mrf_metrics_gateway_false",
        ],
    );
    for edge in value["edges"].as_array_mut().unwrap() {
        if edge["from"] == "metrics_policy.4" {
            edge["from"] = json!("metrics_policy.3");
        }
    }
    node_mut(&mut value, "metrics_policy")["params"]["ports"] = json!(4);
    let plan = Plan::from_value(value).unwrap();
    let placement = placed(&plan, &frame).unwrap();
    // `mrf_metrics_encode` is the enrichment slot's exit 3, where the Multi-Region Failover gateway runs.
    assert_eq!(exits(&frame.slots()[5])[3].0, "mrf_metrics_encode");
    assert!(placement.is_idle(SlotId::new(5), ExitIndex::new(3)));
    assert!(placement.idle_fixed().is_empty());
}

#[test]
fn live_parameter_changes_place_like_the_boot_plan() {
    let boot = stage3_default_program();
    let frame = frame_of(&boot);
    let plan = edited(|value| {
        node_mut(value, "dsd_prefix_filter_blocklist")["params"]["predicate"]["intake_name"]["matches"]["values"]
            ["resolved"] = json!(["tenant.raw.blocked", "tenant.raw.other"]);
    });
    assert_eq!(placed(&plan, &frame).unwrap(), placed(&boot, &frame).unwrap());
}

/// Returns the problems of a placement that must fail, as text, and its capabilities.
fn unplaceable(plan: &Plan, catalog: &MemoryCatalog, frame: &Frame) -> (Vec<String>, Vec<MissingCapability>) {
    let report = validated(plan, catalog);
    let error = place(plan, &report, &OperatorRegistry::native(), frame).unwrap_err();
    let PlacementError::Unplaceable { problems } = &error else {
        panic!("not unplaceable: {}", error)
    };
    (problems.iter().map(ToString::to_string).collect(), error.capabilities())
}

#[test]
fn adding_a_fixed_destination_needs_a_restart() {
    let frame = frame_of(&stage3_default_program());
    let fingerprint = format!("sha256:{}", "8".repeat(64));
    let plan = edited(|value| {
        value["outputs"].as_array_mut().unwrap().push(json!({
            "id": "new_encode", "accepts": {"kinds": "all", "timestamps": "mixed"}, "fingerprint": fingerprint,
            "recipients": ["new_out/primary/0"]
        }));
        node_mut(value, "metrics_policy")["params"]["ports"] = json!(6);
        value["nodes"].as_array_mut().unwrap().push(json!({
            "id": "metrics_policy_5", "op": "emit", "params": {"output": "new_encode"},
            "provenance": {"native": "new_encode"}
        }));
        value["edges"]
            .as_array_mut()
            .unwrap()
            .push(json!({"from": "metrics_policy.5", "to": "metrics_policy_5.in"}));
    });
    let catalog = default_program_catalog().with_output(
        output("new_encode"),
        CatalogOutput {
            accepts: PortType::ANY,
            fingerprint: fingerprint.parse().unwrap(),
            recipients: vec!["new_out/primary/0".parse().unwrap()],
            legacy_fingerprint: None,
        },
    );
    // An output without a binding is a fixed destination, a component of its own: native configuration asks for it,
    // and a restart builds it. The edge to it is reported with the output, once.
    let (problems, capabilities) = unplaceable(&plan, &catalog, &frame);
    assert_eq!(capabilities, vec![MissingCapability::Restart]);
    assert_eq!(
        problems,
        vec![
            "output `new_encode` has no binding, and is not a fixed destination of the topology built at boot: adding \
             a fixed destination needs a restart, since the physical structure is built from native configuration at \
             boot"
                .to_string(),
        ]
    );
}

#[test]
fn a_new_fence_crossing_native_configuration_asks_for_needs_a_restart() {
    let frame = frame_of(&stage3_default_program());

    // Processing after a gateway needs a region there; the topology connects the gateway straight to its encoder.
    let after_host = edited(|value| {
        value["nodes"].as_array_mut().unwrap().push(json!({
            "id": "mrf_metrics_gateway_enrich", "op": "host_enrichment", "provenance": {"native": "mrf_metrics_gateway"}
        }));
        for edge in value["edges"].as_array_mut().unwrap() {
            if edge["from"] == "mrf_metrics_gateway.out" {
                edge["from"] = json!("mrf_metrics_gateway_enrich.out");
            }
        }
        value["edges"]
            .as_array_mut()
            .unwrap()
            .push(json!({"from": "mrf_metrics_gateway.out", "to": "mrf_metrics_gateway_enrich.in"}));
    });
    let (problems, capabilities) = unplaceable(&after_host, &default_program_catalog(), &frame);
    assert_eq!(capabilities, vec![MissingCapability::Restart]);
    assert_eq!(
        problems,
        vec![
            "node `mrf_metrics_gateway_enrich`: it is fed by `mrf_metrics_gateway.out`, which the topology built at \
             boot connects straight to output mrf_metrics_encode, with no region to run this node in: adding a region \
             after a host needs a restart, since the physical structure is built from native configuration at boot"
                .to_string()
        ]
    );

    // OTLP metrics enter the enrichment slot; sending them through the DogStatsD chain needs a new physical edge. (The
    // DogStatsD scope is fed only by DogStatsD, so the edit drops it.)
    let rerouted = edited(|value| {
        value["scopes"]
            .as_object_mut()
            .unwrap()
            .remove("dogstatsd_pre_aggregation");
        value["nodes"].as_array_mut().unwrap().push(json!({
            "id": "dsd_enrich_in", "op": "merge", "params": {"inputs": 2}, "provenance": {"native": "dsd_enrich"}
        }));
        for edge in value["edges"].as_array_mut().unwrap() {
            if edge["from"] == "dsd_in.metrics" {
                edge["to"] = json!("dsd_enrich_in.0");
            }
            if edge["from"] == "otlp_in.metrics" {
                edge["to"] = json!("dsd_enrich_in.1");
            }
        }
        value["edges"]
            .as_array_mut()
            .unwrap()
            .push(json!({"from": "dsd_enrich_in.out", "to": "dsd_enrich.in"}));
        node_mut(value, "metrics_enrich_in")["params"]["inputs"] = json!(4);
        for edge in value["edges"].as_array_mut().unwrap() {
            for (old, new) in [
                ("metrics_enrich_in.2", "metrics_enrich_in.1"),
                ("metrics_enrich_in.3", "metrics_enrich_in.2"),
                ("metrics_enrich_in.4", "metrics_enrich_in.3"),
            ] {
                if edge["to"] == old {
                    edge["to"] = json!(new);
                }
            }
        }
    });
    let (problems, _) = unplaceable(&rerouted, &default_program_catalog(), &frame);
    assert_eq!(
        problems,
        vec![
            "node `dsd_enrich_in`: it joins `dsd_in.metrics` (to `dsd_pre_aggregation`) and `otlp_in.metrics` (to \
             `metrics_enrich`), which the topology built at boot connects to different regions: a new physical edge \
             needs a restart, since the physical structure is built from native configuration at boot"
                .to_string()
        ]
    );
}

#[test]
fn host_components_keep_their_boot_configuration() {
    let frame = frame_of(&stage3_default_program());
    let catalog = default_program_catalog();

    // Aggregation is no host component: it runs as a kernel in its isolation slot, so a change of its parameters
    // places as the boot plan does, and the installer classifies it as a state transition.
    let wider = edited(|value| {
        node_mut(value, "dsd_agg")["params"]["flush_interval"]["resolved"] = json!("30s");
    });
    assert_eq!(
        placed(&wider, &frame).unwrap(),
        placed(&stage3_default_program(), &frame).unwrap()
    );

    // A gateway is an opaque host: reconfiguring it needs a restart.
    let narrower = edited(|value| {
        node_mut(value, "metrics_routing_filter_0")["params"]["component"] = json!("other_filter");
    });
    let (_, capabilities) = unplaceable(&narrower, &catalog, &frame);
    assert_eq!(capabilities, vec![MissingCapability::Restart]);

    // A source cannot be left out: its host keeps producing.
    let without_source = edited(|value| {
        remove_nodes(value, &["trace_sampler"]);
        node_mut(value, "metrics_enrich_in")["params"]["inputs"] = json!(4);
    });
    let (problems, capabilities) = unplaceable(&without_source, &catalog, &frame);
    assert_eq!(capabilities, vec![MissingCapability::Restart]);
    assert_eq!(
        problems,
        vec![
            "node `trace_sampler`: the candidate leaves out this host-owned node, but its host component keeps \
             producing records, which would have nowhere to go: removing a host component needs a restart, since the \
             physical structure is built from native configuration at boot"
                .to_string()
        ]
    );
}

#[test]
fn an_invalid_candidate_is_not_placed() {
    let plan = stage3_default_program();
    let frame = frame_of(&plan);
    let report = Validator::new(&OperatorRegistry::native(), &MemoryCatalog::new()).validate(&plan);
    assert!(!report.is_valid());
    assert!(matches!(
        place(&plan, &report, &OperatorRegistry::native(), &frame),
        Err(PlacementError::Invalid { .. })
    ));
}

// Programs.

fn compiled(plan: &Plan, frame: &Frame) -> CompiledPlan {
    let report = validated(plan, &default_program_catalog());
    compile(plan, &report, &OperatorRegistry::native(), frame).unwrap()
}

fn kernel_nodes(program: &RegionProgram) -> Vec<(String, &'static str, usize)> {
    program
        .kernels()
        .iter()
        .map(|kernel| (kernel.key.node.to_string(), kernel.operator, kernel.ports.len()))
        .collect()
}

fn decision_nodes(program: &RegionProgram) -> Vec<(String, Vec<String>, bool)> {
    program
        .decisions()
        .iter()
        .map(|decision| {
            let root = decision.root();
            (
                root.node.to_string(),
                root.ports.iter().map(ToString::to_string).collect(),
                matches!(root.kind, DecisionKind::Split { .. }),
            )
        })
        .collect()
}

fn reg(register: u16) -> RegisterId {
    RegisterId::new(register)
}

fn to(register: u16) -> Route {
    Route::Register(reg(register))
}

fn to_fixed(register: u16, exit: u16) -> Step {
    Step::Exit {
        register: reg(register),
        exit: ExitIndex::new(exit),
        entry: EntrySource::None,
    }
}

/// An exit to a fixed destination, an output without a binding, which reads no entry, as a fixed component does.
fn to_destination(register: u16, exit: u16) -> Step {
    Step::Exit {
        register: reg(register),
        exit: ExitIndex::new(exit),
        entry: EntrySource::None,
    }
}

fn map(kernel: u16, register: u16) -> Step {
    Step::Map {
        kernel: KernelIndex::new(kernel),
        register: reg(register),
    }
}

fn decide(decision: u16, input: u16, targets: Vec<Route>) -> Step {
    Step::Decide {
        decision: DecisionIndex::new(decision),
        input: reg(input),
        targets,
    }
}

fn to_next(register: u16, exit: u16) -> Step {
    to_entry(register, exit, 0)
}

fn to_entry(register: u16, exit: u16, entry: u16) -> Step {
    Step::Exit {
        register: reg(register),
        exit: ExitIndex::new(exit),
        entry: EntrySource::Region(EntryId::new(entry)),
    }
}

#[test]
fn the_pre_aggregation_programs_map_split_and_hand_each_batch_to_the_next_region() {
    let plan = stage3_default_program();
    let frame = frame_of(&plan);
    let compiled = compiled(&plan, &frame);
    let programs = &compiled.programs[..3];
    for (index, program) in programs.iter().enumerate() {
        assert_eq!(program.slot(), SlotId::new(index as u16));
    }

    let kernels = programs.iter().map(|program| kernel_nodes(program)).collect::<Vec<_>>();
    assert_eq!(
        kernels,
        vec![
            vec![("dsd_enrich".to_string(), "dogstatsd_mapper", 1)],
            vec![("dsd_prefix_filter".to_string(), "namespace_prefix", 1)],
            vec![
                ("dsd_no_agg_split".to_string(), "timestamp_split", 2),
                ("dsd_tag_filterlist".to_string(), "tag_projection", 1),
            ],
        ]
    );
    assert_eq!(
        decision_nodes(&programs[1]),
        vec![(
            "dsd_prefix_filter_blocklist".to_string(),
            vec!["true".to_string(), "false".to_string()],
            true
        )]
    );
    assert!(decision_nodes(&programs[0]).is_empty() && decision_nodes(&programs[2]).is_empty());

    let entries = programs
        .iter()
        .map(|program| {
            let [entry] = program.entries() else {
                panic!("one entry")
            };
            (entry.key, entry.input.to_string(), entry.steps.clone(), entry.registers)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        entries,
        vec![
            // The mapper, then on to the next region, as its entry 0.
            (
                EntryKey::Admission(point(0)),
                "dsd_enrich.in".to_string(),
                vec![map(0, 0), to_next(0, 0)],
                1
            ),
            // The listener filterlist drops in place: its `true` port leads to the `drop`.
            (
                EntryKey::Region(EntryId::new(0)),
                "dsd_prefix_filter.in".to_string(),
                vec![
                    map(0, 0),
                    decide(0, 0, vec![Route::Drop(id("dsd_prefix_filter_blocklist_true")), to(1)]),
                    to_next(1, 0),
                ],
                2
            ),
            // Passthrough points leave for the post-aggregation filter's region, carrying its entry for the merge's
            // second input; the rest for aggregation's isolation slot, as its entry 0.
            (
                EntryKey::Region(EntryId::new(0)),
                "dsd_no_agg_split.in".to_string(),
                vec![
                    Step::Expand {
                        kernel: KernelIndex::new(0),
                        input: reg(0),
                        ports: vec![to(1), to(2)],
                    },
                    to_entry(2, 1, 1),
                    map(1, 1),
                    to_next(1, 0),
                ],
                3
            ),
        ]
    );
    let registers = programs
        .iter()
        .map(|program| program.register_count())
        .collect::<Vec<_>>();
    assert_eq!(registers, vec![1, 2, 3]);
}

#[test]
fn aggregations_program_hands_each_batch_to_its_kernel_and_its_flushes_to_the_post_aggregation_filter() {
    let plan = stage3_default_program();
    let frame = frame_of(&plan);
    let compiled = compiled(&plan, &frame);
    let program = &compiled.programs[3];
    assert_eq!(program.slot(), SlotId::new(3));

    // One kernel, aggregation's, whose state is a lineage keyed by its window, with the default flush interval.
    assert_eq!(kernel_nodes(program), vec![("dsd_agg".to_string(), "aggregate", 1)]);
    let lineage = program.kernels()[0].lineage.as_ref().expect("a state owner's lineage");
    assert_eq!(lineage.key.node, id("dsd_agg"));
    assert_eq!(lineage.flush_interval, std::time::Duration::from_secs(15));
    assert!(decision_nodes(program).is_empty());

    // Batches from the pipeline's last region are a state step; what the kernel emits at its flush ticks enters the
    // post-aggregation filter's region as its entry 0.
    let entries = program
        .entries()
        .iter()
        .map(|entry| (entry.key, entry.input.to_string(), entry.steps.clone(), entry.registers))
        .collect::<Vec<_>>();
    assert_eq!(
        entries,
        vec![
            (
                EntryKey::Region(EntryId::new(0)),
                "dsd_agg.in".to_string(),
                vec![Step::State {
                    kernel: KernelIndex::new(0),
                    register: reg(0),
                    params: StateParamsIndex::new(0),
                }],
                1
            ),
            (
                EntryKey::StateOutput(KernelIndex::new(0)),
                "dsd_agg.out".to_string(),
                vec![to_next(0, 0)],
                1
            ),
        ]
    );

    // R5b pushes the post-aggregation filter into it: its `true` port drops into `dsd_post_agg_filter_true`, and it
    // judges every kind it emits but histograms' statistics.
    let [state] = program.state_params() else {
        panic!("one state parameter entry")
    };
    let site = state.pushed.as_ref().expect("the post-aggregation filter is pushed");
    assert_eq!(
        (&site.owner, &site.filter, site.drop_port.as_str(), &site.drop),
        (
            &id("dsd_agg"),
            &id("dsd_post_agg_filter"),
            "true",
            &id("dsd_post_agg_filter_true")
        )
    );
    assert!(site.drops_matching && !site.histograms);
    assert_eq!(compiled.pushed_filters, vec![site.clone()]);
}

#[test]
fn the_enrichment_program_has_one_entry_per_logical_input() {
    let plan = stage3_default_program();
    let frame = frame_of(&plan);
    let compiled = compiled(&plan, &frame);
    let filter = &compiled.programs[4];
    let program = &compiled.programs[5];

    // The post-aggregation filter's region: no kernel; the filter and the client telemetry tap's copy, joined by an
    // edge, are one decision, whose exits are the filter's `true` port and both ports of the copy. From aggregation's
    // isolation slot, and from the passthrough, the whole slot: the merge before the filter is no step, since one batch
    // only ever reaches one of its inputs.
    assert!(kernel_nodes(filter).is_empty());
    assert_eq!(
        decision_nodes(filter),
        vec![(
            "dsd_post_agg_filter".to_string(),
            vec!["true".to_string(), "false".to_string()],
            true
        )]
    );
    let decision = &filter.decisions()[0];
    assert_eq!(
        decision
            .nodes
            .iter()
            .map(|node| node.node.to_string())
            .collect::<Vec<_>>(),
        vec!["dsd_post_agg_filter", "dsd_post_agg_filter_fanout"]
    );
    assert_eq!(
        decision
            .exits
            .iter()
            .map(|exit| format!("{}.{}", exit.node, exit.port))
            .collect::<Vec<_>>(),
        vec![
            "dsd_post_agg_filter.true",
            "dsd_post_agg_filter_fanout.0",
            "dsd_post_agg_filter_fanout.1"
        ]
    );
    let filtered = vec![
        decide(0, 0, vec![Route::Drop(id("dsd_post_agg_filter_true")), to(1), to(2)]),
        to_next(2, 1),
        to_destination(1, 0), // dsd_client_telemetry
    ];
    let filter_entries = filter
        .entries()
        .iter()
        .map(|entry| (entry.key, entry.input.to_string(), entry.steps.clone(), entry.registers))
        .collect::<Vec<_>>();
    assert_eq!(
        filter_entries,
        vec![
            (
                EntryKey::Region(EntryId::new(0)),
                "dsd_post_agg_filter_in.0".to_string(),
                filtered.clone(),
                3
            ),
            (
                EntryKey::Region(EntryId::new(1)),
                "dsd_post_agg_filter_in.1".to_string(),
                filtered,
                3
            ),
        ]
    );

    assert_eq!(
        kernel_nodes(program),
        vec![
            ("metrics_enrich".to_string(), "host_enrichment", 1),
            ("metrics_enrich_host_tags".to_string(), "host_tags", 1),
        ]
    );
    let tee = |ports: usize| (0..ports).map(|port| port.to_string()).collect::<Vec<_>>();
    assert_eq!(
        decision_nodes(program),
        vec![("metrics_policy".to_string(), tee(5), false)]
    );

    // From a source, and from the filter's region: enrichment and the policy region.
    let enriched = vec![
        map(0, 0),
        map(1, 0),
        decide(0, 0, vec![to(1), to(2), to(3), to(4), to(5)]),
        to_fixed(2, 1),
        to_fixed(3, 2),
        to_fixed(4, 3),
        to_fixed(5, 4),
        to_destination(1, 0), // dd_metrics_encode
    ];
    let entries = program
        .entries()
        .iter()
        .map(|entry| (entry.key, entry.input.to_string(), entry.steps.clone(), entry.registers))
        .collect::<Vec<_>>();
    let mut expected = Vec::new();
    for (point_id, input) in [
        (1, "metrics_enrich_in.1"),
        (2, "metrics_enrich_in.2"),
        (3, "metrics_enrich_in.3"),
        (4, "metrics_enrich_in.4"),
    ] {
        expected.push((
            EntryKey::Admission(point(point_id)),
            input.to_string(),
            enriched.clone(),
            6,
        ));
    }
    expected.push((
        EntryKey::Region(EntryId::new(0)),
        "metrics_enrich_in.0".to_string(),
        enriched,
        6,
    ));
    assert_eq!(entries, expected);

    // Batches find their entry by the entry their header carries, or else by their admission point.
    let header = |entry: Option<u16>, point_id: Option<u32>| BatchHeader {
        entry: entry.map(EntryId::new),
        admission_point: point_id.map(point),
        ..BatchHeader::default()
    };
    let found = |header: BatchHeader| program.entry_for(&header).map(|entry| entry.input.to_string());
    assert_eq!(found(header(None, Some(1))), Some("metrics_enrich_in.1".to_string()));
    assert_eq!(found(header(Some(0), Some(0))), Some("metrics_enrich_in.0".to_string()));
    assert_eq!(found(header(Some(1), Some(0))), None);
    assert_eq!(found(header(None, Some(0))), None);
    assert_eq!(found(header(None, None)), None);
}

#[test]
fn a_split_rejoined_by_a_merge_concatenates_both_branches() {
    let plan = synthetic(
        &[
            ("otlp_in", "source", json!({"binding": "otlp"})),
            ("s", "split", json!({"predicate": {"kind_in": ["gauge"]}})),
            ("a", "host_enrichment", json!({})),
            ("b", "host_enrichment", json!({})),
            ("m", "merge", json!({"inputs": 2})),
            ("m_out", "emit", json!({"output": "out"})),
        ],
        &[
            ("otlp_in.metrics", "s.in"),
            ("s.true", "a.in"),
            ("s.false", "b.in"),
            ("a.out", "m.0"),
            ("b.out", "m.1"),
            ("m.out", "m_out.in"),
        ],
    );
    let registry = OperatorRegistry::native();
    let report = validated(&plan, &synthetic_catalog());
    let frame = Frame::compute(&plan, &registry, &first_node_namer).unwrap();
    let compiled = compile(&plan, &report, &registry, &frame).unwrap();
    let [program] = compiled.programs.as_slice() else {
        panic!("one program")
    };
    assert_eq!(
        program.entries()[0].steps,
        vec![
            decide(0, 0, vec![to(1), to(2)]),
            map(0, 1),
            map(1, 2),
            Step::Concat {
                inputs: vec![reg(1), reg(2)],
                output: reg(3),
            },
            to_destination(3, 0), // out
        ]
    );
}

#[test]
fn an_unresolved_predicate_binding_fails_compilation() {
    let plan = synthetic(
        &[
            ("otlp_in", "source", json!({"binding": "otlp"})),
            ("s", "split", json!({"predicate": {"flag": {"config": "test.flag"}}})),
            ("s_true", "emit", json!({"output": "out"})),
            ("s_false", "drop", json!({})),
        ],
        &[
            ("otlp_in.metrics", "s.in"),
            ("s.true", "s_true.in"),
            ("s.false", "s_false.in"),
        ],
    );
    let registry = OperatorRegistry::native();
    let catalog = synthetic_catalog().with_binding(
        "test.flag".parse().unwrap(),
        crate::ir::ParamType::Bool,
        crate::validate::Liveness::Live,
    );
    let report = validated(&plan, &catalog);
    let frame = Frame::compute(&plan, &registry, &first_node_namer).unwrap();
    let error = compile(&plan, &report, &registry, &frame).unwrap_err();
    assert!(
        matches!(&error, CompileError::UnresolvedBinding { node, .. } if node == &id("s")),
        "{}",
        error
    );
}

#[test]
fn fingerprints_and_kernel_keys_change_only_with_what_they_cover() {
    let boot = stage3_default_program();
    let frame = frame_of(&boot);
    let before = compiled(&boot, &frame);
    let again = compiled(&boot, &frame);
    for (before, again) in before.programs.iter().zip(&again.programs) {
        assert_eq!(
            before.fingerprint(),
            again.fingerprint(),
            "compilation is deterministic"
        );
    }

    // A live change of the listener filterlist changes only the program of the region running it, and no kernel.
    let filtered = edited(|value| {
        node_mut(value, "dsd_prefix_filter_blocklist")["params"]["predicate"]["intake_name"]["matches"]["values"]
            ["resolved"] = json!(["tenant.raw.blocked", "tenant.raw.other"]);
    });
    let after = compiled(&filtered, &frame);
    let changed_programs = |after: &CompiledPlan| {
        (0..5)
            .filter(|&slot| before.programs[slot].fingerprint() != after.programs[slot].fingerprint())
            .collect::<Vec<_>>()
    };
    assert_eq!(changed_programs(&after), vec![1]);
    let keys = |compiled: &CompiledPlan, slot: usize| {
        compiled.programs[slot]
            .kernels()
            .iter()
            .map(|kernel| kernel.key.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(&before, 1), keys(&after, 1));

    // A live change of the tag filterlist changes the tag projection kernel's key, and only that one.
    let projected = edited(|value| {
        node_mut(value, "dsd_tag_filterlist")["params"]["rules"]["resolved"] =
            json!([{"metric_name": "tenant.checkout.latency", "action": "exclude", "tags": ["user_id", "session"]}]);
    });
    let after = compiled(&projected, &frame);
    assert_eq!(changed_programs(&after), vec![2]);
    let (old, new) = (keys(&before, 2), keys(&after, 2));
    let changed = old
        .iter()
        .zip(&new)
        .filter(|(old, new)| old != new)
        .map(|(old, _)| old.node.to_string())
        .collect::<Vec<_>>();
    assert_eq!(changed, vec!["dsd_tag_filterlist".to_string()]);
    assert!(new.iter().all(|key| after.programs[2].kernel_by_key(key).is_some()));
}
