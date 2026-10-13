//! Region drivers in frames on lanes: keyed exits and the progress of several upstream points, what runs at one lane,
//! and what is refused until the lane parts are built.
//!
//! The default program is laid out without pipelining, on two state lanes and one lane elsewhere, so that the
//! pre-aggregation slot's exit into aggregation is a keyed exit.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use saluki_core::{
    data_model::{
        event::{
            metric::{context::Context, Metric},
            Event,
        },
        tags::TagSet,
    },
    execution::{
        AdmissionPointId, AdmissionSchedule, AdmissionTime, BatchHeader, Budget, DrainPolicy, ExitSink, GenerationId,
        GenerationRef, GenerationToken, LaneContext, LaneSplit, Progress, RegionDriver, RegionTurn,
    },
    topology::EventsBuffer,
};
use saluki_metrics::Counter;

use super::{harness::RecordingTelemetry, state::lanes::SlotLanes, *};
use crate::{
    compile::{
        compile, CompiledPlan, ExitRouting, Frame, FrameHosts, FrameOptions, FrameTarget, LanePolicy, RegionProgram,
        Slot,
    },
    ir::{NativeComponentId, NodeId, Plan},
    limits::values::COALESCE_HOLD,
    operators::{KernelBuildContext, OperatorRegistry},
    testing::{default_program_catalog, DEFAULT_PROGRAM_JSON},
    validate::Validator,
};

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    crate::testing::default_program_slot_name(nodes)
}

/// The default program, its frame on `lanes` without pipelining, and its programs compiled into that frame.
fn laid_out(lanes: LanePolicy) -> (Frame, CompiledPlan) {
    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let registry = OperatorRegistry::native();
    let frame = Frame::compute_with(
        &plan,
        &registry,
        &namer,
        &FrameHosts::new(),
        &lanes,
        FrameOptions { pipelining: false },
    )
    .unwrap();
    let report = Validator::new(&registry, &default_program_catalog()).validate(&plan);
    assert!(report.is_valid(), "{}", report);
    let compiled = compile(&plan, &report, &registry, &frame).unwrap();
    (frame, compiled)
}

fn slot<'f>(frame: &'f Frame, name: &str) -> &'f Slot {
    frame
        .slots()
        .iter()
        .find(|slot| slot.name.as_str() == name)
        .unwrap_or_else(|| panic!("no slot `{}`", name))
}

fn generation() -> GenerationRef {
    generation_of(1)
}

fn generation_of(id: u64) -> GenerationRef {
    Arc::new(GenerationToken::new(GenerationId::new(id)))
}

fn built(program: &Arc<RegionProgram>) -> Vec<PreparedKernel> {
    let keys = program
        .kernels()
        .iter()
        .map(|kernel| kernel.key.clone())
        .collect::<Vec<_>>();
    build_kernels(program, &keys, KernelBuildContext::detached).unwrap()
}

fn boot(program: &Arc<RegionProgram>, generation: &GenerationRef, telemetry: &RecordingTelemetry) -> PrepareRegion {
    PrepareRegion::new(
        Arc::clone(generation),
        Arc::clone(program),
        built(program),
        PanicBudget::Zero,
        telemetry,
    )
}

fn with_schedule(config: RegionConfig, generation: &GenerationRef) -> RegionConfig {
    config.with_schedule(AdmissionSchedule::Steady(Arc::clone(generation)).into_handle())
}

/// A batch of `metrics`, stamped by `point` at `millis` milliseconds in `epoch`, of `generation`.
fn stamped_at(generation: &GenerationRef, point: u32, epoch: u32, millis: u64, metrics: Vec<Metric>) -> EventsBuffer {
    let mut buffer = EventsBuffer::default().with_header(BatchHeader {
        admitted_at: Some(AdmissionTime::from_unix_nanos(millis * 1_000_000)),
        admission_point: Some(AdmissionPointId::new(point)),
        epoch,
        generation: Some(Arc::clone(generation)),
        entry: None,
    });
    for metric in metrics {
        assert!(buffer.try_push(Event::Metric(metric)).is_none());
    }
    buffer
}

/// Counters without timestamps, which the default program aggregates.
fn counters(names: &[String]) -> Vec<Metric> {
    names
        .iter()
        .map(|name| Metric::counter(Context::from_parts(name.clone(), TagSet::default()), 1.0))
        .collect()
}

