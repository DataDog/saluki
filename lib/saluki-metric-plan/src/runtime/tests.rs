//! Hand-written region driver tests: exits and entries, generations, kernel reuse, wiring faults, supervision,
//! telemetry, and bounds, each against expectations derived by hand from small plans.

use std::{
    num::NonZeroU64,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use saluki_core::{
    accounting::{ComponentRegistry, MemoryBounds, MemoryBoundsBuilder},
    data_model::{
        event::{
            metric::{context::Context, Metric, ScalarPoints},
            Event,
        },
        tags::TagSet,
    },
    execution::{
        AdmissionPointId, AdmissionTime, BatchHeader, Budget, EntryId, ExitSink, GenerationId, GenerationRef,
        GenerationToken, Kernel, LossClass, LossCount, PanicOutcome, PanicSite, Progress, RegionDriver, RegionPanic,
        RegionTurn, RetireNotifier, Turn,
    },
    support::SubsystemIdentifier,
    topology::EventsBuffer,
};
use saluki_metrics::{test::TestRecorder, MetricsBuilder};
use serde_json::{json, Value};

use super::{frame_tests::RecordingTelemetry, *};
use crate::{
    canonical::PlanHash,
    compile::{compile, CompiledPlan, EntryKey, Frame, FrameTarget, KernelKey, RegionProgram, SlotId},
    install::kernel_reuse,
    ir::{DecodedParams, NativeComponentId, NodeId, OutputId, ParamSpec, ParamType, Plan, RecipientId},
    model::{Field, FieldSet, PortType},
    operators::{
        names, EffectContract, Identity, KernelBuildContext, KernelBuildError, KernelSpec, Loss, LossDecl, OperatorDef,
        OperatorRegistry, PerKind, PortMapping, Ports,
    },
    predicate::{Predicate, StringMatch},
    testing::predicate_json,
    validate::{CatalogOutput, MemoryCatalog, Validator},
};

// A test operator whose kernel instances can be told apart.

const COUNT_PARAMS: &[ParamSpec] = &[ParamSpec::required("label", ParamType::String)];

/// `test_count`: renames every metric to `<name>.<label><n>`, where `n` counts the batches its kernel instance has
/// consumed, so that a test can tell instances apart by their output. It drops metrics named `shed` as capacity loss
/// and panics on a metric named `panic`. It declares 100 bytes as its minimum and 1,000 more as its firm limit. A
/// kernel labelled `unrebuildable` fails to build again after its first build, and one labelled `panicky` panics then.
struct TestCount;

impl OperatorDef for TestCount {
    fn name(&self) -> &'static str {
        "test_count"
    }

    fn summary(&self) -> &'static str {
        "Renames metrics after the batches its kernel has seen."
    }

    fn params(&self) -> &'static [ParamSpec] {
        COUNT_PARAMS
    }

    fn ports(&self, _params: &DecodedParams) -> Ports {
        Ports::single(PortType::ANY, PortMapping::IDENTITY)
    }

    fn contract(&self, _params: &DecodedParams) -> EffectContract {
        let name = FieldSet::of(&[Field::Name]);
        EffectContract {
            reads: PerKind::uniform(name),
            writes: PerKind::uniform(name),
            identity: PerKind::uniform(Identity::Injective { image: None }),
            loss: Loss::Lossy(vec![LossDecl {
                class: LossClass::Capacity,
                description: "metrics named `shed`",
            }]),
            ..EffectContract::passthrough()
        }
    }

    fn specify_bounds(&self, _params: &DecodedParams, builder: &mut MemoryBoundsBuilder) {
        builder.minimum().with_fixed_amount("count state", 100);
        builder.firm().with_fixed_amount("count buffers", 1_000);
    }

    fn kernel(&self, params: &DecodedParams) -> Option<KernelSpec> {
        let label = params.str("label")?.to_string();
        let builds = AtomicUsize::new(0);
        Some(KernelSpec::new(move |_| {
            if builds.fetch_add(1, Ordering::Relaxed) > 0 {
                match label.as_str() {
                    "unrebuildable" => {
                        return Err(KernelBuildError {
                            message: "test_count cannot be rebuilt".to_string(),
                        })
                    }
                    "panicky" => panic!("test_count panicked while rebuilding"),
                    _ => {}
                }
            }
            Ok(Box::new(CountKernel {
                label: label.clone(),
                batches: 0,
            }))
        }))
    }
}

struct CountKernel {
    label: String,
    batches: u64,
}

impl Kernel for CountKernel {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress {
        self.batches += 1;
        let mut shed = LossCount::default();
        input.retain(|event| {
            let Some(metric) = event.try_as_metric_mut() else {
                return true;
            };
            let name = metric.context().name().to_string();
            match name.as_str() {
                "panic" => panic!("test_count was told to panic"),
                "shed" => {
                    shed.events += 1;
                    shed.points += metric.values().len() as u64;
                    false
                }
                _ => {
                    let renamed = metric
                        .context()
                        .with_name(format!("{}.{}{}", name, self.label, self.batches));
                    *metric.context_mut() = renamed;
                    true
                }
            }
        });
        if shed.events > 0 {
            turn.counters()
                .record_loss(LossClass::Capacity, shed.events, shed.points);
        }
        Progress::Done
    }
}

