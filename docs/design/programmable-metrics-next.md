# Programmable metric processing: deployment context and next steps

What we learned about how ADP is deployed after stages 1 to 7 and 9 were built, what that means for the work, and what
the next chunk of work should look like. Read the [index](programmable-metrics-index.md) first for the overall picture.

## Contents

- [Deployment context](#deployment-context)
- [Direction: programs replace native policy](#direction-programs-replace-native-policy)
- [What it means for what we built](#what-it-means-for-what-we-built)
- [Lessons](#lessons)
- [The next chunk of work](#the-next-chunk-of-work)
- [Lower priority](#lower-priority)
- [Open questions](#open-questions)

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

## What it means for what we built

| Work | Relevance at these limits |
|---|---|
| Stages 1 to 6: one plan, fused regions | **The relevant win.** Under a CPU quota, fewer tasks and hops are pure gain. At one worker, stage 3 measured 1.27 times native throughput at 0.77 of native worker time per metric ([implementation risks](programmable-metrics-implementation.md#risks)). |
| Stage 7: live outputs and recipients | Neutral on CPU: per metric within 1.01 of native, allocations 0.56 to 0.89, p99 0.27 to 0.65 ([stage 7 gate](programmable-metrics-stages-7-and-9.md#83-stage-7-gate)). Its value is operational. |
| Stage 9: lanes | **Little production value, and correctly off at these sizes.** ADP's worker count is the process's available parallelism, with a minimum of two (`saluki-app` `util.rs`), and on Linux that follows the cgroup's CPU quota, so every limit up to two cores gives two workers; `lanes.mode: auto` then runs the layout without lanes ([lanes as built](programmable-metrics-stages-7-and-9.md#stage-9-as-built)). Its static key sharding is the fixed-overhead scheme the team wants to avoid for aggregation. |
| R1's fusion cost model | **Wrong for these deployments.** It splits a chain into pipelined regions when one region would exceed the cost of the costliest native component, to buy pipeline parallelism across tasks ([pipelining](programmable-metrics-implementation.md#physical-frame-and-region-programs)). At one core or less that buys nothing and adds a hop and a queue per split. |
| Our measurements | **The wrong regime.** They were saturated throughput at 1 to 16 unconstrained workers. None ran under a CPU quota, with CFS throttling, with bursts during a CPU crunch, or at 50 millicores ([measurement runbook](programmable-metrics-measurement.md)). |
| The stage 9 gate host ([Q2](programmable-metrics-stages-7-and-9.md#12-open-questions)) | No longer a priority: what it would decide does not apply to production. |

**Aggregation today.** Stage 6 made aggregation a kernel whose flush runs in budgeted steps
([drains](programmable-metrics-implementation.md#drains)): it yields every 4,096 contexts or 1,024 emitted metrics, and
the host answers liveness and control between steps. But the host **reads no input while a drain is pending**, so
admission stops for the whole flush: still serial and blocking. A flush costs about 213 ns per context, about 0.2 s of
CPU at 1M contexts, which at 50 millicores is several seconds without admission, while the socket fills and the
datagram pool grows (up to about 256 MiB at its default ceiling,
[decisions](programmable-metrics-status.md#decisions-awaiting-the-owner)).

## Lessons

- **Establish the deployment's axes before designing for one.** Stage 9 followed the spec's plan to restore parallelism
  that fusion had removed. That plan was sound, but nobody checked whether production had cores to parallelize over.
  The first question for the next chunk is what the production regime looks like, measured.
- **Measure in the regime that matters.** A ratio at 8 unconstrained workers says little about 0.3 cores under
  throttling. Gates should be stated at the production limits.
- **The substrate is still the right one.** Generations, cuts, yielding kernels, the delivery-policy slot, and the
  reference interpreter are what the next items build on; none needs a model change.

## The next chunk of work

In order. Each item has its goal, the approach, and the evidence that ends it.

### N1: measure in the production regime

- **Goal:** a baseline of plan against native at production limits, so every later item has a gate.
- **Approach:** the container harness already takes `--cpus` ([test/scaling](../../test/scaling/README.md),
  [runbook](programmable-metrics-measurement.md)). Run at 0.05, 0.3, 0.6, and 1.2 CPUs with lanes off, steady and
  bursty traffic (a burst several times the steady rate, at low and high cardinality, timed across a flush). Measure
  CPU per metric, throttled periods and time (`cpu.stat`), socket and pool drops, the datagram pool's depth, p99
  latency, peak and anonymous memory, and admission stalls during a flush. Add an A/A series per limit, as the gate's
  method requires. Linux is required; the dory VM is enough for these limits.
- **Exit:** a results table in this document, and the gate thresholds for N2 to N4 stated at these limits.

### N2 and N3: one region from decode to encode, with an interleaved flush

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
     isolation slot to any slot; fuse maximally between I/O fences whatever the costs, which deletes R1's cost model
     (plan [P9.7](programmable-metrics-stages-7-and-9.md#p97-r1s-cost-model) lists what it touches). Make lanes opt-in
     (`lanes.mode: off` by default, amendment 157 reversed) while keeping the code; with lanes, aggregation can still
     run in a slot of its own.
  2. **An interleaved flush**, which one task makes required rather than desirable: today a drain stalls only
     aggregation's task while upstream keeps decoding into its queue; in one task it would stall everything. A flush
     drains only closed buckets and admission writes only open ones (buckets close by watermark, and admission time is
     at or past it), so drain steps can interleave with admission under a policy for how much of each turn goes to
     each. What needs design: the kernel contract (`consume` while a `process` or `advance_time` is yielded), the open
     and closed state kept apart so a context touched by both stays correct (a counter admitted during its own
     bucket's flush, idle-counter zeros, context expiry), memory in flight during a long drain, and the reference
     interpreter's and the transition traces' view of interleaving. The stage 6
     [time-driven output](programmable-metrics-implementation.md#time-driven-output) and drains are the starting
     point.
  3. **Cuts and lineages keyed by node, not by slot.** The cut protocol arms over admission points, unchanged without
     the slot boundary, but re-verified: the transition traces and the reference interpreter.
  4. Later, if N1 says it pays: decoding inside the region, and encoding as a terminal kernel, leaving only sockets
     and delivery as tasks.
- **Payoff:** two channel hops, two times a task is woken per batch, and their queues gone, which are both fixed overhead and a
  source of worst-case latency at 0.3 cores.
- **Exit:** semantics unchanged (the interpreter, the compatibility matrix, transition traces 1 to 15); admission never
  stalls on a flush; and at 0.05 and 0.3 CPUs, against native and against today's layout, fewer drops and lower p99
  during a flush, with CPU per metric no worse.

### N4: efficiency per metric

Each is a small part, measured by N1 at the production limits:

- **Passthrough batching.** The plan sends one buffer per input batch where the native batcher filled 1,024 timestamped
  metrics ([implementation risks](programmable-metrics-implementation.md#risks)); the encoder's cost of more, smaller
  buffers is unmeasured.
- **Encode and compression cost** per metric (M3 in the [runbook](programmable-metrics-measurement.md)).
- **The allocator setting.** The thread cache saved 4 to 7% of CPU per point at two CPUs for about 8 MiB more peak
  memory ([decisions](programmable-metrics-status.md#decisions-awaiting-the-owner)); decide it at 0.3 and 1.2 CPUs.
- **The datagram pool's worst case** when CPU-starved: its ceiling and growth under a burst.

### N5: shedding during a CPU crunch

- **Goal:** degrade deliberately rather than by dropping at the socket.
- **Approach:** a design spike on the delivery-policy slot plus signals. Signals and reactive operators are the
  specification's stage 10, and the delivery-policy slot is already open to artifacts in a minimal form
  ([stage 7](programmable-metrics-stages-7-and-9.md#413-the-delivery-policy-slot)). With them, a policy could act on
  queue or pool depth: drop low-value metrics, widen aggregation, or sample. The programmable substrate is uniquely
  suited to this.
- **Exit:** a design reviewed against N1's burst measurements.

## Lower priority

- The stage 9 Linux gate host, lane counts above two, and the per-buffer CPU cost of lanes (risk 31 of the
  [stages 7 and 9 design](programmable-metrics-stages-7-and-9.md#11-risks)).
- Partitioned aggregation. If it is ever needed, the generation and cut machinery can change the partitioning live, as
  a cut, when sustained load justifies it, rather than fixing it at boot.
- Stage 8 (Remote Config), as before.

## Open questions

- Which workloads and burst shapes to model in N1: ideally traces from Nodeless and from the clusters at 1.2 cores.
- What counts as acceptable during a crunch: which drops, how much latency, how much memory.
- Whether SMP experiments can run at 0.3 cores, so that N1's gates also run in CI.
- How much of N2's turn should go to draining against admission, and whether it should adapt to the CPU quota.
