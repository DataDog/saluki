//! The plan-driven topology compared with the legacy one, structure only; behavior is in the installer's tests.

use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    sync::Arc,
};

use saluki_core::{
    accounting::{ComponentBounds, ComponentRegistry},
    components::ComponentType,
    data_model::event::{Event, EventType},
    execution::{AdmissionMode, ExitId},
    topology::{BlueprintSnapshot, DataType, EventsBuffer},
};
use saluki_metric_plan::{
    compile::{Frame, FrameSource, FrameTarget, RegionProgram},
    limits,
    operators::names,
};
use serde_json::Value;

use crate::{
    cli::run::{create_legacy_topology, create_topology},
    config::remote_agent_client_configuration,
    programmable_metrics::{
        lowering::matrix::{matrix_entry, MATRIX},
        outputs::NativeOutputs,
        test_support::{configuration_system, environment_provider, programmable_metrics, ConfigCell},
    },
};

/// The legacy transforms whose work the regions do, besides the `metrics_enrich` chain, whose ID the region keeps.
const FUSED: [&str; 5] = [
    "dsd_enrich",
    "dsd_prefix_filter",
    "dsd_no_agg_split",
    "dsd_tag_filterlist",
    "dsd_post_agg_filter",
];

/// The taps on the DogStatsD source's metrics, outside the plan.
const TAPS: [&str; 2] = ["dsd_stats_out", "dsd_debug_log_out"];

/// The blueprints of both topologies for one configuration, with the plan-driven one's frame and outputs.
struct Topologies {
    legacy: BlueprintSnapshot,
    plan: BlueprintSnapshot,
    frame: Frame,
    outputs: NativeOutputs,
    programs: Vec<Arc<RegionProgram>>,
    registry: ComponentRegistry,
    legacy_registry: ComponentRegistry,
}

async fn topologies(yaml: &str) -> Topologies {
    let system = configuration_system(yaml).await;
    let config = system.config();
    let env = environment_provider(&config).await;
    let client = (!config.control.standalone_mode).then(|| remote_agent_client_configuration(&config).unwrap());
    let legacy_registry = ComponentRegistry::default();
    let (legacy, _) = create_legacy_topology(&system, client.as_ref(), &env, &legacy_registry)
        .await
        .expect("the legacy topology builds");

    let registry = ComponentRegistry::default();
    let cell = ConfigCell::new(config.as_ref().clone());
    let mut metrics = programmable_metrics(&config, cell.live(), &registry).expect("a metrics pipeline is required");
    let (plan, _) = create_topology(&system, &env, &registry, Some(&mut metrics))
        .await
        .expect("the plan-driven topology builds");
    let boot = metrics.boot_topology();
    Topologies {
        legacy: legacy.snapshot().unwrap(),
        plan: plan.snapshot().unwrap(),
        frame: Frame::clone(&boot.frame),
        outputs: boot.outputs.clone(),
        programs: boot.programs.clone(),
        registry,
        legacy_registry,
    }
}

fn edges(snapshot: &BlueprintSnapshot) -> BTreeSet<(String, String)> {
    snapshot
        .edges()
        .iter()
        .map(|edge| (edge.from().to_string(), edge.to().to_string()))
        .collect()
}

fn pairs(pairs: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    pairs
        .iter()
        .map(|(from, to)| (from.to_string(), to.to_string()))
        .collect()
}

/// Returns the component a fixed node of the frame runs as, by the rule the module documents: an opaque host's
/// `component` parameter, or the node's own ID.
fn fixed_component(frame: &Frame, node: &saluki_metric_plan::ir::NodeId) -> String {
    let fixed = frame.fixed_component(node).unwrap();
    if fixed.operator.as_str() == names::OPAQUE_HOST {
        fixed
            .params
            .get("component")
            .and_then(Value::as_str)
            .unwrap()
            .to_string()
    } else {
        node.to_string()
    }
}

