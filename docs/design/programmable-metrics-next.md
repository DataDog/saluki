# Programmable metric processing: deployment context and next steps

What we learned about how ADP is deployed after stages 1 to 7 and 9 were built, what that means for the work, and what
the next chunk of work should look like. Read the [index](programmable-metrics-index.md) first for the overall picture.

## Contents

- [Deployment context](#deployment-context)
- [What it means for what we built](#what-it-means-for-what-we-built)
- [Lessons](#lessons)
- [The next chunk of work](#the-next-chunk-of-work)
- [How we iterate](#how-we-iterate)
- [Hypotheses](#hypotheses)
- [What the first iterations showed](#what-the-first-iterations-showed)
- [What stalls admission at a flush](#what-stalls-admission-at-a-flush)
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

These are from the second run of the matrix (2026-10-14). The first (2026-10-13) charged every run about 20
millicores of the harness's own health check, which ran a process inside ADP's container every second; that took 40%
of the quota at 0.05 and distorted every absolute figure there (see the [log](programmable-metrics-log.md#n1-measuring-at-production-cpu-limits)).
The ratios from 0.3 up barely moved, since both images paid it alike.

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
with 5 to 8% less CPU per metric from 0.3 up (15 to 20% at 0.05), and 12 to 62% less memory (at 0.05 and 3,000 contexts, main holds 88 MiB against
the low tier's 60 MiB bound; the plan 33).

**Noise** (A/A, main against main, spread from p25 to p75 over the median): CPU per metric within 0.7 to 1.7% at the
low tier and 1 to 3% at the heavy tier from 0.3 (A/A ratios 0.98 to 1.02), and 9 to 17% at heavy 0.05, a collapse
cell where only large effects count.

**Findings.**

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

**Gate thresholds for N2 to N4** (against these baselines, the same harness and host; "noise" is the A/A spread above):

- **N2:** *revised (2026-10-15), replacing gates on lading's fastest and slowest seconds:* see
  [what stalls admission at a flush](#what-stalls-admission-at-a-flush). In the cells that cannot keep up (heavy at
  0.3, ultraheavy at 0.3 and 0.6), a higher share of the rate than today's plan; at 0.05, memory inside the SMP bounds;
  CPU per metric no worse than today's plan beyond the noise (2% from 0.3, 3% at heavy 1.2). Stalls are judged in
  sequential runs.
- **N3 and N4:** instructions per metric in the quick loop (noise 0.3 to 0.6%), confirmed in sequential runs for CPU
  time and memory.

The data is in `test/scaling/baselines/n1-2026-10-14/` (per-run tables and summaries); the per-second series and logs
stayed on the measuring host. Rerun a cell with `SCALING_TIERS=<tier> SCALING_LIMITS=<limit> make limits-runs`.

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
- **Revised (2026-10-15):** measured at 100 ms, the drain pauses admission for only 30 to 75 ms at 0.6 to 1.2 cores
  (100 to 290 ms at 0.3); most of the stall is the encoder's burst after it, which a non-blocking drain alone does not
  change. N2 becomes the foundation of a layered approach: see
  [what stalls admission at a flush](#what-stalls-admission-at-a-flush).

### N3: settle the layout for small CPU budgets

- **Goal:** the simplest, cheapest layout at one core or less.
- **Approach:** fuse maximally between fences whatever the costs, which means deleting R1's cost model (plan
  [P9.7](programmable-metrics-stages-7-and-9.md#p97-r1s-cost-model) lists what it touches); make lanes opt-in
  (`lanes.mode: off` by default, amendment 157 reversed) while keeping the code.
- **Exit:** N1's measurements at the four limits show the fused layout at least as good as today's.
- **Status (2026-10-14):** maximal fusion without lanes saves no instructions at any of eight cells and costs about 1%
  where ADP cannot keep up (H2, [results](#what-the-first-iterations-showed)); N3 is a simplification, to be weighed
  after sequential runs measure its effect on stalls and CPU time.

### N4: efficiency per metric

Each is a small part, measured by N1 at the production limits:

- **Passthrough batching.** The plan sends one buffer per input batch where the native batcher filled 1,024 timestamped
  metrics ([implementation risks](programmable-metrics-implementation.md#risks)); the encoder's cost of more, smaller
  buffers is unmeasured.
- **Encode and compression cost** per metric (M3 in the [runbook](programmable-metrics-measurement.md)).
- **The allocator setting.** The thread cache saved 4 to 7% of CPU per point at two CPUs for about 8 MiB more peak
  memory ([decisions](programmable-metrics-status.md#decisions-awaiting-the-owner)); decide it at 0.3 and 1.2 CPUs.
- **The datagram pool's worst case** when CPU-starved: its ceiling and growth under a burst.

**Status (2026-10-14):** these are ADP-wide, not plan work, and moved with the other independent findings to
[ADP efficiency findings](adp-efficiency-findings.md); the thread cache measured 3 to 7.5% fewer instructions per
metric (G2 there).

### N5: shedding during a CPU crunch

- **Goal:** degrade deliberately rather than by dropping at the socket.
- **Approach:** a design spike on the delivery-policy slot plus signals. Signals and reactive operators are the
  specification's stage 10, and the delivery-policy slot is already open to artifacts in a minimal form
  ([stage 7](programmable-metrics-stages-7-and-9.md#413-the-delivery-policy-slot)). With them, a policy could act on
  queue or pool depth: drop low-value metrics, widen aggregation, or sample. The programmable substrate is uniquely
  suited to this.
- **Exit:** a design reviewed against N1's measurements where ADP cannot keep up.

## How we iterate

Each hypothesis is a change with a predicted effect, tested in a loop short enough to run several an hour, and checked
against trusted measurements before it is kept. Three tiers:

| Tier | What | Time | Used for |
|---|---|---|---|
| Quick loop | a variant against the current plan on the focus cells: medium at 0.3, heavy at 0.6 and 1.2, ultraheavy at 1.2, and capacity at 0.3; 30-second windows; cells in parallel; instructions and memory counters per metric (`perf stat`) | 15 to 20 minutes with the build | every hypothesis |
| Milestone | the focus cells against `main`, one cell at a time, three rounds | 1 to 2 hours | a change worth keeping |
| Full matrix | N1's 16 cells, A/A, and capacity | about 9 hours, overnight | before results are recorded |

The quick loop's shortcuts are kept only if, run against the N1 baseline as an A/A and as `main` against the plan, it
reproduces the full matrix's ratios within the A/A noise. On the first N1 data, 30-second windows reproduced the
55-second ratios within 1 to 2% at every cell from 0.3; 10-second windows, shorter than one 15-second flush interval,
are too noisy. Instruction counts
are close to deterministic and unaffected by throttling or neighbors, so they decide hypotheses about CPU per metric in
one round; stalls, capacity, and memory still need the wall-clock measures.

## Hypotheses

Results so far are in [What the first iterations showed](#what-the-first-iterations-showed): H1 dropped, H2 neutral,
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
| H2 | **Fuse maximally (N3).** Delete R1's cost model: one region between fences, no extra hops, queues, or wakeups (today the post-aggregation filter gets its own region at 66 ns against a 64 ns budget). | fewer instructions and wakeups per metric | planned code change |
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

## What the first iterations showed

On 2026-10-14, after the N1 baseline, the quick loop was validated against it, instruction profiles were taken, and
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
plan's operators can be judged (below).

**Hypothesis results** (instructions per metric against the unmodified plan; noise 0.3 to 0.6%):

| | medium 0.3 | heavy 0.6 | heavy 1.2 | ultraheavy 1.2 | low 0.3 | medium 1.2 | heavy 0.3 | ultraheavy 0.6 |
|---|---|---|---|---|---|---|---|---|
| H1: CPU-time sampling one in 16 | -0.5% | +0.4% | -0.2% | -0.3% | | | | |
| H2: maximal fusion without lanes (`SALUKI_EXPERIMENT_FUSE_ALL=1`, a switch in `7c7631a4bb`, since removed) | 0.0% | +1.2% | -0.2% | -0.4% | +0.3% | 0.0% | +1.2% | +1.2% |
| H9: jemalloc's thread cache (`MALLOC_CONF=tcache:true`) | **-7.5%** | **-3.9%** | **-4.2%** | **-3.1%** | | | | |
| H1, H2, and H9 together | -7.7% | -3.8% | -4.4% | -4.2% | | | | |

- **H1 is dropped:** sampling CPU time saves nothing measurable.
- **H2 saves no instructions**, and costs about 1% where ADP cannot keep up. Deleting R1's cost model (N3) is a
  simplification, not a speed-up; its effect on stalls and CPU time is still to be measured sequentially. The
  existing one-lane-per-slot layout, which also fuses maximally, was 4 to 9% worse at two workers because of the lane
  runtime, not fusion.
- **H9 is a real win but ADP-wide**, not plan work: see the ADP findings, G2.

**The plan-specific landscape**, by estimated impact, confidence, and complexity:

| Item | Impact | Confidence | Complexity |
|---|---|---|---|
| **N2, non-blocking flush** | **high for behavior**, the team's stated top problem; little effect on instructions. Revised: the drain is only part of the stall, see [below](#what-stalls-admission-at-a-flush) | high that the problem is real | high (design) |
| H7, fixed cost per batch | small; may explain the plan's 1 to 3% at the low tier | medium | low |
| H3 and H4, dense context IDs and predicate tables | unmeasurable on the default profile, which runs almost no policy | low until a policy-heavy profile is measured | medium |
| H5, aggregation state by dense ID | low for instructions (aggregation is 2 to 3%); medium for flush cost, which feeds N2 | medium | high |
| N3, delete R1's cost model | none for instructions; a simplification | high | medium |
| H8, compact metric buffers | superseded by boxing `Event`'s large variants, ADP-wide (G5) | | |

**Direction.** Efficiency per metric is now mostly an ADP-wide matter, and those items go to the separate document.
The plan work focuses on what the plan can improve structurally and native ADP cannot: **behavior under load, starting
with flush stalls (N2)**, built on the generations, cuts, and yielding kernels already in place. Before H3 and H4, the
`mapper` and `gateways` profiles should be profiled, to see what programmability actually costs when policy runs.

## What stalls admission at a flush

On 2026-10-15 ADP was instrumented to see what happens at a flush (the flow trace, kept as a patch beside the results
in `test/scaling/baselines/stalls-2026-10-15/`, not in the tree): every 100 ms, each component's time blocked sending
downstream and each region's input queue; each drain's duration, steps, and time waiting on its sends; and, from the
host, ADP's CPU time and throttling, lading's bytes, and whether datagrams waited at the socket, lined up on one clock
with wall-clock-aligned instruction profiles. The aggregation flush interval is **15 seconds** (earlier notes said 10).

**What happens at each flush**, the same on the plan and on `main`:

| Phase | Duration | Share of the flush's instructions | Admission |
|---|---|---|---|
| **Drain**: aggregation emits the closed buckets (29,048 series at heavy, about 47,000 at ultraheavy) | 28 to 76 ms at 0.6 and 1.2 cores; 100 to 290 ms at 0.3 | about 3% | paused: the host reads no input while a drain is pending |
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
290 ms at 0.3, but not the encoder's burst, which is most of the stall. The drain is already a resumable, budgeted
kernel call; what the layers below add is what decides when work runs:

1. **Non-blocking drain** (N2 as written): the host admits input between drain steps, which needs a kernel contract for
   consuming while a drain is suspended, with open and closed buckets kept apart. The semantic foundation.
2. **Pacing**: a policy that releases drain steps over time (so many series per 100 ms, or within the headroom) rather
   than back to back, so the encoder's work, which follows its input batch by batch, spreads with them. It needs the
   non-blocking drain, or admission would stop for the whole paced drain. It removes the spike where average demand,
   flush work included, fits under the quota.
3. **Priority for admission**: where demand exceeds the quota, something must wait, and output can wait seconds where
   admission cannot. Tokio has no task priorities and throttling stops the whole cgroup, so this needs the encoder on
   threads of lower CPU weight (a `nice` value or `SCHED_IDLE` within the cgroup) or a per-period budget for output
   work. It acts on the encoder directly, and applies to native ADP too (G9 of the
   [ADP findings](adp-efficiency-findings.md)).
4. **Shedding** (N5): when even delayed output cannot keep up.

Each layer works without the ones after it. The cheapest experiment is the third: running the encoder at a lower
thread priority, in the quick loop, before any design work.

**Gates for stalls**, replacing those on lading's fastest and slowest seconds: admission per 100 ms after each flush
against before it (the flow trace's count of metrics the source admits), the drain's duration, and the share of
throttled periods in the three seconds after a flush; in sequential runs, against these baselines.

**Measurement lessons.** lading sends each second's bytes in about half a second, so per-second rates and any window
compared with another mix its pacing with ADP's behavior; for a Unix datagram socket, `ss` reports the size of the
first datagram waiting, not a queue depth, and the socket's memory is not visible; throttled time in `cpu.stat` is
summed over CPUs; profiles of one flush, and flushes anchored on CPU peaks, both misled here (the plan's encoder first
looked 2.5 times as costly, and its throttling five times as high); four flushes per profile and the same anchor for
both images settled it.

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
- Whether the parallel quick loop's CPU-time skew comes from running slots side by side or from two-core CPU sets.
- Which policy production configurations use (mappers, filters, routing), to choose the policy-heavy workload that H3
  and H4 need.
- How much of N2's turn should go to draining against admission, and whether it should adapt to the CPU quota.
