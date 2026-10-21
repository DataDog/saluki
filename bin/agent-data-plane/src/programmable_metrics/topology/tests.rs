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
    compile::{Frame, FrameSource, FrameTarget, RegionProgram, SharedPredicate},
    limits,
    operators::names,
};

use crate::{
    cli::run::{create_legacy_topology, create_topology},
    config::remote_agent_client_configuration,
    programmable_metrics::{
        lowering::matrix::{matrix_entry, MATRIX},
        resources::NativeOutputs,
        test_support::{configuration_system, environment_provider, programmable_metrics, ConfigCell},
        topology::is_state_slot,
    },
};

/// The legacy DogStatsD transforms whose work the regions do.
const FUSED: [&str; 5] = [
    "dsd_enrich",
    "dsd_prefix_filter",
    "dsd_no_agg_split",
    "dsd_tag_filterlist",
    "dsd_post_agg_filter",
];

/// Returns `true` if the legacy component `id` is one whose work the regions do, besides aggregation and the
/// `metrics_enrich` chain, whose IDs a region keeps on lanes and without DogStatsD: a DogStatsD transform, or a gateway
/// in front of a metrics output (an endpoint routing filter or a failover gateway), which the policy region runs as a
/// `split`.
fn fused(id: &str) -> bool {
    let routing_filter = id
        .strip_prefix("metrics_routing_filter_")
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()));
    FUSED.contains(&id) || routing_filter || id == "mrf_metrics_gateway" || id == "af_metrics_gateway"
}

/// Returns `true` if the legacy component `id` is the encoder of an endpoint routing group, whose stream the
/// plan-driven topology runs as a lease of `dd_metrics_encode`.
fn routing_encoder(id: &str) -> bool {
    id.strip_prefix("metrics_routing_encode_")
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
}

/// The legacy components whose work a region does, and whose ID a region keeps on lanes and without DogStatsD.
const KEPT_IDS: [&str; 2] = ["dsd_agg", "metrics_enrich"];

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