/// A counter with a timestamp, which the default program passes through without aggregating it.
fn timestamped() -> Vec<Metric> {
    vec![Metric::counter(
        Context::from_parts("late".to_string(), TagSet::default()),
        (1_000, 1.0),
    )]
}

fn names(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("m{}", index)).collect()
}

/// What a call wrote: the exit, the lane of the exit's target, if the driver named one, and the buffer, in order.
type Written = Vec<(usize, Option<u16>, EventsBuffer)>;

fn process_at(driver: &mut MetricRegionDriver, exits: usize, batch: EventsBuffer, now: Instant) -> Written {
    let mut sink = ExitSink::new(exits);
    let mut budget = Budget::unlimited();
    let progress = driver.process(&mut RegionTurn::new(now, &mut budget), batch, &mut sink);
    assert_eq!(progress, Progress::Done);
    sink.drain_routed()
        .map(|(exit, lane, buffer)| (exit.get(), lane, buffer))
        .collect()
}

fn process(driver: &mut MetricRegionDriver, exits: usize, batch: EventsBuffer) -> Written {
    process_at(driver, exits, batch, Instant::now())
}

fn end_turn_at(driver: &mut MetricRegionDriver, exits: usize, now: Instant) -> (Option<Instant>, Written) {
    let mut sink = ExitSink::new(exits);
    let mut budget = Budget::unlimited();
    let next = driver.end_turn(&mut RegionTurn::new(now, &mut budget), true, &mut sink);
    let written = sink
        .drain_routed()
        .map(|(exit, lane, buffer)| (exit.get(), lane, buffer))
        .collect();
    (next, written)
}

fn finish(driver: &mut MetricRegionDriver, exits: usize) -> Written {
    let mut sink = ExitSink::new(exits);
    let mut budget = Budget::unlimited();
    let progress = driver.finish(
        &mut RegionTurn::new(Instant::now(), &mut budget),
        &mut sink,
        DrainPolicy::Shutdown,
    );
    assert_eq!(progress, Progress::Done);
    sink.drain_routed()
        .map(|(exit, lane, buffer)| (exit.get(), lane, buffer))
        .collect()
}

/// What a test compares of the buffers one exit received: the lane, the stamp in milliseconds, the epoch, the
/// generation, and the number of events.
fn on_exit(written: &[(usize, Option<u16>, EventsBuffer)], exit: usize) -> Vec<(Option<u16>, u64, u32, u64, usize)> {
    written
        .iter()
        .filter(|(written, ..)| *written == exit)
        .map(|(_, lane, buffer)| {
            let header = buffer.header();
            (
                *lane,
                header.admitted_at.unwrap().as_unix_nanos() / 1_000_000,
                header.epoch,
                header.generation_id().unwrap().get(),
                buffer.len(),
            )
        })
        .collect()
}

/// The pre-aggregation slot's region on two state lanes, with its keyed exit into aggregation.
fn keyed_region(
    routing: RoutingTelemetry,
) -> (
    MetricRegionDriver,
    usize,
    usize,
    Arc<crate::compile::KeyGroups>,
    GenerationRef,
) {
    let (frame, compiled) = laid_out(LanePolicy {
        dogstatsd: 1,
        state: 2,
        post: 1,
    });
    let pre = slot(&frame, "dsd_pre_aggregation");
    let agg = slot(&frame, "dsd_agg");
    let keyed = pre.exit_to(&FrameTarget::Slot(agg.id)).unwrap();
    let ExitRouting::Keyed(groups) = &keyed.routing else {
        panic!("the exit into aggregation is keyed");
    };
    let generation = generation();
    let telemetry = RecordingTelemetry::default();
    let driver = MetricRegionDriver::new(
        RegionConfig::for_slot(pre, &telemetry).with_routing_telemetry(routing),
        boot(&compiled.programs[pre.id.get()], &generation, &telemetry),
    )
    .unwrap();
    assert_eq!(driver.lane(), LaneContext::ONE);
    (
        driver,
        pre.exits.len(),
        keyed.index.get(),
        Arc::clone(groups),
        generation,
    )
}

