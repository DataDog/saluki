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
| Our measurements | **The wrong regime, until N1.** They were saturated throughput at 1 to 16 unconstrained workers. None ran under a CPU quota, with CFS throttling, with bursts during a CPU crunch, or at 50 millicores ([measurement runbook](programmable-metrics-measurement.md)). [N1](#n1-results) now measures that regime. |
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
- **Status:** measured on 2026-10-21; results, findings, and gates below. The harness is `test/scaling/run-regime.sh`
  ([README](../../test/scaling/README.md#the-production-regime-n1),
  [runbook](programmable-metrics-measurement.md#the-production-regime-in-containers-n1)); the summaries are under
  `test/scaling/results/n1-20261021/`.

#### N1 method

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
  an aggregation flush, then 12 s of recovery. Exactly one flush fell inside every burst.
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

#### N1 results

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

#### What N1 says

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
  counters' zeros, expiry).
- **Throttling follows arrival, not average load.** At 30% of capacity and 3,000 contexts, 35 to 56% of periods are
  throttled at 0.3 and 0.05 CPUs, because lading delivers a second's metrics in one clump and ADP's two workers spend the period's quota in
  parallel. Production traffic from many clients is smoother, so these shares are an upper bound for the same average
  rate.
- **A flush stalls admission at high cardinality, and the plan stalls more.** At 3,000 contexts a flush costs too
  little to see. At 100,000, admission in the 2 s after the flush falls to 0.47 to 0.88 of the 2 s before for plan and
  0.35 to 0.79 for native. At 0.3 CPUs, the regime most clusters run, plan's admission falls further than native's
  (0.47 against 0.64, noise 0.15) and it drops more of the burst (shortfall 0.31 against 0.25, noise 0.03). At 0.05 the
  shortfall is also worse for plan (0.67 against 0.56, noise 0.08). This is the stall the [aggregation
  today](#what-it-means-for-what-we-built) paragraph predicts: the region host reads no input while a drain is
  pending, and plan does not absorb the stall in the datagram pool as native does (below). Everywhere else plan drops
  the same or less than native.
- **Plan holds less memory, because the datagram pool barely grows.** Peak memory is 0.54 to 0.71 of native at low
  cardinality and 0.60 to 0.95 at high. In a burst native's `dsd_packet_bufs` pool grows to 1,080 to 28,000 buffers
  (8 KiB each, up to 220 MiB); plan's to 128 to 2,700. Native keeps reading into the pool while aggregation is busy,
  which is memory spent for fewer drops; plan pushes back to the socket sooner. N2's interleaved flush should give
  plan native's drops at plan's memory.
- **Not measured.** Per-metric latency, p99 included: containers have no markers to time, so latency stays with the
  in-process paced curves, and the admission ratio around the flush is its container proxy. Also not measured: the
  Unix socket's queue depth, and admission inside ADP directly (the harness's `longest stall`, at 100 ms resolution,
  only says that stalls of 100 to 600 ms occur in an overloaded burst at 0.05 and 0.3 CPUs on both paths). UDP socket
  drops need a generator that paces within the second. 0.6 CPUs was never overloaded.
- **Noise.** Steady CPU per metric and overloaded throughput at 0.3 and 1.2 CPUs are within 2 to 6%. At 0.05 CPUs and
  for the shortfall at low cardinality, the A/A distance reaches 0.14 to 1.4, too noisy for a ratio gate: there the
  gates below use absolute thresholds or none. The admission ratio's noise is 0.08 at 1.2 CPUs and 0.15 at 0.3.

#### Gates for N2 to N4

**Method.** Judge a change in the same harness on the same host, with N1's steady rates (the `REGIME_STEADY_RATES`
in each series' `series.txt`) and burst, against N1's plan image (or the last accepted change's):
`REGIME_LABELS="plan-a plan-b candidate-a candidate-b"`, `REGIME_CANDIDATE_IMAGE`, three rounds, one series per
cardinality (about 1 h 45 min each in the dory VM). Add `native-a native-b` when a gate compares with native. A ratio is the medians'
candidate / plan; it passes when it meets the threshold, and the candidate's own A/A distance must stay within the
noise column above, or the series is rerun. Each threshold is 1 plus or minus the largest A/A distance N1 measured for
that measure at that limit (over both images, both cardinalities, and for G1 both windows), rounded up to the next 5%;
where that distance exceeds 0.25 the cell has no ratio gate.

| Gate | Measure | 0.05 CPUs | 0.3 CPUs | 0.6 CPUs | 1.2 CPUs | Applies to |
|---|---|---|---|---|---|---|
| G1 | CPU ns/metric, steady and burst plus recovery, both cardinalities | at most 1.15 | at most 1.10 | at most 1.10 | at most 1.05 | N2, N3, N4 (no regression) |
| G2 | An improvement claimed by N4 (the same measure) | below 0.85 | below 0.90 | below 0.90 | below 0.95 | N4 |
| G3 | Overloaded throughput (burst decoded/s) | none (noise up to 0.35) | at least 0.90 | none (not overloaded) | at least 0.95 | N2, N3, N4 |
| G4 | Peak memory | at most 1.15 | at most 1.10 | at most 1.10 | at most 1.20 | N2, N3 (memory in flight during a drain), N4's allocator setting |
| G5 | Shortfall in the burst, 100,000 contexts | at most native's (0.56 in N1) | at most native's (0.25 in N1) | none | at most plan's + 0.02 | N2, N3 exit |
| G6 | Admission after / before the flush, 100,000 contexts | none (noise 1.0) | at least 0.85 | none | at least 0.90 | N2, N3 exit |

- **N2 and N3** exit with G1, G3, and G4 holding, and G5 and G6 met. N1 sets today's values to beat: at 0.3 CPUs plan's
  admission ratio is 0.47 and its shortfall 0.31; at 1.2 CPUs 0.71 and 0.16. G6's 0.85 at 0.3 CPUs is the noise floor
  below 1, which is what "admission never stalls on a flush" can mean at this resolution. The p99 half of N2 and N3's
  exit stays in process.
- **N4** parts each pass G1 and G4, and claim a gain only by G2. The allocator decision reads G1 and G4 at 0.3 and 1.2
  CPUs. The datagram pool's worst case reads the pool and peak memory columns at 0.05 and 0.3 CPUs with 100,000
  contexts, where native grows its pool to 1,080 to 7,000 buffers in an 8 s burst and plan to 128 to 418.
- **What no gate covers yet.** 0.6 CPUs under overload (recalibrate the burst against its measured capacity, 288,000
  and 377,000 metrics/s, first), overloaded throughput at 0.05 CPUs, and any change smaller than G1's tolerance: a
  5% CPU saving at 0.3 CPUs, for instance, needs more rounds or the in-process harness to show.

#### V7b candidate results

Measured on 2026-10-21 by the method above: the candidate image is this branch at `f35066de9e` (V7b, lanes off by
default and in `lanes-off.yaml`), against N1's plan image, three rounds, N1's steady rates and burst. At 100,000
contexts the labels were `native-a native-b plan-a plan-b candidate-a candidate-b` (72 runs; the series was paused
for 80 minutes after 20 runs and resumed on the same host), at 3,000 `plan-a plan-b candidate-a candidate-b` (48
runs). The summaries are under `test/scaling/results/v7b-20261021/`. Ratios are candidate / plan medians; the flush
columns are the state lane's `plan_lane_flush_seconds` and `plan_lane_flush_blocked_seconds` across the burst.

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

**Verdict: V7b does not meet N2's exit.**

| Gate | Result |
|---|---|
| G1 | Holds at 100,000 contexts. Misses at 3,000: 1.115 steady at 0.3 CPUs and 1.144 steady, 1.104 burst at 0.6 (at most 1.10); holds at 0.05 and 1.2. |
| G3 | Holds: 0.967 and 1.003 at 0.3 CPUs, 1.012 and 0.983 at 1.2. |
| G4 | Holds everywhere: at most 1.044. |
| G5 | Misses at 0.05 CPUs (0.67 against native's 0.60, 0.56 in N1; equal to plan's) and at 0.3 (0.375 against native's 0.30, 0.25 in N1). Holds at 1.2 (0.16, plan's + 0.02 is 0.19). |
| G6 | Misses at 0.3 CPUs (0.44, at least 0.85) and at 1.2 (0.73, at least 0.90; plan's is 0.65, native's 0.66). |

The owner's allowance (V7b done if G6 passes and G5's shortfall at 0.3 CPUs is at most 0.31) does not apply: G6
misses, and the shortfall is above 0.31. The candidate's A/A distance exceeds N1's noise in some cells (steady CPU per
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
  bucket. G6's 0.85 and 0.90 are above what any image reaches in this harness.
- **The pace does not spread the flush.** Passes completed in 0.07 s at 1.2 CPUs and 0.58 s at 0.3, not over the 5 s of
  a third of the interval. The pace is a floor: while a pass is pending the region runs a step whenever no batch waits
  (`has_background_work`), and under a CPU quota an idle region is not spare CPU, because the decoders and encoders
  share the quota. So the drain and the encoding it causes land right after the flush, where G6 reads.
- **At 0.3 CPUs the candidate parks less of the burst.** Its datagram pool ends the burst at 129 buffers against plan's
  798 and native's 5,708, and its shortfall is 0.375 against 0.33 and 0.30, with burst throughput 0.97 of plan's. As
  [V7b section 4](programmable-metrics-v7b.md#4-the-datagram-pool-and-back-pressure) expected, G5 is where a burst
  waits: N4's datagram pool item, bounded by G4.
- **At 0.6 CPUs the candidate is worse than plan in the burst.** The 0.6 burst overloads both images slightly in this
  series (decoded 0.96 of plan's, shortfall 0.075 against 0.039, admission 0.60 against 0.81, a distance of 0.27 against
  N1's noise of 0.22). No gate covers 0.6, but it is the same direction as 0.3.
- **CPU per metric rose at 3,000 contexts below 1.2 CPUs.** The resource groups put the increase in the DogStatsD
  decoders (about 13% more per metric at 0.3 and 0.6 CPUs) and the fused region (stateless work and aggregation
  together, about 28% more at 0.6), and none at 1.2. One region task behind the decoders changes how often they wake and
  hand off under throttling. That is a hypothesis for the in-process harness to test, not a measurement.
- **A telemetry oddity.** In 10 of 24 candidate runs at 100,000 contexts, `aggregate_flushes_total` reads 6 to 9 for a
  run where plan's reads 5, with the same metrics flushed, and the lane's flush histogram records fewer completed drains
  than flushes (in one run none), so `plan_lane_flush_blocked_seconds` cannot be read for those runs. The runs where it
  can be read miss G6 as badly, so this does not explain the miss, but the count needs a look.

**What it points to.** The stall that N2 targeted is gone; what remains is the flush's CPU at a full quota and where a
burst waits. In order: cap a pass at its pace rather than running steps whenever the region is idle, so its drain and
encoding spread over the third of the interval as V7b intended; then N4's encode cost per metric and the datagram
pool's growth (G5, bounded by G4); and rerun G1 at 3,000 contexts with the in-process harness to find whether the
decoder cost is real. G6's thresholds should also be revisited: native itself reaches only 0.61 and 0.66.

#### Layout A/B at 3k contexts

G1's miss at 3,000 contexts asked whether fusing aggregation into one region costs CPU. Measured on 2026-10-22: one
image of this branch at `cba809db95` (V7b with the driver fix for the flush count), run fused (`lanes-off.yaml`) and
with state isolated (`isolated.yaml`, `experimental.programmable_metrics.isolate_state: true`: `dsd_pre_aggregation`,
`dsd_agg`, and `metrics_enrich` as three regions, flushes still interleaved), beside N1's plan image, in the regime
harness at 0.3 and 0.6 CPUs with N1's steady rates and burst, three rounds of `plan-a plan-b fused-a fused-b isolated-a
isolated-b` (36 runs, `REGIME_VARIANTS`; summary in `test/scaling/results/layout-ab-20261021/`).

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
  the same in all three. N1's plan image measured 1,881 ns a metric at 0.6 CPUs in the V7b series and 2,027 in this
  one.

**What it means.** The 1.115 and 1.144 that G1 read at 3,000 contexts were noise on this host, or a cost that the
driver fix removed, not a cost of fusion: the layout needs no change. With six runs per image, steady CPU per metric
at 3,000 contexts on 0.3 and 0.6 CPUs resolves about 10% at best. Before gating a change on G1 there, run more rounds or
measure CPU per metric in process. This series took no profile: nothing points at code in one layout and not the other.

### N2 and N3: one region from decode to encode, with an interleaved flush

- **Status (2026-10-21):** built as version 2's step V7b ([design and as built](programmable-metrics-v7b.md),
  amendments 203 to 206): without lanes the metrics path is one region, `dsd_pre_aggregation`, R1's cost model is
  gone, lanes are opt-in, and a tick or boundary flush interleaves with admission, paced to a third of the flush
  interval. Approach items 1 to 3 are done; item 4 (decoding inside the region, encoding as a terminal kernel) is V8's.
  The N1 candidate runs ([results](#v7b-candidate-results)) found the stall gone but the exit not met: G5 and G6 miss
  at 0.3 CPUs, G6 at 1.2, and G1 at 3,000 contexts at 0.3 and 0.6.
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
- How much of N2's turn should go to draining against admission, and whether it should adapt to the CPU quota (V7b's answer: a pace of a third of the flush interval, at least a step every 100 ms, which never reads the quota). The candidate runs say it does not spread the flush: the pace is a floor, and passes
  run ahead whenever the region is idle, completing in 0.07 to 0.6 s ([results](#v7b-candidate-results)).