/// Returns the component a fixed node of the frame runs as, by the rule the module documents: the node's own ID.
fn fixed_component(frame: &Frame, node: &saluki_metric_plan::ir::NodeId) -> String {
    assert!(frame.fixed_component(node).is_some(), "`{}` is fixed", node);
    node.to_string()
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
                FrameTarget::Destination(output) => outputs.get(output.as_str()).unwrap().component.clone(),
                FrameTarget::EncodeHost(host) => frame.encode_host(host).unwrap().encoder.to_string(),
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
        // Without lanes, the metrics path is one region: `dsd_pre_aggregation` with DogStatsD, `metrics_enrich`
        // without.
        assert_eq!(regions.len(), 1, "{}: regions {:?}", name, regions);
        assert!(
            regions.contains("dsd_pre_aggregation") || regions.contains("metrics_enrich"),
            "{}: regions {:?}",
            name,
            regions
        );

        // Components: the legacy ones without the fused transforms, aggregation and enrichment, the gateways, and the
        // routing groups' encoders,
        // whose streams are leases of `dd_metrics_encode`; a region host per slot; and the encode host and delivery
        // host of every fixed host, which run whether or not an output is bound to them (plan 4.15). Every component
        // but the regions and the hosts the legacy topology does not build is the legacy one: same type, input, and
        // outputs.
        let legacy_ids = legacy
            .components()
            .iter()
            .map(|component| component.id().to_string())
            .filter(|id| !fused(id) && !routing_encoder(id) && !KEPT_IDS.contains(&id.as_str()))
            .collect::<BTreeSet<_>>();
        let hosts = frame
            .hosts()
            .iter()
            .flat_map(|host| [host.host.to_string(), host.encoder.to_string()])
            .collect::<BTreeSet<_>>();
        assert!(
            hosts.contains("dd_out") && hosts.contains("mrf_dd_out"),
            "{}: {:?}",
            name,
            hosts
        );
        let plan_ids = plan
            .components()
            .iter()
            .map(|component| component.id().to_string())
            .collect::<BTreeSet<_>>();
        let mut expected = legacy_ids.union(&regions).cloned().collect::<BTreeSet<_>>();
        expected.extend(hosts.iter().cloned());
        assert_eq!(plan_ids, expected, "{}: components", name);
        for component in plan.components() {
            let id = component.id().to_string();
            if regions.contains(&id) {
                continue;
            }
            let Some(native) = legacy.component(&id) else {
                assert!(hosts.contains(&id), "{}: `{}`", name, id);
                continue;
            };
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

        // Edges: the legacy ones, without those leaving or entering a fused transform, a gateway, a routing group's
        // encoder, metrics enrichment, or aggregation, and those leaving an output the frame reads, but with the
        // DogStatsD taps; the frame's; and each encode host's to its delivery host.
        let read = frame
            .admission_points()
            .iter()
            .map(|point| admission_output(frame, point.id))
            .collect::<BTreeSet<_>>();
        let replaced =
            |component: &str| fused(component) || routing_encoder(component) || KEPT_IDS.contains(&component);
        let mut expected = edges(legacy)
            .into_iter()
            .filter(|(from, to)| {
                let source = from.split('.').next().unwrap();
                !replaced(source) && !replaced(to) && (!read.contains(from) || TAPS.contains(&to.as_str()))
            })
            .collect::<BTreeSet<_>>();
        expected.extend(frame_edges(frame, outputs));
        expected.extend(
            frame
                .hosts()
                .iter()
                .map(|host| (host.encoder.to_string(), host.host.to_string())),
        );
        assert_eq!(edges(plan), expected, "{}: edges", name);

        // Every output of a fixed component is an admission point: sources admit, hosts readmit, and every point the
        // frame routes heartbeats, every second, whichever state owners a generation places downstream of it.
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
                let heartbeat = point.route.is_some().then_some(limits::values::HEARTBEAT_PERIOD);
                (point.id, admission_output(frame, point.id), mode, heartbeat)
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
    // DogStatsD alone, with the no-aggregation pipeline: one region, from the mapper through aggregation and
    // enrichment, with an exit to each output, the failover encode host's idle, as it runs idle.
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
        ("dsd_pre_aggregation.dsd_client_telemetry", "dsd_client_telemetry_out"),
        ("dsd_pre_aggregation.dd_metrics_encode", "dd_metrics_encode"),
        ("dsd_pre_aggregation.mrf_metrics_encode", "mrf_metrics_encode"),
        ("mrf_metrics_encode", "mrf_dd_out"),
    ]);
    let legacy = edges(&legacy);
    assert!(native.is_subset(&legacy));
    let expected = legacy.difference(&native).cloned().chain(path).collect::<BTreeSet<_>>();
    assert_eq!(edges(&plan), expected);

    // With Multi-Region Failover: its gateway is a `split` of the policy region, inside the one region, which feeds
    // the encoder directly, where the legacy metrics enrichment fed the gateway, and the gateway the encoder.
    let Topologies { legacy, plan, .. } = topologies(matrix_entry("multi-region failover")).await;
    let gateway = pairs(&[
        ("metrics_enrich", "mrf_metrics_gateway"),
        ("mrf_metrics_gateway", "mrf_metrics_encode"),
    ]);
    let path = pairs(&[
        ("dsd_in.metrics", "dsd_pre_aggregation"),
        ("dsd_pre_aggregation.dsd_client_telemetry", "dsd_client_telemetry_out"),
        ("dsd_pre_aggregation.dd_metrics_encode", "dd_metrics_encode"),
        ("dsd_pre_aggregation.mrf_metrics_encode", "mrf_metrics_encode"),
    ]);
    let legacy = edges(&legacy);
    assert!(native.is_subset(&legacy) && gateway.is_subset(&legacy));
    let expected = legacy
        .difference(&native)
        .cloned()
        .collect::<BTreeSet<_>>()
        .difference(&gateway)
        .cloned()
        .chain(path)
        .collect::<BTreeSet<_>>();
    assert_eq!(edges(&plan), expected);
    assert!(plan.component("mrf_metrics_gateway").is_none());

    // Every branch: five sources, two routing groups, and both failover gateways, which are `split`s of the policy
    // region, so no component stands between the region and an encode host, and the routing groups' streams are
    // leases of `dd_metrics_encode`, which every exit toward them leads to. Every source feeds the one region, which
    // feeds the client telemetry tap and every encoder directly.
    let Topologies { plan, .. } = topologies(matrix_entry("every branch")).await;
    const OUTPUTS: [(&str, &str); 4] = [
        ("dsd_client_telemetry", "dsd_client_telemetry_out"),
        ("dd_metrics_encode", "dd_metrics_encode"),
        ("mrf_metrics_encode", "mrf_metrics_encode"),
        ("af_metrics_encode", "af_metrics_encode"),
    ];
    let mut path = pairs(&[
        // Admission points: the sources, and nothing else.
        ("dsd_in.metrics", "dsd_pre_aggregation"),
        ("otlp_in.metrics", "dsd_pre_aggregation"),
        ("checks_ipc_in.metrics", "dsd_pre_aggregation"),
        ("liveness_in.metrics", "dsd_pre_aggregation"),
        ("trace_sampler.metrics", "dsd_pre_aggregation"),
        // Encode hosts feed their delivery hosts, as the native encoders fed their forwarders.
        ("dd_metrics_encode", "dd_out"),
        ("mrf_metrics_encode", "mrf_dd_out"),
        ("af_metrics_encode", "cluster_agent_out"),
    ]);
    for (exit, target) in OUTPUTS {
        path.insert((format!("dsd_pre_aggregation.{}", exit), target.to_string()));
    }
    let touching = |component: &str| {
        component == "dsd_pre_aggregation"
            || fused(component)
            || KEPT_IDS.contains(&component)
            || OUTPUTS.iter().any(|(_, target)| *target == component)
    };
    let found = edges(&plan)
        .into_iter()
        .filter(|(from, to)| touching(from.split('.').next().unwrap()) || touching(to))
        .collect::<BTreeSet<_>>();
    assert_eq!(found, path);
    assert!(plan.components().iter().all(|component| {
        let id = component.id().to_string();
        !fused(&id) && !routing_encoder(&id) && !KEPT_IDS.contains(&id.as_str())
    }));
}

