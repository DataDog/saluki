# Programmable metric processing: measurements

**Status:** how to run the measurements of stages 7 and 9 on branch `luke/programmable-metrics`, after stage 9. The
design that defines them is [the stages 7 and 9 plan](programmable-metrics-stages-7-and-9.md): section
[8.6](programmable-metrics-stages-7-and-9.md#86-measurement-harness-and-what-is-measured-first) lists M1 to M11 and
records their results, [8.5](programmable-metrics-stages-7-and-9.md#85-stage-9-integration-increments-and-gate) the
increments and the gate they feed, and
[8.7](programmable-metrics-stages-7-and-9.md#87-conditional-parts-measured-options) the conditional parts O1 to O10
they trigger. This document holds the commands, the conditions, and how to read the
output; it records no results. Where it disagrees with the code, the code is authoritative: each measurement's
documentation comment names its knobs and defaults.

## Contents

1. [What is measured](#what-is-measured)
2. [Conditions](#conditions)
3. [The in-process harness](#the-in-process-harness)
4. [M1 to M11](#m1-to-m11)
5. [Scaling curves in process](#scaling-curves-in-process)
6. [Stage 7 gate measurements](#stage-7-gate-measurements)
7. [Scaling curves in containers](#scaling-curves-in-containers)
8. [Linux on a Mac: the Docker VM](#linux-on-a-mac-the-docker-vm)
9. [The gate host](#the-gate-host)

## What is measured

Every in-process measurement is an ignored test in ADP's
`bin/agent-data-plane/src/programmable_metrics/topology/measurement_tests.rs` or its submodules
`measurement_tests/{source_feed,key_skew,post_aggregation}.rs`, or a criterion bench; the container curves are
`test/scaling/`. Each row decides something named in plan 8.6; the decisions so far are in 8.6's two results tables.

| # | What | Where | Decides |
|---|---|---|---|
| M1 | Decode and resolve cost per metric; one saturated stream connection, legacy against plan | `dogstatsd_codec` bench (`dogstatsd_codec_decode`); `source_feed::one_saturated_stream_connection` | Q1, gate item 4 |
| M2 | The datagram decoders' waits, the admission point's hold, and resolver throughput by threads | `source_feed::decoder_pool_waits`; `dogstatsd_codec` bench (`dogstatsd_codec_shared_resolvers`) | O4, O5; whether a ceiling common to both paths flattens the curves |
| M3 | Encode and compress cost per metric, one builder | `encode_host` bench | O6 (S > 1 builder shards) |
| M4 | The state budget's cost under churn | `state_budget` bench (saluki-core); `aggregate_churn` bench (saluki-metric-kernels) | O1 |
| M5 | Key skew over key groups and state lanes; worker time per metric over P | `key_skew::key_skew_over_key_groups_and_state_lanes`; the in-process curves swept over P | the automatic P; whether group movement is needed |
| M6 | Per-buffer fixed cost and what it is made of; allocator contention; buffer fill | `per_buffer_fixed_cost`, `allocator_group_contention`, `source_feed::source_buffer_fill` | N for CPU-time sampling, B, O2, per-decoder resource groups |
| M7 | The cost of the always-connected taps on `dsd_in.metrics` | `source_feed::tap_cost` | that the tap gate saves what it should |
| M8 | A flush storm: 1M contexts ticking, with P lanes ticking together | `source_feed::flush_storm` | O8 (stagger), O6 |
| M9 | RSS and the firm bound | the SMP quality gate at about 250,000 contexts; `lane_tests::the_firm_bound_grows_by_each_lanes_own_bounds_up_to_64_workers` | gate item 6; the `Strict` fit |
| M10 | `SourceClock` contention | `source_feed::decoder_pool_waits` on lanes | per-lane clocks |
| M11 | Per-lane memo hit rates | `source_feed::memo_hits_on_producer_lanes` | per-lane memos at the configured size, or shared |
| Curves | Throughput, CPU, memory, and paced latency over W, legacy against plan | `source_feed::scaling_curves_through_the_dogstatsd_source`; `make scaling-curves` | gate items 1 to 3 |
| 9.2 | OTLP and checks throughput over post-aggregation lanes | `post_aggregation::otlp_and_checks_throughput_on_post_aggregation_lanes` | increment 9.2 |
| Stage 7 | Plan against legacy on the scripted feed, and legacy against itself | the five tests at the top of `measurement_tests.rs` | the stage 7 gate (plan 8.3), and the tolerances |

Two related targets are not measurements: `make test-measurement` checks the harness (below), and `make test-stress`
runs the multi-threaded stress run of plan 6.5 (`installer::lifecycle_tests::lane_stress_*`, `SALUKI_STRESS_SECS` of
traffic, 30 by default), which reads `plan_lane_misrouted_total` for gate item 5.

## Conditions

The thresholds hold only for release builds (the release profile of the workspace: thin LTO, debug assertions on) on
a quiet machine:

- **One measurement at a time.** No two measurements, no builds, and no other heavy work at once; on a laptop, plug in
  power. The commands below run one test per process and one process at a time.
- **Load.** Wait for the one-minute load average to fall below about 4 before each run, and record it at the start and
  the end. Above about 4, legacy fails its own A/A tolerance. An idle macOS desktop sits near 3 (a browser renderer, the
  window server), so a lower target cannot be waited for there; the end of a run reads higher because of the run
  itself.
- **Ratios, not rates.** On a Mac, performance and efficiency cores differ, and legacy's own throughput at eight workers
  moves by a fifth between runs. Compare plan over legacy within one run; do not compare absolute rates across runs.
- **No UDP benchmark beside the demo check.** A loopback UDP flood (M2 at one metric per datagram sends about 340,000
  datagrams a second, and M8 and the curves over `udp` are floods too) makes macOS drop loopback datagrams for every
  process. Never run one beside `make demo-programmable-metrics-check`: the demo's canaries go missing (its loopback
  witness reports such drops as the host's).
- **Datagrams on macOS.** Unix datagram sockets on macOS cap a datagram at 2 KiB with a 4 KiB receive buffer
  (`net.local.dgram.maxdgram`, `net.local.dgram.recvspace`), which would measure the kernel's buffer, so the harness's
  `uds` means Unix stream connections there and the datagram measurements use UDP. A UDP sender cannot wait for room:
  on macOS the dropped share of a saturated UDP run is what the kernel dropped at the source's socket, and throughput
  is still what the source took. On Linux, Unix datagram senders wait for room by default and nothing drops.
- **The Docker VM.** While macOS measurements run, `docker pause` any build container in the VM, so that it does not
  take the Mac's cores.
- **What a Mac decides.** Of the gate, macOS decides item 5 and the shape of the curves at W up to 4 (plan 8.5); items
  1 to 4 and 6 are the gate host's.

Approximate times below are from an 18-core Apple M5 Pro in release; a slower host takes longer.

## The in-process harness

First check the harness itself, in seconds (debug is fine): `make test-measurement` lints saluki-core,
saluki-components, and ADP with their `measurement` features and runs the tests of the probes and the harness that are
not ignored (the generators' pacing, the drop-when-full sender, the workloads, the templates of M6, and one short
source-fed run of each path and lane layout).

The source-fed measurements and `key_skew` need ADP's `measurement` feature, which compiles the probes of saluki-core
and of the DogStatsD source; the stage 7 gate measurements and `per_buffer_fixed_cost` do not, but run the same way
with it. Every in-process command below uses one prefix:

```sh
M() { cargo nextest run -p agent-data-plane --release --features measurement --run-ignored only --no-capture \
  --test-threads 1 -E "test(/$1/)"; }
```

### What the source-fed harness measures

`source_feed` feeds both topologies through the real DogStatsD source: generator threads outside the runtime of the
topology send pre-encoded payloads over loopback UDP, Unix stream connections (length-prefixed frames), or Unix
datagram sockets (Linux only). The topology runs on W workers with `max(2, W - 2)` decoders set explicitly (the native
default reads the host's vCPUs), the real encoders on a global pool of eight threads, and forwarders that discard.

- **Throughput** is the metrics the source dispatched per second over a steady-state window (the `dispatch_items`
  probe on `dsd_in.metrics`, summed over the lane outputs `dsd_in.metrics_<d>` on producer lanes). At saturation this
  is the rate of the slowest stage.
- **CPU** is the process's CPU time (`getrusage`) per dispatched metric, less the generator threads' and the test
  thread's own (thread CPU clocks).
- **Latency** (paced phases) is the time from a generator sending a timestamped sentinel to the main encoder receiving
  it.
- **Memory** is what the process holds at the end of the window less what it held before the topology started (the
  test binary's tracking allocator).
- **Encoded** is what the encoders' payloads carried, after aggregation: it shows the encoders worked; it is not
  throughput.
- **Dropped** is sent less dispatched over the window, and **refused** the part of it the sockets refused.

Every paced phase paces afresh from its own start. Timing probes (lock waits, holds, send waits, clones) are on only in
M2, M7, and M8; item counts are always on.

### Knobs

Each measurement reads its parameters from the environment, with defaults of its own (`Knobs` in
`measurement_tests/source_feed.rs`; the defaults below are the scaling curves').

| Variable | Default | Meaning |
|---|---|---|
| `SALUKI_MEASURE_WORKERS` | `1,2,4,8,16` | the primary runtime workers W, comma-separated |
| `SALUKI_MEASURE_TRANSPORT` | `uds` | `udp`; `uds` (Unix datagrams on Linux, Unix streams on macOS); `uds-stream`; `uds-dgram` |
| `SALUKI_MEASURE_DROP_WHEN_FULL` | `0` | `1`: datagram senders drop what a full socket refuses and count it, as DogStatsD clients do; `0`: they wait for room, as lading does |
| `SALUKI_MEASURE_CONFIG` | `mapper` | `mapper` (a mapper profile, a metric filterlist, and a tag filterlist); `defaults`; `gateways` (`mapper` with two endpoint routing groups and Multi-Region Failover) |
| `SALUKI_MEASURE_WORKLOAD` | `mixed` | `mixed` (the scripted batch as DogStatsD text); `distinct` (a context of its own on seven lines in eight); `passthrough` (timestamped counters only) |
| `SALUKI_MEASURE_LANES` | `off` | the plan path's lanes: `off`; `auto` (ADP's automatic counts at each W); `<state>,<post>`; or `<producer>,<state>,<post>`, each a count or `auto` (decoder `d` feeds producer lane `d mod producer`, which no configuration key does) |
| `SALUKI_MEASURE_ROUNDS` | 5 | alternating rounds per path |
| `SALUKI_MEASURE_WINDOW_SECS` | 5 | the measured window of a phase, after its warm-up |
| `SALUKI_MEASURE_PER_DATAGRAM` | 50 | metrics per payload, within 8 KiB |
| `SALUKI_MEASURE_CONTEXTS` | 256 | context indexes the generators rotate through |
| `SALUKI_MEASURE_GENERATORS` | half the decoders, at least 2 | generator threads |
| `SALUKI_MEASURE_CONNECTIONS_PER_GENERATOR` | 1 | connections or sockets per generator |
| `SALUKI_MEASURE_DECODERS` | `max(2, W - 2)` | the source's decoders |
| `SALUKI_MEASURE_RATE` | half the saturated rate | a paced rate in metrics per second |

**The harness's lanes default to `off`**, the layout without lanes (R1's pipelining), whatever ADP's default. To
measure the defaults as built (`lanes.mode: auto`), set `SALUKI_MEASURE_LANES=auto`; each run prints the lane policy
it ran per W. Some measurements read knobs of their own, named in their sections.

## M1 to M11

### M1: decode cost per metric, and one stream connection

```sh
cargo bench -p saluki-components --features measurement --bench dogstatsd_codec -- dogstatsd_codec_decode   # ~2 min
SALUKI_MEASURE_WORKERS=1,2,4,8 SALUKI_MEASURE_ROUNDS=5 SALUKI_MEASURE_LANES=auto \
  M 'source_feed::one_saturated_stream_connection'                                                         # ~6 min
```

- The bench prints `thrpt` in metrics per second for one decoder (a stream connection's, with no front cache), per
  profile (`long_lines`, modeled on the SMP lading shape: names up to 200 bytes, up to 50 tags of up to 150 bytes;
  `compact`, about 50 lines per datagram), at 3,000 and 100,000 contexts, one and 50 metrics per datagram. The cost per
  metric is `1e9 / thrpt` ns: the source's decode path (framing, parsing, filter, well-known and origin tags, context
  resolution) without event buffers or dispatch.
- The test prints, per W, `M1, one stream connection, W = …: legacy X metrics/s (Y ns/metric), plan X metrics/s (Y
  ns/metric), ratio R`. Gate item 4 is R of at least 0.93. If `1e9 / decode ns` is close to the connection's rate, the
  connection's decoding task bounds it on both paths. The whole pre-aggregation chain on one task, for comparison, is
  the `pre_aggregation_pipeline` group of `cargo bench -p saluki-metric-plan --features test-util --bench
  region_driver`. The container equivalent is `SCALING_TRANSPORT=unix_stream SCALING_GENERATORS=1
  SCALING_CONNECTIONS=1`.

### M2: decoder waits, the admission point, and resolver throughput

```sh
M 'source_feed::decoder_pool_waits'                                            # D in 1, 2, 4, 8; ~3 min
SALUKI_MEASURE_DECODER_COUNTS=2,8,14 M 'source_feed::decoder_pool_waits'      # larger pools
cargo bench -p saluki-components --features measurement --bench dogstatsd_codec -- dogstatsd_codec_shared_resolvers
```

- Datagram transports only (stream connections bypass the decoder pool): Unix datagrams on Linux, UDP on macOS. W is
  `D + 2` unless `SALUKI_MEASURE_WORKERS` is set (then its first value, for every D). `SALUKI_MEASURE_PER_DATAGRAM_LIST`
  replaces the default `1,50`. Senders wait for room unless `SALUKI_MEASURE_DROP_WHEN_FULL=1`.
- Prints, per D, metrics per datagram, and path, a throughput row (with the dropped and refused shares and CPU per
  metric), then a row per probe: `| label | probe | share of the window | records | mean | p99 |`:
  - `dogstatsd_receiver_recv_wait/dsd_in`: every decoder's wait for a datagram from the queue they share, summed over
    decoders; over D, one decoder's idle fraction. Logs from before O4 also have `dogstatsd_receiver_lock_wait`, the
    receiver mutex that O4 removed, and their `recv_wait` was the mutex holder's wait only.
  - `admission_lock_wait` and `admission_lock_hold` on `dsd_in.metrics` (plan path only; legacy has no admission
    point): the hold share is the utilization of the serial point, which caps the plan's curve near 1.0; the wait share
    over D is a decoder's fraction waiting for it. On producer lanes each lane is its own point, and the rows are per
    lane output.
  - `dispatch_send_wait/dsd_in.metrics`: time blocked on full downstream channels (backpressure, not a lock).
- The bench prints `thrpt` at 1, 2, 4, and 8 threads on shared resolvers, at 3,000 and 100,000 contexts, with each
  thread's front cache (O5) and `without_front_cache`. If throughput stops growing with threads below the rates the top
  W needs, the resolvers cap the source. Where both scaling curves flatten at the same W as these limits, the ceiling is
  upstream of the metrics path and gate items 1 to 3 are decided in process (plan 8.5).

### M3: encode and compress cost per metric

```sh
cargo bench -p saluki-components --bench encode_host                    # 1M contexts per iteration, ~3 min
```

Prints `encode_host/{native_datadog_metrics,metrics_encode_host}/{series,series_v3,sketches}/1000000_metrics` and
`.../per_buffer/16384_metrics`, with `thrpt` in metrics per **CPU** second: the measure is process CPU time per
iteration, since the encoder's 2 s flush timeout makes wall time mostly idle (criterion warns that it cannot finish
its samples in time: expected). `1e9 / thrpt` is the CPU cost per metric of the front task, the builder, and zstd at
level 3. One builder runs on one core, so `thrpt` is the most one output can encode per second: if the gate's output
rates exceed it (passthrough-heavy saturation, or 1M contexts flushed within a tick), O6 lands. `per_buffer`'s `1e9 /
thrpt` less `series`' is the front task's cost per buffer. The stage 7 gate holds `metrics_encode_host` within 3% per
metric of `native_datadog_metrics`. `SALUKI_BENCH_ENCODE_CONTEXTS` sets the contexts per iteration.

### M4: the state budget under churn

```sh
cargo bench -p saluki-core --bench state_budget                  # ~3 min
cargo bench -p saluki-metric-kernels --bench aggregate_churn
```

- `state_budget` prints `state_budget/<shape>/<P>` for P in 1, 2, 4, 8, and 16, with `thrpt` in contexts admitted per
  second over all threads (one million re-admitted per iteration, as after a flush that expired them). The shapes are
  `acquire_only` and `with_inserts`, which take a node unit and a global unit per context, `inserts_only` without the
  budget, and the `reserved_*` shapes, which take units through O1's per-batch reservations, cold and warm, in batches
  of the routed buffer sizes (1,024, 512, and 256; 64 as well). The budget's own cost per context per lane is
  `P × 1e9 / thrpt` ns.
- The share that decides O1 (budget operations above 5% of a state lane's time per new context) is measured on
  aggregation itself: `aggregate_churn` prints `aggregate_churn/{exclusive,per_context,reserved}/<P>`, and the
  budgets' share per new context per lane is `per_context / exclusive - 1` and `reserved / exclusive - 1` of the times
  at one P. `state_budget`'s stand-in insert costs a tenth of aggregation's, so `1 - thrpt(with_inserts) /
  thrpt(inserts_only)` overstates the share. Aggregation's admission cost per event is the `aggregation_isolation_slot`
  group of the `region_driver` bench.

### M5: key skew and the automatic P

```sh
M 'key_skew'                                        # 3,000, 100,000, and 1M contexts; under a minute
SALUKI_MEASURE_CONTEXTS_LIST=3000,30000 M 'key_skew'
```

Prints `| contexts | lanes | contexts per group max/mean | contexts per lane max/mean (min/mean) | power-law events per
lane max/mean (min/mean) | hottest context's share |` for P in 2, 4, 8, and 16, with contexts shaped like the SMP
lading generator's, resolved through the source's own decode path, keys as the source computes them, `group = key >>
56`, and groups in contiguous ranges over P. Contexts per lane near 1 means key groups balance contexts. Power-law
events max/mean of at least `hottest share × P` is one hot key (splitting it is a non-goal); what exceeds that is what
group movement (stage 10) could fix.

The other half of M5, worker time per metric at equal throughput per P, which decides the automatic P, is the
in-process curves at W = 8 swept over the state lanes, for the mixed workload and for an admission-heavy one:

```sh
for P in 1 2 4 8; do SALUKI_MEASURE_WORKERS=8 SALUKI_MEASURE_LANES=6,$P,6 M 'source_feed::scaling_curves'; done
for P in 1 2 4 8; do SALUKI_MEASURE_WORKERS=8 SALUKI_MEASURE_LANES=6,$P,6 SALUKI_MEASURE_WORKLOAD=distinct \
  SALUKI_MEASURE_CONTEXTS=100000 M 'source_feed::scaling_curves'; done
```

Read CPU per metric against throughput: a P that adds no throughput and costs CPU per metric does not pay.

### M6: per-buffer fixed cost, allocator contention, and buffer fill

```sh
SALUKI_MEASURE_METRICS=20480000 M 'measurement_tests::per_buffer_fixed_cost'                 # ~1 min; run it twice
SALUKI_MEASURE_METRICS=20480000 SALUKI_MEASURE_PLAN_LANES=1,2,1 M 'measurement_tests::per_buffer_fixed_cost'
SALUKI_CPU_TIME_SAMPLING=1 SALUKI_MEASURE_METRICS=20480000 SALUKI_MEASURE_PLAN_LANES=1,2,1 \
  M 'measurement_tests::per_buffer_fixed_cost'                                               # every CPU-time read
M 'measurement_tests::allocator_group_contention'                                           # seconds to a minute
SALUKI_MEASURE_LANES=auto M 'source_feed::source_buffer_fill'                               # ~1 min
```

- `per_buffer_fixed_cost` runs the scripted feed on four workers. The test's default of 2,048,000 metrics per phase
  finishes in seconds and its fixed cost moves by microseconds between runs: use ten times as many, and run each
  configuration twice. Without `SALUKI_MEASURE_PLAN_LANES` the plan runs without lanes; with
  `<producer>,<state>,<post>` the feed reaches producer lane 0, whose keyed exit routes to the state lanes. The rate of
  CPU-time sampling is the one ADP sets at startup: `SALUKI_CPU_TIME_SAMPLING` if set, otherwise 1 in 16 on Linux with
  lanes and every read without them or on macOS. The first line prints the lanes and the rate.
- It prints, per path, the throughput and the task polls and worker parks per buffer at 1,024 and 128 events per
  buffer, then `| Path | Measure | per metric at 1,024 | per metric at 128 | fixed per buffer | per event |` from `c(B)
  = a + f / B` for each measure: `f` is what a buffer costs apart from its events, `a` the cost per event.
  - `CPU ns`, split into `worker busy ns (inside tasks)` and `CPU ns outside tasks (parking, waking, searching)`. On
    Linux CPU includes the CPU-time reads of every task poll (F7); on macOS those cost nothing, so Linux less macOS is
    the reads' share. The cost outside tasks depends on how many workers idle beside the tasks that can run, so read
    `f` with the throughput it was measured at.
  - `allocations` and `allocated bytes`; `task polls` and `worker parks` (primary runtime); `allocations in group …`
    for the ten groups that allocate most per buffer (`root` is mostly the test's own feed); a last row with the
    allocator counter writes per buffer (four per allocation and its free) and at most two CPU-time reads per poll.
  - N for CPU-time sampling is the rate whose `f` comes close to the macOS figure at the gate host's cost of a read.
- `allocator_group_contention` prints `| threads | one group (ns per allocation and free) | a group per thread |
  difference |` for 1 to 16 threads allocating at once. The difference at D threads, times the allocations per buffer
  and the buffers a decoder dispatches per second, is the time per-decoder resource groups would save; they land if
  that is a material share of a decoder's time (5%, as O1's threshold, is the proposal).
- `source_buffer_fill` prints the `dispatch_items/` probe of every output (buffers, mean items, p50, p99, of B =
  1,024) saturated, at 70% of saturation, and with 500 stream connections at 500,000 metrics/s in all. On state lanes
  the keyed exits' outputs into `dsd_agg` are the routed buffers, whose fill decides B and O2 (O2: routed buffers below
  B / 2 after the hold). Fill well below 1,024 with many connections is the regime where per-buffer cost dominates.

### M7: the taps' cost

```sh
M 'source_feed::tap_cost'                           # under a minute
```

Prints, per path at saturation, `dispatch_clone/dsd_in.metrics` (its share of the window is the cores spent cloning
buffers for the taps) and `dispatch_send_wait`; on Linux also the `dsd_stats_out` resource group's CPU (a share of a
core). `dsd_in.metrics` feeds two taps, `dsd_stats_out` and `dsd_debug_log_out`; both have input gates that stay closed
while no metric statistics collection runs, so at the defaults neither should be cloned for. A clone share above zero
means a tap's gate is open.

### M8: flush storm

```sh
M 'source_feed::flush_storm'                                       # W = 8, 1M contexts at 400,000 metrics/s; ~2 min
SALUKI_MEASURE_LANES=auto M 'source_feed::flush_storm'              # P lanes ticking together
SALUKI_MEASURE_CONTEXTS=250000 SALUKI_MEASURE_RATE=200000 M 'source_feed::flush_storm'
```

Defaults: datagrams that a full socket drops, as DogStatsD clients' are (UDP on macOS; Unix datagrams sent without
waiting for room on Linux), and the `distinct` workload: seven lines in eight carry a context of their own, the eighth a
timestamped counter that keeps the passthrough flowing. At 400,000 metrics/s every one of a million contexts is sent
every 2.9 s, so each tick flushes all of them, and the 45 s window covers at least two of aggregation's 15 s flush
intervals. Startup takes a few seconds (the payloads of a million contexts).

Prints a timeline per path, one row per 100 ms: `| t | sent | refused | dispatched | missing so far | aggregation out
| aggregation send wait (ms) | sentinel p99 (µs) |`, then the totals (metrics sent, refused by the sockets, dropped in
all once the source has dispatched what its queues held). A tick is the row or rows where `aggregation out` jumps: one
metric per context for each 10 s bucket the tick closes, so about one or two million. Read around it:

- `refused` is what the sockets dropped, and `missing so far` (sent less dispatched since the start) is the drops plus
  what the queues hold: a step up at the tick that does not come back down is datagrams lost. On UDP the kernel drops
  at the source's socket without telling the sender, so this column is the one that shows it.
- `aggregation send wait` is the flush's time blocked on its output channels, summed over the state lanes (divide by P
  for one lane's); the rest of the flush is drain time.
- `sentinel p99` shows the stall the tick causes upstream.

O8 (stagger) lands if datagrams drop at the tick; O6 if the time blocked on output dominates each lane's flush.
`SALUKI_MEASURE_DROP_WHEN_FULL=0` makes the senders wait instead, which shows backpressure as a fall in `sent` rather
than as drops.

### M9: RSS and the firm bound

M9 has no in-process harness. The RSS half is the SMP quality gate `quality_gates_rss_dsd_ultraheavy` (about 250,000
contexts, four cores, its bound in `test/smp/regression/adp/experiments.yaml`), which SMP runs on ADP's image: run it
with the defaults, and with lanes off (`experimental.programmable_metrics.lanes.mode: off`) for one lane, and compare.
The firm bound at W = 4 to 64 is the unit test
`topology::lane_tests::the_firm_bound_grows_by_each_lanes_own_bounds_up_to_64_workers`, which asserts what each lane
adds; plan 5.12 records the figures. The container curves report each run's peak memory beside them (`memory.peak`,
which includes page cache).

### M10: `SourceClock` contention

```sh
SALUKI_MEASURE_LANES=auto SALUKI_MEASURE_DECODER_COUNTS=2,8,16 SALUKI_MEASURE_PER_DATAGRAM_LIST=50 \
  M 'source_feed::decoder_pool_waits'                                     # W = 4, 10, 18
```

On lanes each producer lane stamps its own admission point from one source clock. M2's rows then include
`source_clock_lock_wait/…` and `source_clock_lock_hold/…`, summed over the lanes: a wait share near zero and a mean
wait of tens of nanoseconds mean the lanes do not contend, and per-lane clocks with a shared epoch are not needed. A
stamp takes the clock's lock once, so the probes cover all of it.

### M11: per-lane memo hit rates

```sh
M 'source_feed::memo_hits_on_producer_lanes'                     # W = 8, 10,000 context indexes, default sizes
SALUKI_MEASURE_CONTEXTS=100000 SALUKI_MEASURE_MAPPER_CACHE_SIZE=100000 \
  SALUKI_MEASURE_TAG_FILTER_CACHE_CAPACITY=300000 M 'source_feed::memo_hits_on_producer_lanes'
```

Runs the plan path only, saturated over the platform's datagrams, without lanes and on `SALUKI_MEASURE_LANES` (`auto`
by default), and prints `| W | lanes | cache | hits | misses | hit rate |` for the mapper's result cache and the tag
filterlist's context memo. Datagrams reach the decoders in any order, so every producer lane sees every context: if the
rates on lanes match the one without lanes, per-lane memos at the configured size hold the working set and are kept.
`SALUKI_MEASURE_MAPPER_CACHE_SIZE` sets `dogstatsd_mapper_cache_size`, and `SALUKI_MEASURE_TAG_FILTER_CACHE_CAPACITY`
`aggregator_tag_filter_cache_capacity`.

## Scaling curves in process

```sh
SALUKI_MEASURE_LANES=auto M 'source_feed::scaling_curves'                                     # W = 1 to 16, ~25 min
SALUKI_MEASURE_LANES=auto SALUKI_MEASURE_CONFIG=defaults M 'source_feed::scaling_curves'
SALUKI_MEASURE_LANES=auto SALUKI_MEASURE_CONFIG=gateways M 'source_feed::scaling_curves'
SALUKI_MEASURE_LANES=off M 'source_feed::scaling_curves'                                      # without lanes
```

On macOS add `SALUKI_MEASURE_TRANSPORT=udp` to go through the datagram decoders (`uds` is stream connections there),
and `SALUKI_MEASURE_WORKERS=1,2,4,8` to stay within the performance and efficiency cores' meaning. Each run prints its
configuration, then per W the decoders, generators, and plan lanes, and a table `| Phase | Measure | Legacy | Plan |
ratio |` for three phases (saturated; paced at half of legacy's saturation; paced with a metric filterlist change every
50 ms): throughput, encoded, process CPU per metric, allocated bytes per metric, live MiB at the end, primary busy per
metric, the dropped and refused shares, and the paced phases' p50, p99, worst latency, and sentinels lost. Then per path
`metrics/s per worker` and `T(W) / T(W_first)`.

Read against gate items 1 to 3: plan throughput at least 0.93 of legacy's; the plan's `T(W) / T(1)` at least 0.93 of
legacy's; process CPU per metric at most 1.07 of legacy's; paced p99 at most 1.10 of legacy's, the phase with live
changes included. The paced phases' throughput must match the pace on both paths; if it does not, the pacing, not the
topology, is what was measured. The live-change p99 at one worker is bimodal from round to round: use five rounds or
more. The test asserts nothing; it reports.

Increment 9.2's post-aggregation throughput, without DogStatsD (OTLP and the checks source fed to the post-aggregation
slot):

```sh
SALUKI_MEASURE_WORKERS=1,2,4,8 M 'post_aggregation::'
```

Its knobs are `SALUKI_MEASURE_ROUNDS` (3), `SALUKI_MEASURE_POST_LANES` (`min(W, 8)`), and `SALUKI_MEASURE_BATCHES`
(4,000 per phase).

## Stage 7 gate measurements

The five tests at the top of `measurement_tests.rs` compare plan and legacy on the scripted feed (no DogStatsD source),
on one and four workers, with the mapper profile and with `GATEWAYS`, and legacy against itself on four workers. The
plan runs without lanes (the test base configuration's `lanes.mode: off`). CPU is the process's, and the encoders are
the real ones on an eight-thread global pool.

```sh
M 'measurement_tests::the_legacy_topology_against_itself_stays_within_the_tolerances'   # ~10 min
M 'measurement_tests::(the_plan_driven|with_gateways)'                                     # four tests, ~40 min
```

Each prints `| Phase | Measure | Legacy | Plan | ratio |` over four phases (saturated, paced, paced with live updates,
paced passthrough), medians of five rounds, and fails if the plan's saturated throughput ratio is below 0.93, a CPU or
allocated-bytes ratio above 1.07, or a paced p99 ratio above 1.10. Run the A/A test first on a new host: if it fails on
a quiet machine, the tolerances need widening for that host (record its ratios), not the plan. `primary busy
ns/metric` is the older measure, reported for continuity.

## Scaling curves in containers

`test/scaling/` runs legacy and plan as two images side by side, since the legacy path exists only under `cfg(test)` on
this branch. [Its README](../../test/scaling/README.md) is the reference for the containers, the variables, and the
gate's configurations; this section is the procedure.

```sh
make scaling-images                     # legacy from SCALING_LEGACY_BASE (b940bf1f8b), plan from this tree
SCALING_AA=1 make scaling-curves        # first, per host: legacy against legacy (~1.5 h at the defaults)
make scaling-curves                     # legacy against plan, the defaults profile
SCALING_PROFILE=mapper make scaling-curves
SCALING_PROFILE=gateways make scaling-curves
SCALING_WORKERS="1 2 4" SCALING_ROUNDS=3 SCALING_WINDOW_SECONDS=30 make scaling-curves   # a shorter series
SCALING_DRY_RUN=1 test/scaling/run-curves.sh                                             # check a series without Docker
```

For each N in `SCALING_WORKERS`, ADP runs on cores `0` to `N - 1` with N workers and `max(2, N - 2)` decoders, lading's
DogStatsD generator on the next two cores, and lading's blackhole on the two after, for `SCALING_ROUNDS` rounds per
image, alternating which goes first. `make scaling-images` builds the legacy image in a temporary work tree under
`target/` (the merge base of this branch and `main`); `make -C <dir> build-adp-image-release CI_COMMIT_SHA=b940bf1f8b`
on a `git archive` of that commit builds the same image, tagged `saluki-images/agent-data-plane:testing-release`, to
tag again as `SCALING_LEGACY_IMAGE`.

The gate's configurations are one series each (README, "The gate's configurations"): profiles `defaults`, `mapper`,
and `gateways`; `SCALING_CONTEXTS=100000` and `1000000`; histogram-heavy `SCALING_METRIC_WEIGHTS=20:10:10:0:200`;
passthrough-heavy `SCALING_METRIC_WEIGHTS=200:100:0:0:0 SCALING_TIMESTAMP_PROBABILITY=0.9`; 70% of saturation with
`SCALING_RATE`, computed from an earlier saturated series; about 500 stream connections with
`SCALING_TRANSPORT=unix_stream SCALING_GENERATORS=1 SCALING_CONNECTIONS=500`; one stream connection (gate item 4) with
`SCALING_CONNECTIONS=1`; and one decoder with `lanes.mode: auto` with `SCALING_DECODERS=1` (`auto` is the plan
image's default, so no `SCALING_ADP_CONFIG` is needed; other `lanes.*` keys go in a copy of `adp/<profile>.yaml` that
`SCALING_ADP_CONFIG` names).

**CPU-time sampling.** `SCALING_PLAN_CPU_TIME_SAMPLING` defaults to 1, every read, so that the plan image is measured
at legacy's rate; the plan's shipped default on lanes on Linux is 1 in 16. Run the gate's series with
`SCALING_PLAN_CPU_TIME_SAMPLING=16` as well, so that the defaults as built are what the gate decides. (An empty value
falls back to 1.)

**The context limit.** lading's contexts multiply by metric kind and tag set, so an `aggregate_context_limit` near
`SCALING_CONTEXTS` binds within seconds and both images drop at aggregation alike (at 3,000 contexts, a limit of 3,400
dropped about a sixth of the metrics). `run-curves.sh` sets `SCALING_CONTEXT_LIMIT` to ten times the contexts plus
1,000 by default. If you lower it, check ADP's telemetry during a run (`curl -s 127.0.0.1:15100/metrics`) for
`aggregate_active_contexts` reaching the limit or `component_events_dropped_total` growing for `dsd_agg`.

**Reading the output.** A series writes `test/scaling/results/<time>/` (ignored by git; `SCALING_OUT` moves it):
`runs.csv`, `summary.md`, each N's rendered `lading-N.yaml`, and each run's ADP log, and prints `summary.md`. That has
the series' configuration, then per N and image the medians of throughput (decoded metrics/s), throughput per CPU, CPU
ns per metric (the container's cgroup), lading's bytes per decoded metric, peak memory, and the throughput spread from
p25 to p75 against the median; then the plan's throughput, scaling, and CPU ratios to legacy.

- The A/A series' spread, per N, is the noise this host adds: the plan's spread must fit inside it, and a ratio within
  the spread of 1 is not a difference.
- lading's bytes per metric must stay flat over N; if it grows, the generator set the rate: raise
  `SCALING_GENERATORS` or give lading more cores.
- Report absolute throughput per CPU beside the ratios: two flat curves pass items 1 and 2 and say nothing about lanes.
  If both flatten below the top N, M2 in process shows whether the datagram receiver or the resolvers cap them.
- What stays in process: the paced p99 of item 3 and the live filterlist change (containers have no markers to time
  and no configuration stream), and the mapper profile's tag filterlist (lading's tag keys are random).

## Linux on a Mac: the Docker VM

Docker on a Mac runs a Linux VM (here `dory`: 6 vCPUs, 32 GB, arm64, kernel 6.12, cgroup v2) whose vCPUs float over
the performance and efficiency cores. It is enough to check the harnesses end to end on Linux and to measure the
Linux-only costs; it cannot decide the scaling gate.

- **Mounts.** Only the home directory is shared with the VM. A bind mount from elsewhere, such as `/private/tmp`,
  mounts as an empty directory (lading then fails with `NoConfigFiles`). Keep the tree, and any `SCALING_OUT`, under
  `/Users/...`; the default `test/scaling/results/` is.
- **The host check.** `run-curves.sh` reads the CPU count of the machine that runs it (`getconf`, the Mac's 18), not
  the 6 of the VM, so its check passes where the CPU sets cannot be disjoint. Only `SCALING_WORKERS="1 2"` fits in 6
  vCPUs (at N = 2, ADP, lading, and the blackhole fill them).
- **Context limit.** As above: before the default was raised, a smoke series in the VM dropped a sixth of the metrics at
  aggregation on both images.
- **A/A.** Run `SCALING_AA=1` there too before reading any ratio: a smoke series without one has no tolerance.
- **What it decides.** That the images, lading, the cgroup CPU reads, and the source's counters work; the cost of a
  thread CPU-time read in that VM (F7; it differs from the 850 ns F7 cites, so D12's saving and N depend on the host);
  M6's per-buffer costs on Linux, sampled and not; M8 over Unix datagrams. **Not** gate items 1 to 4: two CPU counts,
  vCPUs that are not homogeneous, and no room for lading beside ADP at higher N.

The in-process tests run on Linux from a test binary built in a container from the tree:

```sh
# A toolchain image: the Dockerfile's build-clean stage (the build dependencies and the pinned toolchain).
docker build --target build-clean -t saluki-measure-builder:latest -f docker/Dockerfile.agent-data-plane .
# The tree, copied into a volume (no target/ or .git).
docker run --rm -v saluki-measure-work:/work -v "$PWD":/host:ro saluki-measure-builder sh -c 'mkdir -p /work/src &&
  tar -C /host --exclude=./target --exclude=./.git --exclude=./node_modules -cf - . | tar -C /work/src -xf -'
# ADP's test binary, release, with the measurement feature.
docker run --rm -v saluki-measure-work:/work -v saluki-measure-target:/target \
  -v saluki-measure-cargo:/root/.cargo/registry -e CARGO_TARGET_DIR=/target -w /work/src saluki-measure-builder \
  cargo test -p agent-data-plane --release --features measurement --no-run
# One measurement: the binary's name is in the build output; knobs go in as -e.
docker run --rm -v saluki-measure-work:/work -v saluki-measure-target:/target -w /work/src/bin/agent-data-plane \
  -e SALUKI_MEASURE_METRICS=20480000 saluki-measure-builder \
  /target/release/deps/agent_data_plane-<hash> per_buffer_fixed_cost --ignored --nocapture --test-threads=1
```

The binary takes libtest's arguments: a substring filter, `--ignored` to run only the ignored tests, and one test at a
time. Copy the tree again after a change. The build takes about five minutes of compute in the VM; each image of `make
scaling-images` about three with a warm cache.

## The gate host

Gate items 1 to 4 and 6 wait for a Linux host (plan 8.5,
[Q2](programmable-metrics-stages-7-and-9.md#12-open-questions)). It needs:

- **At least 20 homogeneous cores** for N up to 16 (N for ADP, two for lading, two for the blackhole, on disjoint CPU
  sets), and 36 for N = 32. `run-curves.sh` refuses a smaller host unless `SCALING_ALLOW_SMALL_HOST=1`, which overlaps
  the CPU sets and is not for gating.
- Linux with cgroup v2, Docker, Docker Compose v2, `curl`, and `python3`; the two images; and
  `ghcr.io/datadog/lading` at the `LADING_VERSION` the `Makefile` sets.
- Rust at the version `rust-toolchain.toml` pins, for the in-process items.
- Nothing else running, under the conditions above.

What to run there, in order:

1. `make test-measurement`, then the stage 7 A/A test, then `SCALING_AA=1 make scaling-curves`: the host's tolerances.
2. The container series for every configuration of the gate at N in 1, 2, 4, 8, and 16 (and 32 where the host allows),
   five alternating rounds each, with CPU-time sampling at the shipped rate as well as at 1: items 1 to 3 (throughput,
   scaling, and CPU ratios). Report allocations per metric too, from the in-process curves: on Linux every allocation
   takes `jemalloc`'s one arena lock, so a ceiling common to both paths reads like M2's.
3. The in-process curves with `SALUKI_MEASURE_LANES=auto` over Unix datagrams (the default `uds` on Linux), the mapper,
   defaults, and gateways profiles: the paced p99 and the live-change phase of item 3, five rounds or more.
4. M1 at W = 1 to 16, in process and in containers: item 4.
5. M2 (O4 is provisional until the gate host reruns it against per-decoder queues), M6 at the automatic counts with
   the host's cost of a CPU-time read (N for sampling), the routed fill with about 500 stream connections, and M5's
   sweep over P: whether the automatic counts' caps (P at most 4, M at most 8, D at most 16) may rise.
6. M9: the SMP RSS gate with the defaults and with lanes off at four vCPUs: item 6.

The per-buffer cost of lanes is the risk to watch for item 3, in the paced phases and with about 500 stream
connections (plan 8.6, results of the increments). Whether to delete R1's cost model (plan 8.5, P9.7) needs `lanes.mode:
off` at W = 1 to 3 in the mapper and gateways profiles with and without R1's pipelining; the harness has no knob for
pipelining without lanes, so that comparison needs one first.