// Building the pre-aggregation kernels starts their caches' expiration tasks, which need a runtime.
#[tokio::test]
async fn a_keyed_exit_routes_each_event_to_the_state_lane_that_owns_its_context_key() {
    let (mut driver, exits, keyed, groups, generation) = keyed_region(RoutingTelemetry::noop());

    // Fifty contexts reach both lanes, and both hold what they received open: nothing goes out yet.
    let names = names(50);
    let written = process(
        &mut driver,
        exits,
        stamped_at(&generation, 0, 0, 1_000_000, counters(&names)),
    );
    assert!(on_exit(&written, keyed).is_empty(), "{:?}", on_exit(&written, keyed));

    // A heartbeat sends every open buffer, then its progress to every lane.
    let written = process(&mut driver, exits, stamped_at(&generation, 0, 0, 1_000_200, Vec::new()));
    let routed = written.iter().filter(|(exit, ..)| *exit == keyed).collect::<Vec<_>>();
    let mut events = 0;
    for (_, lane, buffer) in routed.iter().filter(|(.., buffer)| !buffer.is_empty()) {
        let lane = lane.expect("a keyed exit names the lane of every buffer");
        assert_eq!(buffer.header().admission_point, Some(AdmissionPointId::new(0)));
        for event in buffer {
            let key = event.try_as_metric().unwrap().context().key().as_u64();
            assert_eq!(groups.lane_of(key), lane, "each event reaches the lane owning its key");
            events += 1;
        }
    }
    assert_eq!(events, 50, "every event reaches aggregation once");
    assert_eq!(
        on_exit(&written, keyed)
            .into_iter()
            .map(|(lane, at, _, _, events)| (lane, at, events > 0))
            .collect::<Vec<_>>(),
        vec![
            (Some(0), 1_000_000, true),
            (Some(1), 1_000_000, true),
            (Some(0), 1_000_200, false),
            (Some(1), 1_000_200, false),
        ],
        "fifty contexts reach both lanes, before the heartbeat"
    );
    let (next, written) = end_turn_at(&mut driver, exits, Instant::now());
    assert_eq!((next, written.len()), (None, 0), "nothing is held after a heartbeat");
}

#[tokio::test]
async fn a_keyed_exit_sends_what_it_holds_once_the_hold_expires_and_when_its_region_finishes() {
    let (mut driver, exits, keyed, groups, generation) = keyed_region(RoutingTelemetry::noop());
    let start = Instant::now();

    let written = process_at(
        &mut driver,
        exits,
        stamped_at(&generation, 0, 0, 1_000_000, counters(&names(50))),
        start,
    );
    assert!(on_exit(&written, keyed).is_empty());
    let (next, written) = end_turn_at(&mut driver, exits, start + Duration::from_millis(1));
    assert_eq!((next, written.len()), (Some(start + COALESCE_HOLD), 0));
    let (next, written) = end_turn_at(&mut driver, exits, start + COALESCE_HOLD);
    assert_eq!(next, None);
    assert_eq!(
        on_exit(&written, keyed)
            .into_iter()
            .map(|(lane, at, ..)| (lane, at))
            .collect::<Vec<_>>(),
        vec![(Some(0), 1_000_000), (Some(1), 1_000_000)]
    );

    // At finish, what is open goes out, then the point's last stamp to every lane that lacks it.
    process(
        &mut driver,
        exits,
        stamped_at(&generation, 0, 0, 1_000_010, counters(&names(1))),
    );
    process(
        &mut driver,
        exits,
        stamped_at(&generation, 0, 0, 1_000_020, timestamped()),
    );
    let written = finish(&mut driver, exits);
    // The program renames the context before the exit, so its lane is the one its key at the exit gives.
    let (_, lane, buffer) = written
        .iter()
        .find(|(exit, _, buffer)| *exit == keyed && !buffer.is_empty())
        .unwrap();
    let key = buffer
        .into_iter()
        .next()
        .unwrap()
        .try_as_metric()
        .unwrap()
        .context()
        .key()
        .as_u64();
    assert_eq!(*lane, Some(groups.lane_of(key)));
    assert_eq!(
        on_exit(&written, keyed),
        vec![
            (*lane, 1_000_010, 0, 1, 1),
            (Some(0), 1_000_020, 0, 1, 0),
            (Some(1), 1_000_020, 0, 1, 0)
        ]
    );
    assert!(finish(&mut driver, exits).is_empty(), "finishing again sends nothing");
}