/// One configuration's regions, each with the most native transforms that one path through it runs through.
type RegionChains<'a> = (&'a str, &'a [(&'a str, usize)]);

#[tokio::test]
async fn each_region_takes_what_the_native_transforms_it_replaces_held_on_the_way_through_it() {
    // Natively, each transform has an input of 128 buffers, and holds one more while it waits to send. A region that
    // replaces `n` transforms on one path holds, queued and in flight, as many buffers as they did: its input holds
    // `n * 129 - 1`, and it holds one while it waits. Where its paths run through different numbers of transforms, the
    // largest counts, so that no path holds less than natively.
    let cases: [RegionChains; 9] = [
        // The mapper, the prefix filter (with the listener filterlist), the no-aggregation split, and the tag
        // filterlist; aggregation; then the post-aggregation filter and enrichment: seven transforms, 7 * 129 - 1 =
        // 902 buffers, where the three regions before state steps fused held 515 + 128 + 257 = 900. The timestamped
        // metrics the split sends past aggregation run through six of them.
        ("dogstatsd with defaults", &[("dsd_pre_aggregation", 7)]),
        ("no-aggregation pipeline off", &[("dsd_pre_aggregation", 6)]),
        // Enrichment alone: the region holds what the native `metrics_enrich` did, and its fan-out to the main output
        // is lowered from the encoder, whose interconnect remains.
        ("otlp without dogstatsd", &[("metrics_enrich", 1)]),
        ("connected with host tags", &[("dsd_pre_aggregation", 7)]),
        // The post-aggregation filter, enrichment, then the routing filter on the way to its encoder.
        ("one routing group", &[("dsd_pre_aggregation", 8)]),
        // The two filters are parallel: one path runs through enrichment and one of them.
        (
            "two routing groups, a shared group, and an endpoint with empty lists",
            &[("dsd_pre_aggregation", 8)],
        ),
        // The post-aggregation filter, enrichment, and the failover gateway.
        ("multi-region failover", &[("dsd_pre_aggregation", 8)]),
        (
            "mapper profiles, a namespace prefix, and filterlists",
            &[("dsd_pre_aggregation", 7)],
        ),
        ("every branch", &[("dsd_pre_aggregation", 8)]),
    ];
    for (name, chains) in cases {
        let Topologies { plan, frame, .. } = topologies(matrix_entry(name)).await;
        let regions = frame.slots().iter().map(|slot| slot.name.as_str()).collect::<Vec<_>>();
        assert_eq!(
            regions,
            chains.iter().map(|(region, _)| *region).collect::<Vec<_>>(),
            "{}",
            name
        );
        for (region, transforms) in chains {
            let capacity = plan
                .component(region)
                .unwrap()
                .input_capacity()
                .map(|capacity| capacity.get());
            assert_eq!(capacity, Some(transforms * 129 - 1), "{}: `{}`", name, region);
        }
        // Every other component keeps the default capacity.
        for component in plan.components() {
            if component.input().is_some() && !regions.contains(&&**component.id()) {
                assert_eq!(
                    component.input_capacity().map(|capacity| capacity.get()),
                    Some(128),
                    "{}: `{}`",
                    name,
                    component.id()
                );
            }
        }
    }
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

/// Returns the bytes `bounds` declares at or below the component besides struct sizes: what grows with the lists it
/// holds.
fn list_bytes(bounds: &ComponentBounds) -> usize {
    bounds
        .to_exprs()
        .iter()
        .filter(|entry| entry["expr"]["type"] == "Constant")
        .map(|entry| entry["expr"]["value"].as_u64().unwrap() as usize)
        .sum()
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

/// Returns the bounds at `path` below `bounds`, if there are any.
fn bounds_below<'a>(bounds: &'a ComponentBounds, path: &[&str]) -> Option<&'a ComponentBounds> {
    path.iter().try_fold(bounds, |bounds, segment| {
        bounds
            .subcomponents()
            .into_iter()
            .find(|(name, _)| name.as_str() == *segment)
            .map(|(_, bounds)| bounds)
    })
}