fn registry() -> OperatorRegistry {
    static REGISTRY: OnceLock<OperatorRegistry> = OnceLock::new();
    *REGISTRY.get_or_init(|| {
        let operators = OperatorRegistry::native()
            .iter()
            .chain([&TestCount as &'static dyn OperatorDef])
            .collect::<Vec<_>>();
        OperatorRegistry::with_operators(Box::leak(operators.into_boxed_slice()))
    })
}

// Plans.

fn id(value: &str) -> NodeId {
    value.parse().unwrap()
}

/// A node with native provenance, so that lossy and host nodes have authority.
fn node(id: &str, op: &str, params: Value) -> Value {
    json!({"id": id, "op": op, "params": params, "provenance": {"native": "test"}})
}

fn prefix(value: &str) -> Value {
    predicate_json(&Predicate::Name(StringMatch::Prefix(value.to_string())))
}

fn fingerprint(output: &str) -> PlanHash {
    PlanHash::of_bytes(output.as_bytes())
}

fn plan(nodes: Vec<Value>, edges: &[(&str, &str)], outputs: &[&str]) -> Plan {
    let edges = edges
        .iter()
        .map(|(from, to)| json!({"from": from, "to": to}))
        .collect::<Vec<_>>();
    let outputs = outputs
        .iter()
        .map(|output| {
            json!({"id": output, "accepts": {"kinds": "all", "timestamps": "mixed"},
                   "fingerprint": fingerprint(output).to_string(), "recipients": [format!("out/{}", output)]})
        })
        .collect::<Vec<_>>();
    Plan::from_value(json!({"ir_version": 1, "nodes": nodes, "edges": edges, "outputs": outputs})).unwrap()
}

fn catalog() -> MemoryCatalog {
    ["kept", "out"]
        .into_iter()
        .fold(MemoryCatalog::new(), |catalog, output| {
            catalog.with_output(
                OutputId::new(output).unwrap(),
                CatalogOutput {
                    accepts: PortType::ANY,
                    fingerprint: fingerprint(output),
                    recipients: vec![RecipientId::new(format!("out/{}", output)).unwrap()],
                },
            )
        })
}

/// One slot: `src -> count (test_count, label) -> keep (split: name prefix) -> true: emit kept; false: drop dropped`.
///
/// The slot's program, by hand: register 0 holds the batch; `Map(count, r0)`; `Decide(keep, r0, [r1, drop dropped])`;
/// `Exit(r1, exit 0 = kept)`. Two registers.
fn filter_plan(label: &str, kept_prefix: &str) -> Plan {
    plan(
        vec![
            node(
                "src",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
            ),
            node("count", "test_count", json!({"label": label})),
            node("keep", names::SPLIT, json!({"predicate": prefix(kept_prefix)})),
            node("kept_out", names::EMIT, json!({"output": "kept"})),
            node("dropped", names::DROP, json!({})),
        ],
        &[
            ("src.metrics", "count.in"),
            ("count.out", "keep.in"),
            ("keep.true", "kept_out.in"),
            ("keep.false", "dropped.in"),
        ],
        &["kept"],
    )
}

/// Two slots: `src -> d (split: name prefix "a.") -> true: h (opaque host) -> m.0; false: m.1`, then
/// `m (merge) -> e (emit out)`. The merge is one fence deeper than the split, so `d` runs in `region_d` and `m` and
/// `e` in `region_m`, which `region_d` reaches through an exit.
fn detour_plan() -> Plan {
    let any = json!({"kinds": "all", "timestamps": "mixed"});
    plan(
        vec![
            node(
                "src",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
            ),
            node("d", names::SPLIT, json!({"predicate": prefix("a.")})),
            node(
                "h",
                names::OPAQUE_HOST,
                json!({"component": "h", "input": any, "outputs": {"out": any}}),
            ),
            node("m", names::MERGE, json!({"inputs": 2})),
            node("e", names::EMIT, json!({"output": "out"})),
        ],
        &[
            ("src.metrics", "d.in"),
            ("d.true", "h.in"),
            ("h.out", "m.0"),
            ("d.false", "m.1"),
            ("m.out", "e.in"),
        ],
        &["out"],
    )
}

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    NativeComponentId::new(format!("region_{}", nodes[0].local())).map_err(|error| error.to_string())
}

/// Computes the frame of a boot plan.
fn frame_of(plan: &Plan) -> Frame {
    Frame::compute(plan, &registry(), &namer).unwrap()
}

/// Compiles a plan into a frame.
fn compiled(plan: &Plan, frame: &Frame) -> CompiledPlan {
    let report = Validator::new(&registry(), &catalog()).validate(plan);
    assert!(report.is_valid(), "test plan is invalid:\n{}", report);
    compile(plan, &report, &registry(), frame).unwrap()
}

/// Returns a generation token that records its retirement in `retired`.
fn token(id: u64, retired: &Arc<Mutex<Vec<GenerationId>>>) -> GenerationRef {
    let retired = Arc::clone(retired);
    Arc::new(GenerationToken::with_retire_notifier(
        GenerationId::new(id),
        RetireNotifier::new(move |id| retired.lock().unwrap().push(id)),
    ))
}

/// Returns a preparation of `program` for `generation`, with `kernels` built.
fn prepare(
    generation: &GenerationRef, program: &Arc<RegionProgram>, kernels: &[KernelKey], budget: PanicBudget,
    telemetry: &dyn RegisterTelemetry,
) -> PrepareRegion {
    let built = build_kernels(program, kernels, KernelBuildContext::detached).unwrap();
    PrepareRegion::new(Arc::clone(generation), Arc::clone(program), built, budget, telemetry)
}

/// Returns a preparation of `program` for `generation` with every kernel built.
fn prepare_all(
    generation: &GenerationRef, program: &Arc<RegionProgram>, budget: PanicBudget, telemetry: &dyn RegisterTelemetry,
) -> PrepareRegion {
    let keys = program
        .kernels()
        .iter()
        .map(|kernel| kernel.key.clone())
        .collect::<Vec<_>>();
    prepare(generation, program, &keys, budget, telemetry)
}

/// Creates the driver of slot `slot` of a frame, prepared for `generation` with every kernel built.
fn driver(
    frame: &Frame, programs: &[Arc<RegionProgram>], slot: usize, generation: &GenerationRef, budget: PanicBudget,
    telemetry: &RecordingTelemetry,
) -> MetricRegionDriver {
    let boot = prepare_all(generation, &programs[slot], budget, telemetry);
    MetricRegionDriver::new(RegionConfig::for_slot(&frame.slots()[slot], telemetry), boot).unwrap()
}

/// A gauge with `points` timestamped points.
fn metric(name: &str, points: u64) -> Metric {
    let values = ScalarPoints::from_iter((1..=points).map(|point| (NonZeroU64::new(1_000 + point), 1.0)));
    Metric::gauge(Context::from_parts(name.to_string(), TagSet::default()), values)
}

/// A header stamped at admission point `point` with `generation`.
fn header(generation: Option<&GenerationRef>, point: u32) -> BatchHeader {
    BatchHeader {
        admitted_at: Some(AdmissionTime::from_unix_secs(1_000)),
        admission_point: Some(AdmissionPointId::new(point)),
        epoch: 3,
        generation: generation.map(Arc::clone),
        entry: None,
    }
}

/// A batch of one-point gauges, or of the given points per name with `name:points`.
fn batch(header: BatchHeader, names: &[&str]) -> EventsBuffer {
    let mut buffer = EventsBuffer::default().with_header(header);
    for name in names {
        let (name, points) = match name.split_once(':') {
            Some((name, points)) => (name, points.parse().unwrap()),
            None => (*name, 1),
        };
        assert!(buffer.try_push(Event::Metric(metric(name, points))).is_none());
    }
    buffer
}

fn names_of(buffer: &EventsBuffer) -> Vec<String> {
    buffer
        .into_iter()
        .map(|event| event.try_as_metric().unwrap().context().name().to_string())
        .collect()
}

/// One buffer a driver wrote: its exit, its metric names, and its header.
struct Written {
    exit: usize,
    names: Vec<String>,
    header: BatchHeader,
    buffer: EventsBuffer,
}

/// Runs one turn at `now` with an unlimited budget.
fn turn_at<T>(now: Instant, call: impl FnOnce(&mut RegionTurn<'_>) -> T) -> T {
    let mut budget = Budget::unlimited();
    call(&mut RegionTurn::new(now, &mut budget))
}

/// Processes a batch, returning what the driver wrote.
fn process(driver: &mut MetricRegionDriver, exits: usize, batch: EventsBuffer) -> Vec<Written> {
    process_at(driver, exits, batch, Instant::now())
}

fn process_at(driver: &mut MetricRegionDriver, exits: usize, batch: EventsBuffer, now: Instant) -> Vec<Written> {
    let mut sink = ExitSink::new(exits);
    let progress = turn_at(now, |turn| driver.process(turn, batch, &mut sink));
    assert_eq!(progress, Progress::Done, "stateless programs never yield");
    sink.drain()
        .map(|(exit, buffer)| Written {
            exit: exit.get(),
            names: names_of(&buffer),
            header: buffer.header().clone(),
            buffer,
        })
        .collect()
}

fn summary(written: &[Written]) -> Vec<(usize, Vec<String>)> {
    written
        .iter()
        .map(|written| (written.exit, written.names.clone()))
        .collect()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(ToString::to_string).collect()
}

/// Sends a control message, returning the acknowledgment's text on failure.
fn control(driver: &mut MetricRegionDriver, message: RegionControl) -> Result<(), String> {
    let mut sink = ExitSink::new(0);
    turn_at(Instant::now(), |turn| driver.control(turn, message, &mut sink)).map_err(|failure| failure.to_string())
}

// Exits and entries.

#[test]
fn exits_carry_the_batch_header_with_the_consumers_entry_set_only_between_slots() {
    let plan = detour_plan();
    let frame = frame_of(&plan);
    let exit_names = |slot: usize| {
        frame.slots()[slot]
            .exits
            .iter()
            .map(|exit| exit.name.to_string())
            .collect::<Vec<_>>()
    };
    // `region_d` leads to the host, to `region_m`, and, idle, to the output downstream of it; `region_m` to the
    // output. The host's output is admission point 1.
    assert_eq!(exit_names(0), strings(&["h", "region_m", "out"]));
    assert_eq!(exit_names(1), strings(&["out"]));
    assert_eq!(
        frame.admission_point_at(&id("h"), &"out".parse().unwrap()).unwrap().id,
        AdmissionPointId::new(1)
    );

    let programs = compiled(&plan, &frame).programs;
    let region_entry = programs[1]
        .entries()
        .iter()
        .find_map(|entry| match entry.key {
            EntryKey::Region(entry) => Some(entry),
            EntryKey::Admission(_) => None,
        })
        .expect("region_m has an entry for region_d's batches");
    let telemetry = RecordingTelemetry::default();
    let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let mut first = driver(
        &frame,
        &programs,
        0,
        &generation,
        PanicBudget::DEFAULT_PROGRAM,
        &telemetry,
    );
    let mut second = driver(
        &frame,
        &programs,
        1,
        &generation,
        PanicBudget::DEFAULT_PROGRAM,
        &telemetry,
    );

    // `a.` metrics go to the host, the rest to `region_m`, each in order, in the order the steps write them.
    let written = process(
        &mut first,
        3,
        batch(header(Some(&generation), 0), &["a.1", "b.1", "a.2", "b.2"]),
    );
    assert_eq!(
        summary(&written),
        vec![(0, strings(&["a.1", "a.2"])), (1, strings(&["b.1", "b.2"]))]
    );
    for buffer in &written {
        // Every exit buffer carries a copy of the batch's header: its admission time, point, epoch, and generation.
        assert_eq!(buffer.header.admitted_at, Some(AdmissionTime::from_unix_secs(1_000)));
        assert_eq!(buffer.header.admission_point, Some(AdmissionPointId::new(0)));
        assert_eq!(buffer.header.epoch, 3);
        assert_eq!(buffer.header.generation_id(), Some(GenerationId::new(1)));
    }
    // Only the exit to another slot sets the consumer's entry.
    assert_eq!(written[0].header.entry, None);
    assert_eq!(written[1].header.entry, Some(region_entry));

    // `region_m` finds the entry in the header, and a batch the host re-admitted by its admission point; its exit to
    // the output clears the entry.
    let mut written = written;
    let forwarded = written.remove(1).buffer;
    let from_region = process(&mut second, 1, forwarded);
    assert_eq!(summary(&from_region), vec![(0, strings(&["b.1", "b.2"]))]);
    assert_eq!(from_region[0].header.entry, None);
    assert_eq!(from_region[0].header.admission_point, Some(AdmissionPointId::new(0)));
    let from_host = process(&mut second, 1, batch(header(Some(&generation), 1), &["a.1"]));
    assert_eq!(summary(&from_host), vec![(0, strings(&["a.1"]))]);
    assert_eq!(from_host[0].header.admission_point, Some(AdmissionPointId::new(1)));
}

// Generations.

#[test]
fn a_batch_of_an_older_generation_runs_the_older_program_after_a_newer_one_is_prepared() {
    let boot = filter_plan("a", "keep");
    let frame = frame_of(&boot);
    let first = compiled(&boot, &frame).programs;
    let second = compiled(&filter_plan("b", "other"), &frame).programs;
    let retired = Arc::new(Mutex::new(Vec::new()));
    let telemetry = RecordingTelemetry::default();
    let old = token(1, &retired);
    let mut region = driver(&frame, &first, 0, &old, PanicBudget::DEFAULT_PROGRAM, &telemetry);

    let new = token(2, &retired);
    let reuse = kernel_reuse(Some(&first), &second);
    let message = prepare(
        &new,
        &second[0],
        &reuse[0].build,
        PanicBudget::DEFAULT_PROGRAM,
        &telemetry,
    );
    control(&mut region, RegionControl::Prepare(Box::new(message))).unwrap();
    assert_eq!(region.generations(), vec![GenerationId::new(1), GenerationId::new(2)]);
    assert_eq!(
        region.kernel_count(),
        2,
        "the label changed, so the second program has its own kernel"
    );

    // Each batch runs its own generation's program: the first keeps `keep` names and labels them `a`, the second
    // keeps `other` names and labels them `b`.
    let names = ["keep.x", "other.y"];
    let written = process(&mut region, 1, batch(header(Some(&old), 0), &names));
    assert_eq!(summary(&written), vec![(0, strings(&["keep.x.a1"]))]);
    assert_eq!(written[0].header.generation_id(), Some(GenerationId::new(1)));
    let written = process(&mut region, 1, batch(header(Some(&new), 0), &names));
    assert_eq!(summary(&written), vec![(0, strings(&["other.y.b1"]))]);
    assert_eq!(written[0].header.generation_id(), Some(GenerationId::new(2)));
}

#[test]
fn the_older_program_is_released_after_its_last_batch_and_its_token_are_dropped() {
    let boot = filter_plan("a", "keep");
    let frame = frame_of(&boot);
    let first = compiled(&boot, &frame).programs;
    let second = compiled(&filter_plan("b", "keep"), &frame).programs;
    let retired = Arc::new(Mutex::new(Vec::new()));
    let telemetry = RecordingTelemetry::default();
    let old = token(1, &retired);
    let mut region = driver(&frame, &first, 0, &old, PanicBudget::DEFAULT_PROGRAM, &telemetry);
    let new = token(2, &retired);
    let reuse = kernel_reuse(Some(&first), &second);
    let message = prepare(
        &new,
        &second[0],
        &reuse[0].build,
        PanicBudget::DEFAULT_PROGRAM,
        &telemetry,
    );
    control(&mut region, RegionControl::Prepare(Box::new(message))).unwrap();

    // The commit: the installer and the schedule drop the old generation, but a batch stamped with it is in flight.
    let late = batch(header(Some(&old), 0), &["keep.z"]);
    drop(old);
    region.publish_telemetry();
    assert_eq!(region.generations(), vec![GenerationId::new(1), GenerationId::new(2)]);
    assert!(retired.lock().unwrap().is_empty());

    // The last batch still runs the old program. What it wrote holds the generation until downstream drops it.
    let written = process(&mut region, 1, late);
    assert_eq!(summary(&written), vec![(0, strings(&["keep.z.a1"]))]);
    region.publish_telemetry();
    assert_eq!(region.generations().len(), 2);
    drop(written);
    assert_eq!(*retired.lock().unwrap(), vec![GenerationId::new(1)]);

    // The next call releases the program and the kernel only it used.
    assert_eq!(region.kernel_count(), 2);
    region.publish_telemetry();
    assert_eq!(region.generations(), vec![GenerationId::new(2)]);
    assert_eq!(region.kernel_count(), 1);
    let written = process(&mut region, 1, batch(header(Some(&new), 0), &["keep.w"]));
    assert_eq!(summary(&written), vec![(0, strings(&["keep.w.b1"]))]);
}

#[test]
fn a_retired_generation_is_released_off_the_data_path_even_when_no_batch_arrives() {
    let boot = filter_plan("a", "keep");
    let frame = frame_of(&boot);
    let first = compiled(&boot, &frame).programs;
    let second = compiled(&filter_plan("b", "keep"), &frame).programs;
    let retired = Arc::new(Mutex::new(Vec::new()));
    let telemetry = RecordingTelemetry::default();
    let old = token(1, &retired);
    let mut region = driver(&frame, &first, 0, &old, PanicBudget::DEFAULT_PROGRAM, &telemetry);
    let new = token(2, &retired);
    let reuse = kernel_reuse(Some(&first), &second);
    let message = prepare(
        &new,
        &second[0],
        &reuse[0].build,
        PanicBudget::DEFAULT_PROGRAM,
        &telemetry,
    );
    control(&mut region, RegionControl::Prepare(Box::new(message))).unwrap();

    // The old generation retires while the region is idle.
    drop(old);
    assert_eq!(*retired.lock().unwrap(), vec![GenerationId::new(1)]);
    assert_eq!(region.generations(), vec![GenerationId::new(1), GenerationId::new(2)]);

    // Processing a batch never releases it: dropping a program and its kernels does not belong on the data path.
    let written = process(&mut region, 1, batch(header(Some(&new), 0), &["keep.x"]));
    assert_eq!(summary(&written), vec![(0, strings(&["keep.x.b1"]))]);
    assert_eq!(region.generations(), vec![GenerationId::new(1), GenerationId::new(2)]);
    assert_eq!(region.kernel_count(), 2);

    // The release the installer sends once the generation retires does, with the kernel only it used.
    control(&mut region, RegionControl::Release).unwrap();
    assert_eq!(region.generations(), vec![GenerationId::new(2)]);
    assert_eq!(region.kernel_count(), 1);
    // Releasing again changes nothing.
    control(&mut region, RegionControl::Release).unwrap();
    assert_eq!(region.generations(), vec![GenerationId::new(2)]);
}

#[test]
fn generations_share_the_instance_of_a_kernel_with_the_same_key() {
    let boot = filter_plan("a", "keep");
    let frame = frame_of(&boot);
    let first = compiled(&boot, &frame).programs;
    // Only the split changes, so the count kernel keeps its key.
    let second = compiled(&filter_plan("a", "other"), &frame).programs;
    let third = compiled(&filter_plan("c", "keep"), &frame).programs;
    let retired = Arc::new(Mutex::new(Vec::new()));
    let telemetry = RecordingTelemetry::default();
    let one = token(1, &retired);
    let mut region = driver(&frame, &first, 0, &one, PanicBudget::DEFAULT_PROGRAM, &telemetry);

    let reuse = kernel_reuse(Some(&first), &second);
    assert!(
        reuse[0].build.is_empty(),
        "the installer builds nothing for the second program"
    );
    let two = token(2, &retired);
    let message = prepare(&two, &second[0], &[], PanicBudget::DEFAULT_PROGRAM, &telemetry);
    control(&mut region, RegionControl::Prepare(Box::new(message))).unwrap();
    assert_eq!(region.kernel_count(), 1);

    // Both generations run the same instance: its batch count, its state, carries on from one to the other. (What
    // the region writes is dropped at once, so that no exit buffer keeps a generation alive.)
    let written = process(&mut region, 1, batch(header(Some(&one), 0), &["keep.x"]));
    assert_eq!(summary(&written), vec![(0, strings(&["keep.x.a1"]))]);
    drop(written);
    let written = process(&mut region, 1, batch(header(Some(&two), 0), &["other.y"]));
    assert_eq!(summary(&written), vec![(0, strings(&["other.y.a2"]))]);
    drop(written);

    // A new key needs a new instance, which the preparation must bring.
    let three = token(3, &retired);
    let reuse = kernel_reuse(Some(&second), &third);
    let missing = prepare(&three, &third[0], &[], PanicBudget::DEFAULT_PROGRAM, &telemetry);
    let error = control(&mut region, RegionControl::Prepare(Box::new(missing))).unwrap_err();
    assert_eq!(
        error,
        format!(
            "The program of generation 3 in region `region_count` uses kernel `{}`, which the region does not hold and \
             the preparation did not bring; send every kernel the preparation lists to build with the program.",
            reuse[0].build[0]
        )
    );
    assert_eq!(region.generations().len(), 2, "a rejected preparation changes nothing");
    let message = prepare(
        &three,
        &third[0],
        &reuse[0].build,
        PanicBudget::DEFAULT_PROGRAM,
        &telemetry,
    );
    control(&mut region, RegionControl::Prepare(Box::new(message))).unwrap();
    assert_eq!(region.kernel_count(), 2);
    let written = process(&mut region, 1, batch(header(Some(&three), 0), &["keep.q"]));
    assert_eq!(summary(&written), vec![(0, strings(&["keep.q.c1"]))]);
    drop(written);

    // The shared instance is dropped only when the last generation using it is released.
    drop(one);
    region.publish_telemetry();
    assert_eq!(region.generations(), vec![GenerationId::new(2), GenerationId::new(3)]);
    assert_eq!(region.kernel_count(), 2);
    drop(two);
    region.publish_telemetry();
    assert_eq!(region.generations(), vec![GenerationId::new(3)]);
    assert_eq!(region.kernel_count(), 1);
    assert_eq!(
        *retired.lock().unwrap(),
        vec![GenerationId::new(1), GenerationId::new(2)]
    );
}

#[test]
fn abort_drops_a_prepared_generation_and_the_kernels_only_it_uses() {
    let boot = filter_plan("a", "keep");
    let frame = frame_of(&boot);
    let first = compiled(&boot, &frame).programs;
    let second = compiled(&filter_plan("b", "keep"), &frame).programs;
    let telemetry = RecordingTelemetry::default();
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let mut region = driver(&frame, &first, 0, &one, PanicBudget::DEFAULT_PROGRAM, &telemetry);
    // The installer still holds the aborted generation, so only the abort releases it.
    let two: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(2)));
    let message = prepare_all(&two, &second[0], PanicBudget::DEFAULT_PROGRAM, &telemetry);
    control(&mut region, RegionControl::Prepare(Box::new(message))).unwrap();
    assert_eq!(region.kernel_count(), 2);

    let abort = |region: &mut MetricRegionDriver, generation| {
        control(
            region,
            RegionControl::Abort {
                generation: GenerationId::new(generation),
            },
        )
    };
    abort(&mut region, 2).unwrap();
    assert_eq!(region.generations(), vec![GenerationId::new(1)]);
    assert_eq!(region.kernel_count(), 1);
    // Aborting a generation the region does not have does nothing.
    abort(&mut region, 7).unwrap();
    assert_eq!(region.generations(), vec![GenerationId::new(1)]);
    // A batch of the aborted generation is a wiring fault now.
    assert!(process(&mut region, 1, batch(header(Some(&two), 0), &["keep.x"])).is_empty());
}

