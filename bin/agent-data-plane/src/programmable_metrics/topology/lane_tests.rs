//! The plan-driven topology on lanes: how each slot runs its lanes, how every edge into a slot with lanes routes, what
//! each lane holds and declares, and, running, that producer, state, and post-aggregation lanes deliver what the legacy
//! topology does.

use std::{collections::BTreeSet, mem::size_of, num::NonZeroUsize, time::Duration};

use agent_data_plane_config_system::ConfigurationSystem;
use bytesize::ByteSize;
use saluki_app::accounting::{MemoryBoundsConfiguration, MemoryMode as AppMemoryMode};
use saluki_core::{
    accounting::{ComponentBounds, ComponentRegistry},
    data_model::{
        event::{
            metric::{
                context::{Context, ContextResolver},
                Metric,
            },
            Event,
        },
        tags::TagSet,
    },
    topology::{BlueprintSnapshot, EventsBuffer, LaneEdge},
};
use saluki_metric_plan::{
    compile::{Frame, LanePolicy},
    equivalence::compare,
    limits,
};
use saluki_metrics::test::TestRecorder;

use crate::{
    cli::run::{boot_metrics, create_topology, fit_lanes_to_memory, BuiltTopology, LaneFitInputs},
    internal::env::ADPEnvironmentProvider,
    programmable_metrics::{
        installer::{BootCapture, ProgrammableMetrics},
        lanes::{LaneBound, LaneCounts, LaneSettings},
        telemetry::{
            LaneTelemetry, SlotLaneStats, PLAN_LANE_BATCHES_TOTAL, PLAN_LANE_BUSY_NANOS_TOTAL, PLAN_LANE_CONTEXTS,
            PLAN_LANE_EVENTS_TOTAL, PLAN_LANE_INPUT_DEPTH, PLAN_LANE_PROGRESS_TOTAL, PLAN_LANE_ROUTED_BUFFERS_TOTAL,
            PLAN_LANE_ROUTED_EVENTS_TOTAL, PLAN_LANE_STAMP_WAIT_NANOS_TOTAL,
        },
        test_support::{
            configuration_system, configuration_system_with_default_lanes, environment_provider,
            layered_configuration_system, process_start, programmable_metrics, programmable_metrics_on, ConfigCell,
            TEST_WORKERS,
        },
        test_topology::{output_points, Aggregation, Delivery, MetricsPath, Options, TestTopology, PATIENCE},
    },
};

/// Lanes on, with three DogStatsD decoders, so three producer lanes, two state lanes, and two post-aggregation lanes.
const THREE_TWO_TWO: &str = r#"
dogstatsd_workers_count: 3
experimental:
  programmable_metrics:
    lanes:
      mode: auto
      state: 2
      post: 2
"#;

/// Lanes on with one state lane: three producer lanes, which every running test can run, and two post-aggregation
/// lanes.
const THREE_ONE_TWO: &str = r#"
dogstatsd_workers_count: 3
experimental:
  programmable_metrics:
    lanes:
      mode: auto
      state: 1
      post: 2
"#;

/// Lanes on, three producer lanes and one lane of each other kind.
const THREE_ONE_ONE: &str = r#"
dogstatsd_workers_count: 3
experimental:
  programmable_metrics:
    lanes:
      mode: auto
      state: 1
      post: 1
"#;

/// Lanes on, three post-aggregation lanes and one lane of each other kind.
const ONE_ONE_THREE: &str = r#"
dogstatsd_workers_count: 1
experimental:
  programmable_metrics:
    lanes:
      mode: auto
      state: 1
      post: 3
"#;

/// Lanes on, one lane each: the metrics path laid out without R1's pipelining.
const ONE_LANE_EACH: &str = r#"
dogstatsd_workers_count: 1
experimental:
  programmable_metrics:
    lanes:
      mode: auto
      state: 1
      post: 1
"#;

/// Lanes on with automatic counts and three decoders.
const THREE_AUTOMATIC: &str = r#"
dogstatsd_workers_count: 3
experimental:
  programmable_metrics:
    lanes:
      mode: auto
"#;

/// OTLP and the checks source beside DogStatsD.
const OTLP_AND_CHECKS: &str = r#"
data_plane:
  otlp:
    enabled: true
  checks:
    enabled: true
"#;

/// Lanes on, with four producer lanes and four state lanes.
const FOUR_STATE_LANES: &str = r#"
dogstatsd_workers_count: 2
experimental:
  programmable_metrics:
    lanes:
      mode: auto
      state: 4
"#;

/// The plan-driven topology of one configuration, built, with its frame and the registry it declared its bounds in.
struct Built {
    plan: BlueprintSnapshot,
    frame: Frame,
    registry: ComponentRegistry,
    lanes: LaneCounts,
    lane_stats: Vec<SlotLaneStats>,
}

async fn built(yaml: &str) -> Built {
    built_on(yaml, None).await
}

/// Builds the plan-driven topology of the configuration `yaml`, on `lanes`, or on the lanes it asks for.
async fn built_on(yaml: &str, lanes: Option<LaneCounts>) -> Built {
    let system = configuration_system(yaml).await;
    let config = system.config();
    let env = environment_provider(&config).await;
    let registry = ComponentRegistry::default();
    let cell = ConfigCell::new(config.as_ref().clone());
    let metrics = match lanes {
        Some(lanes) => programmable_metrics_on(&config, cell.live(), &registry, lanes),
        None => programmable_metrics(&config, cell.live(), &registry),
    };
    let mut metrics = metrics.expect("a metrics pipeline is required");
    let (plan, _) = create_topology(&system, &env, &registry, Some(&mut metrics))
        .await
        .expect("the plan-driven topology builds");
    let lanes = metrics.boot_lanes();
    let boot = metrics.boot_topology();
    Built {
        plan: plan.snapshot().unwrap(),
        frame: Frame::clone(&boot.frame),
        registry,
        lanes,
        lane_stats: boot.lane_stats.clone(),
    }
}

/// Returns every edge into a component with several lanes, with how it routes.
fn routed_edges(snapshot: &BlueprintSnapshot) -> BTreeSet<(String, String, LaneEdge)> {
    snapshot
        .edges()
        .iter()
        .filter_map(|edge| {
            edge.routing()
                .map(|routing| (edge.from().to_string(), edge.to().to_string(), routing))
        })
        .collect()
}