/// Returns `declared` without the fixed hosts of `frame`, which
/// [`assert_hosts_declare_what_native_encoders_and_forwarders_do`] compares on their own.
fn without_hosts(mut declared: BTreeMap<String, Vec<String>>, frame: &Frame) -> BTreeMap<String, Vec<String>> {
    let prefixes = frame
        .hosts()
        .iter()
        .flat_map(|host| {
            [
                format!("root.topology.primary.encoders.{}.", host.encoder),
                format!("root.topology.primary.forwarders.{}.", host.host),
            ]
        })
        .collect::<Vec<_>>();
    declared.retain(|path, _| !prefixes.iter().any(|prefix| path.starts_with(prefix)));
    declared
}

/// Returns every term `bounds` declares at or below the component, as `min` or `firm` with the term, sorted, without
/// the names of its parts and leaving out the size of each component's own struct: what each term counts.
fn unnamed_terms(bounds: &ComponentBounds) -> Vec<String> {
    fn strip(value: &mut serde_json::Value) {
        if let Some(object) = value.as_object_mut() {
            object.remove("name");
            object.values_mut().for_each(strip);
        } else if let Some(array) = value.as_array_mut() {
            array.iter_mut().for_each(strip);
        }
    }
    let mut terms = bounds
        .to_exprs()
        .into_iter()
        .filter(|entry| entry["expr"]["name"] != "component struct")
        .map(|mut entry| {
            let kind = entry["name"].as_str().unwrap().rsplit('.').next().unwrap().to_string();
            strip(&mut entry["expr"]);
            format!("{} {}", kind, entry["expr"])
        })
        .collect::<Vec<_>>();
    terms.sort();
    terms
}

/// Returns the bytes of the minimum terms `bounds` declares at or below the component for a channel whose name has
/// `channel` in it.
fn channel_bytes(bounds: &ComponentBounds, channel: &str) -> u64 {
    bounds
        .to_exprs()
        .iter()
        .filter(|entry| entry["name"].as_str().unwrap().ends_with(".min"))
        .filter_map(|entry| entry["expr"]["values"].as_array())
        .filter(|values| values[0]["name"].as_str().is_some_and(|name| name.contains(channel)))
        .map(|values| {
            values
                .iter()
                .map(|value| value["value"].as_u64().unwrap())
                .product::<u64>()
        })
        .sum()
}

/// Checks the bounds of the fixed hosts of `frame` against the native encoders and forwarders at their IDs.
///
/// Each encode host declares what the native encoders whose streams are its leases declared together (the one at its
/// ID, and for `dd_metrics_encode` the routing groups' encoders), with three differences (plan 4.6): the names of their
/// channels and buffers, which its leases' builders hold where each native request builder did; the size of its own
/// struct; and one payloads channel, which every lease's builder sends on, where each native encoder had one. Each
/// delivery host declares what the native forwarder does. A host the legacy topology does not build, which runs idle
/// (plan 4.15), declares no lease.
#[track_caller]
fn assert_hosts_declare_what_native_encoders_and_forwarders_do(
    plan: &ComponentBounds, legacy: &ComponentBounds, frame: &Frame, name: &str,
) {
    let firm = |bounds: &ComponentBounds| {
        unnamed_terms(bounds)
            .into_iter()
            .filter(|term| term.starts_with("firm "))
            .collect::<Vec<_>>()
    };
    for host in frame.hosts().iter() {
        let encoder = host.encoder.to_string();
        let delivery = host.host.to_string();
        let encoder_path = ["topology", "primary", "encoders", encoder.as_str()];
        let delivery_path = ["topology", "primary", "forwarders", delivery.as_str()];
        let encode_host = bounds_at(plan, &encoder_path);
        match bounds_below(legacy, &encoder_path) {
            Some(native) => {
                let mut natives = vec![native];
                if encoder == "dd_metrics_encode" {
                    for (id, routing) in bounds_at(legacy, &encoder_path[..3]).subcomponents() {
                        if routing_encoder(id) {
                            natives.push(routing);
                        }
                    }
                }
                let mut terms = natives.iter().flat_map(|native| firm(native)).collect::<Vec<_>>();
                terms.sort();
                assert_eq!(firm(encode_host), terms, "{}: `{}`", name, encoder);
                assert_eq!(
                    channel_bytes(encode_host, "events channel"),
                    natives
                        .iter()
                        .map(|native| channel_bytes(native, "events channel"))
                        .sum::<u64>(),
                    "{}: `{}`",
                    name,
                    encoder
                );
                assert_eq!(
                    channel_bytes(encode_host, "payloads channel"),
                    channel_bytes(native, "payloads channel"),
                    "{}: `{}`",
                    name,
                    encoder
                );
                assert_ne!(channel_bytes(native, "payloads channel"), 0, "{}: `{}`", name, encoder);
            }
            None => assert_eq!(
                encode_host.subcomponents().into_iter().count(),
                0,
                "{}: idle `{}`",
                name,
                encoder
            ),
        }
        let delivery_host = bounds_at(plan, &delivery_path);
        if let Some(native) = bounds_below(legacy, &delivery_path) {
            assert_eq!(
                declared_by_path(delivery_host, &[]),
                declared_by_path(native, &[]),
                "{}: `{}`",
                name,
                delivery
            );
        }
    }
}

