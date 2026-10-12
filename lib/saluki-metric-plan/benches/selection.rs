//! Endpoint routing selection, against hand-written baselines that make the same selection directly, with 1, 3, and 7
//! routing policies, each a `split` on `any(intake_name in names, intake_name prefix_in prefixes)` whose `true` port
//! emits to the policy's output and whose `false` port drops, as the default program lowers endpoint allowlists.
//!
//! Two shapes of the policy region:
//!
//! - **One region** (`selection_function`, `selection_region`): a `tee` to the primary output and to every policy's
//!   filter, with every node in one slot, so that the whole fan-out is one memoized decision. This is the shape the
//!   specification's selection cost describes: a memo hit costs one lookup, whatever the number of policies.
//!   - **Function**: the decision function alone, over a batch of 1,024 metrics, computing each event's exit set, with
//!     the R4 memo (4,096 entries, warmed by the previous iteration) and without it; the baseline computes each
//!     event's intake name once and tests every policy's two matchers, giving a bitmask of outputs.
//!   - **Region**: the region driver running the slot's program over the batch (the decision step, policy loss, and
//!     exit buffers), with and without the memo; the baseline makes the same selection and fills the same buffers:
//!     the original batch to the primary output, and a buffer of copies per policy that selects events.
//! - **The default program** (`selection_default_program`): the stage 4 default program with the policies as its
//!   endpoint routing groups, compiled into its own frame, as ADP runs it. R1 never cuts a decision, so the policy
//!   fan-out and every gateway below it (the routing filters and both failover gateways) are one memoized decision of
//!   the enrichment region, beside the enrichment kernels: the shape of the one-region benchmarks, behind
//!   enrichment. The region runs from a batch entering it (and any region downstream, as a pipeline); the baselines
//!   run the same enrichment kernels, then either make the whole selection at once, filling a buffer of copies per
//!   selecting output (`hand_written_selection`, as the one-region baseline does), or copy the batch for every
//!   gateway and filter each copy in place (`hand_written_fan_out`, the native components' shape).
//!   `enrichment_only` runs the two kernels alone, so that each side's selection cost is its time less that one's.
//! - **Memo misses** (`memo_misses`): what a lookup costs when it hits and when it misses, which sets
//!   `MEMO_COST_FLOOR`. The default program's policy region with one routing group, which R4 memoizes, as a function
//!   and as the enrichment region that runs it, memoized and not; and its listener filterlist, a `split` on one
//!   intake-name leaf, below the floor, as a function, memoized and not: what memoizing it would save and cost. With
//!   contexts the memo holds, and with 16 times as many as it holds, so that nearly every lookup misses and replaces
//!   an entry.
//!
//! Batches cycle over 256 contexts, which the memo holds, and over 16,384 (65,536 for the memo misses), in batches of
//! 1,024 taken in turn, which it cannot. Setup checks that both sides select exactly the same events for every output.

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use saluki_core::{
    data_model::event::{
        metric::{context::Context, Metric},
        Event,
    },
    data_model::tags::{Tag, TagSet},
    execution::{
        AdmissionPointId, AdmissionTime, BatchHeader, Budget, ExitSink, GenerationId, GenerationRef, GenerationToken,
        Kernel, PortOutputs, RegionDriver, RegionTurn, Turn, TurnCounters,
    },
    topology::EventsBuffer,
};
use saluki_metric_kernels::{
    matcher::SortedSetMatcher,
    selection::{is_scalar_series, RawNameSet},
};
use saluki_metric_plan::{
    compile::{
        compile, CompiledPlan, Decision, DecisionFunction, DecisionMemo, EntryKey, ExitSet, Frame, FrameTarget,
        RegionProgram, SlotId,
    },
    ir::{BindingName, NativeComponentId, NodeId, OutputId, ParamType, Plan, RecipientId},
    limits::values::DECISION_MEMO_ENTRIES,
    model::{EventView, PortType},
    operators::{KernelBuildContext, OperatorRegistry},
    runtime::{build_kernels, MetricRegionDriver, NoTelemetry, PanicBudget, PrepareRegion, RegionConfig},
    testing::{default_program_catalog, default_program_slot_name, DEFAULT_PROGRAM_JSON},
    validate::{CatalogOutput, Liveness, MemoryCatalog, Validator},
};
use serde_json::{json, Value};

/// The policies each benchmark runs with.
const POLICIES: [usize; 3] = [1, 3, 7];

/// The numbers of contexts the batches cycle over: within the memo's capacity, and well beyond it.
const CONTEXTS: [usize; 2] = [256, 16_384];

/// The numbers of contexts the memo-miss benchmarks cycle over: within the memo's capacity, and 16 times it.
const MISS_CONTEXTS: [usize; 2] = [256, 65_536];

/// The exact names and the prefixes policy `index` selects.
fn policy_lists(index: usize) -> (Vec<String>, Vec<String>) {
    (
        vec![format!("exact{}.requests", index), format!("exact{}.errors", index)],
        vec![format!("svc{}.", index)],
    )
}

fn fingerprint(output: &str) -> String {
    let hash = output
        .bytes()
        .fold(0u64, |hash, byte| hash.wrapping_mul(31).wrapping_add(u64::from(byte)));
    format!("sha256:{:064x}", hash)
}