#[tokio::test]
async fn a_keyed_exit_counts_what_it_routes_and_the_progress_it_sends_when_its_region_publishes() {
    let cells = [(); 3].map(|_| Arc::new(AtomicU64::new(0)));
    let routing = RoutingTelemetry {
        routed_buffers: Counter::from_arc(Arc::clone(&cells[0])),
        routed_events: Counter::from_arc(Arc::clone(&cells[1])),
        progress: Counter::from_arc(Arc::clone(&cells[2])),
    };
    let values = || cells.each_ref().map(|cell| cell.load(Ordering::Relaxed));
    let (mut driver, exits, _, _, generation) = keyed_region(routing);

    process(
        &mut driver,
        exits,
        stamped_at(&generation, 0, 0, 1_000_000, counters(&names(50))),
    );
    process(&mut driver, exits, stamped_at(&generation, 0, 0, 1_000_200, Vec::new()));
    assert_eq!(values(), [0, 0, 0], "nothing is counted until the region publishes");
    driver.publish_telemetry();
    assert_eq!(
        values(),
        [2, 50, 2],
        "two routed buffers of fifty events, then the heartbeat to each lane"
    );
    driver.publish_telemetry();
    assert_eq!(values(), [2, 50, 2], "what was published is not published again");
}

/// The first lane's region of the pre-aggregation slot on `producers` lanes, with one state lane, prepared for `boot`:
/// its exit into aggregation forwards the progress of every producer lane's point.
fn producer_lane_region(
    producers: u16, lane: u16, boot_generation: &GenerationRef,
) -> (MetricRegionDriver, usize, usize, Arc<RegionProgram>, RecordingTelemetry) {
    let (frame, compiled) = laid_out(LanePolicy {
        dogstatsd: producers,
        state: 1,
        post: 1,
    });
    let pre = slot(&frame, "dsd_pre_aggregation");
    let agg = slot(&frame, "dsd_agg");
    let exit = pre.exit_to(&FrameTarget::Slot(agg.id)).unwrap();
    assert_eq!(exit.routing, ExitRouting::One);
    let points = (0..u32::from(producers)).map(AdmissionPointId::new).collect::<Vec<_>>();
    assert_eq!(exit.progress_points, points);
    let telemetry = RecordingTelemetry::default();
    let program = Arc::clone(&compiled.programs[pre.id.get()]);
    let mut config = RegionConfig::for_slot(pre, &telemetry);
    if producers > 1 {
        let lane = LaneContext {
            index: lane,
            count: producers,
        };
        config = config.with_lane(lane, SlotLanes::new(producers));
    }
    let driver = MetricRegionDriver::new(config, boot(&program, boot_generation, &telemetry)).unwrap();
    (driver, pre.exits.len(), exit.index.get(), program, telemetry)
}

