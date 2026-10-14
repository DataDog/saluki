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
- **Approach, as run (2026-10-13):** `test/scaling/run-limits.sh` ([README](../../test/scaling/README.md#runs-at-production-cpu-limits))
  runs ADP at 0.05, 0.3, 0.6, and 1.2 CPUs on four pinned cores, with production defaults (workers and decoders derived
  from the quota: two of each at every limit, so lanes are off and R1's pipelining is on), against the steady loads of
  the four SMP quality-gate tiers. Bursts are not modeled: a limit can absorb a burst up to the steady load it is shown
  to sustain, which the saturate runs measure. Native is current `main` (`a52c25dd52`); the plan is `eb1c4ee134`. Three
  rounds per cell, images alternating; an A/A series (main against main) at the low and heavy tiers sets the noise.
  The host is a 16-core Graviton (aarch64) Linux machine, so absolute figures may differ on x86; the ratios are what
  the gates use. p99 latency is not measured in containers; CI is not in scope yet.
- **Exit:** a results table in this document, and the gate thresholds for N2 to N4 stated at these limits. **Met:** see
  below.

#### N1 results

The tiers: low is 1 MiB/s over 3,000 contexts, medium 10 MiB/s over 10,000, heavy 50 MiB/s over 100,000, and
ultraheavy 100 MiB/s over 250,000 (about 1,230, 11,900, 60,400, and 123,000 metrics a second). A Unix datagram socket
blocks the sender rather than drop, so "keeps up" means lading reached at least 97% of the tier's rate.

**Who keeps up, and plan against main.** Each cell: whether both keep up, then plan's CPU per metric over main's, and
plan's anonymous memory over main's (medians of three rounds).

| Tier | 0.05 | 0.3 | 0.6 | 1.2 |
|---|---|---|---|---|
| low | both; 1.00, 1.03 | both; 1.01, 1.03 | both; 1.00, 1.05 | both; 1.02, 1.02 |
| medium | neither; **0.84, 0.48** (main 164 MiB against a bound of 100, plan 79) | both; 0.97, 0.91 | both; 0.97, 0.97 | both; 0.97, 0.95 |
| heavy | neither; collapse (below) | neither (69%, 70% of the rate); 0.98, 0.91 | both; 0.96, 0.85 | both; 0.96, 0.94 |
| ultraheavy | neither; collapse (below) | neither (28%); 0.98, 0.97 | neither (77%, 81%); 0.95, 0.97 | both; 0.96, 0.97 |

**Capacity** (saturate mode: the MiB/s lading got through, then the metrics a second ADP decoded; at 0.05 main's MiB/s
includes datagrams it buffered without decoding, so the decoded rate is the measure there):

| Contexts | 0.05 | 0.3 | 0.6 | 1.2 |
|---|---|---|---|---|
| 3,000: main | 5.2 (6,600) | 45 (55,600) | 97 (120,000) | 198 (245,000) |
| 3,000: plan | 5.5 (6,900) | 49 (60,600) | 104 (128,000) | 216 (267,000) |
| 100,000: main | 1.6 (1,300) | 34 (40,800) | 82 (99,100) | 181 (219,000) |
| 100,000: plan | 1.1 (1,240) | 35 (42,200) | 88 (106,000) | 193 (233,000) |

At saturation the plan decodes 5 to 9% more at 3,000 contexts, and 3.5 to 7% more at 100,000 contexts from 0.3 (at
0.05, 5% less, within that cell's noise), at 3 to 10% less CPU per metric, with 24 to 30% less anonymous memory at
3,000 contexts (main exceeds the low tier's 60 MiB bound at
0.05, with 105 MiB; the plan holds 39).

**Noise** (A/A, spread from p25 to p75 over the median): CPU per metric within 1% at the low tier from 0.3 (2 to 4% at
0.05), 0.3 to 2% at the heavy tier at 0.3 and 0.6, about 3% at heavy 1.2 (an A/A ratio of 1.04), and 19 to 25% in the collapse cells
(heavy at 0.05), where only large effects count.

**Findings.**

1. **From 0.3 to 1.2 the plan is a little better everywhere:** 2 to 5% less CPU per metric above the low tier, 3 to 15%
   less anonymous memory, fewer throttled periods, and slightly more metrics through where neither keeps up. At the low
   tier the two are the same.
2. **The fixed cost dominates at small loads.** The low tier costs about 34 millicores on both images at every limit,
   about 27 µs per metric at 1,230 metrics a second: 58% of a 50-millicore quota, throttled in two thirds of its CFS
   periods, before any real load. At 0.05, idle and per-tick costs matter as much as cost per metric.
3. **The flush stall is visible in steady state, even with headroom.** Where both keep up at heavy and ultraheavy,
   lading's fastest second is 1.25 to 1.44 times the rate on both images: it catches up after the socket blocks during a
   flush, for less than a second at 1.2 and longer at 0.6, where the worst second falls to 0.6 to 0.7 of the rate. The
   heavy tier needs about 0.4 cores on average but stalls at 0.6.
4. **Under a crunch, the plan holds memory and main does not.** Where ADP cannot keep up, main keeps reading datagrams
   into its buffer pool (thousands to about 20,000 buffers in use; anonymous memory 190 to 345 MiB at heavy 0.05, past
   the 250 MiB bound), while the plan's backpressure keeps the pool at a few dozen buffers and memory at about half,
   inside every SMP bound.
5. **Open: the plan freezes admission at 0.05 with many contexts.** At heavy 0.05 the plan decodes about 20% less than
   main (850 to 1,460 metrics a second over six runs, against 1,260 to 2,200 over fifteen for main). At ultraheavy
   0.05 it is erratic: 420, 18, and 67 metrics a second against main's 215 to 350. In the run at 18, decoding stops
   for about 25 seconds with the pool empty, so the source is backpressured, while ADP spends its whole quota
   downstream, most likely in the aggregation drain of about 250,000 contexts at 5 ms of CPU per 100 ms period. Main
   also stops decoding but keeps buffering. Not yet investigated; it bears on N2 and N3.

**Gate thresholds for N2 to N4** (against these baselines, the same harness and host; "noise" is the A/A spread above):

- **N2:** in the cells that keep up with stalls (heavy at 0.6 and 1.2, ultraheavy at 1.2), lading's fastest second at
  most 1.1 of the rate and its worst at least 0.9; in the cells that cannot keep up (heavy at 0.3, ultraheavy at 0.3
  and 0.6), a higher share of the rate than today's plan; at 0.05, no admission freeze longer than one flush interval
  (10 s) and memory inside the SMP bounds; CPU per metric no worse than today's plan beyond the noise (1% at low and
  medium, 2% at heavy and ultraheavy from 0.3, 4% at 1.2).
- **N3:** with R1's cost model removed and lanes off, CPU per metric and capacity at least today's plan's within the
  noise at every cell from 0.3, and finding 5 no worse.
- **N4:** each item a reduction in CPU per metric beyond the noise in the cells it targets, with no cell worse; the
  allocator setting decided on the low and heavy tiers at 0.3 and 1.2, where the A/A noise is 0.5 to 3%.

The data is in `test/scaling/results/limits-{aa,paced,saturate}/` on the measuring host (ignored by git); rerun a cell
with `SCALING_TIERS=<tier> SCALING_LIMITS=<limit> make limits-runs`.

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
- **Exit:** a design reviewed against N1's measurements where ADP cannot keep up.

## Lower priority

- The stage 9 Linux gate host, lane counts above two, and the per-buffer CPU cost of lanes (risk 31 of the
  [stages 7 and 9 design](programmable-metrics-stages-7-and-9.md#11-risks)).
- Partitioned aggregation. If it is ever needed, the generation and cut machinery can change the partitioning live, as
  a cut, when sustained load justifies it, rather than fixing it at boot.
- Stage 8 (Remote Config), as before.

## Open questions

- Whether production traffic resembles the SMP tiers: traces from Nodeless and from the clusters at 1.2 cores would
  check N1's workloads.
- What counts as acceptable during a crunch: which drops, how much latency, how much memory.
- Whether SMP experiments can run at 0.3 cores, so that N1's gates also run in CI (deferred: local runs for now).
- Why the plan freezes admission at 0.05 with many contexts (N1 finding 5).
- How much of N2's turn should go to draining against admission, and whether it should adapt to the CPU quota.