/// The policy region of `policies` routing policies, with every node in one region, and its validation catalog.
fn policy_plan(policies: usize) -> (Plan, MemoryCatalog) {
    let node = |id: &str, op: &str, params: Value| {
        let provenance = json!({"native": "policy"});
        json!({"id": id, "op": op, "params": params, "provenance": provenance})
    };
    let mut nodes = vec![
        json!({"id": "src", "op": "source", "params": {"binding": "otlp"}}),
        node("metrics_policy", "tee", json!({"ports": policies + 1})),
        node("primary", "emit", json!({"output": "primary"})),
    ];
    let mut edges = vec![
        json!({"from": "src.metrics", "to": "metrics_policy.in"}),
        json!({"from": "metrics_policy.0", "to": "primary.in"}),
    ];
    let mut outputs = vec!["primary".to_string()];
    for index in 0..policies {
        let (names, prefixes) = policy_lists(index);
        let split = format!("routing_filter_{}", index);
        let output = format!("routing_{}", index);
        nodes.push(node(
            &split,
            "split",
            json!({"predicate": {"any": [
                {"intake_name": {"matches": {"values": names, "prefix": false}}},
                {"intake_name": {"matches": {"values": prefixes, "prefix": true}}},
            ]}}),
        ));
        nodes.push(node(&format!("{}_true", split), "emit", json!({"output": output})));
        nodes.push(node(&format!("{}_false", split), "drop", json!({})));
        edges.push(json!({"from": format!("metrics_policy.{}", index + 1), "to": format!("{}.in", split)}));
        edges.push(json!({"from": format!("{}.true", split), "to": format!("{}_true.in", split)}));
        edges.push(json!({"from": format!("{}.false", split), "to": format!("{}_false.in", split)}));
        outputs.push(output);
    }
    let accepts = json!({"kinds": "all", "timestamps": "mixed"});
    let plan_outputs = outputs
        .iter()
        .map(|output| json!({"id": output, "accepts": accepts, "fingerprint": fingerprint(output)}))
        .collect::<Vec<_>>();
    let plan = Plan::from_value(json!({"ir_version": 1, "nodes": nodes, "edges": edges, "outputs": plan_outputs}))
        .expect("the policy plan is well-formed");
    let catalog = outputs.iter().fold(MemoryCatalog::new(), |catalog, output| {
        catalog.with_output(
            OutputId::new(output.as_str()).unwrap(),
            CatalogOutput {
                accepts: PortType::ANY,
                fingerprint: fingerprint(output).parse().unwrap(),
                recipients: Vec::new(),
                legacy_fingerprint: None,
            },
        )
    });
    (plan, catalog)
}

/// Returns the name of context `context`: a tenth of the contexts are named for each of ten services, of which policy
/// `i` selects service `i` by prefix; every fiftieth is exactly named for a policy; and the rest are selected by no
/// policy.
fn context_name(context: usize) -> String {
    if context.is_multiple_of(50) {
        format!("exact{}.requests", (context / 50) % 7)
    } else {
        format!("svc{}.latency.{}", context % 10, context)
    }
}

/// Batches of 1,024 gauges that cycle over `contexts` contexts, named by `context`: one batch for up to 1,024
/// contexts, and otherwise one for each 1,024, so that iterating over the batches in turn meets every context once
/// per round.
fn batches_of(header: &BatchHeader, contexts: usize, context: impl Fn(usize) -> Context) -> Vec<EventsBuffer> {
    (0..contexts.div_ceil(1024))
        .map(|batch| {
            let mut buffer = EventsBuffer::default().with_header(header.clone());
            for i in 0..1024 {
                let metric = Metric::gauge(context((batch * 1024 + i * 7919) % contexts), 1.0);
                assert!(buffer.try_push(Event::Metric(metric)).is_none());
            }
            buffer
        })
        .collect()
}

/// The one-region benchmarks' batches, of contexts named by [`context_name`], without tags.
fn inputs(header: &BatchHeader, contexts: usize) -> Vec<EventsBuffer> {
    batches_of(header, contexts, |context| {
        Context::from_parts(context_name(context), TagSet::default())
    })
}

/// Returns the batches in turn, forever.
fn rotation<'a>(batches: &'a [EventsBuffer]) -> impl FnMut() -> &'a EventsBuffer {
    let mut next = 0;
    move || {
        let batch = &batches[next % batches.len()];
        next += 1;
        batch
    }
}

/// Each policy's two matchers, over intake names.
fn policy_matchers(policies: usize) -> Vec<(SortedSetMatcher, SortedSetMatcher)> {
    (0..policies)
        .map(|index| {
            let (names, prefixes) = policy_lists(index);
            (
                SortedSetMatcher::new(names, false),
                SortedSetMatcher::new(prefixes, true),
            )
        })
        .collect()
}

/// Returns, as bits from `first`, the policies whose matchers an intake name matches.
fn policies_matched(policies: &[(SortedSetMatcher, SortedSetMatcher)], name: &[u8], first: u32) -> u64 {
    policies
        .iter()
        .enumerate()
        .filter(|(_, (names, prefixes))| names.matches(name) || prefixes.matches(name))
        .fold(0, |selected, (index, _)| selected | (1 << (first + index as u32)))
}

/// Copies the events of a batch whose selection has `bit`, into a buffer of exactly their size, or `None` if no event
/// has it.
fn copies(batch: &EventsBuffer, selected: &[u64], bit: u64) -> Option<EventsBuffer> {
    let count = selected.iter().filter(|mask| **mask & bit != 0).count();
    if count == 0 {
        return None;
    }
    let mut copies = EventsBuffer::with_capacity(count);
    for (event, mask) in batch.into_iter().zip(selected) {
        if mask & bit != 0 {
            let _ = copies.try_push(event.clone());
        }
    }
    Some(copies)
}

/// The hand-written selection of the one-region benchmarks: each policy's two matchers, over each event's intake name,
/// computed once.
struct Baseline {
    policies: Vec<(SortedSetMatcher, SortedSetMatcher)>,
}