#[test]
fn prepare_rejects_programs_the_region_cannot_run_and_changes_nothing() {
    let telemetry = RecordingTelemetry::default();
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));

    // A program for another slot.
    let plan = detour_plan();
    let frame = frame_of(&plan);
    let programs = compiled(&plan, &frame).programs;
    let mut region = driver(&frame, &programs, 0, &one, PanicBudget::DEFAULT_PROGRAM, &telemetry);
    let two: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(2)));
    let other_slot = prepare_all(&two, &programs[1], PanicBudget::DEFAULT_PROGRAM, &telemetry);
    assert_eq!(
        control(&mut region, RegionControl::Prepare(Box::new(other_slot))).unwrap_err(),
        "A program for slot 1 was sent to the region of slot 0; send each slot's program to its own region."
    );

    // A generation prepared twice.
    let again = prepare_all(&one, &programs[0], PanicBudget::DEFAULT_PROGRAM, &telemetry);
    assert_eq!(
        control(&mut region, RegionControl::Prepare(Box::new(again))).unwrap_err(),
        "Generation 1 is already prepared in region `region_d`; prepare each generation in a region once."
    );
    assert_eq!(region.generations(), vec![GenerationId::new(1)]);

    // A program that writes to more exits than the region has.
    let config = RegionConfig {
        exits: 1,
        ..RegionConfig::for_slot(&frame.slots()[0], &telemetry)
    };
    let boot = prepare_all(&one, &programs[0], PanicBudget::DEFAULT_PROGRAM, &telemetry);
    assert_eq!(
        MetricRegionDriver::new(config, boot).unwrap_err().to_string(),
        "The program of slot 0 cannot run at admission point 0: a step writes to exit 1, but the region has 1 \
         exits. Programs must be compiled for the frame the region was built from."
    );

    // A boot program without its kernels.
    let plan = filter_plan("a", "keep");
    let frame = frame_of(&plan);
    let programs = compiled(&plan, &frame).programs;
    let boot = prepare(&one, &programs[0], &[], PanicBudget::DEFAULT_PROGRAM, &telemetry);
    assert_eq!(
        MetricRegionDriver::new(RegionConfig::for_slot(&frame.slots()[0], &telemetry), boot)
            .unwrap_err()
            .to_string(),
        format!(
            "The program of generation 1 in region `region_count` uses kernel `{}`, which the region does not hold \
             and the preparation did not bring; send every kernel the preparation lists to build with the program.",
            programs[0].kernels()[0].key
        )
    );
}