#[tokio::test]
async fn an_exit_toward_a_state_owner_with_several_upstream_points_sends_their_final_headers_and_generation_changes() {
    // Lane 0 of one producer lane, which stamps point 0, and lanes 0 and 1 of two, which stamp points 0 and 1: each
    // lane's program has an entry for its own point.
    for (producers, lane) in [(1, 0), (2, 0), (2, 1)] {
        let several = producers > 1;
        let point = u32::from(lane);
        let stamped_at = |generation: &GenerationRef, epoch: u32, millis: u64, metrics: Vec<Metric>| {
            stamped_at(generation, point, epoch, millis, metrics)
        };
        let (old, new) = (generation(), generation_of(2));
        let (mut driver, exits, agg, program, telemetry) = producer_lane_region(producers, lane, &old);
        let mut budget = Budget::unlimited();
        let mut sink = ExitSink::new(exits);
        driver
            .control(
                &mut RegionTurn::new(Instant::now(), &mut budget),
                RegionControl::Prepare(Box::new(boot(&program, &new, &telemetry))),
                &mut sink,
            )
            .unwrap();
        let one = names(1);

        let written = process(&mut driver, exits, stamped_at(&old, 0, 1_000_000, counters(&one)));
        assert_eq!(on_exit(&written, agg), vec![(None, 1_000_000, 0, 1, 1)]);
        // A batch that leaves by the passthrough only is within the throttle.
        let written = process(&mut driver, exits, stamped_at(&old, 0, 1_000_050, timestamped()));
        assert_eq!(on_exit(&written, agg), vec![]);

        // The clock resets: with several upstream points, the point's final header of epoch 0 goes first.
        let written = process(&mut driver, exits, stamped_at(&old, 1, 600_000, counters(&one)));
        let final_header = several.then_some((None, 1_000_050, 0, 1, 0));
        assert_eq!(
            on_exit(&written, agg),
            final_header
                .into_iter()
                .chain([(None, 600_000, 1, 1, 1)])
                .collect::<Vec<_>>(),
            "lane {} of {} producer lanes",
            lane,
            producers
        );

        // A generation change reaches the state owner at once with several upstream points, and within the
        // throttle, as in stage 6, with one.
        let written = process(&mut driver, exits, stamped_at(&new, 1, 600_010, timestamped()));
        let change = several.then_some((None, 600_010, 1, 2, 0));
        assert_eq!(on_exit(&written, agg), change.into_iter().collect::<Vec<_>>());
        let written = process(&mut driver, exits, stamped_at(&new, 1, 600_020, timestamped()));
        assert_eq!(on_exit(&written, agg), vec![]);

        // When input ends, the point's last header reaches the state owner with several upstream points.
        let last = several.then_some((None, 600_020, 1, 2, 0));
        assert_eq!(
            on_exit(&finish(&mut driver, exits), agg),
            last.into_iter().collect::<Vec<_>>()
        );
        assert_eq!(on_exit(&finish(&mut driver, exits), agg), vec![]);
    }
}

#[tokio::test]
async fn a_state_owner_runs_on_its_lane_with_kernels_built_for_that_lane() {
    let (frame, compiled) = laid_out(LanePolicy {
        dogstatsd: 1,
        state: 2,
        post: 1,
    });
    let agg = slot(&frame, "dsd_agg");
    let program = &compiled.programs[agg.id.get()];
    let generation = generation();
    let telemetry = RecordingTelemetry::default();
    let shared = SlotLanes::new(2);
    let lane = LaneContext { index: 1, count: 2 };
    let config =
        || with_schedule(RegionConfig::for_slot(agg, &telemetry), &generation).with_lane(lane, Arc::clone(&shared));
    // A state owner's kernel built without lanes would keep the rules that couple its lineage's contexts alone.
    let error = MetricRegionDriver::new(config(), boot(program, &generation, &telemetry))
        .unwrap_err()
        .to_string();
    assert!(error.contains("KernelBuildContext::with_lanes"), "{}", error);

    let cell = crate::operators::LaneCell::new();
    let keys = program
        .kernels()
        .iter()
        .map(|kernel| kernel.key.clone())
        .collect::<Vec<_>>();
    let kernels = build_kernels(program, &keys, |node| {
        KernelBuildContext::detached(node).with_lanes(crate::operators::LaneBinding {
            cell: Arc::clone(&cell),
            lane: lane.index,
            lanes: lane.count,
        })
    })
    .unwrap();
    let boot = PrepareRegion::new(
        Arc::clone(&generation),
        Arc::clone(program),
        kernels,
        PanicBudget::DEFAULT_PROGRAM,
        &telemetry,
    );
    let driver = MetricRegionDriver::new(config(), boot).unwrap();
    assert_eq!(driver.lane(), lane);
    assert!(
        cell.share().is_some(),
        "the lineage's kernel created its record in the cell"
    );
}

#[tokio::test]
async fn a_fused_slot_runs_on_its_lane_and_a_region_must_be_told_its_lane() {
    let (frame, compiled) = laid_out(LanePolicy {
        dogstatsd: 1,
        state: 1,
        post: 2,
    });
    let post = slot(&frame, "metrics_enrich");
    assert_eq!(post.lanes, 2);
    let generation = generation();
    let telemetry = RecordingTelemetry::default();
    let program = &compiled.programs[post.id.get()];
    let error = MetricRegionDriver::new(
        RegionConfig::for_slot(post, &telemetry),
        boot(program, &generation, &telemetry),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("its slot runs 2 lanes"), "{}", error);

    let shared = SlotLanes::new(2);
    let lane = LaneContext { index: 1, count: 2 };
    let driver = MetricRegionDriver::new(
        RegionConfig::for_slot(post, &telemetry).with_lane(lane, Arc::clone(&shared)),
        boot(program, &generation, &telemetry),
    )
    .unwrap();
    assert_eq!(driver.lane(), lane);
}