#[tokio::test]
async fn region_bounds_are_their_kernels_native_bounds_and_their_buffers_and_the_rest_is_the_legacy_topologys() {
    let topologies = topologies("").await;
    let plan = topologies.registry.as_bounds();
    let legacy = topologies.legacy_registry.as_bounds();
    let transforms = ["topology", "primary", "transforms"];
    let at = |bounds, component: &str| bounds_at(bounds, &[&transforms[..], &[component]].concat());

    // A region host holds one full buffer per exit while a send waits, and its driver one full batch per register of
    // its program between steps, the memo of each decision it memoizes (4,096 entries of three words: the context, the
    // kind, and one word of exits), the counts per exit set of each decision (a table of 128 slots, each a 32-byte
    // exit set, its events, its points, and a tag word), and once the scratch of its decision steps (an exit set per
    // event of a full batch, and for each exit of its widest decision a count and an output buffer).
    let exit_buffer = size_of::<(ExitId, EventsBuffer)>() + size_of::<Event>() * 1024;
    let register_buffer = size_of::<EventsBuffer>() + size_of::<Event>() * limits::values::BATCH_SIZE;
    let memo = 4096 * 3 * size_of::<u64>();
    let counts = 128 * (32 + 8 + 8 + 8);
    let scratch = |exits: usize| 1024 * 32 + exits * (size_of::<u64>() + size_of::<Option<EventsBuffer>>());
    // The region, with its exits (one to each output), the registers of the largest entry of its boot program, its
    // decisions (the fan-out of the post-aggregation filter to the client telemetry tap, and the policy fan-out) with
    // the exits of the widest, its memoized decisions (none: neither fan-out has a predicate), and each of its kernels
    // with the native transform it is. The default listener filterlist and post-aggregation filter are empty: each is
    // a `split` on `true`, which keeps every metric and compiles to no decision and no predicate, so the region
    // neither steps a decision for it nor declares a predicate (a gateway's predicate is in the gateway test below).
    // Aggregation, a state owner fused with its neighbors, keeps its native component's bounds beside the region, as
    // `dsd_agg`, and is checked below.
    type Region<'a> = (
        &'a str,
        &'a [&'a str],
        usize,
        (usize, usize),
        usize,
        &'a [(&'a str, &'a str)],
    );
    let regions: [Region; 1] = [(
        "dsd_pre_aggregation",
        &["dsd_client_telemetry", "dd_metrics_encode", "mrf_metrics_encode"],
        6,
        (2, 2),
        0,
        &[
            ("dsd_enrich", "dsd_enrich"),
            ("dsd_prefix_filter_namespace_prefix", "dsd_prefix_filter"),
            ("dsd_no_agg_split", "dsd_no_agg_split"),
            ("dsd_tag_filterlist", "dsd_tag_filterlist"),
            ("metrics", "metrics_enrich"),
        ],
    )];
    // A region holding a state owner also holds one drain step of its output, at once.
    let drain_step = size_of::<Event>()
        * (limits::values::DRAIN_STEP_EVENTS as usize + saluki_metric_plan::operators::MAX_HISTOGRAM_SERIES as usize);
    let slot_names = topologies
        .frame
        .slots()
        .iter()
        .map(|slot| slot.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(slot_names, ["dsd_pre_aggregation"]);
    for (region, exits, registers, (decisions, widest), memoized, kernels) in regions {
        let index = slot_names.iter().position(|name| *name == region).unwrap();
        let slot = &topologies.frame.slots()[index];
        assert!(is_state_slot(&topologies.frame, slot), "`{}`", region);
        let slot_exits = slot.exits.iter().map(|exit| exit.name.as_str()).collect::<Vec<_>>();
        assert_eq!(slot_exits, exits, "`{}`", region);
        assert_eq!(topologies.programs[index].register_count(), registers, "`{}`", region);

        let region_bounds = at(&plan, region);
        let (minimum, firm) = own_bytes(region_bounds);
        // The minimum is the host itself, with the driver it holds.
        assert!(minimum > 0, "`{}`", region);
        let program_decisions = topologies.programs[index].decisions();
        assert_eq!(program_decisions.len(), decisions, "`{}`", region);
        assert_eq!(
            program_decisions
                .iter()
                .map(|decision| decision.exits.len())
                .max()
                .unwrap_or_default(),
            widest,
            "`{}`",
            region
        );
        assert_eq!(
            program_decisions.iter().filter(|decision| decision.memoized).count(),
            memoized,
            "`{}`",
            region
        );
        // A region without a decision keeps no decision scratch.
        let kept = if decisions == 0 {
            0
        } else {
            memoized * memo + decisions * counts + scratch(widest)
        };
        assert_eq!(
            firm,
            exits.len() * exit_buffer + registers * register_buffer + kept + drain_step,
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
        let mut expected = kernels.iter().map(|(node, _)| *node).collect::<Vec<_>>();
        expected.sort();
        assert_eq!(nodes, expected, "`{}`", region);
        for (kernel, native) in kernels {
            let kernel_bounds = bounds_at(region_bounds, &[kernel]);
            assert_eq!(
                declared_without_structs(kernel_bounds),
                declared_without_structs(at(&legacy, native)),
                "`{}` in `{}`",
                kernel,
                region
            );
        }
    }
    // Aggregation's kernel declares every term the native component does, where the native component does.
    let declared = declared_without_structs(at(&plan, "dsd_agg"));
    for term in declared_without_structs(at(&legacy, "dsd_agg")) {
        assert!(declared.contains(&term), "`{}` among {:?}", term, declared);
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
    // The post-aggregation filter, empty by default, declares nothing natively but its struct, and the region nothing
    // for it.
    assert!(declared_without_structs(at(&legacy, "dsd_post_agg_filter")).is_empty());

    // Every other component declares what it does natively, the hosts as their native encoders and forwarders do.
    let replaced = [&FUSED[..], &["metrics_enrich", "dsd_pre_aggregation", "dsd_agg"]].concat();
    assert_eq!(
        without_hosts(declared_by_path(&plan, &replaced), &topologies.frame),
        without_hosts(declared_by_path(&legacy, &replaced), &topologies.frame)
    );
    assert_hosts_declare_what_native_encoders_and_forwarders_do(&plan, &legacy, &topologies.frame, "defaults");

    // The interconnects: one into each transform and destination, of the default capacity, 128 buffers, but for the
    // region's, and the in-flight buffers of every source and transform besides. The legacy topology has nine
    // transforms and three destinations. The region replaces seven of its transforms, aggregation included, and its
    // input holds what they held on the way through them, queued and in flight (seven of them, 7 * 129 - 1 buffers),
    // so both topologies hold as many buffers.
    let in_flight_buffer = size_of::<EventsBuffer>() + size_of::<Event>() * 1024;
    let legacy_buffers = 12 * 128;
    let plan_buffers = 5 * 128 + 902;
    for (bounds, transforms, buffers) in [(&legacy, 9, legacy_buffers), (&plan, 3, plan_buffers)] {
        let (minimum, firm) = own_bytes(bounds_at(bounds, &["topology", "primary", "interconnects"]));
        assert_eq!(minimum, buffers * size_of::<EventsBuffer>());
        assert_eq!(firm, (buffers + 1 + transforms) * in_flight_buffer);
    }
    assert_eq!(plan_buffers + 3, legacy_buffers + 9);
}

/// One gateway of a configuration: its node, the strings its lists hold, and how many copies of them, besides its
/// matcher's entries, the legacy gateway declares and the region declares for its predicate.
type Gateway<'a> = (&'a str, &'a [&'a str], usize, usize);