// Wiring faults.

#[test]
fn batches_no_program_can_process_are_dropped_and_counted_as_invalid_loss_of_the_region() {
    let plan = filter_plan("a", "keep");
    let frame = frame_of(&plan);
    let programs = compiled(&plan, &frame).programs;
    let telemetry = RecordingTelemetry::default();
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let unknown: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(9)));
    let mut region = driver(&frame, &programs, 0, &one, PanicBudget::DEFAULT_PROGRAM, &telemetry);

    let entry_header = BatchHeader {
        entry: Some(EntryId::new(3)),
        ..header(Some(&one), 0)
    };
    let cases = [
        ("without a generation", header(None, 0)),
        (
            "of a generation the region was never prepared for",
            header(Some(&unknown), 0),
        ),
        (
            "from an admission point the program has no entry for",
            header(Some(&one), 7),
        ),
        ("for a region entry the program lacks", entry_header),
    ];
    let mut expected = 0;
    for (case, header) in cases {
        // Each batch has two metrics of 2 and 3 points.
        assert!(
            process(&mut region, 1, batch(header, &["keep.a:2", "keep.b:3"])).is_empty(),
            "a batch {} is dropped",
            case
        );
        expected += 5;
        region.publish_telemetry();
        assert_eq!(
            telemetry.loss("region_count", LossClass::Invalid),
            expected,
            "a batch {} is invalid loss",
            case
        );
    }

    // A header-only batch, such as a heartbeat, carries nothing to process and loses nothing.
    assert!(process(
        &mut region,
        1,
        EventsBuffer::with_capacity(0).with_header(header(None, 0))
    )
    .is_empty());
    region.publish_telemetry();
    assert_eq!(telemetry.loss("region_count", LossClass::Invalid), expected);
    // Nothing reached a node.
    assert_eq!(telemetry.port_events("count", "out"), 0);
}