/// Returns the component output an admission point is: a source's named port, or a host's default output.
fn admission_output(frame: &Frame, id: saluki_core::execution::AdmissionPointId) -> String {
    let point = frame.admission_point(id).unwrap();
    let fixed = frame.fixed_component(&point.node).unwrap();
    let component = fixed_component(frame, &point.node);
    if fixed.operator.as_str() == names::SOURCE {
        format!("{}.{}", component, point.port)
    } else {
        component
    }
}

/// Returns the frame's physical edges as blueprint edges.
fn frame_edges(frame: &Frame, outputs: &NativeOutputs) -> BTreeSet<(String, String)> {
    frame
        .edges()
        .iter()
        .map(|edge| {
            let from = match edge.from {
                FrameSource::Admission(id) => admission_output(frame, id),
                FrameSource::Exit { slot, exit } => {
                    let slot = frame.slot(slot).unwrap();
                    format!("{}.{}", slot.name, slot.exits[exit.get()].name)
                }
            };
            let to = match &edge.to {
                FrameTarget::Fixed(node) => fixed_component(frame, node),
                FrameTarget::Slot(slot) => frame.slot(*slot).unwrap().name.to_string(),
                FrameTarget::Output(output) => outputs.get(output.as_str()).unwrap().component.clone(),
            };
            (from, to)
        })
        .collect()
}

