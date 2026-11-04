# Programmable metric processing: deployment context and next steps

What we learned about how ADP is deployed after stages 1 to 7 and 9 were built, what that means for the work, what
the measurements in the production regime found, and what the next chunk of work is. Read the
[index](programmable-metrics-index.md) first for the overall picture.

## Contents

- [Where it stands](#where-it-stands)
- [Deployment context](#deployment-context)
- [What it means for what we built](#what-it-means-for-what-we-built)
- [Lessons](#lessons)
- [Direction: programs replace native policy](#direction-programs-replace-native-policy)
- [The next chunk of work](#the-next-chunk-of-work), with the [gates](#gates-for-n2-to-n4)
- [The two N1 measurements](#the-two-n1-measurements): [N1a](#n1a-steady-load-against-the-smp-tiers) and
  [N1b](#n1b-bursts-across-a-flush)
- [What stalls admission at a flush](#what-stalls-admission-at-a-flush)
- [V7b against N2](#v7b-against-n2): [candidate results](#v7b-candidate-results) and
  [layout A/B](#layout-ab-at-3k-contexts)
- [Iterating on efficiency](#iterating-on-efficiency): [how we iterate](#how-we-iterate),
  [hypotheses](#hypotheses), [what the first iterations showed](#what-the-first-iterations-showed)
- [Lower priority](#lower-priority)
- [Open questions](#open-questions)

## Where it stands

As of 2026-11-04:

- **N1 was measured twice, by two complementary harnesses.** [N1a](#n1a-steady-load-against-the-smp-tiers)
  (`run-limits.sh`, 2026-10-13 and 14) ran steady loads of the four SMP quality-gate tiers at 0.05, 0.3, 0.6, and 1.2
  CPUs; [N1b](#n1b-bursts-across-a-flush) (`run-regime.sh`, 2026-10-20 and 21) ran a steady window and then a burst
  timed across an aggregation flush at the same limits, with an A/A pair per image. They agree:
  - **The plan costs slightly less CPU per metric than native:** 4 to 6% less from the medium tier up in N1a, 0.93 to
    1.00 of native in N1b's steady windows.
  - **The plan holds memory under a CPU crunch, because its datagram pool barely grows**, where native keeps reading
    into its pool (N1a's finding 4; N1b's pool and peak memory columns; ADP finding G7).
  - **Fusion alone saves nothing and costs nothing:** maximal fusion saved no instructions in N1a's quick loop (H2),
    and fused against isolated layouts measured the same CPU in N1b's harness ([layout A/B](#layout-ab-at-3k-contexts)).
  - **What follows a flush is CPU starvation, the same on native:** N1a's instrumented follow-up found that the drain
    pauses admission for only 30 to 75 ms at 0.6 to 1.2 cores, and that most of the stall is the **encoder's burst**
    after it, two to three seconds of encoding and compression, about 85% of a flush's cost, starving admission under
    the quota ([what stalls admission at a flush](#what-stalls-admission-at-a-flush)). N1b's V7b runs saw the same from
    the other side: with the drain no longer blocking, admission still fell after the flush, on every image, at a
    full quota.
- **N2 is now four layers** ([N2](#n2-keep-admitting-through-a-flush-in-four-layers)): a non-blocking drain, pacing,
  priority for admission, and shedding. **Layer 1 is built**, by version 2's step V7b (2026-10-21), together with N3
  ([N2 and N3](#n2-and-n3-one-region-from-decode-to-encode-with-an-interleaved-flush)): without lanes the metrics path
  is one region, R1's cost model is gone, lanes are opt-in, and a flush interleaves with admission. Its blocked flush
  time is 0, but it missed the exit gates G5 and G6, which the encoder's burst explains
  ([V7b against N2](#v7b-against-n2)). The owner decided on 2026-10-21 that V7b is done as optionality and that the
  merged layout stays the default, and deferred flush pacing, encode cost, the pool question, and G6's thresholds.
- **Open:** layer 2, pacing (V7b's pace is a floor: a pass completes in 0.07 to 0.6 s instead of spreading); layer 3,
  priority for admission (the cheapest experiment is the encoder at a lower thread priority, in the quick loop); and
  layer 4, shedding (N5).
- **Efficiency per metric (N4) is mostly ADP-wide** and lives in [ADP efficiency findings](adp-efficiency-findings.md):
  per-metric tag hashing (22 to 29% of instructions), the allocator thread cache (3 to 7.5% fewer instructions), the
  size of `Event`, native buffering when starved, and the encoder's burst (G9) among them.
- **Gates:** N2's layers are judged by stall gates measured inside ADP, N3 and N4 by instructions per metric, and burst
  behavior by N1b's G1 to G5; G6's thresholds are superseded ([gates](#gates-for-n2-to-n4)).
- **On the plan side**, version 2's V8, V9, and V1 are next ([version 2](programmable-metrics-v2.md#what-is-left)).

## Deployment context

From a discussion with the ADP team on 2026-10-13, after stage 9 was committed:

- **CPU limits are small.** No production ADP container has a CPU limit above 1.2 cores. Over a week, every cluster's
  largest limit fell into one of three tiers, 0.3, 0.6, or 1.2 cores. Most clusters are at 0.3, and about 15 reach 1.2.
  There are no large multi-core deployments where scaling across many threads would pay off.
- **The worst case matters more than the throughput ceiling.** In Nodeless, ADP is guaranteed only 50 millicores. More
  generally, the workloads that use the most CPU also emit the most metrics, so ADP has to process its largest bursts
  exactly when CPU is scarcest. Efficiency per metric and behavior under a CPU crunch are what to optimize.
- **Concurrency helps only in places.** Some throughput work has paid off before (the DogStatsD source's separate
  decoder stage, to match the Core Agent), but the team knows of no deployment bottlenecked by concurrency rather than
  by its CPU limit.
- **The biggest practical problem is aggregation, which is serial and blocking.** A fix must not be a static sharding
  scheme, or anything else that pays a fixed overhead even at low load.

## What it means for what we built

The view of 2026-10-13, before N1; the status column notes what changed since.

| Work | Relevance at these limits |
|---|---|
| Stages 1 to 6: one plan, fused regions | **The relevant win.** Under a CPU quota, fewer tasks and hops are pure gain. At one worker, stage 3 measured 1.27 times native throughput at 0.77 of native worker time per metric ([implementation risks](programmable-metrics-implementation.md#risks)). N1 confirmed a smaller margin in the production regime: 4 to 6% less CPU per metric than native from the medium tier up; the plan's own runtime costs less than the Tokio overhead its fusion removes ([first iterations](#what-the-first-iterations-showed)). |
| Stage 7: live outputs and recipients | Neutral on CPU: per metric within 1.01 of native, allocations 0.56 to 0.89, p99 0.27 to 0.65 ([stage 7 gate](programmable-metrics-stages-7-and-9.md#83-stage-7-gate)). Its value is operational. |
| Stage 9: lanes | **Little production value, and correctly off at these sizes.** ADP's worker count is the process's available parallelism, with a minimum of two (`saluki-app` `util.rs`), and on Linux that follows the cgroup's CPU quota, so every limit up to two cores gives two workers; `lanes.mode: auto` then runs the layout without lanes ([lanes as built](programmable-metrics-stages-7-and-9.md#stage-9-as-built)). Its static key sharding is the fixed-overhead scheme the team wants to avoid for aggregation. Since V7b, lanes are opt-in (`lanes.mode: off` by default). |
| R1's fusion cost model | **Wrong for these deployments.** It splits a chain into pipelined regions when one region would exceed the cost of the costliest native component, to buy pipeline parallelism across tasks ([pipelining](programmable-metrics-implementation.md#physical-frame-and-region-programs)). At one core or less that buys nothing and adds a hop and a queue per split. Deleted by V7b. |
| Our measurements | **The wrong regime, until N1.** They were saturated throughput at 1 to 16 unconstrained workers. None ran under a CPU quota, with CFS throttling, with bursts during a CPU crunch, or at 50 millicores ([measurement runbook](programmable-metrics-measurement.md)). [N1a and N1b](#the-two-n1-measurements) now measure that regime. |
| The stage 9 gate host ([Q2](programmable-metrics-stages-7-and-9.md#12-open-questions)) | No longer a priority: what it would decide does not apply to production. |

**Aggregation before V7b.** Stage 6 made aggregation a kernel whose flush runs in budgeted steps
([drains](programmable-metrics-implementation.md#drains)): it yields every 4,096 contexts or 1,024 emitted metrics, and
the host answers liveness and control between steps. But the host **reads no input while a drain is pending**, so
admission stops for the whole flush: still serial and blocking. A flush costs about 213 ns per context, about 0.2 s of
CPU at 1M contexts, which at 50 millicores is several seconds without admission, while the socket fills and the
datagram pool grows (up to about 256 MiB at its default ceiling,
[decisions](programmable-metrics-status.md#decisions-awaiting-the-owner)). *Measured since (2026-10-15):* the drain's
pause is short, 28 to 76 ms at 0.6 and 1.2 cores and 100 to 290 ms at 0.3 for the heavy tier's 29,048 series, and most
of the stall is the encoder's burst after it ([what stalls admission at a flush](#what-stalls-admission-at-a-flush)).
Since V7b, without lanes, a flush interleaves with admission instead.

## Lessons

- **Establish the deployment's axes before designing for one.** Stage 9 followed the spec's plan to restore parallelism
  that fusion had removed. That plan was sound, but nobody checked whether production had cores to parallelize over.
  The first question for the next chunk is what the production regime looks like, measured.
- **Measure in the regime that matters.** A ratio at 8 unconstrained workers says little about 0.3 cores under
  throttling. Gates should be stated at the production limits.
- **Check what the measurement itself costs.** Anything run inside ADP's container, a health check or `docker exec`,
  is charged to the quota being measured: N1a's first run carried about 20 millicores of health checks, 40% of the
  quota at 0.05 ([log](programmable-metrics-log.md#the-health-check-and-the-matrix-again)).
- **Find the mechanism before building the fix.** N2 was designed around the drain; instrumenting a flush showed that
  the drain is about 3% of a flush's cost and the encoder about 85%. V7b removed the drain's stall as designed, and
  the gates it was judged by still read the encoder's.
- **The substrate is still the right one.** Generations, cuts, yielding kernels, the delivery-policy slot, and the
  reference interpreter are what the next items build on; none needs a model change.

## Direction: programs replace native policy

Decided by the owner on 2026-10-14, after the one-scope change (amendments 168 and 169).

**The vision.** The programmable substrate **replaces** ADP's native metrics implementation and its hard-coded policy
configuration. The default program exists to prove that a program can express native semantics; programs must not
become a more complex control surface over native features.

**A program's external inputs** are of three kinds:

| Kind | Examples | How a program uses them |
|---|---|---|
| **Resources** (privileged) | endpoints and recipients, credentials, sources | declared in configuration (since version 2's step V2, the only place); referenced by name |
| **Facts and signals** | hostname, host tags, whether Multi-Region Failover is failing over metrics, whether autoscaling failover is on | referenced as bindings, resolved by the host |
| **Policy settings** | routing allowlists, the metric and tag filterlists, the failover mirroring list, aggregation's window and other parameters | owned by the published program, as literals |

**Detached programs.** A published program detaches from native policy settings: nothing is merged. Native policy
configuration is only an input to lowering the default program, a compatibility front end, as a legacy language compiles
to the same IR. With no artifact, ADP runs the default program, which binds native settings exactly as today, so
equivalence and live configuration changes, Remote Config included, are preserved. Status says plainly when native
policy settings are not applied.

**Where native compatibility lives.** Machinery that exists to work with native features belongs in lowering, the
compatibility layer, never in the core model. Nothing new in the substrate exists only to work with a native feature.

**Constraints that remain:** the telemetry contracts with the Core Agent (such as the `component_id:dsd_agg` series and
the `aggregator.*` remaps); live configuration keeps reaching the default program during the transition; and efficiency
at production CPU limits ([N1](#n1-measure-in-the-production-regime)) gates replacing native.

**The next work** is to simplify, making the demo and the default program easier to read with less magic, in four steps:
a detached export for editing; readable recipient names and emits addressed to recipients; authored programs that bind
only facts and signals; and one aggregation that keeps its state whatever its node is called. The design is
[detached programs](programmable-metrics-detached-programs.md), with amendments 170 to 182.

## The next chunk of work

In order. Each item has its goal, the approach, its status, and the evidence that ends it.

### N1: measure in the production regime

- **Goal:** a baseline of plan against native at production limits, so every later item has a gate.
- **Approach, as planned (2026-10-13):** run at 0.05, 0.3, 0.6, and 1.2 CPUs with lanes off, steady and bursty traffic
  (a burst several times the steady rate, at low and high cardinality, timed across a flush). Measure CPU per metric,
  throttled periods and time (`cpu.stat`), socket and pool drops, the datagram pool's depth, p99 latency, peak and
  anonymous memory, and admission stalls during a flush. Add an A/A series per limit, as the gate's method requires.
  Linux is required.
- **Exit:** a results table in this document, and the gate thresholds for N2 to N4 stated at these limits.
- **Status: met, twice.** [N1a](#n1a-steady-load-against-the-smp-tiers) ran the steady half against the SMP tiers
  (the owner decided against modeling bursts there: a limit absorbs a burst up to the steady load it sustains, which
  saturate runs measure), and its follow-up instrumented a flush. [N1b](#n1b-bursts-across-a-flush) ran the burst half
  across a flush. [How they relate](#the-two-n1-measurements); the gates are [below](#gates-for-n2-to-n4).

### N2: keep admitting through a flush, in four layers

- **Goal:** admission continues during a flush, with no sharding and no fixed overhead at low load.
- **Approach, as first written:** a flush drains only closed buckets, and admission writes only open ones: buckets
  close by watermark, and admission time is at or past the watermark. So the host can keep reading input while a drain
  is pending, and interleave admission with drain steps, under a policy for how much of each turn goes to each. What
  needs design: the kernel contract (`consume` while a `process` or `advance_time` is yielded), the open and closed
  state kept apart so a context touched by both stays correct (a counter admitted during its own bucket's flush,
  idle-counter zeros, context expiry), memory in flight during a long drain, and the reference interpreter's and the
  transition traces' view of interleaving. The stage 6
  [time-driven output](programmable-metrics-implementation.md#time-driven-output) and drains are the starting point.
- **Revised (2026-10-15): four layers.** Measured at 100 ms, the drain pauses admission for only 30 to 75 ms at 0.6 to
  1.2 cores (100 to 290 ms at 0.3); most of the stall is the encoder's burst after it, which a non-blocking drain alone
  does not change ([what stalls admission at a flush](#what-stalls-admission-at-a-flush)). The drain is already a
  resumable, budgeted kernel call; what the layers add is what decides when work runs. Each layer works without the
  ones after it.
  1. **Non-blocking drain** (the approach above): the host admits input between drain steps, which needs a kernel
     contract for consuming while a drain is suspended, with open and closed buckets kept apart. The semantic
     foundation. **Built by V7b** without lanes
     ([N2 and N3](#n2-and-n3-one-region-from-decode-to-encode-with-an-interleaved-flush)): blocked flush time is 0 in
     every candidate run whose drains completed ([results](#v7b-candidate-results)).
  2. **Pacing:** a policy that releases drain steps over time (so many series per 100 ms, or within the headroom)
     rather than back to back, so the encoder's work, which follows its input batch by batch, spreads with them. It
     needs the non-blocking drain, or admission would stop for the whole paced drain. It removes the spike where
     average demand, flush work included, fits under the quota. **Open.** V7b paces a pass to a third of the flush
     interval, but only as a floor: the region runs a step whenever no batch waits, so a pass completes in 0.07 to
     0.6 s and the encoding lands right after the flush. Capping a pass at its pace is the deferred change.
  3. **Priority for admission:** where demand exceeds the quota, something must wait, and output can wait seconds where
     admission cannot. Tokio has no task priorities and throttling stops the whole cgroup, so this needs the encoder on
     threads of lower CPU weight (a `nice` value or `SCHED_IDLE` within the cgroup) or a per-period budget for output
     work. It acts on the encoder directly, and applies to native ADP too (ADP finding G9). **Open**, and the cheapest
     experiment: the encoder at a lower thread priority, in the quick loop, before any design work. It bears on V8,
     whose plan makes encoding a terminal kernel in the region: then only a per-period budget for output work could
     give admission priority.
  4. **Shedding** ([N5](#n5-shedding-during-a-cpu-crunch)): when even delayed output cannot keep up.
- **Exit:** semantics unchanged (the interpreter, the compatibility matrix, transition traces 1 to 15); admission never
  stalls on a flush, judged by the [stall gates](#gates-for-n2-to-n4); and at 0.05 and 0.3 CPUs, fewer drops and lower
  p99 than native during a flush, with CPU per metric no worse. The p99 half stays in process.

### N2 and N3: one region from decode to encode, with an interleaved flush

N3, to settle the layout for small CPU budgets, asked for the simplest, cheapest layout at one core or less: fuse
maximally between fences whatever the costs, deleting R1's cost model (plan
[P9.7](programmable-metrics-stages-7-and-9.md#p97-r1s-cost-model) lists what it touches), and make lanes opt-in
(`lanes.mode: off` by default, amendment 157 reversed) while keeping the code; its exit was N1's measurements showing
the fused layout at least as good as today's. It was built together with N2's first layer, as below. Maximal fusion without
lanes saves no instructions at any of eight cells and costs about 1% where ADP cannot keep up (H2,
[results](#what-the-first-iterations-showed)), and fused against isolated measured the same CPU
([layout A/B](#layout-ab-at-3k-contexts)): N3 is a simplification, not a speed-up.

- **Status (2026-10-21):** built as version 2's step V7b ([design and as built](programmable-metrics-v7b.md),
  amendments 203 to 206): without lanes the metrics path is one region, `dsd_pre_aggregation`, R1's cost model is
  gone, lanes are opt-in, and a tick or boundary flush interleaves with admission, paced to a third of the flush
  interval. Approach items 1 to 3 are done; item 4 (decoding inside the region, encoding as a terminal kernel) is V8's.
  The N1b candidate runs ([results](#v7b-candidate-results)) found the stall gone but the exit not met: G5 and G6 miss
  at 0.3 CPUs, G6 at 1.2, and G1 at 3,000 contexts at 0.3 and 0.6 (G1's miss was later shown to be noise, by the
  [layout A/B](#layout-ab-at-3k-contexts)). Decided 2026-10-21: done as optionality, the merged layout stays the
  default, and the gaps (flush pacing, encode cost, G5's pool question, G6's thresholds) are deferred
  ([outcome](programmable-metrics-v7b.md#65-as-built)). Against the four layers, V7b is layer 1.
- **Goal:** at one core or less, run the whole metrics path between its I/O as one region, one task, with admission
  that never stalls on a flush, no sharding, and no fixed overhead at low load.
- **Why it is possible now.** Only I/O still needs a task of its own: the sources' sockets in, and delivery's HTTP,
  retries, and disk out. Aggregation was the last hard case, and since stage 6 it is a kernel; it runs apart only by
  design. The isolation slot gives one owner to its ticks, watermark, drains, and cuts, and the runtime allows state
  steps only there (`runtime/code.rs`). The split between `dsd_pre_aggregation` and `metrics_enrich` follows from
  that: the timestamp split's passthrough edge goes around aggregation, and with aggregation in its own task, one
  region on both sides of it would be a loop over bounded channels. In one task, aggregation's output is the next step
  of the same program, and the passthrough edge is a register. Today's boundaries and why each exists:

  | Boundary | Needed |
  |---|---|
  | Sources (socket I/O; DogStatsD's decoder) | The I/O, yes; decoding is CPU work a region could run |
  | Delivery hosts (HTTP, retries, persistence) | Yes |
  | Encode hosts (payloads, a flush timeout, leases) | Partly: encoding could become a terminal kernel |
  | The `dsd_agg` isolation slot | No longer: a design choice of stage 6 |
  | `dsd_pre_aggregation` apart from `metrics_enrich` | Only while aggregation has its own task |
  | Lanes | Only with cores to spare, which production does not have |

- **Approach:**
  1. **State steps in fused regions.** Generalize the region host's tick, watermark, and drain driving from the
     isolation slot to any slot; fuse maximally between I/O fences whatever the costs, which deletes R1's cost model.
     Make lanes opt-in while keeping the code; with lanes, aggregation can still run in a slot of its own.
  2. **An interleaved flush**, which one task makes required rather than desirable: today a drain stalls only
     aggregation's task while upstream keeps decoding into its queue; in one task it would stall everything. This is
     N2's first layer, with the design questions listed there.
  3. **Cuts and lineages keyed by node, not by slot.** The cut protocol arms over admission points, unchanged without
     the slot boundary, but re-verified: the transition traces and the reference interpreter.
  4. Later, if N1 says it pays: decoding inside the region, and encoding as a terminal kernel, leaving only sockets
     and delivery as tasks. Weigh it against N2's third layer, which wants the encoder where it can run at a lower
     priority.
- **Payoff:** two channel hops, two times a task is woken per batch, and their queues gone, which are both fixed
  overhead and a source of worst-case latency at 0.3 cores.
- **Exit:** semantics unchanged (the interpreter, the compatibility matrix, transition traces 1 to 15); admission never
  stalls on a flush; and at 0.05 and 0.3 CPUs, against native and against today's layout, fewer drops and lower p99
  during a flush, with CPU per metric no worse.

### N4: efficiency per metric

Each is a small part, measured by N1 at the production limits:

- **Passthrough batching.** The plan sends one buffer per input batch where the native batcher filled 1,024 timestamped
  metrics ([implementation risks](programmable-metrics-implementation.md#risks)); the encoder's cost of more, smaller
  buffers is unmeasured.
- **Encode and compression cost** per metric (M3 in the [runbook](programmable-metrics-measurement.md)). Since the
  stall study, this is also the size of the encoder's burst after a flush (ADP finding G9).
- **The allocator setting.** The thread cache saved 4 to 7% of CPU per point at two CPUs for about 8 MiB more peak
  memory ([decisions](programmable-metrics-status.md#decisions-awaiting-the-owner)); decide it at 0.3 and 1.2 CPUs.
- **The datagram pool's worst case** when CPU-starved: its ceiling and growth under a burst.
- **Per-context cost at high cardinality** (from N1b): at 0.05 CPUs with 100,000 contexts, about 50 µs a metric,
  growing with live contexts: context resolution and interning, the idle counters' zeros, expiry.

**Status (2026-10-14):** these are ADP-wide, not plan work, and moved with the other independent findings to
[ADP efficiency findings](adp-efficiency-findings.md); the thread cache measured 3 to 7.5% fewer instructions per
metric (G2 there), and the per-context cost at 0.05 is close to G8 there. The plan-specific hypotheses are under
[iterating on efficiency](#iterating-on-efficiency).

### N5: shedding during a CPU crunch

N2's fourth layer.

- **Goal:** degrade deliberately rather than by dropping at the socket.
- **Approach:** a design spike on the delivery-policy slot plus signals. Signals and reactive operators are the
  specification's stage 10, and the delivery-policy slot is already open to artifacts in a minimal form
  ([stage 7](programmable-metrics-stages-7-and-9.md#413-the-delivery-policy-slot)). With them, a policy could act on
  queue or pool depth: drop low-value metrics, widen aggregation, or sample. The programmable substrate is uniquely
  suited to this.
- **Exit:** a design reviewed against N1's measurements where ADP cannot keep up (N1a's cells below 97% of the rate)
  and N1b's bursts.

### Gates for N2 to N4

Three sets of gates, each from the harness that measures it best. Judge a change in the same harness and on the same
host as its baseline.

**N2's layers: the stall gates** (2026-10-15, measured inside ADP, in sequential `run-limits.sh` runs, against the
baselines of [what stalls admission at a flush](#what-stalls-admission-at-a-flush)). Today they need the flow-trace
patch in `test/scaling/baselines/stalls-2026-10-15/` (it applies to `7c7631a4bb`, before version 2), so a gate on a
current image first needs that patch ported or telemetry designed for it:

- admission per 100 ms after each flush against before it (the flow trace's count of metrics the source admits, or
  telemetry designed for it, such as the time admission pauses per flush);
- the drain's duration;
- the share of throttled periods in the three seconds after a flush.

They replace N1a's first N2 gates on lading's fastest and slowest seconds, which lading's one-second pacing
confounded (*superseded*). From N1a's gates these still hold: in the cells that cannot keep up (heavy at 0.3, and
the `ultraheavy` tier at 0.3 and 0.6), a higher share of the rate than today's plan; at 0.05, memory inside the SMP
bounds; CPU per metric no worse than today's plan beyond the noise (2% from 0.3, 3% at heavy 1.2).

**N3 and N4: instructions per metric** in the quick loop (noise 0.3 to 0.6%, [how we iterate](#how-we-iterate)),
confirmed in sequential runs for CPU time and memory.

**Bursts across a flush: G1 to G5** (N1b's `run-regime.sh`). These guard against regressions in the overloaded burst
and say where a burst waits. Judge a change with N1b's steady rates (the `REGIME_STEADY_RATES` in each series'
`series.txt`) and burst, against N1b's plan image (or the last accepted change's):
`REGIME_LABELS="plan-a plan-b candidate-a candidate-b"`, `REGIME_CANDIDATE_IMAGE`, three rounds, one series per
cardinality (about 1 h 45 min each in the dory VM). Add `native-a native-b` when a gate compares with native. A ratio is
the medians' candidate / plan; it passes when it meets the threshold, and the candidate's own A/A distance must stay
within N1b's noise column, or the series is rerun. Each threshold is 1 plus or minus the largest A/A distance N1b
measured for that measure at that limit (over both images, both cardinalities, and for G1 both windows), rounded up to
the next 5%; where that distance exceeds 0.25 the cell has no ratio gate.

| Gate | Measure | 0.05 CPUs | 0.3 CPUs | 0.6 CPUs | 1.2 CPUs | Applies to |
|---|---|---|---|---|---|---|
| G1 | CPU ns/metric, steady and burst plus recovery, both cardinalities | at most 1.15 | at most 1.10 | at most 1.10 | at most 1.05 | N2, N3, N4 (no regression) |
| G2 | An improvement claimed by N4 (the same measure) | below 0.85 | below 0.90 | below 0.90 | below 0.95 | N4 |
| G3 | Overloaded throughput (burst decoded/s) | none (noise up to 0.35) | at least 0.90 | none (not overloaded) | at least 0.95 | N2, N3, N4 |
| G4 | Peak memory | at most 1.15 | at most 1.10 | at most 1.10 | at most 1.20 | N2, N3 (memory in flight during a drain), N4's allocator setting |
| G5 | Shortfall in the burst, 100,000 contexts | at most native's (0.56 in N1b) | at most native's (0.25 in N1b) | none | at most plan's + 0.02 | N2, N3 exit |
| G6 | Admission after / before the flush, 100,000 contexts | none (noise 1.0) | ~~at least 0.85~~ | none | ~~at least 0.90~~ | *superseded as an exit threshold* |

- **G6's thresholds are superseded.** They asked for 0.85 and 0.90, the noise floor below 1, which is what "admission
  never stalls on a flush" seemed to mean at this resolution. But no image reaches them: native reads 0.61 and 0.66 in
  the V7b series, because what G6 reads is mostly the encoder's burst under the quota, not the drain. G6 is also
  measured from the sender (the generators' bytes in the 2 s after the flush against the 2 s before, inside an
  overloaded burst, where lading always has bytes waiting, so its pacing matters less than for single seconds but not
  nothing). Keep reading it as a coarse container proxy beside the stall gates; set new thresholds, if any, once
  pacing or priority has a candidate.
- **N2 and N3** exit with G1, G3, and G4 holding, G5 met, and the stall gates improved. N1b sets today's values to
  beat: at 0.3 CPUs plan's admission ratio is 0.47 and its shortfall 0.31; at 1.2 CPUs 0.71 and 0.16. The p99 half of
  N2 and N3's exit stays in process.
- **N4** parts each pass G1 and G4, and claim a gain only by G2 or by instructions per metric. The allocator decision
  reads G1 and G4 at 0.3 and 1.2 CPUs. The datagram pool's worst case reads the pool and peak memory columns at 0.05 and
  0.3 CPUs with 100,000 contexts, where native grows its pool to 1,080 to 7,000 buffers in an 8 s burst and plan to
  128 to 418.
- **What no gate covers yet.** 0.6 CPUs under overload in N1b (recalibrate the burst against its measured capacity,
  288,000 and 377,000 metrics/s, first), overloaded throughput at 0.05 CPUs, and any change smaller than G1's
  tolerance: a 5% CPU saving at 0.3 CPUs, for instance, needs more rounds, the quick loop's instruction counts, or the
  in-process harness to show.
- **Naming.** G1 to G6 here are gates. The [ADP efficiency findings](adp-efficiency-findings.md) number their findings
  G1 to G9 too; this document calls those "ADP finding G9" and the like.

## The two N1 measurements

Two harnesses measured N1 independently, a week apart, and answer different questions. Use N1a's for steady
efficiency, capacity, instruction counts, and what happens inside ADP at a flush; use N1b's for a burst across a flush
and for judging a candidate image with A/A pairs. The [README](../../test/scaling/README.md#which-harness-to-use)
says which to run for what.

| | N1a: steady load against the SMP tiers | N1b: bursts across a flush |
|---|---|---|
| Harness | `test/scaling/run-limits.sh` with `compose.limits.yaml`, `summarize_limits.py` | `test/scaling/run-regime.sh` with the `regime` profile, `regime_sampler.py`, `summarize-regime.py` |
| When | 2026-10-13 and 14 (rerun after the health-check fix); flush instrumented 2026-10-15 | 2026-10-20 and 21; V7b candidate 2026-10-21; layout A/B 2026-10-22 |
| Host | 16-core Graviton2 (aarch64) Linux; ADP on four pinned cores | the dory VM (6 vCPUs on an M5 Pro, arm64, kernel 6.12); ADP on two cores |
| Native image | current `main` (`a52c25dd52`) | the merge base (`legacy-b940bf1f8b`) |
| Plan image | `eb1c4ee134` (before version 2; R1's pipelining, isolated aggregation) | `99a2209a0b` (version 2 through V7a, lanes off); candidates V7b `f35066de9e` and `cba809db95` |
| ADP's settings | production defaults: workers and decoders from the quota (two each) | two workers and two decoders, set explicitly (what the quota gives) |
| Load | the four SMP tiers, steady (`paced`) or as fast as possible (`saturate`) | 30% of native capacity for 30 s, then four times that (120% of capacity) for 8 s from 3 s before a flush, then 12 s recovery; 3,000 and 100,000 contexts |
| Samples | the host reads ADP's cgroup every second; `perf stat` and instruction profiles in the quick loop; the flow trace at 100 ms | a sampler container reads ADP's cgroup and the generators' bytes every 100 ms |
| Noise | A/A main against main, spread p25 to p75 | an A/A pair per image, the larger A/A distance from 1 |

**Where they agree.** The plan's CPU per metric is a little below native's (N1a 4 to 6% less from the medium tier up,
1 to 3% above at the low tier, about the noise; N1b 0.93 to 1.00 steady). The plan sustains more at saturation (N1a 5
to 9% from 0.3 up; N1b's overloaded throughput 1.05 to 1.08 of native at low cardinality). Under a crunch native grows
its datagram pool and the plan does not (N1a hundreds to 20,000 buffers against a few dozen; N1b 1,080 to 28,250
against 128 to 2,676), so the plan holds 32 to 45% (N1a, where it cannot keep up) and 5 to 46% (N1b, peak) less
memory. At 0.05 CPUs with 100,000 contexts or more, both images struggle: N1a found them collapsing erratically, N1b
found per-context cost dominating and ADP falling behind even at 830 metrics/s. And a flush disturbs admission on both
images.

**Where they differ, and why.** N1b found that at 0.3 CPUs with 100,000 contexts the plan's admission fell further
after a flush than native's (0.47 against 0.64) and it dropped more of the burst (shortfall 0.31 against 0.25), where
the stall study found the plan and `main` the same at a flush. The stall study ran steady tiers, and N1b an overloaded
burst: the difference is where a burst waits. Native keeps reading into its datagram pool while aggregation is busy;
the plan pushes back to the socket sooner, so in an overloaded burst its shortfall is higher and its memory lower. That
is G5's question, and ADP finding G7's from native's side. The images also differ (N1a's plan predates version 2,
N1b's has V2 to V7a), as do the hosts, so compare ratios, not absolute figures, across the two.

**lading's pacing, seen by both.** lading releases each second's bytes at the start of the second. Over UDP they leave
in one clump of under 100 ms (N1b, which is why both harnesses use a Unix datagram socket); over a Unix datagram
socket, where the sender blocks while the socket is full, they take as long as ADP needs to read them, about half a
second at N1a's loads. Either way, per-second rates measured at the sender mix lading's pacing with ADP's stalls.

### N1a: steady load against the SMP tiers

#### N1a method

`test/scaling/run-limits.sh` ([README](../../test/scaling/README.md#runs-at-production-cpu-limits-run-limitssh)) runs
ADP at 0.05, 0.3, 0.6, and 1.2 CPUs on four pinned cores, with production defaults (workers and decoders derived from
the quota: two of each at every limit, so lanes are off and R1's pipelining is on), against the steady loads of the
four SMP quality-gate tiers. Bursts are not modeled: a limit can absorb a burst up to the steady load it is shown to
sustain, which the saturate runs measure. Native is current `main` (`a52c25dd52`); the plan is `eb1c4ee134`. Three
rounds per cell, images alternating; an A/A series (main against main) at the low and heavy tiers sets the noise. The
host is a 16-core Graviton (aarch64) Linux machine, so absolute figures may differ on x86; the ratios are what the
gates use. p99 latency is not measured in containers; CI is not in scope yet.

#### N1a results

These are from the second run of the matrix (2026-10-14). The first (2026-10-13) charged every run about 20
millicores of the harness's own health check, which ran a process inside ADP's container every second; that took 40%
of the quota at 0.05 and distorted every absolute figure there (see the
[log](programmable-metrics-log.md#n1a-measuring-at-production-cpu-limits)). The ratios from 0.3 up barely moved, since
both images paid it alike.

The tiers: low is 1 MiB/s over 3,000 contexts, medium 10 MiB/s over 10,000, heavy 50 MiB/s over 100,000, and
ultraheavy 100 MiB/s over 250,000 (about 1,230, 11,900, 60,400, and 123,000 metrics a second). A Unix datagram socket
blocks the sender rather than drop, so "keeps up" means lading reached at least 97% of the tier's rate.

**Who keeps up, and plan against main.** Each cell: whether both keep up (or the share of the rate each reaches), then
plan's CPU per metric over main's, and plan's peak memory over main's (medians of three rounds).

| Tier | 0.05 | 0.3 | 0.6 | 1.2 |
|---|---|---|---|---|
| low | both; 0.99, 1.03 | both; 1.03, 1.00 | both; 1.03, 1.00 | both; 1.02, 1.03 |
| medium | neither (52%, 49%); 0.98, **0.65** | both; 0.96, 0.97 | both; 0.96, 0.91 | both; 0.95, 0.96 |
| heavy | neither (4%, 3%); collapse | neither (73%, 77%); 0.95, 0.89 | both; 0.94, 0.95 | both; 0.95, 0.93 |
| ultraheavy | neither; collapse | neither (30%, 32%); 0.95, 0.98 | neither (80%, 84%); 0.94, 0.97 | both; 0.95, 0.98 |

**Capacity** (saturate mode: the MiB/s lading got through, then the metrics a second ADP decoded):

| Contexts | 0.05 | 0.3 | 0.6 | 1.2 |
|---|---|---|---|---|
| 3,000: main | 5.9 (7,030) | 48.8 (60,300) | 99.0 (122,000) | 197 (243,000) |
| 3,000: plan | 6.5 (8,000) | 52.3 (64,500) | 107.5 (133,000) | 214 (265,000) |
| 100,000: main | 1.6 (1,830) | 36.4 (43,900) | 87.4 (106,000) | 189 (227,000) |
| 100,000: plan | 1.9 (2,280) | 38.4 (46,300) | 92.0 (111,000) | 199 (240,000) |

At saturation the plan decodes 5 to 9% more from 0.3 up, 14% more at 3,000 contexts and 24% more at 100,000 at 0.05,
with 5 to 8% less CPU per metric from 0.3 up (15 to 20% at 0.05), and 12 to 62% less memory (at 0.05 and 3,000
contexts, main holds 88 MiB against the low tier's 60 MiB bound; the plan 33).

**Noise** (A/A, main against main, spread from p25 to p75 over the median): CPU per metric within 0.7 to 1.7% at the
low tier and 1 to 3% at the heavy tier from 0.3 (A/A ratios 0.98 to 1.02), and 9 to 17% at heavy 0.05, a collapse
cell where only large effects count.

The data is in `test/scaling/baselines/n1-2026-10-14/` (per-run tables and summaries); the per-second series and logs
stayed on the measuring host. Rerun a cell with `SCALING_TIERS=<tier> SCALING_LIMITS=<limit> make limits-runs`.

#### What N1a says

1. **From medium load up, the plan is better everywhere:** 4 to 6% less CPU per metric from 0.3 up, beyond the A/A
   noise, 2 to 11% less peak memory, and 5 to 9% more capacity. At the low tier the plan is 1 to 3% above main in CPU
   per metric, about the size of the noise there.
2. **Idle and fixed costs are small.** An idle ADP costs 2.3 to 2.8 millicores on either image; the low tier costs
   about 15. (The first run's "fixed cost dominates" finding was the health check.)
3. **A flush disturbs admission even with headroom.** Where both keep up at heavy and ultraheavy, lading's fastest
   second reaches 1.2 to 1.37 times the rate on both images and its slowest falls to 0.56 to 0.83. *Corrected
   (2026-10-15):* lading sends each second's bytes in about half a second and then idles, so per-second rates mix its
   own pacing with ADP's stalls, and these figures do not measure the stall; it is measured at 100 ms in
   [what stalls admission at a flush](#what-stalls-admission-at-a-flush).
4. **Under a crunch, the plan holds memory and main does not.** Where ADP cannot keep up, main keeps reading datagrams
   into its buffer pool (hundreds to 20,000 buffers in use, peak memory up to 243 to 259 MiB at 0.05), while the plan's
   backpressure keeps the pool at a few dozen buffers and memory 32 to 45% lower, inside every SMP bound (main exceeds
   the medium tier's at 0.05).
5. **At 0.05 with 100,000 contexts or more, both images collapse, erratically.** Decoded rates vary several-fold from
   run to run on both (at ultraheavy 0.05, 54 to 700 metrics a second for main and 195 to 737 for the plan), sometimes
   with long stretches where nothing is decoded. An earlier reading of a plan-specific "freeze" was this, made worse by
   the health check: with the full quota the two images are the same within the noise there.

### N1b: bursts across a flush

The summaries are under `test/scaling/results/n1-20261021/`; the harness is `test/scaling/run-regime.sh`
([README](../../test/scaling/README.md#bursts-across-a-flush-run-regimesh),
[runbook](programmable-metrics-measurement.md#the-production-regime-in-containers-n1)).

#### N1b method

- **Images.** Native is the `main` image at the branch's merge base (`legacy-b940bf1f8b`, the native metrics path as
  production runs it). Plan is this branch at `99a2209a0b` (version 2 through V7a) running the default program, with
  `lanes.mode: off`.
- **The container.** ADP runs under `--cpus` 0.05, 0.3, 0.6, or 1.2 on two cores, with two workers and two DogStatsD
  decoders, which is what ADP picks under any quota up to two CPUs. The host is the dory VM (6 vCPUs on an M5 Pro,
  kernel 6.12, cgroup v2, CFS period 100 ms): lading and the blackhole run on cores of their own, and a sampler reads
  ADP's cgroup every 100 ms from outside it.
- **Traffic.** DogStatsD over a Unix datagram socket, the SMP case's metric shape (about 860 bytes a metric), at 3,000
  contexts (low cardinality) and 100,000 (high). Per limit, a 30 s steady window at 30% of the native image's saturated
  rate at that limit, then a burst of four times the steady rate (120% of native capacity) for 8 s, starting 3 s before
  an aggregation flush, then 12 s of recovery. Exactly one flush fell inside every burst. The flush interval is 15 s
  (`DEFAULT_AGGREGATE_FLUSH_INTERVAL`, and the `aggregate` operator's default); buckets are 10 s wide.
- **Repetitions.** Labels native-a, native-b, plan-a, and plan-b, three rounds each in a rotating order: 96 runs, six
  per image, limit, and cardinality, so each image has an A/A pair at every limit. The noise column below is the larger
  of the two images' A/A distances from 1.
- **Drops.** Over a Unix datagram socket the sender blocks while the socket is full, so drops are lading's
  **shortfall**: what it was configured to send and did not write, which is what a non-blocking client would have
  dropped. UDP was tried first, but lading sends each second's bytes in one clump at the start of the second, and over
  UDP that overflows the default 212,992-byte receive buffer at any rate, so kernel drops would measure lading.
- **Calibration.** Saturated rates (one run per image): at low cardinality 11,700, 92,300, 288,000, and 831,000
  metrics/s for native at 0.05, 0.3, 0.6, and 1.2 CPUs; at high cardinality 2,900, 46,900, 377,000, and 658,000. The 0.6
  calibration came after the series (the series interpolated 0.6 and underestimated it), so at 0.6 the burst was at or
  below capacity and is not an overload measurement.
- **The health check** ran every second only until ADP was healthy, from the harness's first run, so N1b did not carry
  the overhead N1a's first run did.

#### N1b results

Medians of six runs per image. **Noise** is the larger A/A distance from 1 of the two images, for the ratio beside it.

Steady window, at 30% of native capacity:

| Limit | Contexts | Metrics/s | CPU ns/metric, native / plan | Plan / native (noise) | CPU used, cores (plan) | Throttled periods, native / plan | Anonymous memory MiB, native / plan |
|---|---|---|---|---|---|---|---|
| 0.05 | 3,000 | 3,520 | 6,363 / 5,955 | 0.936 (0.017) | 0.021 | 0.56 / 0.52 | 28.8 / 24.0 |
| 0.3 | 3,000 | 27,700 | 3,716 / 3,628 | 0.976 (0.036) | 0.102 | 0.36 / 0.35 | 42.5 / 31.6 |
| 0.6 | 3,000 | 78,500 | 2,011 / 1,945 | 0.967 (0.032) | 0.153 | 0.27 / 0.26 | 56.0 / 35.1 |
| 1.2 | 3,000 | 250,000 | 1,591 / 1,492 | 0.938 (0.044) | 0.373 | 0.33 / 0.25 | 60.2 / 36.4 |
| 0.05 | 100,000 | 830 | 50,001 / 48,166 | 0.963 (0.055) | 0.040 | 0.89 / 0.85 | 65.9 / 67.7 |
| 0.3 | 100,000 | 14,200 | 11,530 / 10,844 | 0.941 (0.014) | 0.156 | 0.62 / 0.59 | 337 / 318 |
| 0.6 | 100,000 | 51,200 | 3,195 / 3,184 | 0.997 (0.083) | 0.164 | 0.32 / 0.35 | 318 / 294 |
| 1.2 | 100,000 | 196,000 | 2,062 / 1,914 | 0.928 (0.030) | 0.382 | 0.33 / 0.37 | 331 / 302 |

Burst across a flush (the burst window for throughput and drops; burst and recovery for memory):

| Limit | Contexts | Decoded/s, plan / native (noise) | Shortfall, native / plan | Admission after / before the flush, native / plan (noise) | Peak memory MiB, native / plan | Pool buffers at burst end, native / plan |
|---|---|---|---|---|---|---|
| 0.05 | 3,000 | 1.078 (0.144) | 0.21 / 0.22 | 0.65 / 0.83 (1.4) | 79 / 43 | 3,721 / 129 |
| 0.3 | 3,000 | 1.053 (0.061) | 0.20 / 0.15 | 1.04 / 1.02 (0.23) | 69 / 44 | 3,118 / 733 |
| 0.6 | 3,000 | 1.002 (0.021) | 0 / 0 (not overloaded) | 0.98 / 0.96 (0.23) | 80 / 52 | 4,290 / 1,290 |
| 1.2 | 3,000 | 1.081 (0.036) | 0.21 / 0.15 | 0.97 / 1.00 (0.03) | 90 / 64 | 4,582 / 1,563 |
| 0.05 | 100,000 | 0.882 (0.35) | 0.56 / 0.67 | 0.35 / 0.88 (1.0) | 113 / 107 | 1,080 / 128 |
| 0.3 | 100,000 | 0.977 (0.034) | **0.25 / 0.31** | **0.64 / 0.47** (0.15) | 467 / 396 | 6,995 / 418 |
| 0.6 | 100,000 | 0.996 (0.040) | 0 / 0 (not overloaded) | 0.79 / 0.74 (0.22) | 498 / 353 | 18,851 / 1,180 |
| 1.2 | 100,000 | 1.067 (0.011) | 0.21 / 0.16 | 0.66 / 0.71 (0.08) | 603 / 359 | 28,250 / 2,676 |

Throttled periods during an overloaded burst are 0.89 to 1.00 for both images at every limit. The full tables, with
throttled time, CPU per metric through the burst and recovery, the pool's growth, and each image's A/A ratio and spread
per measure, are each series' `summary.md`.

#### What N1b says

- **The plan path costs no more CPU per metric than native in the production regime.** Steady CPU per metric is 0.93
  to 1.00 of native at every limit and cardinality, and through the burst and recovery 0.91 to 0.98 except at 0.05 CPUs
  with high cardinality (1.10, inside that cell's noise of 0.10). Plan's overloaded throughput is 1.05 to 1.08 of native
  at low cardinality and 1.07 at 1.2 CPUs with high cardinality.
- **CPU per metric grows steeply as the limit falls, on both paths.** At low cardinality it is 1.5 µs at 1.2 CPUs, 2.0
  at 0.6, 3.7 at 0.3, and 6.4 at 0.05; at saturation 1.4, 3.2, and 4.2 µs at 1.2, 0.3, and 0.05. Under a quota the
  process runs in slices of the 100 ms period, and fixed per-second work weighs more as the budget shrinks.
- **High cardinality at 50 millicores is dominated by per-context cost.** At 830 metrics/s over 100,000 contexts, ADP
  uses 0.040 of its 0.05 CPUs, about 50 µs per metric, is throttled in 85 to 89% of periods, and already falls behind
  (shortfall 4 to 5% in the steady window). Calibration's 2,900 metrics/s held only while few contexts existed; the
  cost grows with live contexts. This is the first thing to profile for N4 (context resolution and interning, the idle
  counters' zeros, expiry), and it matches N1a's erratic collapse at 0.05 (ADP finding G8).
- **Throttling follows arrival, not average load.** At 30% of capacity and 3,000 contexts, 35 to 56% of periods are
  throttled at 0.3 and 0.05 CPUs, because lading delivers a second's metrics in one clump and ADP's two workers spend
  the period's quota in parallel. Production traffic from many clients is smoother, so these shares are an upper bound
  for the same average rate.
- **A flush slows admission at high cardinality, and the plan slows more.** At 3,000 contexts a flush costs too little
  to see. At 100,000, admission in the 2 s after the flush falls to 0.47 to 0.88 of the 2 s before for plan and 0.35
  to 0.79 for native. At 0.3 CPUs, the regime most clusters run, plan's admission falls further than native's (0.47
  against 0.64, noise 0.15) and it drops more of the burst (shortfall 0.31 against 0.25, noise 0.03). At 0.05 the
  shortfall is also worse for plan (0.67 against 0.56, noise 0.08). This was read at the time as the stall that the
  [aggregation before V7b](#what-it-means-for-what-we-built) paragraph predicts, the region host reading no input while
  a drain is pending. *Since corrected:* the stall study puts most of the fall in the encoder's burst, the same on
  both images, and the V7b runs showed admission falling as far with the drain no longer blocking; what separates the
  plan from native here is that it does not absorb the burst in the datagram pool as native does (below). Everywhere
  else plan drops the same or less than native.
- **Plan holds less memory, because the datagram pool barely grows.** Peak memory is 0.54 to 0.71 of native at low
  cardinality and 0.60 to 0.95 at high. In a burst native's `dsd_packet_bufs` pool grows to 1,080 to 28,000 buffers
  (8 KiB each, up to 220 MiB); plan's to 128 to 2,700. Native keeps reading into the pool while aggregation is busy,
  which is memory spent for fewer drops; plan pushes back to the socket sooner. N2's interleaved flush was expected to
  give plan native's drops at plan's memory; V7b did not (G5, [results](#v7b-candidate-results)).
- **Not measured.** Per-metric latency, p99 included: containers have no markers to time, so latency stays with the
  in-process paced curves, and the admission ratio around the flush is its container proxy. Also not measured: the
  Unix socket's queue depth, and admission inside ADP directly (the harness's `longest stall`, at 100 ms resolution,
  only says that stalls of 100 to 600 ms occur in an overloaded burst at 0.05 and 0.3 CPUs on both paths; N1a's flow
  trace measures admission inside ADP). UDP socket drops need a generator that paces within the second. 0.6 CPUs was
  never overloaded.
- **Noise.** Steady CPU per metric and overloaded throughput at 0.3 and 1.2 CPUs are within 2 to 6%. At 0.05 CPUs and
  for the shortfall at low cardinality, the A/A distance reaches 0.14 to 1.4, too noisy for a ratio gate: there the
  gates use absolute thresholds or none. The admission ratio's noise is 0.08 at 1.2 CPUs and 0.15 at 0.3.

## What stalls admission at a flush

On 2026-10-15 ADP was instrumented to see what happens at a flush (the flow trace, kept as a patch beside the results
in `test/scaling/baselines/stalls-2026-10-15/`, not in the tree), in `run-limits.sh` at the heavy tier at 0.3, 0.6,
and 1.2 cores and the `ultraheavy` tier at 1.2, `main` and the plan at `eb1c4ee134` (before V7b): every 100 ms, each
component's time blocked sending downstream and each region's input queue; each drain's duration, steps, and time
waiting on its sends; and, from the host, ADP's CPU time and throttling, lading's bytes, and whether datagrams waited
at the socket, lined up on one clock with wall-clock-aligned instruction profiles.

The aggregation flush interval is **15 seconds**, as the code says (`DEFAULT_AGGREGATE_FLUSH_INTERVAL` in
`lib/agent-data-plane-config/src/defaults.rs`, and the `aggregate` operator's default); several earlier notes had
assumed 10, likely from the 10-second bucket width. N1b's harness and the V7b design already used 15.

**What happens at each flush**, the same on the plan and on `main`:

| Phase | Duration | Share of the flush's instructions | Admission |
|---|---|---|---|
| **Drain**: aggregation emits the closed buckets (29,048 series at heavy, about 47,000 at ultraheavy) | 28 to 76 ms at 0.6 and 1.2 cores; 100 to 290 ms at 0.3 | about 3% | paused: the host reads no input while a drain is pending (before V7b) |
| **Encode and compress** the emitted series | the encoder is busy 2.3 s after a flush at 1.2 cores, 3.0 s at 0.6, and idle between flushes | **about 85%**: about 820 million instructions per flush at heavy, of which zstd (level 3) is about 360 million | competes with decoding for the CPU quota |
| Runtime, kernel, forwarder | the same window | about 12% | |

- **A flush costs about 33,000 instructions per emitted series** (about 960 million per flush at heavy), five to six
  times the cost of decoding an input metric, and all of it lands in the two to three seconds after the flush.
- **It is CPU starvation, not backpressure:** no component waits more than 0.3 ms a second to send, input queues stay
  at a few batches, and a drain's sends wait 0.1 to 0.2 ms in all. In the three seconds after a flush at heavy 1.2, ADP
  uses about 2.3 times its CPU of the three seconds before, and the share of throttled CFS periods rises from 36 to 44%
  to 54 to 61%.
- **The effect on admission:** at 0.6 cores admission halves for about two seconds after a flush, with datagrams waiting
  at the socket throughout; at 0.3, admission falls about 43% over the three seconds after.
- **The plan and `main` are the same:** the same series flushed and encoded per flush, the same instructions per flush,
  and the same throttling when both are anchored the same way.

**What it means for N2.** A non-blocking drain removes the drain's pause, 30 to 75 ms at 0.6 to 1.2 cores and 100 to
290 ms at 0.3, but not the encoder's burst, which is most of the stall. Hence the
[four layers](#n2-keep-admitting-through-a-flush-in-four-layers), and the [stall gates](#gates-for-n2-to-n4) that
replace those on lading's fastest and slowest seconds. V7b, built a week later without this result, delivered the first
layer, and its candidate runs read the encoder's burst ([below](#v7b-against-n2)).

**Measurement lessons.** lading sends each second's bytes in about half a second, so per-second rates and any window
compared with another mix its pacing with ADP's behavior; for a Unix datagram socket, `ss` reports the size of the
first datagram waiting, not a queue depth, and the socket's memory is not visible; throttled time in `cpu.stat` is
summed over CPUs; profiles of one flush, and flushes anchored on CPU peaks, both misled here (the plan's encoder first
looked 2.5 times as costly, and its throttling five times as high); four flushes per profile and the same anchor for
both images settled it.

## V7b against N2

V7b is N2's first layer and N3, judged in N1b's harness. Read against the stall study, its results say: the drain no
longer stalls admission, and what G6 still reads after a flush is the encoder's burst under the quota.

### V7b candidate results

Measured on 2026-10-21 by the [G1 to G6 method](#gates-for-n2-to-n4): the candidate image is this branch at
`f35066de9e` (V7b, lanes off by default and in `lanes-off.yaml`), against N1b's plan image, three rounds, N1b's steady
rates and burst. At 100,000 contexts the labels were `native-a native-b plan-a plan-b candidate-a candidate-b` (72
runs; the series was paused for 80 minutes after 20 runs and resumed on the same host), at 3,000 `plan-a plan-b
candidate-a candidate-b` (48 runs). The summaries are under `test/scaling/results/v7b-20261021/`. Ratios are candidate
/ plan medians; the flush columns are the state lane's `plan_lane_flush_seconds` and `plan_lane_flush_blocked_seconds`
across the burst.

100,000 contexts:

| Limit | G1 CPU/metric, steady and burst | G3 burst decoded/s | G4 peak memory | Shortfall, native / plan / candidate | Admission after / before, native / plan / candidate | Pool at burst end, native / plan / candidate | Flush s, blocked s |
|---|---|---|---|---|---|---|---|
| 0.05 | 0.952, 0.940 | 1.165 (no gate) | 0.983 | 0.60 / 0.67 / 0.67 | 3.4 / 0.62 / 0.90 (no gate) | 1,460 / 128 / 128 | 0.70, 0 |
| 0.3 | 0.991, 1.022 | 0.967 | 1.041 | 0.30 / 0.33 / **0.375** | 0.61 / 0.51 / **0.44** | 5,708 / 798 / 129 | 0.58, 0 |
| 0.6 | 1.012, 1.011 | 0.961 (no gate) | 1.017 | 0.03 / 0.04 / 0.075 | 0.89 / 0.81 / 0.60 (no gate) | 19,029 / 1,196 / 1,528 | 0.13, 0 |
| 1.2 | 1.000, 0.991 | 1.012 | 1.004 | 0.23 / 0.17 / 0.16 | 0.66 / 0.65 / **0.73** | 32,768 / 1,670 / 1,786 | 0.07, 0 |

3,000 contexts:

| Limit | G1 CPU/metric, steady and burst | G3 burst decoded/s | G4 peak memory | Shortfall, plan / candidate | Admission after / before, plan / candidate |
|---|---|---|---|---|---|
| 0.05 | 0.984, 1.058 | 1.028 (no gate) | 1.044 | 0.26 / 0.24 | 0.80 / 0.79 |
| 0.3 | **1.115**, 1.021 | 1.003 | 0.990 | 0.13 / 0.13 | 0.90 / 0.93 |
| 0.6 | **1.144**, **1.104** | 0.960 (no gate) | 1.014 | 0 / 0 | 1.01 / 1.03 |
| 1.2 | 0.986, 1.009 | 0.983 | 0.996 | 0.14 / 0.15 | 0.99 / 1.00 |

**Verdict: V7b does not meet N2's exit as G1 to G6 stated it.**

| Gate | Result |
|---|---|
| G1 | Holds at 100,000 contexts. Misses at 3,000: 1.115 steady at 0.3 CPUs and 1.144 steady, 1.104 burst at 0.6 (at most 1.10); holds at 0.05 and 1.2. *The [layout A/B](#layout-ab-at-3k-contexts) later showed this miss to be noise on this host, or a cost the driver fix removed.* |
| G3 | Holds: 0.967 and 1.003 at 0.3 CPUs, 1.012 and 0.983 at 1.2. |
| G4 | Holds everywhere: at most 1.044. |
| G5 | Misses at 0.05 CPUs (0.67 against native's 0.60, 0.56 in N1b; equal to plan's) and at 0.3 (0.375 against native's 0.30, 0.25 in N1b). Holds at 1.2 (0.16, plan's + 0.02 is 0.19). |
| G6 | Misses at 0.3 CPUs (0.44, at least 0.85) and at 1.2 (0.73, at least 0.90; plan's is 0.65, native's 0.66). *G6's thresholds are now superseded: no image reaches them, for the reason the stall study found.* |

The owner's allowance (V7b done if G6 passes and G5's shortfall at 0.3 CPUs is at most 0.31) does not apply: G6
misses, and the shortfall is above 0.31. The candidate's A/A distance exceeds N1b's noise in some cells (steady CPU per
metric at 0.6 CPUs, 1.09 at both cardinalities; admission at 0.3 CPUs with 100,000 contexts, 0.77), and so does plan's
in this series (its steady CPU spread at 0.3 CPUs with 3,000 contexts, 0.12), so by the method these cells are to be
rerun. A rerun cannot pass G5 or G6 at 0.3 CPUs: every candidate run misses them (admission 0.31 to 0.48, shortfall
0.33 to 0.42). It should confirm G1 at 3,000 contexts: at 0.6 CPUs every candidate run (1,973 to 2,288 ns) is above
plan's median (1,881), while at 0.3 plan's own runs span 2,957 to 3,870.

What the candidate runs say:

- **The flush no longer stalls admission.** `plan_lane_flush_blocked_seconds` is 0 across the burst in every candidate
  run whose drains completed, and passes interleave, flushing up to 4,600 contexts on touch at 0.3 and 0.6 CPUs. That
  telemetry counts a batch held by the room check, so passes did not fall back to a blocking flush. At 0.05 CPUs the
  admission ratio rose from 0.62 to 0.90 and the longest stall fell from 750 to 600 ms. At 1.2 the ratio rose from 0.65
  to 0.73, and admission recovers within about 1 s of the flush instead of 1.5 s.
- **But admission still falls after a flush, on every image, at the full quota.** In the 500 ms bins of the sampler, all
  three images use their whole quota from the flush on, and read about half as much from the socket for 1 to 2 s. Native
  falls to 0.61 at 0.3 CPUs and 0.66 at 1.2. So what G6 reads is no longer a stall: it is CPU that the flush's
  consequences take from admission in that window. That is the drain, encoding its output (at 0.3 CPUs with 100,000
  contexts, encoding is about half of the CPU the resource groups account for), and contexts created anew in the next
  bucket. G6's 0.85 and 0.90 are above what any image reaches in this harness. The stall study, made on N1a's host
  before V7b, attributes about 85% of a flush's instructions to the encoder: this is N2's second and third layers'
  problem, not the first's.
- **The pace does not spread the flush.** Passes completed in 0.07 s at 1.2 CPUs and 0.58 s at 0.3, not over the 5 s of
  a third of the interval. The pace is a floor: while a pass is pending the region runs a step whenever no batch waits
  (`has_background_work`), and under a CPU quota an idle region is not spare CPU, because the decoders and encoders
  share the quota. So the drain and the encoding it causes land right after the flush, where G6 reads.
- **At 0.3 CPUs the candidate parks less of the burst.** Its datagram pool ends the burst at 129 buffers against plan's
  798 and native's 5,708, and its shortfall is 0.375 against 0.33 and 0.30, with burst throughput 0.97 of plan's. As
  [V7b section 4](programmable-metrics-v7b.md#4-the-datagram-pool-and-back-pressure) expected, G5 is where a burst
  waits: N4's datagram pool item, bounded by G4, and ADP finding G7 from native's side.
- **At 0.6 CPUs the candidate is worse than plan in the burst.** The 0.6 burst overloads both images slightly in this
  series (decoded 0.96 of plan's, shortfall 0.075 against 0.039, admission 0.60 against 0.81, a distance of 0.27 against
  N1b's noise of 0.22). No gate covers 0.6, but it is the same direction as 0.3.
- **CPU per metric rose at 3,000 contexts below 1.2 CPUs.** The resource groups put the increase in the DogStatsD
  decoders (about 13% more per metric at 0.3 and 0.6 CPUs) and the fused region (stateless work and aggregation
  together, about 28% more at 0.6), and none at 1.2. One region task behind the decoders changes how often they wake and
  hand off under throttling. That was a hypothesis; the [layout A/B](#layout-ab-at-3k-contexts) found no such cost.
- **A telemetry oddity, since fixed.** In 10 of 24 candidate runs at 100,000 contexts, `aggregate_flushes_total` read 6
  to 9 for a run where plan's read 5, with the same metrics flushed, and the lane's flush histogram recorded fewer
  completed drains than flushes (in one run none), so `plan_lane_flush_blocked_seconds` could not be read for those
  runs. The runs where it could be read miss G6 as badly, so this does not explain the miss. The cause was a gated tick
  released again by every header during its pass, which then flushed a second time; `cba809db95` fixed it.

**What it points to.** The stall that N2 targeted is gone; what remains is the flush's CPU at a full quota and where a
burst waits. In the four layers' terms: pacing (cap a pass at its pace rather than running steps whenever the region is
idle, so its drain and the encoding it feeds spread over the third of the interval as V7b intended), then priority for
admission (the encoder on threads of lower weight, or a per-period budget for output work); N4's encode cost per metric
shrinks the burst either way, and the datagram pool's growth is G5's question, bounded by G4. G6's thresholds should be
revisited: native itself reaches only 0.61 and 0.66. All of these were deferred by the owner's decision of 2026-10-21.

### Layout A/B at 3k contexts

G1's miss at 3,000 contexts asked whether fusing aggregation into one region costs CPU. Measured on 2026-10-22: one
image of this branch at `cba809db95` (V7b with the driver fix for the flush count), run fused (`lanes-off.yaml`) and
with state isolated (`isolated.yaml`, `experimental.programmable_metrics.isolate_state: true`: `dsd_pre_aggregation`,
`dsd_agg`, and `metrics_enrich` as three regions, flushes still interleaved), beside N1b's plan image, in the regime
harness at 0.3 and 0.6 CPUs with N1b's steady rates and burst, three rounds of `plan-a plan-b fused-a fused-b
isolated-a isolated-b` (36 runs, `REGIME_VARIANTS`; summary in `test/scaling/results/layout-ab-20261021/`).

| Limit | Steady CPU ns/metric, plan / fused / isolated | Fused / plan | Isolated / fused | A/A, plan / fused / isolated | Spread, plan / fused / isolated | Burst and recovery, fused / plan |
|---|---|---|---|---|---|---|
| 0.3 | 3,175 / 3,279 / 3,506 | 1.033 | 1.069 | 1.049 / 0.905 / 1.200 | 0.19 / 0.18 / 0.14 | 0.999 |
| 0.6 | 2,027 / 2,015 / 1,923 | 0.994 | 0.955 | 0.976 / 0.954 / 1.046 | 0.14 / 0.08 / 0.08 | 0.996 |

- **The fused layout costs no more than the isolated one.** Every ratio is inside the A/A distances, and fused is the
  cheaper layout at 0.3 CPUs. The single runs span 2,470 to 3,950 ns at 0.3 CPUs and 1,740 to 2,340 at 0.6 for every
  image. Burst throughput, shortfall, peak memory, and the flush count (two per steady window and one per burst
  everywhere) match too.
- **The region's own work is the same.** In process, the region driver bench's `layout_steady_admission` group (the
  whole metrics path, 3,000 contexts, release, on the Mac) costs 277 ns a metric fused and 275 isolated in batches of
  1,024 (confidence intervals 270 to 290 and 273 to 279), 276 against 280 in batches of 128, and 326 against 343 in
  batches of 16, where fused saves a hop. Interleaving on or off changes nothing outside the noise (276 and 277
  isolated at 1,024). In the containers, the resource groups give the regions (`dsd_pre_aggregation`, `dsd_agg`, and
  `metrics_enrich` together) 710, 740, and 675 ns a metric fused, isolated, and plan at 0.3 CPUs, and 412, 394, and
  412 at 0.6. Batches enter the regions at 830 events at 0.3 CPUs and 950 at 0.6 in every layout, and the runtime
  polls the decoders and regions as often.
- **The DogStatsD decoders are where runs differ.** They take about three quarters of the CPU (2,560 to 2,780 ns a
  metric at 0.3 CPUs, 1,620 to 1,680 at 0.6) and vary from run to run by more than the layouts differ, in code that is
  the same in all three. N1b's plan image measured 1,881 ns a metric at 0.6 CPUs in the V7b series and 2,027 in this
  one.

**What it means.** The 1.115 and 1.144 that G1 read at 3,000 contexts were noise on this host, or a cost that the
driver fix removed, not a cost of fusion: the layout needs no change. This agrees with N1a's H2, where maximal fusion
saved no instructions. With six runs per image, steady CPU per metric at 3,000 contexts on 0.3 and 0.6 CPUs resolves
about 10% at best. Before gating a change on G1 there, run more rounds, count instructions in the quick loop, or
measure CPU per metric in process. This series took no profile: nothing points at code in one layout and not the
other.

## Iterating on efficiency

The quick loop and the plan-specific hypotheses, from N1a's follow-up. The ADP-wide findings it produced are in
[ADP efficiency findings](adp-efficiency-findings.md).

### How we iterate

Each hypothesis is a change with a predicted effect, tested in a loop short enough to run several an hour, and checked
against trusted measurements before it is kept. Three tiers, all in `run-limits.sh`:

| Tier | What | Time | Used for |
|---|---|---|---|
| Quick loop | a variant against the current plan on the focus cells: medium at 0.3, heavy at 0.6 and 1.2, ultraheavy at 1.2, and capacity at 0.3; 30-second windows; cells in parallel; instructions and memory counters per metric (`perf stat`) | 15 to 20 minutes with the build | every hypothesis |
| Milestone | the focus cells against `main`, one cell at a time, three rounds | 1 to 2 hours | a change worth keeping |
| Full matrix | N1a's 16 cells, A/A, and capacity | about 9 hours, overnight | before results are recorded |

The quick loop's shortcuts are kept only if, run against the N1a baseline as an A/A and as `main` against the plan, it
reproduces the full matrix's ratios within the A/A noise. On the first N1a data, 30-second windows reproduced the
55-second ratios within 1 to 2% at every cell from 0.3; 10-second windows, shorter than one 15-second flush interval,
are too noisy. Instruction counts are close to deterministic and unaffected by throttling or neighbors, so they decide
hypotheses about CPU per metric in one round; stalls, capacity, and memory still need the wall-clock measures. For a
burst across a flush, or a candidate judged with A/A pairs, use N1b's harness and [G1 to G5](#gates-for-n2-to-n4).

### Hypotheses

Results so far are in [what the first iterations showed](#what-the-first-iterations-showed): H1 dropped, H2 neutral,
H8 and H9 moved to the [ADP findings](adp-efficiency-findings.md).

Fundamentals before cleverness: memory layout and fixed costs first, new machinery later. Every hypothesis changes how
an installed generation runs, never what the program means: the IR, generations, artifacts, the reference interpreter,
and the equivalence oracle stay as they are, and each change must pass them.

**First, measure where the instructions go.** For the focus cells: instructions, cycles, L1D and L2 refills, last-level
misses, TLB walks, and backend stalls per decoded metric, for `main` and the plan, and an instruction profile of each.
`main` runs the work the two share (decoding, context resolution, encoding), so what the plan spends beyond it is the
cost of programmability. Low IPC with many misses in the plan's operators and the aggregation kernel points to layout
(H3 to H6); high IPC with too many instructions points to fixed costs and hops (H1, H2). Also a `size_of` audit of
`Event`, `Metric`, `ContextInner`, and `TagSet`.

**The layout today.** A batch is a `Vec<Event>`, each slot the size of the largest `Event` variant. A `Metric` holds a
`Context` (an `Arc<ContextInner>`), its points inline, and metadata. `ContextInner` holds the name (a `MetaString`),
the host, and two `TagSet`s, each a small vector of `Arc<FrozenTagSet>` whose tags are `MetaString`s. A predicate on a
name walks event, `Arc`, string; one on tags walks event, `Arc`, tag set, `Arc`, and each tag's bytes; and each
operator walks again for each metric. With 100,000 contexts, most of those are cache misses.

**The query view.** The program is a query plan, a batch a vector, an operator a query operator. Databases made
interpreted plans fast by processing compact vectors instead of rows (MonetDB/X100, DuckDB, Velox), with physical
techniques that leave the logical plan alone. In order:

| # | Hypothesis | Prediction | Cost |
|---|---|---|---|
| H1 | **CPU-time sampling.** Without lanes, the default at every production limit, the plan reads the thread's CPU time twice per task poll and kernel call, a system call on Linux; `SALUKI_CPU_TIME_SAMPLING=16` samples one in 16. | fewer instructions per metric, most at small batches | an environment variable |
| H2 | **Fuse maximally (N3).** Delete R1's cost model: one region between fences, no extra hops, queues, or wakeups (today the post-aggregation filter gets its own region at 66 ns against a 64 ns budget). | fewer instructions and wakeups per metric | planned code change (since built by V7b) |
| H3 | **Dense context IDs (dictionary encoding).** The resolver gives each context a dense slot index, tagged with a generation so a reused slot is never mistaken for an old one; the resolver owns them, so they survive generations and cuts. | the foundation for H4 and H5 | moderate |
| H4 | **Predicate and decision tables.** A predicate's or decision's result for a context is fixed within a generation: compute it once, store it in an array indexed by the dense ID, owned by the generation and built lazily, so a new generation starts a new table. A predicate becomes one indexed load. | fewer misses and instructions in filters, mappers, and routing | moderate |
| H5 | **Aggregation state indexed by dense ID.** Arrays instead of a hash map keyed by context: admission is a bounds check and a load, and a flush a linear scan the prefetcher follows. | lower admission cost, and a flush well under today's 213 ns per context | larger |
| H6 | **Selection vectors and late materialization.** A filter yields the positions that pass and routing a list per exit; events stay in place until an exit needs a buffer, so fan-out stops copying and cloning. Then a few columns extracted once per batch at region entry (dense ID, kind, value, timestamp), with `Vec<Event>` kept as the row store at the boundaries. | fewer copies, clones, and misses | moderate |
| H7 | **Fixed cost per batch.** Generation and entry lookups cached per admission point, the header and progress loops skipped when no exit needs them, and telemetry counted per batch. | fewer instructions per metric, most at small batches | small |
| H8 | **Compact elements** in the metrics path: a buffer whose element is not sized by the largest `Event` variant. | more metrics per cache line | moderate |
| H9 | **ADP-wide costs**, for `main` too: the tracking allocator's wrapper, jemalloc's thread cache (N4), encoding and compression, internal telemetry. | varies | varies |

Later, if the profiles call for it: columnar decoding in the source, fused native kernels for sequences the default
program always contains, generated code. The risks to design for: a stale dense ID must never reach a reused slot (the
interpreter and the transition traces check it); extracting columns costs something per batch and must pay at the
batch sizes seen, small timer-flushed batches included; and aggregation state carried across cuts holds dense IDs, so
their stability across generations is part of H3's contract.

### What the first iterations showed

On 2026-10-14, after the N1a baseline, the quick loop was validated against it, instruction profiles were taken, and
H1, H2, and H9 were tested. The findings that do not depend on the plan, and could land in ADP on their own, are
collected in [ADP efficiency findings](adp-efficiency-findings.md); this section keeps what bears on the plan.

**The quick loop measures instructions, not CPU time.** Run as an A/A at the focus cells, instructions per metric
agreed within 0.3% with a spread of at most 0.6%, about four times tighter than CPU time. But run as `main` against the
plan, its CPU per metric missed the sequential baseline by up to 4% at two cells, twice the A/A noise, and its memory
by up to 9% even in the A/A; the parallel slots or the two-core CPU sets are the likely cause, not yet separated. So
the quick loop decides hypotheses about instruction count, and CPU time, stalls, and memory are judged in sequential
runs.

**Where the instructions go** (instruction-sampled profiles at medium 0.3, heavy 1.2, and ultraheavy 1.2; shares of
ADP's instructions, similar on both images):

| Component | Share | Whose |
|---|---|---|
| Hashing, mostly context keys in the DogStatsD source | 22 to 29% | shared |
| DogStatsD decoding and newline framing | 25 to 30% | shared |
| Kernel: socket receives, epoll | 17 to 31% | shared |
| Allocator | 3 to 6% | shared |
| Tokio runtime | 8 to 9% on `main`, about 1% on the plan | the plan's fusion saves it |
| The plan's runtime: regions, kernels, decisions | 5 to 6% | plan |
| Aggregation | 2 to 3% | both |

So more than 85% of the instructions are in code the two share, and the plan's own runtime costs less than the Tokio
overhead its fusion removes: by instruction count, programmability is already free on this workload. These profiles
run the default configuration, which has almost no policy operators; a policy-heavy profile is needed before the
plan's operators can be judged (below). N1b's containers agree on the shape: the DogStatsD decoders take about three
quarters of the CPU ([layout A/B](#layout-ab-at-3k-contexts)).

**Hypothesis results** (instructions per metric against the unmodified plan; noise 0.3 to 0.6%):

| | medium 0.3 | heavy 0.6 | heavy 1.2 | ultraheavy 1.2 | low 0.3 | medium 1.2 | heavy 0.3 | ultraheavy 0.6 |
|---|---|---|---|---|---|---|---|---|
| H1: CPU-time sampling one in 16 | -0.5% | +0.4% | -0.2% | -0.3% | | | | |
| H2: maximal fusion without lanes (`SALUKI_EXPERIMENT_FUSE_ALL=1`, a switch in `7c7631a4bb`, since removed) | 0.0% | +1.2% | -0.2% | -0.4% | +0.3% | 0.0% | +1.2% | +1.2% |
| H9: jemalloc's thread cache (`MALLOC_CONF=tcache:true`) | **-7.5%** | **-3.9%** | **-4.2%** | **-3.1%** | | | | |
| H1, H2, and H9 together | -7.7% | -3.8% | -4.4% | -4.2% | | | | |

- **H1 is dropped:** sampling CPU time saves nothing measurable.
- **H2 saves no instructions**, and costs about 1% where ADP cannot keep up. Deleting R1's cost model (N3) is a
  simplification, not a speed-up. The existing one-lane-per-slot layout, which also fuses maximally, was 4 to 9% worse
  at two workers because of the lane runtime, not fusion. V7b has since built maximal fusion, and N1b's
  [layout A/B](#layout-ab-at-3k-contexts) found it costs no CPU against an isolated layout either.
- **H9 is a real win but ADP-wide**, not plan work: see the ADP findings, G2.

**The plan-specific landscape**, by estimated impact, confidence, and complexity:

| Item | Impact | Confidence | Complexity |
|---|---|---|---|
| **N2, non-blocking flush** | **high for behavior**, the team's stated top problem; little effect on instructions. Revised: the drain is only part of the stall, see [what stalls admission at a flush](#what-stalls-admission-at-a-flush); built by V7b as layer 1 | high that the problem is real | high (design) |
| H7, fixed cost per batch | small; may explain the plan's 1 to 3% at the low tier | medium | low |
| H3 and H4, dense context IDs and predicate tables | unmeasurable on the default profile, which runs almost no policy | low until a policy-heavy profile is measured | medium |
| H5, aggregation state by dense ID | low for instructions (aggregation is 2 to 3%); medium for flush cost, which feeds N2 | medium | high |
| N3, delete R1's cost model | none for instructions; a simplification; built by V7b | high | medium |
| H8, compact metric buffers | superseded by boxing `Event`'s large variants, ADP-wide (ADP finding G5) | | |

**Direction.** Efficiency per metric is now mostly an ADP-wide matter, and those items go to the separate document.
The plan work focuses on what the plan can improve structurally and native ADP cannot: **behavior under load, starting
with flush stalls (N2)**, built on the generations, cuts, and yielding kernels already in place. Before H3 and H4, the
`mapper` and `gateways` profiles should be profiled, to see what programmability actually costs when policy runs.

## Lower priority

- The stage 9 Linux gate host, lane counts above two, and the per-buffer CPU cost of lanes (risk 31 of the
  [stages 7 and 9 design](programmable-metrics-stages-7-and-9.md#11-risks)).
- Partitioned aggregation. If it is ever needed, the generation and cut machinery can change the partitioning live, as
  a cut, when sustained load justifies it, rather than fixing it at boot.
- Stage 8 (Remote Config), as before.

## Open questions

- Whether production traffic resembles the SMP tiers and N1b's bursts: traces from Nodeless and from the clusters at
  1.2 cores would check N1's workloads and burst shapes.
- What counts as acceptable during a crunch: which drops, how much latency, how much memory.
- Whether SMP experiments can run at 0.3 cores, so that N1's gates also run in CI (deferred: local runs for now).
- Whether the parallel quick loop's CPU-time skew comes from running slots side by side or from two-core CPU sets.
- Which policy production configurations use (mappers, filters, routing), to choose the policy-heavy workload that H3
  and H4 need.
- How much of a turn should go to draining against admission, and whether it should adapt to the CPU quota. V7b's
  answer: a pace of a third of the flush interval, at least a step every 100 ms, which never reads the quota. The
  candidate runs say it does not spread the flush: the pace is a floor, and passes run ahead whenever the region is
  idle, completing in 0.07 to 0.6 s ([results](#v7b-candidate-results)). N2's pacing layer is the open answer.
- Where the encoder should run once N2's third layer is designed: on threads of lower weight (which V8's encoding as
  a terminal kernel would rule out) or under a per-period budget for output work inside the region.
- What thresholds, if any, should replace G6's, and whether the stall gates need telemetry in ADP rather than the
  flow-trace patch.