// Supervision.

#[test]
fn a_panic_in_the_default_program_fails_the_region() {
    let plan = filter_plan("a", "keep");
    let frame = frame_of(&plan);
    let programs = compiled(&plan, &frame).programs;
    let telemetry = RecordingTelemetry::default();
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let mut region = driver(&frame, &programs, 0, &one, PanicBudget::DEFAULT_PROGRAM, &telemetry);

    let panicking = batch(header(Some(&one), 0), &["keep.a", "panic"]);
    let mut sink = ExitSink::new(1);
    let result = catch_unwind(AssertUnwindSafe(|| {
        turn_at(Instant::now(), |turn| region.process(turn, panicking, &mut sink))
    }));
    assert!(result.is_err(), "the kernel panics");
    match region.reset_after_panic(&RegionPanic::new(PanicSite::Process, "test_count was told to panic", 2)) {
        PanicOutcome::Fail(error) => assert_eq!(
            error.to_string(),
            "Region `region_count` panicked in node `count` of generation 1, whose panic budget is zero, as the \
             default program's is, so the region fails as a native component does."
        ),
        PanicOutcome::Continue => panic!("the default program's region must fail"),
    }
}

#[test]
fn a_panic_outside_any_program_fails_the_region() {
    let plan = filter_plan("a", "keep");
    let frame = frame_of(&plan);
    let programs = compiled(&plan, &frame).programs;
    let telemetry = RecordingTelemetry::default();
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    // Even a generation that survives panics in its program does not make the driver's own code recoverable.
    let mut region = driver(&frame, &programs, 0, &one, PanicBudget::ARTIFACT, &telemetry);
    match region.reset_after_panic(&RegionPanic::new(PanicSite::Control, "boom", 0)) {
        PanicOutcome::Fail(error) => assert_eq!(
            error.to_string(),
            "Region `region_count` panicked outside the program of any generation, which it cannot recover from."
        ),
        PanicOutcome::Continue => panic!("a panic outside any program must fail the region"),
    }
}