fn routed(edges: &[(&str, &str, LaneEdge)]) -> BTreeSet<(String, String, LaneEdge)> {
    edges
        .iter()
        .map(|(from, to, routing)| (from.to_string(), to.to_string(), *routing))
        .collect()
}

fn input_lanes(snapshot: &BlueprintSnapshot, component: &str) -> u16 {
    snapshot
        .component(component)
        .and_then(|component| component.input_lanes())
        .map_or(0, |lanes| lanes.get())
}

fn input_capacity(snapshot: &BlueprintSnapshot, component: &str) -> usize {
    snapshot
        .component(component)
        .and_then(|component| component.input_capacity())
        .map_or(0, |capacity| capacity.get())
}

#[tokio::test]
async fn on_lanes_each_slot_runs_its_lanes_and_each_edge_into_lanes_routes_as_its_frame_says() {
    let Built { plan, frame, lanes, .. } = built(THREE_TWO_TWO).await;
    assert!(lanes.lanes_on());

    // Without R1's pipelining, the metrics path is three slots, each on its lanes.
    let slots = frame
        .slots()
        .iter()
        .map(|slot| (slot.name.to_string(), slot.lanes))
        .collect::<Vec<_>>();
    assert_eq!(
        slots,
        [
            ("dsd_pre_aggregation".to_string(), 3),
            ("dsd_agg".to_string(), 2),
            ("metrics_enrich".to_string(), 2)
        ]
    );
    for (slot, lanes) in slots {
        assert_eq!(input_lanes(&plan, &slot), lanes, "`{}`", slot);
    }

    // The DogStatsD source's metrics split into one output per producer lane, each feeding its lane.
    let outputs = plan
        .component("dsd_in")
        .unwrap()
        .outputs()
        .iter()
        .map(|output| output.id().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        outputs,
        [
            "dsd_in.events",
            "dsd_in.metrics_0",
            "dsd_in.metrics_1",
            "dsd_in.metrics_2",
            "dsd_in.service_checks"
        ]
    );
    // Into aggregation's state lanes, by key; the passthrough by the producer lane; aggregation's output spread.
    assert_eq!(
        routed_edges(&plan),
        routed(&[
            ("dsd_in.metrics_0", "dsd_pre_aggregation", LaneEdge::Fixed(0)),
            ("dsd_in.metrics_1", "dsd_pre_aggregation", LaneEdge::Fixed(1)),
            ("dsd_in.metrics_2", "dsd_pre_aggregation", LaneEdge::Fixed(2)),
            ("dsd_pre_aggregation.dsd_agg", "dsd_agg", LaneEdge::Keyed),
            (
                "dsd_pre_aggregation.metrics_enrich",
                "metrics_enrich",
                LaneEdge::Affinity
            ),
            ("dsd_agg.metrics_enrich", "metrics_enrich", LaneEdge::Spread),
        ])
    );
    // Both taps read every lane's metrics.
    let edges = plan
        .edges()
        .iter()
        .map(|edge| (edge.from().to_string(), edge.to().to_string()))
        .collect::<BTreeSet<_>>();
    for lane in 0..3 {
        let output = format!("dsd_in.metrics_{}", lane);
        assert!(
            edges.contains(&(output.clone(), "dsd_stats_out".to_string())),
            "{}",
            output
        );
    }
    // The producer lanes stamp the source's points themselves: no dispatcher stamps a DogStatsD output.
    assert!(plan
        .admission_points()
        .iter()
        .all(|point| !point.output().to_string().starts_with("dsd_in.")));
}

#[tokio::test]
async fn on_one_producer_lane_the_source_keeps_its_metrics_output_and_its_lane_stamps_it() {
    let Built { plan, frame, .. } = built(ONE_LANE_EACH).await;
    assert!(frame.slots().iter().all(|slot| slot.lanes == 1));
    assert_eq!(frame.slots().len(), 3, "R1's pipelining is off on lanes");
    let outputs = plan
        .component("dsd_in")
        .unwrap()
        .outputs()
        .iter()
        .map(|output| output.id().to_string())
        .collect::<Vec<_>>();
    assert_eq!(outputs, ["dsd_in.events", "dsd_in.metrics", "dsd_in.service_checks"]);
    assert!(routed_edges(&plan).is_empty());
    // The lane stamps the point, which is owned: the source's dispatcher stamps nothing.
    assert!(plan
        .admission_points()
        .iter()
        .all(|point| point.output().to_string() != "dsd_in.metrics"));
}

#[tokio::test]
async fn each_lane_of_a_slot_holds_its_share_of_what_the_slot_holds_on_one_lane() {
    // On one lane each, maximal fusion makes the pre-aggregation chain of four transforms one slot (4 × 129 − 1),
    // aggregation one (128), and the post-aggregation filter and enrichment one (2 × 129 − 1). On lanes, each lane of a
    // stateless slot holds `ceil(capacity / lanes)`; each state lane, fed buffers of 1,024 events at two lanes,
    // `ceil(128 × 1,024 / (2 × 1,024))`.
    let one = built(ONE_LANE_EACH).await;
    assert_eq!(input_capacity(&one.plan, "dsd_pre_aggregation"), 515);
    assert_eq!(input_capacity(&one.plan, "dsd_agg"), 128);
    assert_eq!(input_capacity(&one.plan, "metrics_enrich"), 257);

    let lanes = built(THREE_TWO_TWO).await;
    assert_eq!(input_capacity(&lanes.plan, "dsd_pre_aggregation"), 172);
    assert_eq!(input_capacity(&lanes.plan, "dsd_agg"), 64);
    assert_eq!(input_capacity(&lanes.plan, "metrics_enrich"), 129);
}

#[tokio::test]
async fn four_state_lanes_hold_routed_buffers_of_512_events_and_as_many_events_as_one_lane_does() {
    // At four state lanes, routed buffers hold `clamp(2,048 / 4, 256, 1,024)` = 512 events, so each lane holds
    // `ceil(128 × 1,024 / (4 × 512))` = 64 of them: 131,072 events in all, as one lane of 128 full buffers does.
    let lanes = built(FOUR_STATE_LANES).await;
    assert_eq!(input_lanes(&lanes.plan, "dsd_agg"), 4);
    assert_eq!(input_capacity(&lanes.plan, "dsd_agg"), 64);
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

/// Returns the names of the subcomponents directly below `bounds`.
fn below(bounds: &ComponentBounds) -> BTreeSet<String> {
    bounds
        .subcomponents()
        .into_iter()
        .map(|(name, _)| name.clone())
        .collect()
}

const TRANSFORMS: [&str; 3] = ["topology", "primary", "transforms"];

fn region_bounds<'a>(bounds: &'a ComponentBounds, region: &str) -> &'a ComponentBounds {
    let mut path = TRANSFORMS.to_vec();
    path.push(region);
    bounds_at(bounds, &path)
}