impl Baseline {
    fn new(policies: usize) -> Self {
        Self {
            policies: policy_matchers(policies),
        }
    }

    /// Returns the outputs an event goes to: bit 0 for the primary output, and bit `i + 1` for policy `i`'s.
    fn select(&self, event: &Event) -> u64 {
        let Some(metric) = event.try_as_metric() else {
            return 1;
        };
        let mut view = EventView::new(metric);
        match view.intake_name() {
            Some(name) => 1 | policies_matched(&self.policies, name, 1),
            None => 1,
        }
    }

    /// Routes a batch: copies to each selecting policy's buffer, of exactly its size, and the original to the primary,
    /// each with its output as [`select`][Self::select] numbers them.
    fn route(&self, batch: EventsBuffer, selected: &mut Vec<u64>) -> Vec<(usize, EventsBuffer)> {
        selected.clear();
        selected.extend((&batch).into_iter().map(|event| self.select(event)));
        let mut routed = Vec::with_capacity(self.policies.len() + 1);
        for index in 0..self.policies.len() {
            if let Some(copies) = copies(&batch, selected, 1 << (index + 1)) {
                routed.push((index + 1, copies));
            }
        }
        routed.push((0, batch));
        routed
    }
}

/// Returns the outputs an exit set selects, as [`Baseline::select`] numbers them: an exit is a selection when it leads
/// to an `emit`, and the primary output's is the tee's port 0.
fn selected_outputs(decision: &Decision, set: ExitSet) -> u64 {
    set.iter().fold(0, |selected, exit| {
        let target = &decision.exits[exit.get()];
        match (target.node.local().as_str(), target.port.as_str()) {
            ("metrics_policy", "0") => selected | 1,
            (split, "true") => {
                let index = split
                    .trim_start_matches("routing_filter_")
                    .parse::<usize>()
                    .expect("a routing filter");
                selected | (1 << (index + 1))
            }
            _ => selected,
        }
    })
}

/// Returns the names of a buffer's metrics, in order.
fn names(buffer: &EventsBuffer) -> Vec<String> {
    buffer
        .into_iter()
        .map(|event| event.try_as_metric().unwrap().context().to_string())
        .collect()
}

fn namer(nodes: &[NodeId]) -> Result<NativeComponentId, String> {
    NativeComponentId::new(format!("{}_region", nodes[0].local())).map_err(|error| error.to_string())
}

/// The policy region compiled into its frame: the frame, the decision, and the slot's program.
struct Compiled {
    frame: Frame,
    decision: Decision,
    program: Arc<RegionProgram>,
}

fn compiled(policies: usize) -> Compiled {
    let (plan, catalog) = policy_plan(policies);
    let registry = OperatorRegistry::native();
    let report = Validator::new(&registry, &catalog).validate(&plan);
    assert!(report.is_valid(), "{}", report);
    let frame = Frame::compute(&plan, &registry, &namer).unwrap();
    let compiled = compile(&plan, &report, &registry, &frame).unwrap();
    let [program] = compiled.programs.as_slice() else {
        panic!("the policy region is one slot")
    };
    let [decision] = program.decisions() else {
        panic!("the fan-out is one decision")
    };
    assert_eq!(decision.nodes.len(), policies + 1);
    assert!(
        decision.memoized,
        "the routing policies read names, so the decision is memoized"
    );
    Compiled {
        decision: decision.clone(),
        program: Arc::clone(program),
        frame,
    }
}

fn header(generation: &GenerationRef, point: AdmissionPointId) -> BatchHeader {
    BatchHeader {
        admitted_at: Some(AdmissionTime::from_unix_secs(1_700_000_003)),
        admission_point: Some(point),
        epoch: 0,
        generation: Some(Arc::clone(generation)),
        entry: None,
    }
}

/// Creates the driver of one slot, prepared for `generation` with the slot's program and its kernels, and with memos
/// of `memo_entries` entries (none for zero).
fn driver(
    frame: &Frame, slot: SlotId, program: &Arc<RegionProgram>, generation: &GenerationRef, memo_entries: usize,
) -> MetricRegionDriver {
    let keys = program.kernels().iter().map(|kernel| &kernel.key);
    let kernels = build_kernels(program, keys, KernelBuildContext::detached).unwrap();
    let boot = PrepareRegion::new(
        Arc::clone(generation),
        Arc::clone(program),
        kernels,
        PanicBudget::DEFAULT_PROGRAM,
        &NoTelemetry,
    );
    let config = RegionConfig::for_slot(frame.slot(slot).unwrap(), &NoTelemetry).with_memo_entries(memo_entries);
    MetricRegionDriver::new(config, boot).unwrap()
}