/// One gateway configuration: its name in the lowering matrix, its regions, the transforms of the legacy topology, its
/// gateways, and the memoized decisions of each region.
type GatewayCase<'a> = (&'a str, &'a [&'a str], usize, &'a [Gateway<'a>], &'a [usize]);

#[tokio::test]
async fn gateways_run_in_regions_and_declare_their_lists_as_the_legacy_gateways_do() {
    // (configuration, the regions, the legacy topology's transforms, the gateways, the memoized decisions of each
    // region): the legacy gateways are transforms of their own, with an interconnect each, where the plan-driven
    // topology runs them as decisions of its one region. The DogStatsD-only topology has nine legacy transforms, seven
    // of which the region replaces (see above); each gateway adds a legacy transform. The policy decision is memoized
    // where its predicates cost at least `MEMO_COST_FLOOR` (25 ns): with two routing filters, a prefix match and an
    // exact match on the intake name, not with one exact match or a failover gateway alone; the filters on one
    // intake-name leaf never are.
    let cases: [GatewayCase; 4] = [
        // The legacy filter keeps a copy of its source's allowlist; Multi-Region Failover's predicate holds it once, in
        // its name match: its emptiness test, over a literal allowlist, normalizes to a constant.
        (
            "multi-region failover mirroring an allowlist",
            &["dsd_pre_aggregation"],
            10,
            &[("mrf_metrics_gateway", &["requests.count", "orders.total"], 1, 1)],
            &[0],
        ),
        // A routing filter's predicate holds its names and prefixes once, as the legacy filter's source does: an exact
        // match too cheap to memoize.
        (
            "one routing group",
            &["dsd_pre_aggregation"],
            10,
            &[("metrics_routing_filter_0", &["orders.total"], 1, 1)],
            &[0],
        ),
        // Two groups (the endpoint with empty lists has none, and two endpoints share one), ordered by their lists.
        (
            "two routing groups, a shared group, and an endpoint with empty lists",
            &["dsd_pre_aggregation"],
            11,
            &[
                ("metrics_routing_filter_0", &["catalog."], 1, 1),
                ("metrics_routing_filter_1", &["orders.total"], 1, 1),
            ],
            &[1],
        ),
        // The legacy autoscaling gateway keeps only its matcher; the predicate also holds the names.
        (
            "autoscaling failover",
            &["dsd_pre_aggregation"],
            10,
            &[("af_metrics_gateway", &["requests.count"], 0, 1)],
            &[0],
        ),
    ];
    let in_flight_buffer = size_of::<EventsBuffer>() + size_of::<Event>() * 1024;
    let exit_buffer = size_of::<(ExitId, EventsBuffer)>() + size_of::<Event>() * 1024;
    let register_buffer = size_of::<EventsBuffer>() + size_of::<Event>() * limits::values::BATCH_SIZE;
    let strings = |list: &[&str]| {
        list.iter()
            .map(|value| value.len() + size_of::<String>())
            .sum::<usize>()
    };
    let entries = |list: &[&str]| {
        list.iter()
            .map(|value| value.len() + size_of::<stringtheory::MetaString>())
            .sum::<usize>()
    };
    let memo = 4096 * 3 * size_of::<u64>();
    let counts = 128 * (32 + 8 + 8 + 8);
    let scratch = |exits: usize| 1024 * 32 + exits * (size_of::<u64>() + size_of::<Option<EventsBuffer>>());
    for (name, regions, legacy_transforms, expected_gateways, memoized) in cases {
        let topologies = topologies(matrix_entry(name)).await;
        let frame = &topologies.frame;
        let slots = frame.slots().iter().map(|slot| slot.name.as_str()).collect::<Vec<_>>();
        assert_eq!(slots, regions, "{}", name);

        let gateways = topologies
            .legacy
            .components()
            .iter()
            .map(|component| component.id().to_string())
            .filter(|id| fused(id) && !FUSED.contains(&id.as_str()))
            .collect::<Vec<_>>();
        let mut expected = expected_gateways
            .iter()
            .map(|(gateway, ..)| gateway.to_string())
            .collect::<Vec<_>>();
        expected.sort();
        let mut found = gateways.clone();
        found.sort();
        assert_eq!(found, expected, "{}", name);
        assert_eq!(gateways.len(), legacy_transforms - 9, "{}", name);

        let plan = topologies.registry.as_bounds();
        let legacy = topologies.legacy_registry.as_bounds();
        let transforms = ["topology", "primary", "transforms"];
        let below = |bounds: &ComponentBounds| {
            bounds_at(bounds, &transforms)
                .subcomponents()
                .into_iter()
                .map(|(name, _)| name.to_string())
                .collect::<BTreeSet<_>>()
        };

        // A gateway is a decision of a region, which declares its predicate under its node: itself, and the lists it
        // holds, the matcher's entries exactly as the legacy gateway declared them, and the copies of the strings it
        // keeps, at least as many as the legacy gateway did.
        for (gateway, list, legacy_copies, copies) in expected_gateways {
            assert!(below(&legacy).contains(*gateway), "{}: `{}`", name, gateway);
            assert!(!below(&plan).contains(*gateway), "{}: `{}`", name, gateway);
            let node = gateway.parse().unwrap();
            let slot = frame
                .slots()
                .iter()
                .find(|slot| slot.nodes.contains(&node))
                .unwrap_or_else(|| panic!("{}: `{}` runs in a region", name, gateway));
            let declared = bounds_at(&plan, &[&transforms[..], &[slot.name.as_str(), gateway]].concat());
            let native = bounds_at(&legacy, &[&transforms[..], &[gateway]].concat());
            assert_eq!(
                list_bytes(native),
                legacy_copies * strings(list) + entries(list),
                "{}: `{}`",
                name,
                gateway
            );
            assert_eq!(
                list_bytes(declared),
                copies * strings(list) + entries(list),
                "{}: `{}`",
                name,
                gateway
            );
            assert_eq!(
                declared.total_firm_limit_bytes(),
                size_of::<SharedPredicate>() + list_bytes(declared),
                "{}: `{}`",
                name,
                gateway
            );
        }

        // A region declares a full buffer per exit and per register, and what its decisions keep (see above): the
        // memo of each memoized decision (of at most 64 exits, so three words an entry), counts per decision, and the
        // scratch of its widest decision; and a region holding aggregation one drain step of its output.
        for ((slot, program), memoized) in frame.slots().iter().zip(&topologies.programs).zip(memoized) {
            let drain_step = if is_state_slot(frame, slot) {
                size_of::<Event>()
                    * (limits::values::DRAIN_STEP_EVENTS as usize
                        + saluki_metric_plan::operators::MAX_HISTOGRAM_SERIES as usize)
            } else {
                0
            };
            let region = bounds_at(&plan, &[&transforms[..], &[slot.name.as_str()]].concat());
            let (_, firm) = own_bytes(region);
            let decisions = program.decisions();
            assert_eq!(
                decisions.iter().filter(|decision| decision.memoized).count(),
                *memoized,
                "{}: `{}`",
                name,
                slot.name
            );
            let widest = decisions
                .iter()
                .map(|decision| decision.exits.len())
                .max()
                .unwrap_or_default();
            assert!(widest <= 64, "{}: `{}`", name, slot.name);
            let kept = if decisions.is_empty() {
                0
            } else {
                memoized * memo + decisions.len() * counts + scratch(widest)
            };
            assert_eq!(
                firm,
                slot.exits.len() * exit_buffer + program.register_count() * register_buffer + kept + drain_step,
                "{}: `{}`",
                name,
                slot.name
            );
        }

        // Every other component declares what it does natively, the hosts as their native encoders and forwarders
        // do; the routing groups' encoders are gone, their streams being leases of `dd_metrics_encode`.
        let mut replaced = [&FUSED[..], &KEPT_IDS[..], regions].concat();
        replaced.extend(gateways.iter().map(String::as_str));
        let mut native = without_hosts(declared_by_path(&legacy, &replaced), frame);
        native.retain(|path, _| !path.starts_with("root.topology.primary.encoders.metrics_routing_encode_"));
        assert_eq!(
            without_hosts(declared_by_path(&plan, &replaced), frame),
            native,
            "{}",
            name
        );
        assert_hosts_declare_what_native_encoders_and_forwarders_do(&plan, &legacy, frame, name);

        // One interconnect, and one transform's in-flight buffers, per transform: the gateways' are gone. A region's
        // input holds what the transforms it replaces held on the way through it (see
        // `each_region_takes_what_the_native_transforms_it_replaces_held_on_the_way_through_it`), so the gateways in
        // front of the outputs, which are parallel, leave as many buffers as one gateway held.
        // The region replaces six fused transforms and aggregation.
        let plan_transforms = legacy_transforms - 7 - gateways.len() + regions.len();
        let region_buffers = regions
            .iter()
            .map(|region| {
                topologies
                    .plan
                    .component(region)
                    .unwrap()
                    .input_capacity()
                    .unwrap()
                    .get()
            })
            .sum::<usize>();
        let legacy_buffers = (legacy_transforms + 3) * 128;
        let plan_buffers = (plan_transforms - regions.len() + 3) * 128 + region_buffers;
        for (bounds, transforms, buffers) in [
            (&legacy, legacy_transforms, legacy_buffers),
            (&plan, plan_transforms, plan_buffers),
        ] {
            let (minimum, firm) = own_bytes(bounds_at(bounds, &["topology", "primary", "interconnects"]));
            assert_eq!(minimum, buffers * size_of::<EventsBuffer>(), "{}", name);
            assert_eq!(firm, (buffers + 1 + transforms) * in_flight_buffer, "{}", name);
        }
        assert_eq!(
            plan_buffers + plan_transforms + (gateways.len() - 1) * 129,
            legacy_buffers + legacy_transforms,
            "{}",
            name
        );
    }
}

#[test]
fn the_hosts_limits_are_the_plans() {
    // `saluki-components` cannot depend on the plan crate, so its hosts keep copies of the limits the installer
    // plans with: a lease's shard channel, the drain deadline, and how many recipients drain at once.
    use saluki_components::{encoders::encode_host, forwarders::delivery};

    assert_eq!(
        encode_host::LEASE_QUEUE_BUFFERS as u64,
        limits::values::LEASE_QUEUE_BUFFERS
    );
    assert_eq!(delivery::DRAIN_DEADLINE, limits::values::DRAIN_DEADLINE);
    assert_eq!(
        delivery::MAX_DRAINING_RECIPIENTS as u64,
        limits::values::MAX_DRAINING_RECIPIENTS
    );
}
