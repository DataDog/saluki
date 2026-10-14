# ADP efficiency findings at production CPU limits

Findings about ADP's efficiency that do not depend on the programmable metrics work: each could be investigated and
land in ADP on its own, and each applies to native ADP (`main`) as much as to the plan. They came out of measuring the
plan against `main` at production CPU limits (N1 and the first iterations in
[programmable metrics: next steps](programmable-metrics-next.md)), and the plan-specific results stay there.

## Contents

- [Context](#context)
- [Summary](#summary)
- [Findings](#findings)
- [Non-findings](#non-findings)
- [Measuring ADP under CPU limits](#measuring-adp-under-cpu-limits)

## Context

Production ADP containers have CPU limits of 0.3, 0.6, or 1.2 cores, and Nodeless guarantees 50 millicores, with the
largest metric bursts arriving when CPU is scarcest. At these sizes ADP runs two worker threads and two DogStatsD
decoders whatever the limit, so what matters is the work per metric and the behavior when ADP cannot keep up, not
scaling across cores.

All figures here are from one 16-core Graviton2 (Neoverse-N1, aarch64) Linux host, Docker 28.5, lading 0.33.0,
`main` at `a52c25dd52`, under the steady DogStatsD loads of the SMP quality-gate cases (`quality_gates_rss_dsd_*`:
low is 1 MiB/s over 3,000 contexts, medium 10 MiB/s over 10,000, heavy 50 MiB/s over 100,000, ultraheavy 100 MiB/s
over 250,000). Those loads send 2 to 50 tags of 3 to 150 characters per metric, more and longer than most real traffic,
which matters for G1. Absolute figures may differ on x86.

## Summary

| # | Finding | Estimated impact | Confidence | Complexity |
|---|---|---|---|---|
| G1 | Context hashing in the DogStatsD source hashes every tag and inserts each hash into a set, for every metric, before the context cache lookup | 5 to 14% of instructions on these loads; less with fewer, shorter tags | medium: the cost is measured, the savings are not | low to medium |
| G2 | jemalloc's thread cache is compiled off | 3 to 7.5% fewer instructions per metric when on, measured | high for CPU; the memory cost is not yet measured against the SMP bounds | trivial, but a tradeoff to decide |
| G3 | Kernel time is 17 to 31% of instructions | potentially large | low: not broken down yet | medium |
| G4 | Decoding and newline framing are 25 to 30% of instructions | medium | low: not broken down yet | medium to high |
| G5 | `Event` is 336 bytes, set by its largest variant, while a `Metric` is 160: 52% of a metric's slot is padding | unknown: cache density and buffer size | the sizes are certain; the effect is not measured | low (boxing the large variants) |
| G6 | The resource-tracking allocator wraps every allocation | small: part of the allocator's 3 to 6% | medium | low to medium |
| G7 | When CPU-starved, native ADP keeps reading datagrams into its buffer pool, up to about 20,000 buffers, past the SMP memory bounds | memory safety under a crunch | high | medium |
| G8 | At 0.05 cores with 100,000 contexts or more, ADP collapses erratically: decoded rates vary several-fold from run to run | behavior under a crunch | high that it happens; the cause is unknown | unknown |

## Findings

### G1: context hashing per metric

**What.** For every metric, before looking up its context, the DogStatsD source computes the context key in
`hash_context_with_host_and_seen` (`lib/saluki-core/src/data_model/event/metric/context/hash.rs`): it hashes the name
and host, then hashes **each tag separately** and **inserts each tag's hash into a set** (`PrehashedHashSet`) to skip
duplicates, XORing the rest together so that tag order does not matter; then the same for origin tags. The set is
needed for correctness as written: XOR cancels a duplicated tag. All of this runs whether or not the context is
already cached, since the key is what the cache is looked up by.

**Evidence.** Instruction-sampled profiles (`perf record -e inst_retired`, call stacks) at medium 0.3, heavy 1.2, and
ultraheavy 1.2, identical in shape on `main` and the plan: hashing is 22 to 29% of ADP's instructions, and 14 to 16%
is in hashing leaf functions reached from `handle_frame`: `hashbrown::HashMap::insert` (the deduplication set) 7 to
8%, `foldhash::hash_bytes_long` (tags longer than 16 bytes) 5%, and the function itself 1.5%.

**Approaches**, to be measured:

- **A cheaper deduplication.** For the usual small tag counts, a linear scan over a small inline array of the hashes
  seen, or sorting the hashes and skipping equal neighbors, instead of a hash-set insert per tag. This alone targets
  the 7 to 8%.
- **A first-level cache keyed on the raw bytes.** Clients usually send a metric's tags in the same order every time,
  so a hash of the name and the tag section exactly as received (order-sensitive, one pass over contiguous bytes) can
  key a cache of resolved contexts; only on a miss does the order-oblivious path run. This targets most of the 14 to
  16%, at the cost of a second cache and its memory.
- A cheaper hash for short strings, if the profile still shows it.

**Validation.** Instructions per metric in the quick loop (noise 0.3 to 0.6%), then a microbenchmark of context
resolution over realistic tag counts, then SMP. The context key must not change meaning: contexts that differ only in
tag order or duplicates must still resolve to the same context.

### G2: jemalloc's thread cache

**What.** `.cargo/config.toml` compiles `narenas:1,tcache:false` into every Linux build (since #378, for low RSS on
SMP's targets), so every allocation takes the one arena's lock.

**Evidence.** With `MALLOC_CONF=tcache:true` (read at run time, layered over the compiled setting, so `narenas:1`
stays), instructions per metric fell 7.5% at medium 0.3, 3.9% at heavy 0.6, 4.2% at heavy 1.2, and 3.1% at ultraheavy
1.2 (the plan's image; noise 0.3 to 0.6%). An earlier measurement at two CPUs found +7.1% throughput and -6.9% CPU per
point for `main`, for about +8 MiB of peak memory.

**Next.** Measure memory sequentially, one cell at a time, against the SMP RSS bounds (60, 100, 250, and 420 MiB for
the four tiers; at 1.2 cores ADP peaks at about 31, 70, 135, and 195 MiB today, so there is room), then decide. With
the cache on, the tracking allocator's shared counters may become the next point of contention (G6).

### G3: kernel time

**What.** 17 to 31% of ADP's instructions are in the kernel, more on `main` (which also spends 8 to 9% in the Tokio
runtime, against about 1% on the plan, whose fused regions wake less).

**Next.** Break it down with kernel symbols: per-datagram receives (`recvmsg`), epoll, futex wake-ups, copies. Batched
receives (`recvmmsg`) and fewer wake-ups are the obvious candidates.

### G4: decoding and framing

**What.** DogStatsD decoding is 20 to 24% of instructions, and newline framing (`memchr` in `NewlineFramer`, plus
`BytesMut::extend_from_slice`) another 5 to 6%.

**Next.** A profile of the decoder alone, over the SMP payload shape and a realistic one.

### G5: the size of `Event`

**What.** From a `size_of` audit of the data model:

| Type | Size (bytes) |
|---|---|
| `Event` | 336 |
| `Trace` | 336 |
| `EventD` | 200 |
| `Log` | 192 |
| `Metric` | 160 |
| `ServiceCheck` | 160 |
| `MetricValues` | 104 (`ScalarPoints` 72: up to four points inline) |
| `MetricMetadata` | 48 |
| `Context` | 8 (an `Arc`) |
| `TagSet` | 32 |
| `MetaString`, `Tag` | 24 |

`EventsBuffer` holds `Event`s, so a metric takes 336 bytes of buffer where it needs 160, and a 1,024-event buffer is
336 KiB where metrics alone need 160 KiB, against a 64 KiB L1 data cache.

**Approach.** Box the rare large variants (`Trace`, then `Log` and `EventD`), which brings `Event` to about 160 bytes,
the size of `Metric`; then consider shrinking `Metric` itself (its values and metadata). Measure instructions and L1D
and L2 refills per metric before and after.

### G6: the tracking allocator

**What.** Every allocation passes through `TrackingAllocator`, which attributes it to the current resource group. In
the profiles the allocator as a whole is 3 to 6% of instructions. With jemalloc's thread cache on (G2), the shared
group counters may become the next contention point; per-decoder resource groups would avoid it.

### G7: native buffering when CPU-starved

**What.** When ADP cannot keep up, native ADP keeps reading datagrams from the socket into its buffer pool
(`dsd_packet_bufs`) faster than it decodes them: hundreds to about 20,000 buffers in use, peak memory up to 243 to
259 MiB at 0.05 cores, over the SMP bound at the medium tier (129 MiB against 100) and in saturation at the low tier
(90 against 60). The plan, whose regions apply backpressure to the source, holds a few dozen buffers and 32 to 45%
less memory in the same cells, inside every bound, and decodes as much or more.

**Approach.** Bound the pool, or stop reading the socket when downstream is full, so that a starved ADP leaves
datagrams in the kernel's socket buffer (where the sender blocks, or drops on UDP) instead of growing its own memory.
The plan's behavior shows that it costs no throughput.

### G8: erratic collapse when starved with many contexts

**What.** At 0.05 cores with 100,000 contexts or more, both `main` and the plan decode at rates that vary several-fold
from run to run under identical settings (at 250,000 contexts, 54 to 700 metrics a second for `main` and 195 to 737
for the plan), sometimes with stretches of 15 to 25 seconds where nothing is decoded at all. With so little CPU only
about 18,000 contexts are ever active, so the aggregation flush is cheap; the cause is not yet known.

**Next.** Profile a run in each mode, and compare context resolution, the interner, and aggregation between a fast
run and a slow one.

## Non-findings

- **Idle cost is small:** 2.3 to 2.8 millicores on either image with no traffic. A first measurement of 17 to 22
  millicores was the measuring harness's own health check (below).
- **Sampling the CPU time of resource groups** one call in 16, instead of every call, saves nothing measurable
  (within 0.5% of instructions at four cells).

## Measuring ADP under CPU limits

Lessons from building the harness (`test/scaling/run-limits.sh`; see its [README](../../test/scaling/README.md)):

- **Anything run inside ADP's container is charged to ADP's quota.** A Docker health check running `test -S` every
  second cost about 20 millicores, 40% of a 50-millicore quota; so does `docker exec cat` to read counters. Health
  checks should run every second only while starting (`start_interval`), and counters should be read from the host
  (`/sys/fs/cgroup/docker/<id>/` under Docker's cgroupfs driver).
- **Instruction counts are the precise measure of CPU efficiency:** at the same cells, instructions per metric agreed
  within 0.3% between two runs of the same image, where CPU time agreed within 1 to 3%. `perf stat --for-each-cgroup
  docker/<id>` counts a container's threads; on this VM only three hardware counters are available, so instructions
  and cycles should be pinned (`:D`) and the memory events left to share the third.
- **A Unix datagram socket blocks the sender rather than drop,** so falling behind shows as the generator sending less
  than its target, not as drops. lading's per-second rate is too lumpy below about 10 MiB/s to show short stalls.
- **jemalloc's settings can be changed at run time** with `MALLOC_CONF` (not `_RJEM_MALLOC_CONF`), without a rebuild;
  `abort_conf:true` with an invalid option confirms the variable is read.
- **At 0.05 cores with many contexts, results need many runs:** a single series' medians can mislead by tens of percent
  (G8).

To reproduce, build the images (`make limits-images`) and run a cell, for example
`SCALING_TIERS=heavy SCALING_LIMITS=0.3 make limits-runs`; for instruction counts, add `SCALING_PERF=1`, and to compare
a run-time setting, name variants, for example
`SCALING_VARIANTS="main=saluki-images/agent-data-plane:main tcache=saluki-images/agent-data-plane:main;MALLOC_CONF=tcache:true"`.