fn bench_function(c: &mut Criterion) {
    let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let mut group = c.benchmark_group("selection_function");
    group.throughput(Throughput::Elements(1024));
    for policies in POLICIES {
        let compiled = compiled(policies);
        let function = DecisionFunction::new(&compiled.decision).unwrap();
        let baseline = Baseline::new(policies);
        for contexts in CONTEXTS {
            let batches = inputs(&header(&generation, AdmissionPointId::new(0)), contexts);
            let mut memo = DecisionMemo::for_exits(DECISION_MEMO_ENTRIES as usize, compiled.decision.exits.len());
            // Equal selection: every event goes to the same outputs either way, memoized or not.
            for event in batches.iter().flatten() {
                let expected = baseline.select(event);
                assert_eq!(selected_outputs(&compiled.decision, function.evaluate(event)), expected);
                assert_eq!(
                    selected_outputs(&compiled.decision, function.evaluate_memoized(event, &mut memo)),
                    expected
                );
            }

            let parameter = format!("{} policies, {} contexts", policies, contexts);
            group.bench_function(BenchmarkId::new("decision_function", &parameter), |b| {
                let mut next = rotation(&batches);
                b.iter(|| {
                    next()
                        .into_iter()
                        .fold(ExitSet::EMPTY, |all, event| all.union(function.evaluate(event)))
                })
            });
            group.bench_function(BenchmarkId::new("decision_function_memoized", &parameter), |b| {
                let mut next = rotation(&batches);
                b.iter(|| {
                    next().into_iter().fold(ExitSet::EMPTY, |all, event| {
                        all.union(function.evaluate_memoized(event, &mut memo))
                    })
                })
            });
            group.bench_function(BenchmarkId::new("hand_written_baseline", &parameter), |b| {
                let mut next = rotation(&batches);
                b.iter(|| next().into_iter().fold(0u64, |all, event| all | baseline.select(event)))
            });
        }
    }
    group.finish();
}

fn bench_region(c: &mut Criterion) {
    let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let now = Instant::now();
    let mut group = c.benchmark_group("selection_region");
    group.throughput(Throughput::Elements(1024));
    for policies in POLICIES {
        let compiled = compiled(policies);
        let slot = &compiled.frame.slots()[0];
        let baseline = Baseline::new(policies);
        for contexts in CONTEXTS {
            let batches = inputs(&header(&generation, AdmissionPointId::new(0)), contexts);
            let batch = &batches[0];
            let mut memoized = driver(
                &compiled.frame,
                slot.id,
                &compiled.program,
                &generation,
                DECISION_MEMO_ENTRIES as usize,
            );
            let mut evaluated = driver(&compiled.frame, slot.id, &compiled.program, &generation, 0);
            let mut sink = ExitSink::new(slot.exits.len());
            let mut selected = Vec::new();

            // Equal delivered work: each output receives the same events, in the same order, from the region, memoized
            // or not, as from the baseline.
            let mut expected = baseline
                .route(batch.clone(), &mut selected)
                .into_iter()
                .map(|(output, buffer)| {
                    let output = if output == 0 {
                        "primary".to_string()
                    } else {
                        format!("routing_{}", output - 1)
                    };
                    (output, names(&buffer))
                })
                .collect::<Vec<_>>();
            expected.sort();
            for region in [&mut memoized, &mut evaluated] {
                let mut budget = Budget::unlimited();
                region.process(&mut RegionTurn::new(now, &mut budget), batch.clone(), &mut sink);
                let mut delivered = sink
                    .drain()
                    .map(|(exit, buffer)| match &slot.exits[exit.get()].target {
                        FrameTarget::Destination(output) => (output.to_string(), names(&buffer)),
                        target => panic!("an exit to {:?}", target),
                    })
                    .collect::<Vec<_>>();
                delivered.sort();
                assert_eq!(delivered, expected);
            }

            let parameter = format!("{} policies, {} contexts", policies, contexts);
            for (name, region) in [("region_memoized", &mut memoized), ("region_evaluated", &mut evaluated)] {
                group.bench_function(BenchmarkId::new(name, &parameter), |b| {
                    let mut budget = Budget::unlimited();
                    let mut next = rotation(&batches);
                    b.iter_batched(
                        || next().clone(),
                        |batch| {
                            region.process(&mut RegionTurn::new(now, &mut budget), batch, &mut sink);
                            sink.drain().count()
                        },
                        BatchSize::SmallInput,
                    )
                });
            }
            group.bench_function(BenchmarkId::new("hand_written_baseline", &parameter), |b| {
                let mut next = rotation(&batches);
                b.iter_batched(
                    || next().clone(),
                    |batch| baseline.route(batch, &mut selected).len(),
                    BatchSize::SmallInput,
                )
            });
        }
    }
    group.finish();
}

// The default program's frame: a region per routing filter.

/// Returns the configuration binding of policy `index`'s list `list` (`names` or `prefixes`), and its group key.
fn routing_binding(index: usize, list: &str) -> String {
    format!("metrics_endpoint_routing.group.{:016x}.{}", 0xa000 + index, list)
}