#[tokio::test]
async fn each_lane_declares_its_drivers_bounds_and_aggregations_state_is_declared_once() {
    let one = built(ONE_LANE_EACH).await;
    let lanes = built(THREE_TWO_TWO).await;
    let (one, lanes) = (one.registry.as_bounds(), lanes.registry.as_bounds());

    // A stateless slot's lanes each declare what its one lane declares, under the lane's own subcomponent, as its
    // kernels are accounted under the lane's resource group.
    let enrich = region_bounds(&lanes, "metrics_enrich");
    let one_enrich = region_bounds(&one, "metrics_enrich");
    assert!(
        below(enrich).contains("lane_0") && below(enrich).contains("lane_1"),
        "{:?}",
        below(enrich)
    );
    for lane in ["lane_0", "lane_1"] {
        let lane_bounds = bounds_at(enrich, &[lane]);
        assert_eq!(below(lane_bounds), below(one_enrich), "{}", lane);
        for kernel in below(one_enrich) {
            assert_eq!(
                bounds_at(lane_bounds, &[&kernel]).total_firm_limit_bytes(),
                bounds_at(one_enrich, &[&kernel]).total_firm_limit_bytes(),
                "{}: `{}`",
                lane,
                kernel
            );
        }
    }

    // Aggregation's contexts are partitioned over its state lanes, which draw on one budget: the state at the context
    // limit is declared once, where one lane declares it, at the slot's own level, and each lane, the first included,
    // declares the same rest of a lane's driver, far less.
    let agg = region_bounds(&lanes, "dsd_agg");
    let lane_0 = bounds_at(agg, &["lane_0"]).total_firm_limit_bytes();
    let lane_1 = bounds_at(agg, &["lane_1"]).total_firm_limit_bytes();
    assert_eq!(lane_0, lane_1, "every state lane declares one lane's driver");
    let state = agg.total_firm_limit_bytes() - lane_0 - lane_1;
    assert!(
        lane_0 * 100 < state,
        "a lane: {} bytes, the state: {} bytes",
        lane_0,
        state
    );
    // The state's bound expressions are where the slot on one lane declares them, which the sizing guide reads.
    assert_eq!(context_limit_terms(&lanes), context_limit_terms(&one));
    assert_eq!(
        context_limit_terms(&lanes),
        ["root.topology.primary.transforms.dsd_agg.firm"]
    );
    // Two lanes declare less than twice one lane: the state is declared once.
    let one_agg = region_bounds(&one, "dsd_agg").total_firm_limit_bytes();
    let two = agg.total_firm_limit_bytes();
    assert!(two < one_agg + one_agg / 10, "{} on two lanes, {} on one", two, one_agg);
}

