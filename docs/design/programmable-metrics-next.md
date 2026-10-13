# Programmable metric processing: deployment context and next steps

What we learned about how ADP is deployed after stages 1 to 7 and 9 were built, what that means for the work, and what
the next chunk of work should look like. Read the [index](programmable-metrics-index.md) first for the overall picture.

## Contents

- [Deployment context](#deployment-context)
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

### N2: make the aggregation flush non-blocking

- **Goal:** admission continues during a flush, with no sharding and no fixed overhead at low load.
- **Approach:** a flush drains only closed buckets, and admission writes only open ones: buckets close by watermark,
  and admission time is at or past the watermark. So the host can keep reading input while a drain is pending, and
  interleave admission with drain steps, under a policy for how much of each turn goes to each. What needs design: the
  kernel contract (`consume` while a `process` or `advance_time` is yielded), the open and closed state kept apart so a
  context touched by both stays correct (a counter admitted during its own bucket's flush, idle-counter zeros, context
  expiry), memory in flight during a long drain, and the reference interpreter's and the transition traces' view of
  interleaving. The stage 6 [time-driven output](programmable-metrics-implementation.md#time-driven-output) and drains
  are the starting point.
- **Exit:** semantics unchanged (the interpreter, the compatibility matrix, transition traces 1 to 15); admission never
  stalls on a flush; and at 0.05 and 0.3 CPUs, fewer drops and lower p99 than native during a flush, with CPU per metric
  no worse.

### N3: settle the layout for small CPU budgets

- **Goal:** the simplest, cheapest layout at one core or less.
- **Approach:** fuse maximally between fences whatever the costs, which means deleting R1's cost model (plan
  [P9.7](programmable-metrics-stages-7-and-9.md#p97-r1s-cost-model) lists what it touches); make lanes opt-in
  (`lanes.mode: off` by default, amendment 157 reversed) while keeping the code.
- **Exit:** N1's measurements at the four limits show the fused layout at least as good as today's.

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