#[test]
fn a_panic_within_the_budget_rebuilds_the_kernels_of_its_generation_and_the_region_continues() {
    let plan = filter_plan("a", "keep");
    let frame = frame_of(&plan);
    let programs = compiled(&plan, &frame).programs;
    let telemetry = RecordingTelemetry::default();
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let mut region = driver(&frame, &programs, 0, &one, PanicBudget::ARTIFACT, &telemetry);

    let written = process(&mut region, 1, batch(header(Some(&one), 0), &["keep.a"]));
    assert_eq!(summary(&written), vec![(0, strings(&["keep.a.a1"]))]);

    let panicking = batch(header(Some(&one), 0), &["keep.b", "panic"]);
    let mut sink = ExitSink::new(1);
    let result = catch_unwind(AssertUnwindSafe(|| {
        turn_at(Instant::now(), |turn| region.process(turn, panicking, &mut sink))
    }));
    assert!(result.is_err(), "the kernel panics");
    assert!(matches!(
        region.reset_after_panic(&RegionPanic::new(PanicSite::Process, "test_count was told to panic", 2)),
        PanicOutcome::Continue
    ));

    // The kernel was rebuilt from its spec, so its batch count starts over, and nothing of the panicked batch is
    // left in the region's buffers.
    let written = process(&mut region, 1, batch(header(Some(&one), 0), &["keep.c"]));
    assert_eq!(summary(&written), vec![(0, strings(&["keep.c.a1"]))]);
}

/// Runs a batch that panics in the region, as the host does, and returns what the driver decides.
fn panic_in(region: &mut MetricRegionDriver, generation: &GenerationRef, at: Instant) -> PanicOutcome {
    let panicking = batch(header(Some(generation), 0), &["panic"]);
    let mut sink = ExitSink::new(1);
    let result = catch_unwind(AssertUnwindSafe(|| {
        turn_at(at, |turn| region.process(turn, panicking, &mut sink))
    }));
    assert!(result.is_err(), "the kernel panics");
    region.reset_after_panic(&RegionPanic::new(PanicSite::Process, "test_count was told to panic", 1))
}

/// Collects the generation failures a region reports.
#[derive(Clone, Default)]
struct Failures(Arc<Mutex<Vec<(GenerationId, String)>>>);

impl Failures {
    fn reporter(&self) -> GenerationFailureReporter {
        let failures = Arc::clone(&self.0);
        Arc::new(move |generation, reason| failures.lock().unwrap().push((generation, reason.to_string())))
    }

    fn take(&self) -> Vec<(GenerationId, String)> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[test]
fn a_panic_rebuilds_only_the_kernels_of_the_generation_that_panicked() {
    let boot = filter_plan("a", "keep");
    let frame = frame_of(&boot);
    let first = compiled(&boot, &frame).programs;
    let second = compiled(&filter_plan("b", "keep"), &frame).programs;
    let telemetry = RecordingTelemetry::default();
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let mut region = driver(&frame, &first, 0, &one, PanicBudget::ARTIFACT, &telemetry);
    let two: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(2)));
    let reuse = kernel_reuse(Some(&first), &second);
    let message = prepare(&two, &second[0], &reuse[0].build, PanicBudget::ARTIFACT, &telemetry);
    control(&mut region, RegionControl::Prepare(Box::new(message))).unwrap();

    for generation in [&one, &two] {
        process(&mut region, 1, batch(header(Some(generation), 0), &["keep.x"]));
    }
    assert!(matches!(
        panic_in(&mut region, &two, Instant::now()),
        PanicOutcome::Continue
    ));
    // Generation 2's kernel starts over; generation 1's, which the panicking call never ran, keeps its count.
    let written = process(&mut region, 1, batch(header(Some(&one), 0), &["keep.y"]));
    assert_eq!(summary(&written), vec![(0, strings(&["keep.y.a2"]))]);
    let written = process(&mut region, 1, batch(header(Some(&two), 0), &["keep.y"]));
    assert_eq!(summary(&written), vec![(0, strings(&["keep.y.b1"]))]);
}