#[tokio::test]
async fn the_plan_driven_topology_is_the_legacy_one_with_regions_in_place_of_the_fused_transforms() {
    for (name, yaml) in MATRIX {
        let topologies = topologies(yaml).await;
        let Topologies {
            legacy,
            plan,
            frame,
            outputs,
            ..
        } = &topologies;
        let regions = frame
            .slots()
            .iter()
            .map(|slot| slot.name.to_string())
            .collect::<BTreeSet<_>>();
        assert!(regions.contains("metrics_enrich"), "{}: regions {:?}", name, regions);

        // Components: the legacy ones without the fused transforms, and a region host per slot. Every component but
        // the regions is the legacy one: same type, input, and outputs.
        let legacy_ids = legacy
            .components()
            .iter()
            .map(|component| component.id().to_string())
            .filter(|id| !FUSED.contains(&id.as_str()))
            .collect::<BTreeSet<_>>();
        let plan_ids = plan
            .components()
            .iter()
            .map(|component| component.id().to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            plan_ids,
            legacy_ids.union(&regions).cloned().collect::<BTreeSet<_>>(),
            "{}: components",
            name
        );
        for component in plan.components() {
            let id = component.id().to_string();
            if regions.contains(&id) {
                continue;
            }
            let native = legacy.component(&id).unwrap();
            assert_eq!(
                component.component_type(),
                native.component_type(),
                "{}: `{}`",
                name,
                id
            );
            assert_eq!(component.input(), native.input(), "{}: `{}`", name, id);
            assert_eq!(component.outputs(), native.outputs(), "{}: `{}`", name, id);
        }

        // A region host is a metrics transform whose outputs are its slot's exits (a snapshot lists them sorted).
        for slot in frame.slots() {
            let component = plan.component(slot.name.as_str()).unwrap();
            assert_eq!(component.component_type(), ComponentType::Transform, "{}", name);
            assert_eq!(component.input(), Some(DataType::Event(EventType::Metric)), "{}", name);
            let exits = component
                .outputs()
                .iter()
                .map(|output| (output.id().to_string(), output.data_type()))
                .collect::<BTreeSet<_>>();
            let expected = slot
                .exits
                .iter()
                .map(|exit| {
                    (
                        format!("{}.{}", slot.name, exit.name),
                        DataType::Event(EventType::Metric),
                    )
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(exits.len(), slot.exits.len(), "{}: exits of `{}`", name, slot.name);
            assert_eq!(exits, expected, "{}: exits of `{}`", name, slot.name);
        }

        // Edges: the legacy ones, without those leaving or entering a fused transform or metrics enrichment and those
        // leaving an output the frame reads, but with the DogStatsD taps; and the frame's.
        let read = frame
            .admission_points()
            .iter()
            .map(|point| admission_output(frame, point.id))
            .collect::<BTreeSet<_>>();
        let replaced = |component: &str| FUSED.contains(&component) || component == "metrics_enrich";
        let mut expected = edges(legacy)
            .into_iter()
            .filter(|(from, to)| {
                let source = from.split('.').next().unwrap();
                !replaced(source) && !replaced(to) && (!read.contains(from) || TAPS.contains(&to.as_str()))
            })
            .collect::<BTreeSet<_>>();
        expected.extend(frame_edges(frame, outputs));
        assert_eq!(edges(plan), expected, "{}: edges", name);

        // Every output of a fixed component is an admission point: sources admit, hosts readmit, and none sends
        // heartbeats yet.
        let mut points = plan
            .admission_points()
            .iter()
            .map(|point| (point.id(), point.output().to_string(), point.mode(), point.heartbeat()))
            .collect::<Vec<_>>();
        points.sort_by_key(|(id, ..)| *id);
        let expected = frame
            .admission_points()
            .iter()
            .map(|point| {
                let source = frame.fixed_component(&point.node).unwrap().operator.as_str() == names::SOURCE;
                let mode = if source {
                    AdmissionMode::Admit
                } else {
                    AdmissionMode::Readmit
                };
                (point.id, admission_output(frame, point.id), mode, None)
            })
            .collect::<Vec<_>>();
        assert_eq!(points, expected, "{}: admission points", name);
        assert!(legacy.admission_points().is_empty(), "{}", name);
    }
}

/// The edges of the metrics path, written by hand from the default program and the frame's rules, so that the
/// comparison above cannot pass by deriving both sides from the same mistake.
#[tokio::test]
async fn the_metrics_path_has_exactly_the_edges_of_its_frame() {
    // DogStatsD alone, with the no-aggregation pipeline: two regions, and an idle exit to each output from the
    // pre-aggregation region.
    let Topologies { legacy, plan, .. } = topologies("").await;
    let native = pairs(&[
        ("dsd_agg", "dsd_post_agg_filter"),
        ("dsd_enrich", "dsd_prefix_filter"),
        ("dsd_in.metrics", "dsd_enrich"),
        ("dsd_no_agg_split", "dsd_tag_filterlist"),
        ("dsd_no_agg_split.passthrough", "dsd_post_agg_filter"),
        ("dsd_post_agg_filter", "dsd_client_telemetry_out"),
        ("dsd_post_agg_filter", "metrics_enrich"),
        ("dsd_prefix_filter", "dsd_no_agg_split"),
        ("dsd_tag_filterlist", "dsd_agg"),
        ("metrics_enrich", "dd_metrics_encode"),
    ]);
    let path = pairs(&[
        ("dsd_in.metrics", "dsd_pre_aggregation"),
        ("dsd_agg", "metrics_enrich"),
        ("dsd_pre_aggregation.dsd_agg", "dsd_agg"),
        ("dsd_pre_aggregation.metrics_enrich", "metrics_enrich"),
        ("dsd_pre_aggregation.dsd_client_telemetry", "dsd_client_telemetry_out"),
        ("dsd_pre_aggregation.dd_metrics_encode", "dd_metrics_encode"),
        ("metrics_enrich.dsd_client_telemetry", "dsd_client_telemetry_out"),
        ("metrics_enrich.dd_metrics_encode", "dd_metrics_encode"),
    ]);
    let legacy = edges(&legacy);
    assert!(native.is_subset(&legacy));
    let expected = legacy.difference(&native).cloned().chain(path).collect::<BTreeSet<_>>();
    assert_eq!(edges(&plan), expected);

    // Every branch: five sources, two routing groups, and both failover gateways, whose outputs connect straight to
    // their encoders. Its mapper profile and namespace prefix make the pre-aggregation chain costly enough that R1's
    // cost model pipelines it over three regions (the mapper, the prefix with the listener filterlist, and the
    // timestamp split with the tag filterlist), and its host tags and five-way fan-out keep enrichment from the
    // post-aggregation filter.
    let Topologies { plan, .. } = topologies(matrix_entry("every branch")).await;
    const OUTPUTS: [(&str, &str); 6] = [
        ("dsd_client_telemetry", "dsd_client_telemetry_out"),
        ("dd_metrics_encode", "dd_metrics_encode"),
        ("metrics_routing_encode_0", "metrics_routing_encode_0"),
        ("metrics_routing_encode_1", "metrics_routing_encode_1"),
        ("mrf_metrics_encode", "mrf_metrics_encode"),
        ("af_metrics_encode", "af_metrics_encode"),
    ];
    // Each region's exits that lead somewhere, then the outputs it has an idle exit to.
    type Region<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [&'a str]);
    let regions: [Region; 5] = [
        (
            "dsd_pre_aggregation",
            &[("dsd_prefix_filter_region", "dsd_prefix_filter_region")],
            &OUTPUTS.map(|(output, _)| output),
        ),
        (
            "dsd_prefix_filter_region",
            &[("dsd_no_agg_split_region", "dsd_no_agg_split_region")],
            &OUTPUTS.map(|(output, _)| output),
        ),
        (
            "dsd_no_agg_split_region",
            &[
                ("dsd_agg", "dsd_agg"),
                ("dsd_post_agg_filter_region", "dsd_post_agg_filter_region"),
            ],
            &OUTPUTS.map(|(output, _)| output),
        ),
        (
            "dsd_post_agg_filter_region",
            &[
                ("dsd_client_telemetry", "dsd_client_telemetry_out"),
                ("metrics_enrich", "metrics_enrich"),
            ],
            &[
                "dd_metrics_encode",
                "metrics_routing_encode_0",
                "metrics_routing_encode_1",
                "mrf_metrics_encode",
                "af_metrics_encode",
            ],
        ),
        (
            "metrics_enrich",
            &[
                ("dd_metrics_encode", "dd_metrics_encode"),
                ("metrics_routing_filter_0", "metrics_routing_filter_0"),
                ("metrics_routing_filter_1", "metrics_routing_filter_1"),
                ("mrf_metrics_gateway", "mrf_metrics_gateway"),
                ("af_metrics_gateway", "af_metrics_gateway"),
            ],
            &[
                "metrics_routing_encode_0",
                "metrics_routing_encode_1",
                "mrf_metrics_encode",
                "af_metrics_encode",
            ],
        ),
    ];
    let mut path = pairs(&[
        // Admission points.
        ("dsd_in.metrics", "dsd_pre_aggregation"),
        ("dsd_agg", "dsd_post_agg_filter_region"),
        ("otlp_in.metrics", "metrics_enrich"),
        ("checks_ipc_in.metrics", "metrics_enrich"),
        ("liveness_in.metrics", "metrics_enrich"),
        ("trace_sampler.metrics", "metrics_enrich"),
        ("metrics_routing_filter_0", "metrics_routing_encode_0"),
        ("metrics_routing_filter_1", "metrics_routing_encode_1"),
        ("mrf_metrics_gateway", "mrf_metrics_encode"),
        ("af_metrics_gateway", "af_metrics_encode"),
        // Encoders feed their forwarders, as natively.
        ("dd_metrics_encode", "dd_out"),
        ("metrics_routing_encode_0", "dd_out"),
        ("metrics_routing_encode_1", "dd_out"),
        ("mrf_metrics_encode", "mrf_dd_out"),
        ("af_metrics_encode", "cluster_agent_out"),
    ]);
    for (region, used, idle) in regions {
        for (exit, target) in used {
            path.insert((format!("{}.{}", region, exit), target.to_string()));
        }
        for output in idle {
            let component = OUTPUTS.iter().find(|(id, _)| id == output).unwrap().1;
            path.insert((format!("{}.{}", region, output), component.to_string()));
        }
    }
    let touching = |component: &str| {
        regions.iter().any(|(region, _, _)| *region == component)
            || [
                "dsd_agg",
                "metrics_routing_filter_0",
                "metrics_routing_filter_1",
                "mrf_metrics_gateway",
                "af_metrics_gateway",
                "dsd_client_telemetry_out",
                "dd_metrics_encode",
                "metrics_routing_encode_0",
                "metrics_routing_encode_1",
                "mrf_metrics_encode",
                "af_metrics_encode",
            ]
            .contains(&component)
    };
    let found = edges(&plan)
        .into_iter()
        .filter(|(from, to)| touching(from.split('.').next().unwrap()) || touching(to))
        .collect::<BTreeSet<_>>();
    assert_eq!(found, path);
}

/// Returns the bounds of the component at `path` below the registry's root.
fn bounds_at<'a>(bounds: &'a ComponentBounds, path: &[&str]) -> &'a ComponentBounds {
    path.iter().fold(bounds, |bounds, segment| {
        bounds
            .subcomponents()
            .into_iter()
            .find(|(name, _)| name.as_str() == *segment)
            .map(|(_, bounds)| bounds)
            .unwrap_or_else(|| panic!("no `{}` below {:?}", segment, path))
    })
}

/// Returns the bytes `bounds` declares for the component itself, without its subcomponents: its minimum, and its firm
/// limit besides the minimum.
fn own_bytes(bounds: &ComponentBounds) -> (usize, usize) {
    let (minimum, firm) = bounds
        .subcomponents()
        .into_iter()
        .fold((0, 0), |(minimum, firm), (_, subcomponent)| {
            (
                minimum + subcomponent.total_minimum_required_bytes(),
                firm + subcomponent.total_firm_limit_bytes(),
            )
        });
    let minimum = bounds.total_minimum_required_bytes() - minimum;
    (minimum, bounds.total_firm_limit_bytes() - firm - minimum)
}

/// Returns every bound expression `bounds` declares at or below the component, as `min` or `firm` with the expression,
/// sorted, leaving out the size of each component's own struct.
fn declared_without_structs(bounds: &ComponentBounds) -> Vec<String> {
    let mut declared = bounds
        .to_exprs()
        .into_iter()
        .filter(|entry| entry["expr"]["name"] != "component struct")
        .map(|entry| {
            let name = entry["name"].as_str().unwrap();
            format!("{} {}", name.rsplit('.').next().unwrap(), entry["expr"])
        })
        .collect::<Vec<_>>();
    declared.sort();
    declared
}

/// Returns every bound expression of the tree, by the path of the component that declares it, sorted, leaving out the
/// components below `transforms` named in `except` and the interconnects.
fn declared_by_path(bounds: &ComponentBounds, except: &[&str]) -> BTreeMap<String, Vec<String>> {
    let mut declared = BTreeMap::<String, Vec<String>>::new();
    for entry in bounds.to_exprs() {
        let name = entry["name"].as_str().unwrap();
        let skipped = name.starts_with("root.topology.primary.interconnects.")
            || except
                .iter()
                .any(|component| name.starts_with(&format!("root.topology.primary.transforms.{}.", component)));
        if !skipped {
            declared
                .entry(name.to_string())
                .or_default()
                .push(entry["expr"].to_string());
        }
    }
    for expressions in declared.values_mut() {
        expressions.sort();
    }
    declared
}

#[tokio::test]
async fn region_bounds_are_their_kernels_native_bounds_and_their_buffers_and_the_rest_is_the_legacy_topologys() {
    let topologies = topologies("").await;
    let plan = topologies.registry.as_bounds();
    let legacy = topologies.legacy_registry.as_bounds();
    let transforms = ["topology", "primary", "transforms"];
    let at = |bounds, component: &str| bounds_at(bounds, &[&transforms[..], &[component]].concat());

    // A region host holds one full buffer per exit while a send waits, and its driver one full batch per register of
    // its program between steps.
    let exit_buffer = size_of::<(ExitId, EventsBuffer)>() + size_of::<Event>() * 1024;
    let register_buffer = size_of::<EventsBuffer>() + size_of::<Event>() * limits::values::BATCH_SIZE;
    // Each region, with its exits (where it sends, then the outputs it keeps an idle exit to for later generations),
    // the registers of the largest entry of its boot program, and the native transform each of its kernels is.
    type Region<'a> = (&'a str, &'a [&'a str], usize, &'a [&'a str]);
    let regions: [Region; 2] = [
        (
            "dsd_pre_aggregation",
            &["dsd_agg", "metrics_enrich", "dsd_client_telemetry", "dd_metrics_encode"],
            4,
            &[
                "dsd_enrich",
                "dsd_prefix_filter",
                "dsd_no_agg_split",
                "dsd_tag_filterlist",
            ],
        ),
        (
            "metrics_enrich",
            &["dsd_client_telemetry", "dd_metrics_encode"],
            5,
            &["metrics_enrich"],
        ),
    ];
    for (index, (region, exits, registers, kernels)) in regions.into_iter().enumerate() {
        let slot = &topologies.frame.slots()[index];
        assert_eq!(slot.name.as_str(), region);
        let slot_exits = slot.exits.iter().map(|exit| exit.name.as_str()).collect::<Vec<_>>();
        assert_eq!(slot_exits, exits, "`{}`", region);
        assert_eq!(topologies.programs[index].register_count(), registers, "`{}`", region);

        let region_bounds = at(&plan, region);
        let (minimum, firm) = own_bytes(region_bounds);
        // The minimum is the host itself, with the driver it holds.
        assert!(minimum > 0, "`{}`", region);
        assert_eq!(
            firm,
            exits.len() * exit_buffer + registers * register_buffer,
            "`{}`",
            region
        );

        // Each kernel declares what its native transform does, but for the transform's own struct.
        let mut nodes = region_bounds
            .subcomponents()
            .into_iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        nodes.sort();
        let mut expected = kernels.to_vec();
        expected.sort();
        assert_eq!(nodes, expected, "`{}`", region);
        for kernel in kernels {
            let kernel_bounds = bounds_at(region_bounds, &[kernel]);
            assert_eq!(
                declared_without_structs(kernel_bounds),
                declared_without_structs(at(&legacy, kernel)),
                "`{}` in `{}`",
                kernel,
                region
            );
        }
    }
    // The tag filterlist's context cache is bounded as natively: its capacity in entries.
    assert_eq!(
        at(&plan, "dsd_pre_aggregation")
            .subcomponents()
            .into_iter()
            .find(|(name, _)| name.as_str() == "dsd_tag_filterlist")
            .unwrap()
            .1
            .total_firm_limit_bytes(),
        100_000 * saluki_metric_kernels::tag_projection::CONTEXT_CACHE_ENTRY_BYTES
    );
    // The post-aggregation filter, which runs as a decision of the `metrics_enrich` region, declares nothing natively
    // but its struct.
    assert!(declared_without_structs(at(&legacy, "dsd_post_agg_filter")).is_empty());

    // Every other component declares what it does natively.
    let replaced = [&FUSED[..], &["metrics_enrich", "dsd_pre_aggregation"]].concat();
    assert_eq!(declared_by_path(&plan, &replaced), declared_by_path(&legacy, &replaced));

    // The interconnects: one of the default capacity, 128 buffers, into each transform and destination, and the
    // in-flight buffers of every source and transform besides. The legacy topology has nine transforms and three
    // destinations; the regions replace six of its transforms with two, so the plan-driven one has four interconnects,
    // and four transforms' in-flight buffers, fewer.
    let in_flight_buffer = size_of::<EventsBuffer>() + size_of::<Event>() * 1024;
    for (bounds, transforms, destinations) in [(&legacy, 9, 3), (&plan, 5, 3)] {
        let interconnects = transforms + destinations;
        let (minimum, firm) = own_bytes(bounds_at(bounds, &["topology", "primary", "interconnects"]));
        assert_eq!(minimum, interconnects * 128 * size_of::<EventsBuffer>());
        assert_eq!(firm, (interconnects * 128 + 1 + transforms) * in_flight_buffer);
    }
}