/// Returns the names of the bounds whose expressions read aggregation's context limit, sorted and without repeats.
fn context_limit_terms(bounds: &ComponentBounds) -> Vec<String> {
    bounds
        .to_exprs()
        .into_iter()
        .filter(|entry| entry["expr"].to_string().contains("aggregate_context_limit"))
        .filter_map(|entry| entry["name"].as_str().map(str::to_string))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Returns the firm bytes of the primary topology a registry declares.
fn topology_firm_bytes(registry: &ComponentRegistry) -> usize {
    bounds_at(&registry.as_bounds(), &["topology", "primary"]).total_firm_limit_bytes()
}

/// Returns the firm bytes of the primary topology of the configuration `yaml` on `lanes`.
async fn topology_firm_bytes_on(yaml: &str, lanes: LaneCounts) -> usize {
    topology_firm_bytes(&built_on(yaml, Some(lanes)).await.registry)
}

#[tokio::test]
async fn the_firm_bound_grows_by_each_lanes_own_bounds_up_to_64_workers() {
    // The automatic counts for W workers and, as the native default gives, `W − 2` decoders: at 64 workers, 16
    // producer lanes, 4 state lanes, and 8 post-aggregation lanes, the most the automatic counts give.
    let mut counts = Vec::new();
    for workers in [4usize, 8, 16, 32, 64] {
        let yaml = format!(
            "dogstatsd_workers_count: {}\nexperimental: {{programmable_metrics: {{lanes: {{mode: auto}}}}}}",
            workers - 2
        );
        let system = configuration_system(&yaml).await;
        counts.push((
            workers,
            yaml,
            LaneSettings::from_config(&system.config(), workers).counts,
        ));
    }
    let at_64 = counts.last().map(|(_, _, counts)| counts.lanes).unwrap();
    assert_eq!((at_64.dogstatsd, at_64.state, at_64.post), (16, 4, 8));

    // What one more lane of each kind adds to the bound of one lane each: its kernels, memos, and buffers. A state
    // lane adds none of aggregation's state, which the state budget bounds for every lane at once.
    let base = topology_firm_bytes_on(ONE_LANE_EACH, LaneCounts::on(LanePolicy::ONE)).await;
    let with = |dogstatsd, state, post| LaneCounts::on(LanePolicy { dogstatsd, state, post });
    let producer = topology_firm_bytes_on(ONE_LANE_EACH, with(2, 1, 1)).await - base;
    let state = topology_firm_bytes_on(ONE_LANE_EACH, with(1, 2, 1)).await - base;
    let post = topology_firm_bytes_on(ONE_LANE_EACH, with(1, 1, 2)).await - base;
    let aggregation =
        region_bounds(&built(ONE_LANE_EACH).await.registry.as_bounds(), "dsd_agg").total_firm_limit_bytes();
    assert!(
        state * 10 < aggregation,
        "a state lane adds {} bytes; aggregation's slot holds {}",
        state,
        aggregation
    );

    // At every worker count, the bound grows by every lane's own bytes, as one lane more of its kind adds, and by each
    // datagram decoder's front cache of 4,096 contexts beyond the one decoder of the base, up to the rounding of each
    // lane's input capacity, `lanes × ceil(capacity / lanes)`, which differs from what one more lane rounds to by at
    // most one buffer per lane either way.
    let buffer = size_of::<EventsBuffer>() + limits::values::BATCH_SIZE * size_of::<Event>();
    let front_cache = ContextResolver::front_cache_bytes(NonZeroUsize::new(4_096).unwrap());
    for (workers, yaml, lanes) in counts {
        let LanePolicy {
            dogstatsd,
            state: state_lanes,
            post: post_lanes,
        } = lanes.lanes;
        let bytes = topology_firm_bytes_on(&yaml, lanes).await;
        let decoders = workers - 2;
        let expected = base
            + producer * usize::from(dogstatsd - 1)
            + state * usize::from(state_lanes - 1)
            + post * usize::from(post_lanes - 1)
            + front_cache * (decoders - 1);
        let rounding = buffer * usize::from(dogstatsd + state_lanes + post_lanes);
        println!(
            "firm bound at {} workers, lanes {{{}, {}, {}}}: {} bytes ({:.1} MiB), {:.1} MiB over one lane each",
            workers,
            dogstatsd,
            state_lanes,
            post_lanes,
            bytes,
            bytes as f64 / MIB,
            bytes.saturating_sub(base) as f64 / MIB
        );
        assert!(
            bytes.abs_diff(expected) <= rounding,
            "at {} workers: {} bytes, expected {} within {}",
            workers,
            bytes,
            expected,
            rounding
        );
    }
    println!(
        "one more lane adds: producer {:.2} MiB, state {:.2} MiB, post-aggregation {:.2} MiB",
        producer as f64 / MIB,
        state as f64 / MIB,
        post as f64 / MIB
    );
}

/// The bytes of a MiB, for reports.
const MIB: f64 = 1024.0 * 1024.0;

/// Builds the topology of the configuration `system` on `lanes`, as `run.rs` boots it, declaring its bounds in
/// `registry`; returns what `run.rs` runs.
async fn booted(
    system: &ConfigurationSystem, env: &ADPEnvironmentProvider, capture: &BootCapture, lanes: LaneCounts,
    registry: &ComponentRegistry,
) -> BuiltTopology {
    let mut metrics = boot_metrics(Some(capture), lanes, registry).expect("the metrics path boots");
    let (blueprint, surfaces) = create_topology(system, env, registry, metrics.as_mut())
        .await
        .expect("the topology builds");
    (blueprint, surfaces, metrics)
}

/// Returns the firm bytes a registry declares in all.
fn firm_bytes(registry: &ComponentRegistry) -> u64 {
    registry.as_bounds().total_firm_limit_bytes() as u64
}

/// Returns the firm bytes of the process booted on `lanes`, measured in a registry of its own, and the room one
/// generation's overlap takes beside them.
async fn bound_on(
    system: &ConfigurationSystem, env: &ADPEnvironmentProvider, capture: &BootCapture, lanes: LaneCounts,
) -> LaneBound {
    let registry = ComponentRegistry::default();
    let (_, _, metrics) = booted(system, env, capture, lanes, &registry).await;
    LaneBound {
        firm: firm_bytes(&registry),
        overlap: metrics.as_ref().map_or(0, ProgrammableMetrics::instance_overlap_bytes),
    }
}

/// Returns `strict` memory bounds of a grant of exactly `grant` bytes.
fn strict(grant: u64) -> MemoryBoundsConfiguration {
    MemoryBoundsConfiguration {
        memory_limit: Some(ByteSize::b(grant)),
        memory_slop_factor: 0.0,
        enable_global_limiter: false,
        memory_mode: AppMemoryMode::Strict,
    }
}

#[tokio::test]
async fn a_strict_grant_too_small_for_the_automatic_lanes_boots_with_fewer_and_refuses_counts_set_explicitly() {
    // Three decoders on four workers: three producer lanes, two state lanes, and three post-aggregation lanes.
    let system = configuration_system(THREE_AUTOMATIC).await;
    let config = system.config();
    let env = environment_provider(&config).await;
    let capture = ProgrammableMetrics::capture(process_start(), &system, &env, None)
        .await
        .expect("the host facts are captured")
        .expect("a metrics pipeline is required");
    let settings = LaneSettings::from_config(&config, TEST_WORKERS);
    let on = |dogstatsd, state, post| LaneCounts::on(LanePolicy { dogstatsd, state, post });
    assert_eq!(settings.counts, on(3, 2, 3));

    // A grant that the post-aggregation lanes at one, and everything else as asked, fit exactly with room for a live
    // change that rebuilds a slot's kernels, as the layout without lanes does, and the counts asked for do not.
    let asked = bound_on(&system, &env, &capture, settings.counts).await;
    let fitting = bound_on(&system, &env, &capture, on(3, 2, 1)).await;
    let without_lanes = bound_on(&system, &env, &capture, LaneCounts::OFF).await;
    let with_room = |bound: LaneBound| bound.firm + bound.overlap;
    assert!(
        with_room(fitting) < with_room(asked) && with_room(without_lanes) <= with_room(fitting),
        "{:?} {:?} {:?}",
        asked,
        fitting,
        without_lanes
    );
    // The room is the producer lanes' slot's kernels, on each of its three lanes: the tag filterlist's memo, mostly.
    assert!(fitting.overlap > 3 * 6_000_000, "{:?}", fitting);

    let registry = ComponentRegistry::default();
    let first = booted(&system, &env, &capture, settings.counts, &registry).await;
    let bounds = strict(with_room(fitting));
    let inputs = LaneFitInputs {
        config_system: &system,
        env_provider: &env,
        component_registry: &registry,
        bounds: &bounds,
        settings: &settings,
        capture: Some(&capture),
    };
    let (_, _, metrics) = fit_lanes_to_memory(&inputs, first).await.expect("fewer lanes fit");
    // The topology was built again, on the fewer lanes, and its bounds, alone in the registry, fit the grant with
    // room for the change.
    let lanes = metrics.as_ref().map(ProgrammableMetrics::boot_lanes);
    assert_eq!(lanes, Some(on(3, 2, 1)));
    assert_eq!(firm_bytes(&registry), fitting.firm);

    // A grant that fits the counts asked for with room for a change keeps them, and the topology built.
    let registry = ComponentRegistry::default();
    let first = booted(&system, &env, &capture, settings.counts, &registry).await;
    let bounds = strict(with_room(asked));
    let inputs = LaneFitInputs {
        bounds: &bounds,
        component_registry: &registry,
        ..inputs
    };
    let (_, _, metrics) = fit_lanes_to_memory(&inputs, first).await.expect("the lanes fit");
    assert_eq!(
        metrics.as_ref().map(ProgrammableMetrics::boot_lanes),
        Some(settings.counts)
    );

    // A grant the counts asked for fit by their firm bound alone, with no room for a change, which the layout without
    // lanes has: fewer lanes, which leave the room.
    let registry = ComponentRegistry::default();
    let first = booted(&system, &env, &capture, settings.counts, &registry).await;
    let bounds = strict(asked.firm);
    let inputs = LaneFitInputs {
        bounds: &bounds,
        component_registry: &registry,
        ..inputs
    };
    let (_, _, metrics) = fit_lanes_to_memory(&inputs, first).await.expect("fewer lanes fit");
    let lanes = metrics.as_ref().map(ProgrammableMetrics::boot_lanes).unwrap();
    assert_ne!(lanes, settings.counts);
    let room = metrics.as_ref().map_or(0, ProgrammableMetrics::instance_overlap_bytes);
    assert!(firm_bytes(&registry) + room <= asked.firm, "{:?}", lanes);

    // With the post-aggregation lanes set, only the automatic state and producer lanes can make room; if even one lane
    // of each does not fit, booting fails, naming the key that set them.
    let explicit = LaneSettings {
        explicit_post: true,
        ..settings
    };
    let least = bound_on(&system, &env, &capture, on(1, 1, 3)).await.firm;
    assert!(without_lanes.firm < least, "{:?} {}", without_lanes, least);
    let registry = ComponentRegistry::default();
    let first = booted(&system, &env, &capture, settings.counts, &registry).await;
    let bounds = strict(least - 1);
    let inputs = LaneFitInputs {
        bounds: &bounds,
        component_registry: &registry,
        settings: &explicit,
        ..inputs
    };
    let error = match fit_lanes_to_memory(&inputs, first).await {
        Ok(_) => panic!("the explicit lanes do not fit"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("`experimental.programmable_metrics.lanes.post`"),
        "{}",
        error
    );
    assert!(!error.contains("lanes.state"), "{}", error);
}

// ---------------------------------------------------------------------------------------------------------------------
// Running: producer lanes and post-aggregation lanes, with one state lane.
// ---------------------------------------------------------------------------------------------------------------------

/// What every configuration adds, so that the native no-aggregation split sends what it holds when it stops.
const FLUSH_PASSTHROUGH_ON_STOP: &str = "aggregate_passthrough_idle_flush_timeout: 0s";

fn context(name: &'static str, tags: &[&'static str]) -> Context {
    Context::from_static_parts(name, tags)
}

/// DogStatsD-like batches: every kind of metric, untimestamped and timestamped, gauge contexts repeating across
/// batches, each context in one place in the stream, so that every context's points stay in order on one lane.
fn batches() -> Vec<Vec<Metric>> {
    vec![
        vec![
            Metric::counter(context("orders.total", &["env:prod"]), 2.0),
            Metric::gauge(context("checkout.cart", &["user_id:7"]), 5.0),
            Metric::set(context("visitors", &["env:prod"]), "u1"),
            Metric::distribution(context("checkout.latency", &["env:dev"]), &[1.0, 2.0][..]),
            Metric::histogram(context("request.duration", &[]), [3.0, 4.0]),
            Metric::counter(context("requests.count", &["service:web"]), 4.0),
            Metric::gauge(context("catalog.items", &[]), 7.0),
        ],
        vec![
            Metric::counter(context("orders.total", &["env:prod"]), 3.0),
            Metric::gauge(context("checkout.cart", &["user_id:7"]), 6.0),
            Metric::set(context("visitors", &["env:prod"]), "u2"),
            Metric::histogram(context("request.duration", &[]), 5.0),
            Metric::counter(context("orders.total", &["env:qa"]), (1_700_000_000, 5.0)),
            Metric::gauge(context("catalog.ts", &["env:prod"]), (1_700_000_030, 4.0)),
        ],
        vec![
            Metric::gauge(context("checkout.cart", &["user_id:7"]), 2.0),
            Metric::counter(context("raw.requests.cart", &["keep:a"]), 2.0),
            Metric::distribution(context("request.size", &["env:staging"]), (1_700_000_060, 8.0)),
        ],
    ]
}

/// Returns the producer lane a context's points go to, of `lanes`: one lane per context, so that its points stay in
/// order, as a stream connection's do.
fn lane_of(metric: &Metric, lanes: usize) -> usize {
    metric.context().name().bytes().map(usize::from).sum::<usize>() % lanes
}

/// The batches of the sources that enter the metrics path after aggregation, which spread over the post-aggregation
/// lanes: those of OTLP and of the checks source, timestamped or not.
fn other_batches(feed: &str) -> Vec<Vec<Metric>> {
    match feed {
        "otlp_in.metrics" => vec![
            vec![
                Metric::counter(context("otel.requests", &["service:api"]), (1_700_000_100, 10.0)),
                Metric::gauge(context("otel.cpu", &["service:api"]), (1_700_000_100, 0.5)),
            ],
            vec![Metric::gauge(context("otel.memory", &[]), 64.0)],
            vec![Metric::distribution(context("otel.latency", &[]), (1_700_000_110, 2.0))],
        ],
        "checks_ipc_in.metrics" => vec![
            vec![Metric::gauge(context("check.uptime", &["check:disk"]), 1.0)],
            vec![Metric::counter(context("check.runs", &[]), (1_700_000_200, 1.0))],
        ],
        _ => Vec::new(),
    }
}

/// Runs a path of the configuration `layers` over the batches, the plan-driven one feeding each producer lane the
/// contexts [`lane_of`] gives it, and every other source its own batches, and returns what the outputs received.
async fn run(layers: &[&str], path: MetricsPath, lanes: usize) -> Vec<Delivery> {
    let system = layered_configuration_system(layers).await;
    let topology = TestTopology::start(&system, Options::new(path, Aggregation::Clocked), |_| {}).await;
    for feed in ["otlp_in.metrics", "checks_ipc_in.metrics"] {
        if topology.has_feed(feed) {
            for batch in other_batches(feed) {
                topology.feed(feed, batch);
            }
        }
    }
    for batch in batches() {
        match path {
            MetricsPath::Legacy => topology.feed("dsd_in.metrics", batch),
            MetricsPath::Plan => {
                let mut shares = vec![Vec::new(); lanes];
                for metric in batch {
                    let lane = lane_of(&metric, lanes);
                    shares[lane].push(metric);
                }
                for (lane, share) in shares.into_iter().enumerate().filter(|(_, share)| !share.is_empty()) {
                    let feed = if lanes == 1 {
                        "dsd_in.metrics".to_string()
                    } else {
                        format!("dsd_in.metrics_{}", lane)
                    };
                    topology.feed(&feed, share);
                }
            }
        }
    }
    topology.stop().await
}

#[tokio::test]
async fn producer_lanes_with_one_state_lane_deliver_what_the_legacy_topology_does() {
    for (name, lanes, yaml) in [
        ("three producer lanes, one post-aggregation lane", 3, THREE_ONE_ONE),
        ("three producer lanes, two post-aggregation lanes", 3, THREE_ONE_TWO),
        ("one producer lane, three post-aggregation lanes", 1, ONE_ONE_THREE),
        (
            "three producer lanes, two post-aggregation lanes, otlp and checks",
            3,
            THREE_ONE_TWO,
        ),
    ] {
        // OTLP and the checks source spread over the post-aggregation lanes in the last case.
        let others = match name.ends_with("otlp and checks") {
            true => OTLP_AND_CHECKS,
            false => "",
        };
        let legacy = run(&[FLUSH_PASSTHROUGH_ON_STOP, others], MetricsPath::Legacy, 1).await;
        let plan = run(&[FLUSH_PASSTHROUGH_ON_STOP, others, yaml], MetricsPath::Plan, lanes).await;
        assert!(!plan.is_empty(), "{}: delivered nothing", name);
        if !others.is_empty() {
            let names = delivered_names(&plan);
            for name in ["otel.requests", "otel.memory", "check.uptime", "check.runs"] {
                assert!(
                    names.iter().any(|delivered| delivered == name),
                    "`{}` was not delivered: {:?}",
                    name,
                    names
                );
            }
        }
        if let Err(mismatch) = compare(&output_points(&legacy), &output_points(&plan)) {
            panic!(
                "{}: the plan-driven topology on lanes differs from the legacy one:\n{}",
                name, mismatch
            );
        }
    }
}

#[tokio::test]
async fn producer_lanes_and_state_lanes_deliver_what_the_legacy_topology_does() {
    for state in [2, 4] {
        for post in [1, 4] {
            let yaml = format!(
                "dogstatsd_workers_count: 4\nexperimental:\n  programmable_metrics:\n    lanes: {{mode: auto, \
                 state: {}, post: {}}}",
                state, post
            );
            let legacy = run(&[FLUSH_PASSTHROUGH_ON_STOP], MetricsPath::Legacy, 1).await;
            let plan = run(&[FLUSH_PASSTHROUGH_ON_STOP, &yaml], MetricsPath::Plan, 4).await;
            if let Err(mismatch) = compare(&output_points(&legacy), &output_points(&plan)) {
                panic!(
                    "P = {}, M = {}: the plan-driven topology on lanes differs:\n{}",
                    state, post, mismatch
                );
            }
        }
    }
}

#[tokio::test]
async fn a_context_dump_on_producer_lanes_reads_the_contexts_aggregation_holds() {
    let system = layered_configuration_system(&[THREE_ONE_TWO]).await;
    let topology = TestTopology::start(&system, Options::new(MetricsPath::Plan, Aggregation::Native), |_| {}).await;
    topology.feed(
        "dsd_in.metrics_0",
        vec![Metric::counter(context("dump.first", &[]), 1.0)],
    );
    topology.feed(
        "dsd_in.metrics_2",
        vec![Metric::counter(context("dump.second", &[]), 1.0)],
    );
    let names = loop {
        let entries = topology.context_dump().await.expect("aggregation answers");
        let names = entries
            .iter()
            .map(|entry| entry.context().name().to_string())
            .collect::<BTreeSet<_>>();
        if names.len() == 2 {
            break names;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(names, ["dump.first", "dump.second"].map(String::from).into());
    topology.stop().await;
}

#[tokio::test]
async fn the_lanes_publish_what_each_received_by_component_and_lane() {
    let recorder = TestRecorder::default();
    let system = layered_configuration_system(&[THREE_ONE_TWO]).await;
    let counter = |name: &'static str, component: &str, lane: &str| {
        recorder.counter(metrics::Key::from_parts(
            name,
            &[("component_id", component.to_string()), ("lane", lane.to_string())],
        ))
    };
    {
        let _recorder = metrics::set_default_local_recorder(&recorder);
        let mut topology =
            TestTopology::start(&system, Options::new(MetricsPath::Plan, Aggregation::Native), |_| {}).await;
        // Timestamped counters pass aggregation by, so they are delivered while the topology runs.
        let metric = Metric::counter(context("lane.one", &[]), (1_700_000_000, 1.0));
        topology.feed("dsd_in.metrics_1", vec![metric; 3]);
        topology
            .wait_for("the counters", |deliveries| {
                deliveries.iter().map(|delivery| delivery.metrics.len()).sum::<usize>() >= 3
            })
            .await;
        // The lanes' series are published once a second.
        let published = tokio::time::timeout(PATIENCE, async {
            while counter(PLAN_LANE_EVENTS_TOTAL, "dsd_pre_aggregation", "1") != Some(3) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(published.is_ok(), "lane 1's events are published");
        topology.stop().await;
    }
    // Lane 1 of the producer lanes received the batch fed to it, and the other producer lanes nothing.
    assert_eq!(counter(PLAN_LANE_EVENTS_TOTAL, "dsd_pre_aggregation", "1"), Some(3));
    assert!(counter(PLAN_LANE_BATCHES_TOTAL, "dsd_pre_aggregation", "1").is_some_and(|batches| batches >= 1));
    assert!(counter(PLAN_LANE_BUSY_NANOS_TOTAL, "dsd_pre_aggregation", "1").is_some_and(|nanos| nanos > 0));
    assert_eq!(counter(PLAN_LANE_EVENTS_TOTAL, "dsd_pre_aggregation", "0"), Some(0));
    // Every lane of every slot has its series.
    for (component, lanes) in [("dsd_pre_aggregation", 3), ("dsd_agg", 1), ("metrics_enrich", 2)] {
        for lane in 0..lanes {
            assert!(
                counter(PLAN_LANE_EVENTS_TOTAL, component, &lane.to_string()).is_some(),
                "`{}` lane {}",
                component,
                lane
            );
        }
    }
}

#[tokio::test]
async fn running_state_lanes_publish_what_their_keyed_exits_route_and_their_state_owners_hold() {
    // Three producer lanes route 60 counters of their own by key to two state lanes: once the series are published,
    // the producer lanes' routed events add up to what they routed, every counter is held by one state lane, and no
    // state lane admitted one another lane owns.
    const CONTEXTS: usize = 60;
    let recorder = TestRecorder::default();
    let system = layered_configuration_system(&[THREE_TWO_TWO]).await;
    let sum = |name: &'static str, component: &str, gauge: bool| -> f64 {
        (0..3)
            .map(|lane| {
                let key = metrics::Key::from_parts(
                    name,
                    &[("component_id", component.to_string()), ("lane", lane.to_string())],
                );
                if gauge {
                    recorder.gauge(key).unwrap_or_default()
                } else {
                    recorder.counter(key).unwrap_or_default() as f64
                }
            })
            .sum()
    };
    {
        let _recorder = metrics::set_default_local_recorder(&recorder);
        let topology =
            TestTopology::start(&system, Options::new(MetricsPath::Plan, Aggregation::Clocked), |_| {}).await;
        for lane in 0..3 {
            let batch = (0..CONTEXTS / 3)
                .map(|index| {
                    Metric::counter(
                        Context::from_parts(format!("routed.{}.{}", lane, index), TagSet::default()),
                        1.0,
                    )
                })
                .collect::<Vec<_>>();
            topology.feed(&format!("dsd_in.metrics_{}", lane), batch);
        }
        let published = tokio::time::timeout(PATIENCE, async {
            while sum(PLAN_LANE_ROUTED_EVENTS_TOTAL, "dsd_pre_aggregation", false) < CONTEXTS as f64
                || sum(PLAN_LANE_CONTEXTS, "dsd_agg", true) < CONTEXTS as f64
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(
            published.is_ok(),
            "routed {} and held {} of {} counters",
            sum(PLAN_LANE_ROUTED_EVENTS_TOTAL, "dsd_pre_aggregation", false),
            sum(PLAN_LANE_CONTEXTS, "dsd_agg", true),
            CONTEXTS
        );
        topology.stop().await;
    }
    assert_eq!(
        sum(PLAN_LANE_ROUTED_EVENTS_TOTAL, "dsd_pre_aggregation", false),
        CONTEXTS as f64
    );
    assert!(sum(PLAN_LANE_ROUTED_BUFFERS_TOTAL, "dsd_pre_aggregation", false) >= 2.0);
    assert_eq!(sum("plan_lane_misrouted_total", "dsd_agg", false), 0.0);
    // Both state lanes hold some of them: the keys spread over both lanes' groups.
    let held = |lane: u16| {
        recorder
            .gauge(metrics::Key::from_parts(
                PLAN_LANE_CONTEXTS,
                &[("component_id", "dsd_agg".to_string()), ("lane", lane.to_string())],
            ))
            .unwrap_or_default()
    };
    assert!(held(0) > 0.0 && held(1) > 0.0, "{} and {}", held(0), held(1));
}

#[tokio::test]
async fn every_lane_publishes_its_counters_as_counters_and_routing_and_contexts_where_its_slot_routes_or_holds_state() {
    use std::sync::atomic::Ordering;

    let built = built(THREE_TWO_TWO).await;
    let recorder = TestRecorder::default();
    let slot = |component: &str| {
        built
            .lane_stats
            .iter()
            .find(|slot| slot.component == component)
            .unwrap_or_else(|| panic!("no statistics of `{}`", component))
    };
    // Pre-aggregation's keyed exits route to aggregation's two state lanes, which hold its contexts.
    let (pre, agg) = (slot("dsd_pre_aggregation"), slot("dsd_agg"));
    assert!(pre.routes && !pre.holds_state, "{:?}", pre);
    assert!(!agg.routes && agg.holds_state, "{:?}", agg);
    assert!(!slot("metrics_enrich").routes && !slot("metrics_enrich").holds_state);
    {
        let _recorder = metrics::set_default_local_recorder(&recorder);
        let telemetry = LaneTelemetry::new(&built.lane_stats);
        // What a lane's driver counts, as its keyed exits route and its state owners hold.
        pre.drivers[1].routed_buffers.fetch_add(3, Ordering::Relaxed);
        pre.drivers[1].routed_events.fetch_add(1_500, Ordering::Relaxed);
        pre.drivers[1].progress.fetch_add(2, Ordering::Relaxed);
        agg.drivers[1].contexts.store(42.0f64.to_bits(), Ordering::Relaxed);
        telemetry.publish();
        // A second publication adds only what was counted since.
        pre.drivers[1].routed_buffers.fetch_add(1, Ordering::Relaxed);
        telemetry.publish();
    }
    let key = |name: &'static str, component: &str, lane: u16| {
        metrics::Key::from_parts(
            name,
            &[("component_id", component.to_string()), ("lane", lane.to_string())],
        )
    };
    let counter = |name, component, lane| recorder.counter(key(name, component, lane));
    let gauge = |name, component, lane| recorder.gauge(key(name, component, lane));
    assert_eq!(
        counter(PLAN_LANE_ROUTED_BUFFERS_TOTAL, "dsd_pre_aggregation", 1),
        Some(4)
    );
    assert_eq!(
        counter(PLAN_LANE_ROUTED_EVENTS_TOTAL, "dsd_pre_aggregation", 1),
        Some(1_500)
    );
    assert_eq!(counter(PLAN_LANE_PROGRESS_TOTAL, "dsd_pre_aggregation", 1), Some(2));
    assert_eq!(
        counter(PLAN_LANE_ROUTED_BUFFERS_TOTAL, "dsd_pre_aggregation", 0),
        Some(0)
    );
    assert_eq!(gauge(PLAN_LANE_CONTEXTS, "dsd_agg", 1), Some(42.0));
    assert_eq!(gauge(PLAN_LANE_CONTEXTS, "dsd_agg", 0), Some(0.0));
    // Only the lanes that route have routing series, and only the state lanes count contexts.
    assert_eq!(counter(PLAN_LANE_ROUTED_BUFFERS_TOTAL, "dsd_agg", 0), None);
    assert_eq!(gauge(PLAN_LANE_CONTEXTS, "dsd_pre_aggregation", 0), None);
    assert_eq!(gauge(PLAN_LANE_CONTEXTS, "metrics_enrich", 0), None);
    // Every lane of every slot has its host's series: the times it measures as counters of nanoseconds, its input's
    // depth as a gauge.
    for (component, lanes) in [("dsd_pre_aggregation", 3), ("dsd_agg", 2), ("metrics_enrich", 2)] {
        for lane in 0..lanes {
            for name in [
                PLAN_LANE_BATCHES_TOTAL,
                PLAN_LANE_EVENTS_TOTAL,
                PLAN_LANE_BUSY_NANOS_TOTAL,
                PLAN_LANE_STAMP_WAIT_NANOS_TOTAL,
            ] {
                assert!(
                    counter(name, component, lane).is_some(),
                    "`{}` of `{}` lane {}",
                    name,
                    component,
                    lane
                );
                assert!(gauge(name, component, lane).is_none(), "`{}` is a counter", name);
            }
            assert!(gauge(PLAN_LANE_INPUT_DEPTH, component, lane).is_some());
        }
    }
}

/// Waits until the installer's active generation is `generation` and the only one alive, with nothing preparing or
/// pending.
async fn settled_on(topology: &TestTopology, generation: u64) {
    let status = topology.status().expect("the plan-driven path has an installer");
    let settled = tokio::time::timeout(PATIENCE, async {
        loop {
            let status = status.load();
            let installer = &status.installer;
            let active = installer.active.as_ref().map(|active| active.id.get());
            let live = installer.live.iter().map(|id| id.get()).collect::<Vec<_>>();
            if active == Some(generation)
                && live == [generation]
                && installer.preparing.is_none()
                && installer.pending.is_none()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "generation {} never settled: {}",
        generation,
        status.load()
    );
}

/// Returns the names of the metrics the main output received, in arrival order.
fn delivered_names(deliveries: &[Delivery]) -> Vec<String> {
    deliveries
        .iter()
        .filter(|delivery| delivery.output == "dd_metrics_encode")
        .flat_map(|delivery| {
            delivery
                .metrics
                .iter()
                .map(|metric| metric.context().name().to_string())
        })
        .collect()
}

#[tokio::test]
async fn a_live_change_prepares_every_lane_and_the_replaced_generation_retires_from_all_of_them() {
    use serde_json::json;

    use crate::{
        internal::ConfigUpdatesWorker,
        programmable_metrics::test_support::{connected_configuration_system, update},
    };

    let (system, updates, stream) = connected_configuration_system(THREE_ONE_TWO).await;
    let mut topology = TestTopology::start(
        &system,
        Options::new(MetricsPath::Plan, Aggregation::Native),
        |supervisor| supervisor.add_worker(ConfigUpdatesWorker::new(updates)),
    )
    .await;
    settled_on(&topology, 1).await;
    let timestamped = |name: &'static str| Metric::counter(context(name, &[]), (1_700_000_000, 1.0));

    // Before the change, every producer lane delivers what it is fed.
    for lane in 0..3 {
        topology.feed(&format!("dsd_in.metrics_{}", lane), vec![timestamped("before.blocked")]);
    }
    topology
        .wait_for("the metrics before the change", |deliveries| {
            delivered_names(deliveries).len() == 3
        })
        .await;

    // A filterlist edit installs generation 2, which every lane of every slot prepares; generation 1 retires once its
    // last batch has left every lane.
    stream
        .send(update("metric_filterlist", json!(["after.blocked"])))
        .await
        .unwrap();
    settled_on(&topology, 2).await;

    // Every producer lane now runs generation 2's filter.
    for lane in 0..3 {
        topology.feed(
            &format!("dsd_in.metrics_{}", lane),
            vec![timestamped("after.blocked"), timestamped("after.kept")],
        );
    }
    topology
        .wait_for("the metrics after the change", |deliveries| {
            delivered_names(deliveries).len() == 6
        })
        .await;
    let deliveries = topology.stop().await;
    let names = delivered_names(&deliveries);
    assert_eq!(
        names.iter().filter(|name| name.as_str() == "after.kept").count(),
        3,
        "{:?}",
        names
    );
    assert!(!names.iter().any(|name| name == "after.blocked"), "{:?}", names);
    // What the lanes delivered after the change carries generation 2.
    let generations = deliveries
        .iter()
        .filter(|delivery| {
            delivery
                .metrics
                .iter()
                .any(|metric| metric.context().name() == "after.kept")
        })
        .map(|delivery| delivery.generation)
        .collect::<BTreeSet<_>>();
    assert_eq!(generations, [Some(2)].into());
}

#[tokio::test]
#[should_panic(expected = "stands in for one state lane")]
async fn a_test_stand_in_for_aggregation_is_refused_on_several_state_lanes() {
    // Gated and scripted aggregation replace the slot with one transform, which can neither take routed buffers on two
    // lanes' channels nor stamp two lanes' output points: the test topology says so rather than misroute.
    let system = configuration_system(THREE_TWO_TWO).await;
    drop(TestTopology::start(&system, Options::new(MetricsPath::Plan, Aggregation::Gated), |_| {}).await);
}

#[tokio::test]
async fn by_default_the_metrics_path_runs_on_the_automatic_lanes_without_r1s_pipelining() {
    // Increment 9.5: with only the decoder count set, three decoders on the tests' four workers give three producer
    // lanes, two state lanes, and three post-aggregation lanes, laid out without R1's pipelining.
    let system = configuration_system_with_default_lanes("dogstatsd_workers_count: 3").await;
    let config = system.config();
    let settings = LaneSettings::from_config(&config, TEST_WORKERS);
    let expected = LaneCounts::on(LanePolicy {
        dogstatsd: 3,
        state: 2,
        post: 3,
    });
    assert_eq!(settings.counts, expected);
    assert!(!settings.explicit_state && !settings.explicit_post);
    let cell = ConfigCell::new(config.as_ref().clone());
    let registry = ComponentRegistry::default();
    let mut metrics = programmable_metrics(&config, cell.live(), &registry).expect("a metrics pipeline is required");
    assert_eq!(metrics.boot_lanes(), expected);
    let frame = &metrics.boot_topology().frame;
    let lanes = |name: &str| {
        frame
            .slots()
            .iter()
            .find(|slot| slot.name.as_str() == name)
            .map(|slot| slot.lanes)
    };
    assert_eq!(
        (lanes("dsd_pre_aggregation"), lanes("dsd_agg"), lanes("metrics_enrich")),
        (Some(3), Some(2), Some(3))
    );
}