#[test]
fn a_kernel_that_cannot_be_rebuilt_fails_the_generations_that_use_it() {
    // (label, why the rebuild fails)
    let cases = [
        (
            "unrebuildable",
            "Failed to build the kernel: test_count cannot be rebuilt",
        ),
        ("panicky", "its factory panicked: test_count panicked while rebuilding"),
    ];
    for (label, why) in cases {
        let plan = filter_plan(label, "keep");
        let frame = frame_of(&plan);
        let programs = compiled(&plan, &frame).programs;
        let telemetry = RecordingTelemetry::default();
        let failures = Failures::default();
        let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
        let boot = prepare_all(&one, &programs[0], PanicBudget::ARTIFACT, &telemetry);
        let config = RegionConfig::for_slot(&frame.slots()[0], &telemetry).with_failure_reporter(failures.reporter());
        let mut region = MetricRegionDriver::new(config, boot).unwrap();

        // An artifact generation alone: it fails, the failure is reported, and the region continues without it.
        assert!(
            matches!(panic_in(&mut region, &one, Instant::now()), PanicOutcome::Continue),
            "{}",
            label
        );
        let reason = format!("the kernel of node `count` could not be rebuilt after a panic: {}", why);
        assert_eq!(
            failures.take(),
            vec![(GenerationId::new(1), reason.clone())],
            "{}",
            label
        );
        assert!(process(&mut region, 1, batch(header(Some(&one), 0), &["keep.a:3"])).is_empty());
        region.publish_telemetry();
        assert_eq!(telemetry.loss("region_count", LossClass::Invalid), 3, "{}", label);
    }

    // A default-program generation that shares the kernel cannot continue with the instance that panicked.
    let plan = filter_plan("unrebuildable", "keep");
    let frame = frame_of(&plan);
    let first = compiled(&plan, &frame).programs;
    let second = compiled(&filter_plan("unrebuildable", "other"), &frame).programs;
    let telemetry = RecordingTelemetry::default();
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let mut region = driver(&frame, &first, 0, &one, PanicBudget::DEFAULT_PROGRAM, &telemetry);
    let two: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(2)));
    let message = prepare(&two, &second[0], &[], PanicBudget::ARTIFACT, &telemetry);
    control(&mut region, RegionControl::Prepare(Box::new(message))).unwrap();
    assert_eq!(region.kernel_count(), 1, "both programs share the count kernel");
    match panic_in(&mut region, &two, Instant::now()) {
        PanicOutcome::Fail(error) => assert_eq!(
            error.to_string(),
            "Region `region_count` cannot continue generation 1, whose panic budget is zero: the kernel of node \
             `count` could not be rebuilt after a panic: Failed to build the kernel: test_count cannot be rebuilt."
        ),
        PanicOutcome::Continue => panic!("the default program cannot continue with a kernel that panicked"),
    }
}

#[test]
fn a_generation_that_exceeds_its_panic_budget_fails_and_its_batches_are_dropped() {
    let budget = PanicBudget::Limited {
        per_window: 1,
        window: Duration::from_secs(60),
        total: 2,
    };
    // Panics at these seconds, and whether the generation survives each: one panic per minute at most, and two in
    // all.
    let cases: [&[(u64, bool)]; 2] = [&[(0, true), (61, true), (200, false)], &[(0, true), (30, false)]];
    for panics in cases {
        let plan = filter_plan("a", "keep");
        let frame = frame_of(&plan);
        let programs = compiled(&plan, &frame).programs;
        let telemetry = RecordingTelemetry::default();
        let failures = Failures::default();
        let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
        let boot = prepare_all(&one, &programs[0], budget, &telemetry);
        let config = RegionConfig::for_slot(&frame.slots()[0], &telemetry).with_failure_reporter(failures.reporter());
        let mut region = MetricRegionDriver::new(config, boot).unwrap();
        let start = Instant::now();

        for (seconds, survives) in panics {
            let at = start + Duration::from_secs(*seconds);
            let panicking = batch(header(Some(&one), 0), &["panic"]);
            let mut sink = ExitSink::new(1);
            let result = catch_unwind(AssertUnwindSafe(|| {
                turn_at(at, |turn| region.process(turn, panicking, &mut sink))
            }));
            assert!(result.is_err(), "the kernel panics");
            // The region continues either way: only the generation fails.
            assert!(matches!(
                region.reset_after_panic(&RegionPanic::new(PanicSite::Process, "boom", 1)),
                PanicOutcome::Continue
            ));

            let written = process_at(&mut region, 1, batch(header(Some(&one), 0), &["keep.a:4"]), at);
            region.publish_telemetry();
            if *survives {
                assert_eq!(summary(&written), vec![(0, strings(&["keep.a.a1"]))], "at {}s", seconds);
                assert_eq!(telemetry.loss("region_count", LossClass::Invalid), 0);
                assert_eq!(failures.take(), Vec::new());
            } else {
                assert!(
                    written.is_empty(),
                    "the failed generation's batch at {}s is dropped",
                    seconds
                );
                assert_eq!(telemetry.loss("region_count", LossClass::Invalid), 4);
                let total = panics.len();
                assert_eq!(
                    failures.take(),
                    vec![(
                        GenerationId::new(1),
                        format!(
                            "it exceeded its panic budget, with {} panics, the last in node `count`",
                            total
                        )
                    )]
                );
            }
        }
    }
}

// Telemetry.