/// The stage 4 default program with `policies` endpoint routing groups in place of its two, policy `i` selecting what
/// [`policy_lists`] lists through its group's bindings, and the catalog it validates against.
///
/// The policy fan-out's ports are the primary output's, then each routing filter's, then the Multi-Region Failover
/// gateway's and the autoscaling failover gateway's, as lowering numbers them.
fn default_program_with(policies: usize) -> (Plan, MemoryCatalog) {
    let mut value = serde_json::from_str::<Value>(DEFAULT_PROGRAM_JSON).unwrap();
    let routing = |id: &Value| id.as_str().is_some_and(|id| id.starts_with("metrics_routing_"));
    let nodes = value["nodes"].as_array_mut().unwrap();
    nodes.retain(|node| !routing(&node["id"]));
    for node in nodes.iter_mut() {
        if node["id"] == "metrics_policy" {
            node["params"]["ports"] = json!(policies + 3);
        }
    }
    let edges = value["edges"].as_array_mut().unwrap();
    edges.retain(|edge| !routing(&edge["from"]) && !routing(&edge["to"]));
    for edge in edges.iter_mut() {
        if edge["from"] == "metrics_policy.3" {
            edge["from"] = json!(format!("metrics_policy.{}", policies + 1));
        } else if edge["from"] == "metrics_policy.4" {
            edge["from"] = json!(format!("metrics_policy.{}", policies + 2));
        }
    }
    value["outputs"]
        .as_array_mut()
        .unwrap()
        .retain(|output| !routing(&output["id"]));

    let mut catalog = default_program_catalog();
    for index in 0..policies {
        let (names, prefixes) = policy_lists(index);
        let split = format!("metrics_routing_filter_{}", index);
        let output = format!("metrics_routing_encode_{}", index);
        let recipient = format!("dd_out/additional_{}/0", index);
        let matches = |list: &str, values: &[String], prefix: bool| {
            let values = json!({"config": routing_binding(index, list), "resolved": values});
            json!({"intake_name": {"matches": {"values": values, "prefix": prefix}}})
        };
        let predicate = json!({"any": [matches("names", &names, false), matches("prefixes", &prefixes, true)]});
        let provenance = |native: &str| json!({"native": native});
        value["nodes"].as_array_mut().unwrap().extend([
            json!({"id": split, "op": "split", "params": {"predicate": predicate}, "provenance": provenance(&split)}),
            json!({"id": format!("{}_true", split), "op": "emit", "params": {"output": output},
                   "provenance": provenance(&output)}),
            json!({"id": format!("{}_false", split), "op": "drop", "provenance": provenance(&split)}),
        ]);
        value["edges"].as_array_mut().unwrap().extend([
            json!({"from": format!("metrics_policy.{}", index + 1), "to": format!("{}.in", split)}),
            json!({"from": format!("{}.true", split), "to": format!("{}_true.in", split)}),
            json!({"from": format!("{}.false", split), "to": format!("{}_false.in", split)}),
        ]);
        let accepts = json!({"kinds": "all", "timestamps": "mixed"});
        value["outputs"].as_array_mut().unwrap().push(json!({
            "id": output, "accepts": accepts, "fingerprint": fingerprint(&output), "recipients": [recipient],
            "delivery": "native"
        }));
        catalog = catalog.with_output(
            OutputId::new(output.as_str()).unwrap(),
            CatalogOutput {
                accepts: PortType::ANY,
                fingerprint: fingerprint(&output).parse().unwrap(),
                recipients: vec![RecipientId::new(recipient).unwrap()],
                legacy_fingerprint: None,
            },
        );
        for list in ["names", "prefixes"] {
            catalog = catalog.with_binding(
                BindingName::new(routing_binding(index, list)).unwrap(),
                ParamType::List(&ParamType::String),
                Liveness::Startup,
            );
        }
    }
    (Plan::from_value(value).unwrap(), catalog)
}

/// A program compiled into its own frame.
struct DefaultProgram {
    frame: Frame,
    compiled: CompiledPlan,
}

impl DefaultProgram {
    fn new(plan: &Plan, catalog: &MemoryCatalog) -> Self {
        let registry = OperatorRegistry::native();
        let report = Validator::new(&registry, catalog).validate(plan);
        assert!(report.is_valid(), "{}", report);
        let frame = Frame::compute(plan, &registry, &default_program_slot_name).unwrap();
        let compiled = compile(plan, &report, &registry, &frame).unwrap();
        Self { frame, compiled }
    }

    /// Returns the slot named `name`.
    fn slot(&self, name: &str) -> SlotId {
        self.frame
            .slots()
            .iter()
            .find(|slot| slot.name.as_str() == name)
            .unwrap_or_else(|| panic!("no slot `{}`", name))
            .id
    }

    fn program(&self, slot: SlotId) -> &Arc<RegionProgram> {
        &self.compiled.programs[slot.get()]
    }

    /// Returns the decision whose first node is `node`.
    fn decision(&self, node: &str) -> (SlotId, &Decision) {
        self.compiled
            .programs
            .iter()
            .find_map(|program| {
                program
                    .decisions()
                    .iter()
                    .find(|decision| decision.root().node.local().as_str() == node)
                    .map(|decision| (program.slot(), decision))
            })
            .unwrap_or_else(|| panic!("no decision starts at `{}`", node))
    }

    /// Returns `slot` and every slot downstream of it, in the frame's slot order.
    fn downstream(&self, slot: SlotId) -> Vec<SlotId> {
        let mut found = vec![slot];
        let mut next = 0;
        while let Some(&current) = found.get(next) {
            next += 1;
            for exit in &self.frame.slot(current).unwrap().exits {
                if let FrameTarget::Slot(target) = exit.target {
                    if !found.contains(&target) {
                        found.push(target);
                    }
                }
            }
        }
        found.sort();
        found
    }
}

/// One region of a pipeline: its slot, its driver, and the sink its exits are written to.
struct Stage {
    slot: SlotId,
    driver: MetricRegionDriver,
    sink: ExitSink,
}

/// A chain of region drivers, one per slot of a pipeline: what one writes to an exit leading to the next is handed to
/// the next, as the topology delivers it, and everything else is delivered, by exit target.
struct Pipeline {
    frame: Frame,
    stages: Vec<Stage>,
}

impl Pipeline {
    fn new(program: &DefaultProgram, generation: &GenerationRef, slots: &[SlotId], memo_entries: usize) -> Self {
        let stages = slots
            .iter()
            .map(|&slot| Stage {
                slot,
                driver: driver(&program.frame, slot, program.program(slot), generation, memo_entries),
                sink: ExitSink::new(program.frame.slot(slot).unwrap().exits.len()),
            })
            .collect();
        Self {
            frame: program.frame.clone(),
            stages,
        }
    }