#[tokio::test]
async fn a_preparation_splits_into_one_per_lane_and_a_kernel_message_never_splits() {
    let (frame, compiled) = laid_out(LanePolicy::ONE);
    let pre = slot(&frame, "dsd_pre_aggregation");
    let program = &compiled.programs[pre.id.get()];
    let generation = generation();
    let telemetry = RecordingTelemetry::default();
    let shares = |count: usize| {
        (0..count)
            .map(|_| LanePreparation {
                kernels: built(program),
                state: Vec::new(),
            })
            .collect::<Vec<_>>()
    };

    let prepare = boot(program, &generation, &telemetry).with_lanes(shares(3));
    assert_eq!(prepare.lane_count(), 3);
    let parts = RegionControl::Prepare(Box::new(prepare)).split(3).unwrap();
    assert_eq!(parts.len(), 3);
    for part in &parts {
        let RegionControl::Prepare(part) = part else {
            panic!("a preparation splits into preparations");
        };
        assert_eq!(part.lane_count(), 0);
        assert_eq!(part.generation().id(), GenerationId::new(1));
    }
    // A preparation with shares for another number of lanes stays whole; one without shares splits for one lane.
    let mismatched = boot(program, &generation, &telemetry).with_lanes(shares(2));
    assert!(RegionControl::Prepare(Box::new(mismatched)).split(3).is_err());
    assert_eq!(
        RegionControl::Prepare(Box::new(boot(program, &generation, &telemetry)))
            .split(1)
            .unwrap()
            .len(),
        1
    );

    assert_eq!(
        RegionControl::Abort {
            generation: GenerationId::new(1)
        }
        .split(2)
        .unwrap()
        .len(),
        2
    );
    assert_eq!(RegionControl::Release.split(4).unwrap().len(), 4);
    let kernel = RegionControl::Kernel(KernelControl {
        target: KernelTarget::Stateful,
        lane: Some(1),
        message: Box::new(()),
        reply: Box::new(|_| {}),
    });
    assert!(kernel.split(2).is_err());

    // A lane given a preparation that still carries its shares prepares its own.
    let shared = SlotLanes::new(1);
    let driver = MetricRegionDriver::new(
        RegionConfig::for_slot(pre, &telemetry).with_lane(LaneContext::ONE, shared),
        boot(program, &generation, &telemetry).with_lanes(shares(1)),
    )
    .unwrap();
    assert_eq!(driver.generations(), vec![GenerationId::new(1)]);
}

#[test]
fn a_slot_with_a_keyed_exit_bounds_the_routed_buffer_it_keeps_open_for_each_state_lane() {
    use saluki_core::{
        accounting::{ComponentRegistry, MemoryBounds},
        support::SubsystemIdentifier,
    };

    let plan = Plan::from_json(DEFAULT_PROGRAM_JSON).unwrap();
    let registry = OperatorRegistry::native();
    let firm = |state: u16| {
        let (frame, compiled) = laid_out(LanePolicy {
            dogstatsd: 1,
            state,
            post: 1,
        });
        let pre = slot(&frame, "dsd_pre_aggregation");
        let bounds = RegionBounds::for_slot(&plan, &registry, &compiled.programs[pre.id.get()], pre).unwrap();
        let components = ComponentRegistry::default();
        bounds.specify_bounds(&mut components.bounds_builder(&SubsystemIdentifier::from_segments(["region"])));
        components.as_bounds().total_firm_limit_bytes()
    };
    let routed = |lanes: usize, events: usize| lanes * (size_of::<EventsBuffer>() + events * size_of::<Event>());
    let one = firm(1);
    // Two state lanes take 1,024 events each before they are sent, four take 512, and sixteen 256.
    assert_eq!(firm(2) - one, routed(2, 1_024));
    assert_eq!(firm(4) - one, routed(4, 512));
    assert_eq!(firm(16) - one, routed(16, 256));
}