#[test]
fn counts_are_kept_per_turn_and_published_only_when_the_host_asks() {
    let plan = filter_plan("a", "keep");
    let frame = frame_of(&plan);
    let programs = compiled(&plan, &frame).programs;
    let telemetry = RecordingTelemetry::default();
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let mut region = driver(&frame, &programs, 0, &one, PanicBudget::DEFAULT_PROGRAM, &telemetry);

    // The count kernel sheds `shed` (4 points) as capacity loss and passes three metrics on its `out` port; the split
    // keeps the two `keep` metrics and drops `drop.b` (3 points) as policy loss of `dropped`.
    let names = ["keep.a:2", "drop.b:3", "shed:4", "keep.c"];
    let written = process(&mut region, 1, batch(header(Some(&one), 0), &names));
    assert_eq!(summary(&written), vec![(0, strings(&["keep.a.a1", "keep.c.a1"]))]);
    let published = |telemetry: &RecordingTelemetry| {
        [
            telemetry.port_events("count", "out"),
            telemetry.port_events("keep", "true"),
            telemetry.port_events("keep", "false"),
            telemetry.loss("dropped", LossClass::Policy),
            telemetry.loss("count", LossClass::Capacity),
            telemetry.loss("count", LossClass::Policy),
        ]
    };
    assert_eq!(published(&telemetry), [0; 6], "nothing is published during a turn");

    region.publish_telemetry();
    assert_eq!(published(&telemetry), [3, 2, 1, 3, 4, 0]);
    region.publish_telemetry();
    assert_eq!(published(&telemetry), [3, 2, 1, 3, 4, 0], "counts are published once");

    process(&mut region, 1, batch(header(Some(&one), 0), &["drop.d:2"]));
    region.publish_telemetry();
    assert_eq!(published(&telemetry), [4, 2, 2, 5, 4, 0]);
}

#[test]
fn plan_telemetry_registers_node_events_and_loss_counters_tagged_with_the_node() {
    let recorder = TestRecorder::default();
    let _local = metrics::set_default_local_recorder(&recorder);

    let plan = filter_plan("a", "keep");
    let frame = frame_of(&plan);
    let programs = compiled(&plan, &frame).programs;
    let telemetry = PlanTelemetry::new(MetricsBuilder::default());
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let boot = prepare_all(&one, &programs[0], PanicBudget::DEFAULT_PROGRAM, &telemetry);
    let mut region = MetricRegionDriver::new(RegionConfig::for_slot(&frame.slots()[0], &telemetry), boot).unwrap();

    process(&mut region, 1, batch(header(Some(&one), 0), &["keep.a", "drop.b:3"]));
    process(&mut region, 1, batch(header(None, 0), &["x:2"]));
    region.publish_telemetry();

    let events = |node: &'static str, port: &'static str| {
        recorder.counter((PLAN_NODE_EVENTS_TOTAL, &[("component_id", node), ("port", port)]))
    };
    let loss = |component: &'static str, class: &'static str| {
        recorder.counter((PLAN_LOSS_TOTAL, &[("component_id", component), ("class", class)]))
    };
    assert_eq!(events("count", "out"), Some(2));
    assert_eq!(events("keep", "true"), Some(1));
    assert_eq!(events("keep", "false"), Some(1));
    assert_eq!(loss("dropped", "policy"), Some(3));
    assert_eq!(loss("count", "capacity"), Some(0));
    assert_eq!(loss("region_count", "invalid"), Some(2));
}

// Bounds.

#[test]
fn region_bounds_add_each_kernels_declared_bounds_and_a_full_buffer_per_register() {
    let plan = filter_plan("a", "keep");
    let frame = frame_of(&plan);
    let programs = compiled(&plan, &frame).programs;
    let bounds = RegionBounds::new(&plan, &registry(), &programs[0]).unwrap();

    let components = ComponentRegistry::default();
    bounds.specify_bounds(&mut components.bounds_builder(&SubsystemIdentifier::from_segments(["region"])));
    let totals = components.as_bounds();

    // The count kernel declares 100 bytes minimum and 1,000 more firm; the program has two registers (the batch, and
    // the split's `true` port), each holding at most one full events buffer.
    let full_buffer = std::mem::size_of::<EventsBuffer>() + 1024 * std::mem::size_of::<Event>();
    assert_eq!(totals.total_minimum_required_bytes(), 100);
    assert_eq!(totals.total_firm_limit_bytes(), 100 + 1_000 + 2 * full_buffer);

    // Bounds need the plan the program was compiled from.
    assert_eq!(
        RegionBounds::new(&plan_without_count(), &registry(), &programs[0])
            .unwrap_err()
            .to_string(),
        "The plan has no node `count`, which the program has a kernel for; compute bounds with the plan the program \
         was compiled from."
    );
}

/// A plan without the filter plan's `count` node.
fn plan_without_count() -> Plan {
    plan(
        vec![
            node(
                "src",
                names::SOURCE,
                json!({"binding": "dogstatsd", "timestamps": "mixed"}),
            ),
            node("kept_out", names::EMIT, json!({"output": "kept"})),
        ],
        &[("src.metrics", "kept_out.in")],
        &["kept"],
    )
}

#[test]
fn region_config_for_a_slot_takes_its_name_and_exits_from_the_frame() {
    let plan = detour_plan();
    let frame = frame_of(&plan);
    let telemetry = RecordingTelemetry::default();
    let config = RegionConfig::for_slot(&frame.slots()[0], &telemetry);
    assert_eq!(config.slot, SlotId::new(0));
    assert_eq!(config.name.as_str(), "region_d");
    assert_eq!(config.exits, 3);
    config.invalid_loss.increment(2);
    assert_eq!(telemetry.loss("region_d", LossClass::Invalid), 2);
    assert!(matches!(
        frame.slots()[0].exits[1].target,
        FrameTarget::Slot(slot) if slot == SlotId::new(1)
    ));
}

#[test]
fn a_region_host_builder_takes_the_driver_and_its_bounds_with_the_slots_exits_as_outputs() {
    use std::num::NonZeroUsize;

    use saluki_core::{components::transforms::TransformBuilder, execution::control_mailbox};

    let plan = detour_plan();
    let frame = frame_of(&plan);
    let programs = compiled(&plan, &frame).programs;
    let telemetry = RecordingTelemetry::default();
    let one: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let slot = &frame.slots()[0];
    let region = driver(&frame, &programs, 0, &one, PanicBudget::DEFAULT_PROGRAM, &telemetry);
    let (_control, mailbox) = control_mailbox::<RegionControl>();
    let builder = saluki_core::execution::RegionHostBuilder::from_driver(
        slot.exits.iter().map(|exit| exit.name.to_string()),
        NonZeroUsize::new(crate::limits::values::WORK_PER_TURN as usize).unwrap(),
        mailbox,
        region,
    )
    .unwrap()
    .with_bounds(RegionBounds::new(&plan, &registry(), &programs[0]).unwrap());

    // Exit `i` of the slot is output `i` of the region's component.
    let outputs = builder
        .outputs()
        .iter()
        .map(|output| output.output_name().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(outputs, strings(&["h", "region_m", "out"]));
}