    /// Runs a batch through the pipeline, returning what left it, by target.
    fn run(&mut self, batch: EventsBuffer, now: Instant, budget: &mut Budget) -> Vec<(FrameTarget, EventsBuffer)> {
        let mut delivered = Vec::new();
        let mut pending = vec![(0, batch)];
        let slots = self.stages.iter().map(|stage| stage.slot).collect::<Vec<_>>();
        while let Some((index, batch)) = pending.pop() {
            let stage = &mut self.stages[index];
            stage
                .driver
                .process(&mut RegionTurn::new(now, budget), batch, &mut stage.sink);
            let exits = &self.frame.slot(stage.slot).unwrap().exits;
            for (exit, buffer) in stage.sink.drain() {
                let target = exits[exit.get()].target.clone();
                let next = match &target {
                    FrameTarget::Slot(next) => slots.iter().position(|slot| slot == next),
                    _ => None,
                };
                match next {
                    Some(next) => pending.push((next, buffer)),
                    None => delivered.push((target, buffer)),
                }
            }
        }
        delivered
    }
}

/// The batches entering the enrichment region: gauges named by [`context_name`], except that every hundredth context
/// is a `requests.count` series, which the autoscaling failover gateway forwards, each with a tag of its own.
fn enrichment_inputs(header: &BatchHeader, contexts: usize) -> Vec<EventsBuffer> {
    batches_of(header, contexts, |context| {
        if context % 100 == 1 {
            let tags = [Tag::from(format!("instance:{}", context))]
                .into_iter()
                .collect::<TagSet>();
            Context::from_parts("requests.count", tags)
        } else {
            Context::from_parts(context_name(context), TagSet::default())
        }
    })
}

/// Runs one kernel on a buffer in place, with the batch's header.
fn in_place(
    kernel: &mut Box<dyn Kernel>, header: &BatchHeader, now: Instant, counters: &mut TurnCounters,
    batch: &mut EventsBuffer,
) {
    let mut budget = Budget::unlimited();
    let mut ports = PortOutputs::new(1);
    let mut turn = Turn::new(header, now, &mut budget, &mut ports, counters);
    kernel.consume(&mut turn, batch);
}

fn kernel_of(program: &RegionProgram, node: &str) -> Box<dyn Kernel> {
    let kernel = program
        .kernels()
        .iter()
        .find(|kernel| kernel.key.node.local().as_str() == node)
        .unwrap_or_else(|| panic!("no kernel `{}`", node));
    kernel
        .spec
        .build(&KernelBuildContext::detached(&kernel.key.node))
        .unwrap()
}

/// The hand-written post-enrichment path of the default program: the enrichment kernels, then the policy fan-out to
/// the primary output, every routing policy, Multi-Region Failover (which forwards everything with the fixture's
/// configuration), and autoscaling failover (scalar `requests.count` series).
///
/// Outputs are numbered as [`select`][Self::select] numbers them: 0 for the primary output, `i + 1` for policy `i`'s,
/// then Multi-Region Failover's and autoscaling failover's.
struct DefaultProgramBaseline {
    enrichment: Box<dyn Kernel>,
    host_tags: Box<dyn Kernel>,
    policies: Vec<(SortedSetMatcher, SortedSetMatcher)>,
    autoscaling: RawNameSet,
    counters: TurnCounters,
    selected: Vec<u64>,
}

impl DefaultProgramBaseline {
    fn new(program: &DefaultProgram, policies: usize) -> Self {
        let enrich = program.program(program.slot("metrics_enrich"));
        Self {
            enrichment: kernel_of(enrich, "metrics_enrich"),
            host_tags: kernel_of(enrich, "metrics_enrich_host_tags"),
            policies: policy_matchers(policies),
            autoscaling: RawNameSet::new(["requests.count"]),
            counters: TurnCounters::default(),
            selected: Vec::new(),
        }
    }

    fn mrf(&self) -> usize {
        self.policies.len() + 1
    }

    fn autoscaling(&self) -> usize {
        self.policies.len() + 2
    }

    /// Returns the name of output `index`.
    fn output(&self, index: usize) -> String {
        if index == 0 {
            "dd_metrics_encode".to_string()
        } else if index == self.mrf() {
            "mrf_metrics_encode".to_string()
        } else if index == self.autoscaling() {
            "af_metrics_encode".to_string()
        } else {
            format!("metrics_routing_encode_{}", index - 1)
        }
    }

    /// Runs the enrichment kernels over a batch, in place, returning its header.
    fn enrich(&mut self, batch: &mut EventsBuffer, now: Instant) -> BatchHeader {
        let header = batch.take_header();
        in_place(&mut self.enrichment, &header, now, &mut self.counters, batch);
        in_place(&mut self.host_tags, &header, now, &mut self.counters, batch);
        header
    }

    /// Returns the outputs an enriched event goes to.
    fn select(&self, event: &Event) -> u64 {
        let always = 1 | (1 << self.mrf());
        let Some(metric) = event.try_as_metric() else {
            return always;
        };
        let mut view = EventView::new(metric);
        let mut selected = always;
        if is_scalar_series(metric.values()) && self.autoscaling.contains(view.name()) {
            selected |= 1 << self.autoscaling();
        }
        if let Some(name) = view.intake_name() {
            selected |= policies_matched(&self.policies, name, 1);
        }
        selected
    }

    /// The whole selection at once: enrichment, then each event's outputs, then a buffer of copies of exactly its size
    /// for each selecting output but the primary one, which receives the original.
    fn selection(&mut self, mut batch: EventsBuffer, now: Instant) -> Vec<(usize, EventsBuffer)> {
        let header = self.enrich(&mut batch, now);
        let mut selected = std::mem::take(&mut self.selected);
        selected.clear();
        selected.extend((&batch).into_iter().map(|event| self.select(event)));
        let mut routed = Vec::with_capacity(self.autoscaling() + 1);
        for output in 1..=self.autoscaling() {
            if let Some(mut copies) = copies(&batch, &selected, 1 << output) {
                copies.set_header(header.clone());
                routed.push((output, copies));
            }
        }
        batch.set_header(header);
        routed.push((0, batch));
        self.selected = selected;
        routed
    }

    /// The native components' shape: enrichment, then a copy of the batch for every gateway, each filtered in place.
    fn fan_out(&mut self, mut batch: EventsBuffer, now: Instant) -> Vec<(usize, EventsBuffer)> {
        let header = self.enrich(&mut batch, now);
        let mut routed = Vec::with_capacity(self.autoscaling() + 1);
        for (index, (names, prefixes)) in self.policies.iter().enumerate() {
            let mut copy = batch.clone();
            copy.retain(|event| {
                event.try_as_metric().is_some_and(|metric| {
                    EventView::new(metric)
                        .intake_name()
                        .is_some_and(|name| names.matches(name) || prefixes.matches(name))
                })
            });
            if !copy.is_empty() {
                copy.set_header(header.clone());
                routed.push((index + 1, copy));
            }
        }
        // Multi-Region Failover forwards everything with the fixture's configuration.
        let mut mirrored = batch.clone();
        mirrored.set_header(header.clone());
        routed.push((self.mrf(), mirrored));
        let mut autoscaled = batch.clone();
        autoscaled.retain(|event| {
            event.try_as_metric().is_some_and(|metric| {
                is_scalar_series(metric.values()) && self.autoscaling.contains(metric.context().name())
            })
        });
        if !autoscaled.is_empty() {
            autoscaled.set_header(header.clone());
            routed.push((self.autoscaling(), autoscaled));
        }
        batch.set_header(header);
        routed.push((0, batch));
        routed
    }

    /// The enrichment kernels alone.
    fn enrichment_only(&mut self, mut batch: EventsBuffer, now: Instant) -> EventsBuffer {
        let header = self.enrich(&mut batch, now);
        batch.set_header(header);
        batch
    }
}

/// Returns what each output received, by output name, each output's events sorted.
fn by_output(delivered: impl IntoIterator<Item = (String, EventsBuffer)>) -> BTreeMap<String, Vec<String>> {
    let mut outputs = BTreeMap::<String, Vec<String>>::new();
    for (output, buffer) in delivered {
        outputs.entry(output).or_default().extend(names(&buffer));
    }
    for events in outputs.values_mut() {
        events.sort();
    }
    outputs
}

fn bench_default_program(c: &mut Criterion) {
    // Kernels with caches need a Tokio runtime to be built in.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let now = Instant::now();

    let mut group = c.benchmark_group("selection_default_program");
    group.throughput(Throughput::Elements(1024));
    for policies in POLICIES {
        let (plan, catalog) = default_program_with(policies);
        let program = DefaultProgram::new(&plan, &catalog);
        // R1 never cuts a decision: the policy fan-out, every routing filter, and both failover gateways are one
        // decision of the enrichment region, and it is memoized.
        let (slot, decision) = program.decision("metrics_policy");
        assert_eq!(slot, program.slot("metrics_enrich"));
        assert_eq!(decision.nodes.len(), policies + 3);
        assert!(decision.memoized);
        let slots = program.downstream(program.slot("metrics_enrich"));
        let otlp = program
            .frame
            .admission_point_at(&"otlp_in".parse().unwrap(), &"metrics".parse().unwrap())
            .unwrap()
            .id;
        let mut memoized = Pipeline::new(&program, &generation, &slots, DECISION_MEMO_ENTRIES as usize);
        let mut evaluated = Pipeline::new(&program, &generation, &slots, 0);
        let mut baseline = DefaultProgramBaseline::new(&program, policies);

        for contexts in CONTEXTS {
            let batches = enrichment_inputs(&header(&generation, otlp), contexts);

            // Equal delivered work: every output receives the same events from the regions, memoized or not, and from
            // either baseline.
            for batch in &batches {
                let expected = by_output(
                    baseline
                        .selection(batch.clone(), now)
                        .into_iter()
                        .map(|(output, buffer)| (baseline.output(output), buffer)),
                );
                let fanned_out = by_output(
                    baseline
                        .fan_out(batch.clone(), now)
                        .into_iter()
                        .map(|(output, buffer)| (baseline.output(output), buffer)),
                );
                assert_eq!(fanned_out, expected);
                for pipeline in [&mut memoized, &mut evaluated] {
                    let delivered = pipeline
                        .run(batch.clone(), now, &mut Budget::unlimited())
                        .into_iter()
                        .map(|(target, buffer)| match target {
                            FrameTarget::Destination(output) => (output.to_string(), buffer),
                            target => panic!("a delivery to {:?}", target),
                        });
                    assert_eq!(by_output(delivered), expected);
                }
            }

            let parameter = format!("{} policies, {} contexts", policies, contexts);
            for (name, pipeline) in [
                ("regions_memoized", &mut memoized),
                ("regions_evaluated", &mut evaluated),
            ] {
                group.bench_function(BenchmarkId::new(name, &parameter), |b| {
                    let mut budget = Budget::unlimited();
                    let mut next = rotation(&batches);
                    b.iter_batched(
                        || next().clone(),
                        |batch| pipeline.run(batch, now, &mut budget).len(),
                        BatchSize::SmallInput,
                    )
                });
            }
            group.bench_function(BenchmarkId::new("hand_written_selection", &parameter), |b| {
                let mut next = rotation(&batches);
                b.iter_batched(
                    || next().clone(),
                    |batch| baseline.selection(batch, now).len(),
                    BatchSize::SmallInput,
                )
            });
            group.bench_function(BenchmarkId::new("hand_written_fan_out", &parameter), |b| {
                let mut next = rotation(&batches);
                b.iter_batched(
                    || next().clone(),
                    |batch| baseline.fan_out(batch, now).len(),
                    BatchSize::SmallInput,
                )
            });
            group.bench_function(BenchmarkId::new("enrichment_only", &parameter), |b| {
                let mut next = rotation(&batches);
                b.iter_batched(
                    || next().clone(),
                    |batch| baseline.enrichment_only(batch, now).len(),
                    BatchSize::SmallInput,
                )
            });
        }
    }
    group.finish();
}

// Memo misses: the default program's decisions with more contexts than their memos hold.

/// The batches the listener filterlist's region receives from the mapper's: gauges named for applications, every
/// fiftieth one the blocked name, over `contexts` contexts.
fn listener_inputs(header: &BatchHeader, contexts: usize) -> Vec<EventsBuffer> {
    batches_of(header, contexts, |context| {
        if context.is_multiple_of(50) {
            Context::from_parts("raw.blocked", TagSet::default())
        } else {
            Context::from_parts(format!("app.queue{}", context), TagSet::default())
        }
    })
}

/// The batches as the listener filterlist's decision sees them: after the namespace prefix.
fn prefixed(batches: &[EventsBuffer]) -> Vec<EventsBuffer> {
    batches
        .iter()
        .map(|batch| {
            let mut prefixed = EventsBuffer::with_capacity(batch.len());
            for event in batch {
                let metric = event.try_as_metric().unwrap();
                let name = format!("tenant.{}", metric.context().name());
                let context = Context::from_parts(name, TagSet::default());
                assert!(prefixed.try_push(Event::Metric(Metric::gauge(context, 1.0))).is_none());
            }
            prefixed
        })
        .collect()
}

fn bench_memo_misses(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let generation: GenerationRef = Arc::new(GenerationToken::new(GenerationId::new(1)));
    let now = Instant::now();
    let (plan, catalog) = default_program_with(1);
    let program = DefaultProgram::new(&plan, &catalog);

    let mut group = c.benchmark_group("memo_misses");
    group.throughput(Throughput::Elements(1024));
    let otlp = program
        .frame
        .admission_point_at(&"otlp_in".parse().unwrap(), &"metrics".parse().unwrap())
        .unwrap()
        .id;
    for (name, root, memoized) in [
        ("listener_filterlist", "dsd_prefix_filter_blocklist", false),
        ("policy_region", "metrics_policy", true),
    ] {
        let (slot, decision) = program.decision(root);
        assert_eq!(decision.memoized, memoized, "`{}`", root);
        let function = DecisionFunction::new(decision).unwrap();
        // How batches enter the decision's slot: the listener filterlist's from the mapper's region, by the one entry
        // of its slot, and the policy region's at OTLP's admission point, into the enrichment region.
        let stamped = if memoized {
            assert_eq!(slot, program.slot("metrics_enrich"));
            header(&generation, otlp)
        } else {
            let [entry] = program.program(slot).entries() else {
                panic!("`{}` has one entry", root)
            };
            let EntryKey::Region(entry) = entry.key else {
                panic!("`{}` is fed by another region", root)
            };
            let mut stamped = header(&generation, AdmissionPointId::new(0));
            stamped.entry = Some(entry);
            stamped
        };

        for contexts in MISS_CONTEXTS {
            // What the decision sees: for the listener filterlist, names after the namespace prefix.
            let (batches, decided) = if name == "listener_filterlist" {
                let batches = listener_inputs(&stamped, contexts);
                let decided = prefixed(&batches);
                (batches, decided)
            } else {
                let batches = inputs(&stamped, contexts);
                (batches.clone(), batches)
            };
            let mut memo = DecisionMemo::for_exits(DECISION_MEMO_ENTRIES as usize, decision.exits.len());
            for event in decided.iter().flatten() {
                assert_eq!(function.evaluate_memoized(event, &mut memo), function.evaluate(event));
            }

            let parameter = format!("{}, {} contexts", name, contexts);
            group.bench_function(BenchmarkId::new("function_evaluated", &parameter), |b| {
                let mut next = rotation(&decided);
                b.iter(|| {
                    next()
                        .into_iter()
                        .fold(ExitSet::EMPTY, |all, event| all.union(function.evaluate(event)))
                })
            });
            group.bench_function(BenchmarkId::new("function_memoized", &parameter), |b| {
                let mut next = rotation(&decided);
                b.iter(|| {
                    next().into_iter().fold(ExitSet::EMPTY, |all, event| {
                        all.union(function.evaluate_memoized(event, &mut memo))
                    })
                })
            });
            // A region memoizes only the decisions R4 memoizes.
            if !memoized {
                continue;
            }
            let exits = program.frame.slot(slot).unwrap().exits.len();
            for (variant, entries) in [
                ("region_memoized", DECISION_MEMO_ENTRIES as usize),
                ("region_evaluated", 0),
            ] {
                let mut region = driver(&program.frame, slot, program.program(slot), &generation, entries);
                let mut sink = ExitSink::new(exits);
                group.bench_function(BenchmarkId::new(variant, &parameter), |b| {
                    let mut budget = Budget::unlimited();
                    let mut next = rotation(&batches);
                    b.iter_batched(
                        || next().clone(),
                        |batch| {
                            region.process(&mut RegionTurn::new(now, &mut budget), batch, &mut sink);
                            sink.drain().count()
                        },
                        BatchSize::SmallInput,
                    )
                });
            }
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_function,
    bench_region,
    bench_default_program,
    bench_memo_misses
);
criterion_main!(benches);
