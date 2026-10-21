# Programmable metric processing: V7b, one region with an interleaved flush

The design of version 2's step V7b ([version 2](programmable-metrics-v2.md#the-simplifications-in-order)), which is
[N2 and N3](programmable-metrics-next.md#n2-and-n3-one-region-from-decode-to-encode-with-an-interleaved-flush) of the
next chunk of work. It builds on V7a as built ([state owners in any
region](programmable-metrics-v2.md#v7a-state-owners-in-any-region)) and is judged by N1's gates G1 to G6
([gates](programmable-metrics-next.md#gates-for-n2-to-n4)). The owner decided its questions on 2026-10-21
([section 7](#7-owner-questions)). **Built** (2026-10-21): the interfaces step, the six parts, and the integration
([6.3](#63-integration-z)); what integration decided or found is in [6.5](#65-as-built). The N1 candidate runs
([6.4](#64-validation)) come next. Line references are to `3a871929f3`. Amendments 203 to 206 are proposed in
[section 8](#8-proposed-amendments).

## Contents

- [1. In one paragraph](#1-in-one-paragraph)
- [2. What changes](#2-what-changes)
- [3. The interleaved flush](#3-the-interleaved-flush)
- [4. The datagram pool and back-pressure](#4-the-datagram-pool-and-back-pressure)
- [5. The reference interpreter and the transition traces](#5-the-reference-interpreter-and-the-transition-traces)
- [6. Implementation plan](#6-implementation-plan)
- [7. Owner questions](#7-owner-questions)
- [8. Proposed amendments](#8-proposed-amendments)

## 1. In one paragraph

Today a DogStatsD metric crosses three tasks between the source's decoders and the encoders: one runs the filters
before aggregation, one runs aggregation, and one runs enrichment and routing, each behind a queue of its own. When
aggregation flushes, its task reads nothing until the flush is done, the queues in front of it fill, and the source
stops reading the socket. After this step everything between the source and the encoders runs as one task with one
input queue, and a flush no longer stops admission: it works through the closed time buckets a slice at a time, while
the same task keeps admitting new metrics into the open ones. When a new metric reaches a context the flush has not
reached yet, that context is flushed first, on the spot, so every flushed and every admitted value is exactly what it
would have been had the flush finished before the new metrics arrived; only the order in which records leave changes.
A large flush is spread over a few seconds so that its cost never lands at once, and a small one finishes in its first
slice. The cost model that split the path into several tasks is deleted, lanes become something you turn on, and the
datagram pool stays as it is: it was never smaller in plan mode than in native mode.

## 2. What changes

### 2.1 State steps in fused regions

V7a made the runtime indifferent to where a state step runs; the frame still isolates it. `aggregate` declares
`Boundary::Isolated` (`operators/aggregate.rs:290`), `compile::roles` makes it `Role::Isolated`, and
`Layout::compute` gives each such node a slot of its own (`compile/frame.rs:2084-2089`), counts it as a fence for depth
(`:1916`), and marks every edge that touches it `FusionVerdict::Isolation` (`:1224-1229`). V7b changes four things:

1. **The frame.** `FrameOptions { isolate_state }` replaces `pipelining` (`frame.rs:334-349`). Without it, a
   `Role::Isolated` node fuses with its synchronous neighbors like any other (the union at `:1886-1892`), only
   `Role::Fixed` counts as a fence, and no slot is `SlotKind::Isolation`. Fusion is maximal between the fences that
   remain: fixed components (the sources), I/O hosts (encode and delivery), and opaque hosts. A state owner's output
   point comes from placement's allocation per owner (`output_points`, `placement.rs:1568`), as a sub-region's does
   since V7a. With
   `isolate_state`, which ADP sets whenever lanes are on, the frame is today's.
2. **Placement.** `Regions::partition` (`compile/placement.rs:1249-1283`) runs a state owner in the main region only
   when every one of its inputs enters the slot from outside it. V7b: a state owner runs in the highest region among
   its inputs in the slot (the main region when it has none), if that region runs no state owner yet and has one lane,
   or the owner is the slot's boot owner; otherwise it opens a sub-region, as today. A region still runs at most one
   state owner (`runtime/code.rs:366-374`, `StateRegionConfig::of`, `hold_state`), which keeps the driver as it is.
3. **The runtime.** Nothing about where a state step runs: the driver already runs stateless entries beside a state
   step (`entry_runs_state`, `pass_through`) and the owner's consumers in its output entry. What changes is the drain
   ([section 3](#3-the-interleaved-flush)).
4. **ADP.** The fused slot is a state slot (`is_state_slot`, `topology.rs:1238`), with the drain-step turn budget
   (`:1230`). Its input capacity follows the stage 5 rule (`region_input_capacity`, `:805-893`): the longest path
   replaces seven native components, so 7 × 129 − 1 = 902 buffers, where today's three regions hold 515 + 128 + 257 =
   900 together (status invariant 10 holds). The context snapshot bridge addresses the slot that holds aggregation.

### 2.2 R1's cost model is deleted

R1 split a fused chain into pipelined regions to buy parallelism across tasks, which at one core or less buys nothing
and adds a hop and a queue per split ([what it means](programmable-metrics-next.md#what-it-means-for-what-we-built)).
The deletion is plan P9.7's ([5.10](programmable-metrics-stages-7-and-9.md#510-removing-r1s-cost-model)), with current
locations:

- **Frame** (`compile/frame.rs`): `REGION_COST_FLOOR` (`:137-140`), `FrameOptions::pipelining`, `Slot::cost`
  (`:635-637`), `FusionVerdict::Pipelined` (`:720-731`), `Frame::cost_budget` (`:880`, `:1334-1338`),
  `Layout::{cost, budget}` (`:1861-1865`), the cost and staging block (`:1934-2068`), the stage term of slot grouping
  (`:2073`), the lanes check's refusal of pipelining (`:1510-1516`), and the module's `# Pipelining` documentation.
- **Operators**: `OperatorDef::cost` and its default (`operators/mod.rs:207-217`), `ConfiguredOperator::cost`
  (`:755-759`), the estimates of the mapper, namespace prefix, timestamp split, and tag projection
  (`dogstatsd.rs`), host tags (`enrich.rs:105-108`), and tee (`routing.rs:226-230`), and `decision_cost` with
  `MEMO_LOOKUP_COST` (`routing.rs:103-136`).
- **Explain**: `LayoutEntry::pipelining`, `SlotEntry::cost`, the `pipelined` verdict (`explain/physical.rs:608-617`),
  and the cost lines and R1 wording of the text form (`explain/text.rs:284-288`, `:316-333`).
- **Placement and rewrites**: the one-hop pass-through limit (`passes_through`, `placement.rs:806-812`), the boot-slot
  pinning that exists so that a candidate pipelines as the boot plan does (`:899-905`), and R5a's refusal to move a
  filter across an R1 cut.
- **Kept for R4**, moved into decision compilation: the split's estimate (`Split::cost`, `evaluation_cost`,
  `leaves_cost`, `ROUTE_COST`, `INTAKE_NAME_COST`) and `MEMO_COST_FLOOR`; R4's call (`compile/program.rs:1281-1290`)
  reads it there. R7's static costs are independent.
- **ADP**: `LaneCounts::pipelining` doubles as the lanes-on flag (`programmable_metrics/lanes.rs:100-141`); it becomes
  a field of its own, and ADP passes `isolate_state` from it.
- **Tests**: the cost-model tests (`compile/tests.rs:801-1040`, `runtime/frame_tests.rs:1197-1236`, the R1 half of
  `operators/tests.rs:1803-1860`), the pipelining cases of `compile/lane_tests.rs`, the case of
  `compile/state_region_tests.rs:235` that builds two paths from R1 cuts (rebuilt with a tee), ADP's topology pins, and
  ten golden explain files. About 340 lines of code and 450 of tests go. Amendment 47 is withdrawn and 122 adopted.

### 2.3 Lanes are opt-in

`lanes.mode` defaults to `off` (amendment 157 reversed): `#[default]` moves from `Auto` to `Off` in `LanesMode`
(`lib/agent-data-plane-config/src/control.rs:201-212`), and the `SALUKI_KEYS` entry
(`lib/datadog-agent/config-overlay-model/src/saluki_keys.rs:196-247`) and the generated reference follow, by the
configuration-system workflow. At every production limit `auto` already ran without lanes (two workers, amendment
167), so production's layout changes by fusion, not by this flip, which matters only from three workers. The lane code
stays: `auto` and explicit counts lay out as today, with V7a's layout (state owners isolated, other owners in
sub-regions on one lane), blocking drains, and no new work that partitions state across lanes.

### 2.4 The default program's frame afterward

| | Today, lanes off, two workers | After V7b |
|---|---|---|
| Source `dsd_in` | a reader per socket, two decoders, the pool shrinker | unchanged |
| Slots | `dsd_pre_aggregation` (515 buffers), `dsd_agg` (128), `metrics_enrich` (257); with mapper or gateway profiles R1 adds `dsd_prefix_filter_region`, `dsd_no_agg_split_region`, or `dsd_post_agg_filter_region` | one slot and one task, `dsd_pre_aggregation` ([question 2](#7-owner-questions)), 902 buffers |
| An aggregated metric | three region tasks, two channel hops | one task, no hop |
| A timestamped metric (passthrough) | a hop from `dsd_pre_aggregation` to `metrics_enrich` | a register |
| Hosts | `dd_metrics_encode`, `mrf_metrics_encode`, `dd_out`, `mrf_dd_out` (and `af_*` with a Cluster Agent) | unchanged |
| Taps | `dsd_stats_out`, `dsd_debug_log_out`, `dsd_client_telemetry_out` | unchanged |

This is V8's trivial frame in all but name: V8 deletes the frame's native shape and makes encoders terminal kernels.

### 2.5 Sub-regions

- **Lanes off.** The default program runs none. A sub-region opens only for a slot's second state owner: an
  aggregation on the passthrough branch beside `dsd_agg`, or an owner fed by another owner. Those keep V7a's mechanism
  unchanged (a task, an input channel, internal exits, a budget credit), and their drains interleave too, since a
  sub-region runs one lane.
- **Lanes on.** V7a's layout, unchanged.
- **Not in V7b**: several owners in one region, which needs a state per owner in the driver and a queue inside the
  region for a chain's derived batches ([question 3](#7-owner-questions)).

## 3. The interleaved flush

### 3.1 What today's kernel and driver do

`AggregationState` (`saluki-metric-kernels/src/aggregate/state.rs`) holds a `HashTable<(Context, AggregatedMetric)>`.
A pass (`FlushCursor`, `:235`) visits the table's buckets by position (`flush_next`, `:598`), which stays valid only
because nothing is inserted while a flush is in progress. A flush fixes its zero-value buckets and split timestamp
when it starts (`start_flush_since`, `:488`); at each context it merges the counter zeros up to the context's expiry,
splits off the values at or before `align(F) − 1`, emits them, and queues the context for removal when it is empty and
expired, removing the queue at the end (`finish_flush`, `:720`). `AggregateKernel::consume` (`kernel.rs:362`) asserts
that no call is pending and completes one if it is. The driver accepts no input while a drain is pending
(`accepts_isolated`, `runtime/driver/state.rs:2444`), the host reads input only when nothing is pending
(`saluki-core/src/execution/region/host.rs:1772`), and a batch that starts a drain is stashed until it ends. So
admission stops for the whole flush.

### 3.2 Why open and closed buckets alone do not keep a context correct

Admission after a flush starts writes only open buckets for values without a timestamp: `F ≤ floor(W)`, and every
batch admitted later has `t_a ≥ W` in its epoch, so its bucket starts at or after `align(F)`, past the split. But a
context's result depends on more than its buckets:

1. **A kind or interval conflict** replaces every point the context holds, closed ones included (`insert`, `:345`):
   admitting first loses closed points the flush would have emitted.
2. **`last_seen`** decides a counter's expiry and how far its zeros reach, read at the visit: admitting first keeps a
   counter the flush would expire, and emits zeros past its expiry.
3. **Removal and re-creation.** A context whose buckets all closed (every gauge, set, histogram, and distribution, and
   expired counters) is removed, and the next metric creates it again with its own metadata: admitting first keeps the
   old entry and its metadata.
4. **Timestamped values**, in a program that aggregates them, can land in closed buckets.
5. **The deferred removal** would remove a queued context that admission has since given new points.
6. **New contexts** can rehash the table, moving every position, and an entry inserted ahead of the cursor would be
   visited, zeros included.
7. **Budgets.** The units of contexts the flush removes come back at its end: a batch admitted before then can be
   refused at the limit where it would have been admitted after.

### 3.3 Flush on touch

The rule: **a pass is fixed when it starts, and visits every context the table holds at that moment exactly once,
before anything admitted later touches it.**

- `AggregatedMetric` gains `pass: u32` and the state counts passes. Pass `k` visits the entries whose `pass` is below
  `k` and sets it to `k`; an entry inserted while pass `k` is pending is born with `k`, so the pass skips it.
- When admission finds an entry with `pass < k` while pass `k` is pending, it first visits it with the pass's plan
  (zeros, expiry, split, emission, removal), then applies the metric, creating the entry anew if the visit removed it.
  The body of `flush_next` for one entry becomes one function, which the cursor and admission both call.
- An interleaved pass removes a context at its visit, in place, and gives back its budget unit then; `remove` moves no
  other entry, as drains rely on today. Every other pass keeps the deferred removal queue, the native transform's
  `Flush` API (`aggregate/mod.rs:671`) among them, whose order under an abandoned flush is native's.
- The table never rehashes while a pass is pending. An insertion needs `len() < capacity()`, which for `hashbrown` is
  room without growth (capacity is items plus growth left, and only an insertion at zero growth left rehashes,
  `raw.rs:1038` in 0.17); without it, `consume` completes the pass first and then inserts.
- A new context refused while a pass is pending makes `consume` complete the pass and retry the metric.
- What a visit on touch emits is output of the pending call ([3.4](#34-the-kernel-contract)).

Touch moves work rather than adding it: one comparison per admitted metric while a pass is pending, and at most eight
bytes per context with alignment. The driver's room check ([3.7](#37-how-much-of-a-turn-drains))
keeps the two fallbacks rare; they exist so that the kernel is exact on its own.

### 3.4 The kernel contract

A refinement of [Yielding](../../lib/saluki-core/src/execution/kernel.rs) in the `Kernel` documentation:

> **Admission while advancing.** A kernel whose `admits_while_advancing` returns `true` may be given batches while its
> `advance_time` is yielded, never while a `control` or a `finish` is. Over the yielded call and those batches
> together, it produces exactly the output, losses, and state it would have had the call completed before the first
> of the batches. Its `consume` writes nothing of its own, so whatever it writes while `advance_time` is yielded is
> output of that call, which the host delivers with it.

- `Kernel::admits_while_advancing(&self) -> bool`, default `false`; `AggregateKernel` returns `true` unless it runs on
  lanes.
- Two read-only control messages, answered whatever is pending: `RoomToAdmit { contexts, fits }`, true when `contexts`
  new contexts fit without growing the table and strictly below the node limit of the turn's parameters and the shared
  budgets, so that none can be refused; and `PassProgress { visited, total }`, the contexts the pending pass has
  visited, by its cursor or on touch, and those it holds.
- A `ClockReset` and a `finish` still need the pending call completed first, as today.

### 3.5 The cases

| Case | What happens | Why it equals a flush that finished first |
|---|---|---|
| A counter admitted during its own bucket's flush | The point lands in an open bucket; an unvisited context is visited first, emitting its closed points and zeros, with expiry decided on its old `last_seen` | Per context: the flush, then the metric |
| Idle-counter zeros | Fixed at the start from `last_flush` and `F`; each context takes them at its visit, with the `last_seen` it had before any later metric | The same zero range per context |
| Context expiry | Decided at the visit; an expired, empty counter is removed, and the metric creates it with its own metadata | As natively after a flush |
| Gauges and sets | Removed at the visit when every bucket closed; a later metric creates a fresh entry | Native removes, then the metric creates |
| Histograms and distributions | Expanded and emitted at the visit, from closed buckets only; samples for open buckets merge afterward, in arrival order | Merge order, and so every floating-point sum, unchanged |
| A kind or interval conflict | Closed points leave first; the replacement replaces only open points, counted as policy loss | Native's flush emitted them first |
| Timestamped values in closed buckets | Admitted after the visit; emitted at the next flush | As after a native flush |
| A context created during the pass | Born with the pass's number; never visited by it | It did not exist at the flush |
| The context limit | No refusal while a pass is pending; the end of the pass clears the breach flag on a state below the limit whenever native's is ([3.11](#311-invariants-and-why-the-output-is-the-same)) | Same admissions and the same two log lines |
| R5b's emission filter | Touch visits apply the pass's parameters, filter included; their suppressed counts go with the call that delivers the records | Same records skipped and counted |

### 3.6 What still blocks

- **Reset drains, final retirements, and shutdown** emit or discard everything held, open buckets included, so nothing
  may enter the instance meanwhile. They are rare (a clock jump past 300 s, a cut's crossing, the end of input) and
  run as today.
- **Instances on lanes**, whose flushes resolve in the lanes' shared record. Interleaving them would be new lane work.
- **A batch that must wait**: one for which the room check fails, or one that reaches an instance whose queued pass
  has not started.
- **A pass behind its pace**, which runs before input until it completes ([3.7](#37-how-much-of-a-turn-drains)).

### 3.7 How much of a turn drains

In the driver (`runtime/driver/state.rs`), a **pass** is a drain entry of kind `Tick` or `Boundary` on an instance not
on lanes, while interleaving is on (`RegionConfig::with_interleaving`, on by default, off in tests that compare both
ways).

- **Admission.** While the drain's current entry is a pass, the region accepts input, and `process` admits a batch at
  once unless its header starts a blocking drain, the drain queue holds an entry not yet started for an instance the
  batch reaches, or `RoomToAdmit` fails for an instance with a pending pass. `RoomToAdmit` is asked with the batch's
  length times the number of paths from the batch's entry to the state step, which `code.rs` counts when it translates
  the entry. A batch that cannot enter is stashed and its `process` yields, as today, so the pass completes first.
- **The pace.** When a pass starts, the driver records `t₀`, the contexts it holds `N` (`PassProgress`), and `D`, the
  flush interval of its stamping generation times `FLUSH_SPREAD`, a third by default
  ([question 1](#7-owner-questions)). The pace is the faster of two: the whole pass within `D`, and one step per
  `FLUSH_STEP_PERIOD` (100 ms). The pass is **behind** while it has visited fewer contexts than that pace has reached.
- **Behind.** A pass is behind once it misses its deadline by a whole step: it has visited fewer contexts than the
  pace had reached a step earlier, which a pass whose steps run at their deadlines never has. A starved region
  prefers flushing to admitting more ([question 1](#7-owner-questions)): **once behind, a pass runs ahead of
  admission until it completes**, not only until it catches up. The region accepts no input (`accepts_input` is
  false), `next_deadline` is now, and the host's deadline branch, ahead of input in its biased `select!`, runs one step
  after another under the drain-step budget (4,096 contexts or 1,024 records), as a blocking drain does today. So under
  starvation flushes never back up behind admitted data.
- **One pass at a time.** A new pass never starts while one is pending: the instance's next tick waits for its pass to
  complete ([3.9](#39-watermark-and-ticks-while-a-pass-is-pending)), and the tick or boundary flush of another
  instance waits in the drain queue, as today.
- **On pace**: `next_deadline` is when it would fall a step behind, and the driver's new `has_background_work` is
  true, so the host runs a step whenever no batch waits: a new last branch of the same `select!`, through
  `consume_budget` like the branch that resumes a yielded call, so that other tasks run between steps.
- **Small passes.** A pass runs its first step when it starts, and the step period bounds a small one: a flush at
  3,000 contexts, three steps of 1,024 records, completes within about 300 ms even when input never runs out, and at
  once when the region has a moment idle.
- **Under a CPU quota.** `D` is wall time, so a flush completes within `D` at any quota, and when input never runs out
  it takes its cost spread over `D`: at 100,000 contexts, about 21 ms of drain plus the region's work on its output,
  over five seconds instead of at once. When a quota cannot fit the pace, the pass falls behind and then runs first
  until it completes, which is today's behavior from that point on. The pace never reads the quota.
- **Touch counts.** Visits on touch are progress, so a burst over the pass's contexts does much of the flush through
  admission, and the cursor skips what it did.

### 3.8 Memory in flight during a long pass

- No queue fills because of a flush: the region keeps reading. Today the queues in front of aggregation (643 buffers
  of up to 1,024 events) fill during one.
- The kernel copies nothing: unvisited contexts keep their closed values where they are, and the table does not grow
  while a pass is pending. Entries that are emptied or expired but not yet visited stay until their visit: at most the
  table's size at the start, already allocated.
- Output in flight is one step's, `DRAIN_STEP_EVENTS + MAX_HISTOGRAM_SERIES` records as today, plus what touch writes
  during one batch: at most the batch's events that reach the owner times `MAX_HISTOGRAM_SERIES`, about six records
  per context with the default histogram aggregates, delivered when the batch's run ends. Both go to bounded exits,
  and the second bound joins the region's firm bounds.

### 3.9 Watermark and ticks while a pass is pending

- A pass's flush time `F`, stamp, generation, and parameters are fixed when it starts (`decide_step`), as for any
  drain: one flush, one generation.
- Headers admitted during a pass advance the tracker as usual, and the watermark moves past `F` without touching it.
- The instance ticks no more until its pass completes; its next deadline is set then (`complete_instance`), and one
  already passed fires at once, as today. `FLUSH_SPREAD` below one keeps the pace within the interval.
- Ticks of other instances, and the gated ticks that admitted headers release, join the drain queue, as today.
- A heartbeat during a pass is progress as usual.
- After a step, the derived point's progress is forwarded as today (`forward_drain_progress`). What touch wrote counts
  as the step's output for that rule, so the step keeps its own record of the exits it wrote, apart from the batch's
  (`scratch.written` is the batch's today).
- A header of a new epoch (a clock reset) during a pass is a blocking drain: its batch is stashed, the pass completes,
  the reset drains run, then the batch, since the kernel completes a yielded call before a `ClockReset`.

### 3.10 Cuts that arrive while a pass is pending

- Control messages are served between steps as today: `Prepare` adds programs and bindings, `Abort` marks instances
  removed, and `Release` never frees an instance a cursor references.
- Batches of a cut's new generation may now enter during a pass, into a retained instance (with that generation's
  admission parameters) or into a cut's new lineage. The pass keeps the generation it was stamped with at its start.
- The crossing: the header that shows every upstream point switched is stashed with its batch, the retiring instance's
  pass completes, its final retirement runs, and then the batch enters, so at the crossing both lineages are as they
  are today. A boundary flush, of an owner kept while one downstream is cut, is a pass and interleaves.
- Arming and cancellation lock and read admission points, never regions: unchanged. N2's item 3 (cuts keyed by node)
  is V7a's, and is re-verified here by the transition traces
  ([section 5](#5-the-reference-interpreter-and-the-transition-traces)).

### 3.11 Invariants, and why the output is the same

1. A pass is fixed when it starts: `F`, stamp, generation, parameters, zero buckets, split, and the contexts it visits.
2. It visits each of those contexts exactly once, before anything admitted after its start touches it.
3. It never visits a context created after its start.
4. While it is pending the table never rehashes, and every removal is in place.
5. No admission is refused while it is pending: a batch that could be refused, or that needs the table to grow, waits.
6. Only tick and boundary flushes interleave, and only on instances not on lanes.

By 1 to 3, every context sees the flush at `F` and then the admitted metrics in arrival order, which is what a flush
that finished first gives it. Contexts interact in aggregation only through budgets, table positions, and the end of
the pass. Positions are 4. Budgets are 5: native, after its flush, holds no more contexts than the interleaved state
does, so whatever the interleaved state admits native admits, and nothing is refused. The end of a pass clears the
breach flag when the state is below the limit, judged without the contexts created during the pass: what the state
holds then is what native's holds after its flush. So delivered points, loss by class, budget use, the breach flag and
its two log lines, and `last_flush` are the same, even after the kernel's own fallback ([3.3](#33-flush-on-touch)),
reached only when the driver did not check: exactness does not depend on the room check, which only keeps the
fallbacks rare.

What may differ: the order in which records leave, and with it how encoders cut payloads; when they leave in wall time,
up to `D` later under saturation; the table's positions, which order the next flush; and context gauges between steps,
as during any yielded drain today. The equivalence oracle (`equivalence.rs`) compares multisets of points per output,
order unobserved.

## 4. The datagram pool and back-pressure

**The premise does not hold in the code.** Both modes build the same elastic pool, `dsd_packet_bufs`, from 128 to
32,768 buffers of 8 KiB (`dogstatsd_buffer_count` and `dogstatsd_buffer_count_max`; `build_io_buffer_pool`,
`saluki-components/src/sources/dogstatsd/mod.rs:1261-1273`), and ADP's plan path configures it no differently
(`cli/run.rs:1193-1197`). Plan's pool could grow as native's does; in N1 it grew to 128 to 2,700 buffers against
native's 1,080 to 28,000 because the pipeline behind it pushed back differently. Two candidate reasons, neither
confirmed:

- The plan image's source has stage 9's lock-free datagram queue (`datagram_queue.rs`), whose decoders never wait for
  one another; the native image is `main` at the merge base, whose decoders take datagrams one at a time behind a
  mutex. Plan's decoders keep up and turn datagrams into decoded events, which costs decode CPU during the crunch,
  while native's fall behind and the reader parks raw datagrams in the pool, cheap in CPU and costly in memory.
- Plan's regions hold native's total of decoded-event buffers in front of aggregation (515 + 128), which absorb a burst
  as decoded events before the decoders block.

**What V7b changes.** The flush no longer stops the region's input, and its cost is spread over `D`, so the queue in
front of the region fills only when the whole path is behind, never because of a flush. This is aimed at G6: in the
overloaded burst every cycle is spoken for, so whatever a flush costs, its drain and the encoding of its output, comes
out of admission in the window it lands in. Today it lands in the 2 s after the flush that G6 reads; spread over a
third of a 15 s interval, at most two fifths of it does.

**Recommendation: do not change the pool in V7b.** Interleaving and the pace aim at G6; G5 is the uncertain gate.
The shortfall is set by throughput over the burst plus what is parked: V7b raises throughput a little (two hops fewer
per metric, part of the flush moved past the burst) and parks no more. At 0.3 CPUs native dropped less (0.25 against
0.31) while its pool held about 6,600 more buffers than plan's, 51 MiB. Making the pool absorb sooner, by blocking the
decoders earlier, costs 8 KiB per datagram at any cardinality, and G4 allows the candidate 1.10 times plan's peak at
0.3 CPUs: 40 MiB more at 100,000 contexts (396 MiB), but 4.4 MiB, about 550 buffers, at 3,000 (44 MiB),
where native parked 3,118 in the same burst. So a pool that parks as native's does fails G4 at low cardinality. Read
the candidate's pool and peak memory columns; if G5 misses, the decision of where a burst waits (raw datagrams or
decoded events, set by the region's input capacity) is N4's datagram pool item, with G4 as its bound
([question 4](#7-owner-questions)).

## 5. The reference interpreter and the transition traces

The interpreter runs a flush whole at the trace event that makes it due (`interpret/drains.rs`), and a trace's order
is arrival order. Interleaving changes neither: by [3.11](#311-invariants-and-why-the-output-is-the-same) the runtime's
output equals what it gives with the pass finished before the batches that arrived during it, which is the trace's
order. So the interpreter needs no model of interleaving, and still defines the semantics; the proof obligation moves
to tests that make interleaving maximal:

- **The kernel, directly.** `property_test_an_interleaved_flush_is_a_flush_before_its_batches`: random contexts of
  every kind, with and without timestamps, counter expiry times, kind conflicts, repeated and new contexts, a shared
  budget near its limit, and random step budgets down to one context; batches admitted between steps against the same
  pass finished first. Equal output multisets, losses by class, budget use, held state (`held`, snapshot, `last_flush`,
  breach flag), and suppressed counts. Unit tests cover each row of [3.5](#35-the-cases) and both fallbacks.
- **The runtime against the interpreter.** The runtime harness gains drain budgets of a few contexts and arrival of
  batches during passes, and `property_test_interleaved_drains_match_the_interpreter` checks it with `compare`,
  exactly, over the generators the equivalence tests use. The same traces run with interleaving off and must agree.
- **Transition traces 1 to 15**, on the fused layout, with a drain budget small enough that every flush spans
  batches, and **trace 16**: a pass pending when a cut's crossing arrives, then when a clock reset does, then a boundary
  flush that interleaves.
- **Lanes**: unchanged, and so are their oracles (`compare_partitioned`), since lanes do not interleave.

## 6. Implementation plan

### 6.1 Interfaces step (I)

One commit that compiles with behavior unchanged, so that the parts can start from it:

- `saluki-core`: `Kernel::admits_while_advancing` (default `false`) and the refinement of
  [3.4](#34-the-kernel-contract) in `execution/kernel.rs`; `RegionDriver::has_background_work` (default `false`) and
  its contract in `execution/region/driver.rs`.
- `saluki-metric-kernels`: the types `RoomToAdmit` and `PassProgress`, documented; the kernel answers them trivially.
- `saluki-metric-plan`: `FrameOptions { isolate_state }` beside `pipelining`, every caller passing `true`;
  `limits::values::{FLUSH_SPREAD, FLUSH_STEP_PERIOD}`; `RegionConfig::with_interleaving`, off until integration.
- ADP: `LaneCounts` gets its own lanes-on flag.

### 6.2 Parts

Each part owns its files; no two parts edit one file.

| Part | Owns | Work |
|---|---|---|
| P1 frame and placement | `saluki-metric-plan/src/compile/{frame, placement, decision, program, mod}.rs` and the compile tests (`tests.rs`, `lane_tests.rs`, `state_region_tests.rs`, `decision/tests.rs`); `operators/{mod, dogstatsd, enrich, routing, tests}.rs`; `explain/**`; `fixtures/explain/**`; `runtime/frame_tests.rs`; R5a's rewrite | Delete R1 ([2.2](#22-r1s-cost-model-is-deleted)), `pipelining` included; `isolate_state`; the placement rule ([2.1](#21-state-steps-in-fused-regions)); `property_test_frames_fuse_maximally_between_fences` for both settings; golden files |
| P2 kernel | `saluki-metric-kernels/src/aggregate/{state, mod, kernel}.rs` and their tests | Flush on touch, pass numbers, removal at visit, the two fallbacks, the control messages; the kernel property test |
| P3 host | `saluki-core/src/execution/region/{host.rs, tests.rs}` | The background branch; host tests that a driver with background work runs it only while no batch waits, yields between steps, and that a deadline of now still runs ahead of input |
| P4 driver | `saluki-metric-plan/src/runtime/{driver.rs, driver/state.rs, code.rs, harness.rs, telemetry.rs, state_tests.rs, state_tests/**, transition_tests.rs, tests.rs}`, `limits.rs` | Passes, admission while one is pending, the room check and path counts, the pace, touch output as the step's output, the step's own written exits; `plan_lane_flush_blocked_seconds` redefined as the time a pass held admission, and a count of contexts flushed on touch; the runtime property test and trace 16 |
| P5 ADP and configuration | `bin/agent-data-plane/src/programmable_metrics/{lanes.rs, topology.rs, topology/**, installer.rs, installer/**, snapshot_bridge.rs}`, `cli/run.rs`; `lib/agent-data-plane-config/src/control.rs`, `lib/agent-data-plane-config-system/src/saluki_only.rs`, `lib/datadog-agent/config-overlay-model/src/saluki_keys.rs` and the generated reference | `lanes.mode: off` by default; `isolate_state` from the lanes-on flag; the fused slot as a state slot and its name; the snapshot bridge; topology, lane, and telemetry contract tests |
| P6 demo | `bin/programmable-metrics-demo/**` | Its uses of `pipelining`, and the topology it shows, one region |

P4 tests interleaving with a kernel of its own until P2 merges; P5's layout pins follow P1's frame. Neither waits.

### 6.3 Integration (Z)

Merge the parts; turn interleaving on and pass `isolate_state` from the lanes-on flag; regenerate golden files
(`SALUKI_METRIC_PLAN_UPDATE_EXPLAIN=1`) and review them; run the full suites once; update the documents each step
updates ([version 2](programmable-metrics-v2.md#documents-each-step-updates)): status invariants 4 and 5, changes users
can see (the components that go, the name, `lanes.mode`, explain, when aggregated points leave under saturation), the
amendments index and the [stages 7 and 9
amendments](programmable-metrics-stages-7-and-9.md#10-proposed-spec-amendments), the progress table, the log, and N2's
status in the next-steps document. Then the N1 candidate runs, and a commit for code and one for documents.

### 6.4 Validation

- **While iterating**, per part: `cargo nextest run -p saluki-metric-kernels -E 'test(/aggregate::/)'`,
  `cargo nextest run -p saluki-core -E 'test(/execution::region/)'`, and the plan crate and ADP filtered to what
  the part changed.
- **At integration**: `cargo nextest run -p saluki-metric-plan -E 'not test(/property_test_/)'`, the release property
  tests (`cargo nextest run -p saluki-metric-plan --release -E 'test(/property_test_/)'`, and the kernels crate's),
  `cargo nextest run -p agent-data-plane -E 'test(/programmable_metrics::/)'` (the lowering matrix and the
  differential tests against native included), the configuration crates, `cargo nextest run -p
  programmable-metrics-demo`, `make demo-programmable-metrics-check`, clippy, `cargo check --workspace --tests`,
  `make check-fmt`, and `vale --minAlertLevel=error` on changed documents.
- **The N1 candidate** ([method](programmable-metrics-next.md#gates-for-n2-to-n4)): build the candidate image and run,
  in the dory VM, one series per cardinality against N1's plan image, three rounds, with N1's steady rates. At 100,000
  contexts native is included, since G5 compares with it:

```sh
REGIME_PLAN_IMAGE=saluki-images/agent-data-plane:plan-n1 \
  REGIME_CANDIDATE_IMAGE=saluki-images/agent-data-plane:candidate-v7b \
  REGIME_LABELS="native-a native-b plan-a plan-b candidate-a candidate-b" \
  REGIME_CONTEXTS=100000 REGIME_CACHE="2000 Mb" \
  REGIME_STEADY_RATES="0.05:739 0.3:11840 0.6:43470 1.2:166110" test/scaling/run-regime.sh
REGIME_PLAN_IMAGE=saluki-images/agent-data-plane:plan-n1 \
  REGIME_CANDIDATE_IMAGE=saluki-images/agent-data-plane:candidate-v7b \
  REGIME_LABELS="plan-a plan-b candidate-a candidate-b" REGIME_CONTEXTS=3000 \
  REGIME_STEADY_RATES="0.05:2947 0.3:23295 0.6:66020 1.2:209900" test/scaling/run-regime.sh
```

  The exit is G1, G3, and G4 holding at both cardinalities, and G5 and G6 met at 100,000 contexts; the candidate's
  A/A distance must stay within N1's noise. The pool, peak memory, and `plan_lane_flush_blocked_seconds` columns say
  where a miss comes from. The p99 half of N2's exit stays with the in-process paced curves
  ([runbook](programmable-metrics-measurement.md)), timed across a flush.

### 6.5 As built

What integration decided, found, or left, against the text above:

- **Interleaving is on by default** (`RegionConfig::for_slot`); the frame harness and the state tests' direct region
  turn it off unless a test asks for it, so that blocking drains stay tested. ADP lays the frame out with
  `FrameOptions { isolate_state: lanes_on }`; the plan crate's `FrameOptions::default()` still isolates state, for its
  tests and golden files.
- **Accounting** (decided at integration). A state owner accounts under its own node's name wherever it runs: in its
  region's group when an isolation slot is named after it (aggregation on lanes), as a component of its own beside the
  region when it is fused with its neighbors (aggregation without lanes: resource group and bounds `dsd_agg`, so the
  sizing guide's `dsd_agg.aggregate_context_limit` resolves), and under the region in a sub-region
  (`KernelAccounting`, `RegionBounds::beside`). Only the owner's kernel draws on the budgets of state, not the stateless
  kernels fused beside it.
- **A drain split over several deliveries sends its progress once** (decided at integration). Every batch a drain
  writes carries its stamp, so records keep their flush time on the way to outputs, but a state owner downstream counts
  a derived point's time only from the header-only batch the upstream owner sends on each progress exit once the drain
  completed (`WatermarkTracker::observe_with`); a data batch that is the point's first, or the first of an epoch,
  starts its progress at the start of the timeline. So a chained owner in another region never closes a bucket the
  rest of the drain still reaches (transition trace i17), and the same gap closes for blocking drains that yield across
  tasks, which P4 noted. Resets, shutdown, and drains while another lineage retires send that header only where they
  wrote, which is exactly the progress their data used to carry; cut handovers are unchanged. A first attempt held the
  stamp on the data instead, which changed the flush time of records that a downstream region passes on to an output.
- **Found: a renamed owner placed into the main region.** Placement let a candidate's new state owner take a fused
  slot's main region once the boot owner was gone from the plan, but the region keeps the boot owner's state for as long
  as it runs, so the preparation failed. A slot whose boot plan runs a state owner now keeps its main region for that
  owner; any other opens a sub-region, as V7a placed a renamed aggregation.
- **Found: the rewrite harness's contract.** With R1's cuts gone, R5a moves filters it was kept from moving before, and
  the pipeline's release property test found an author's filter left dropping into a native drop after R3 folded the
  native `split` before it, which R5a copied. The product's rule allows it (only contract nodes move whole); the
  harness's contract now names native drops, as its structural check, which keeps every native node, assumes.
- **Telemetry.** Without lanes, the state slot's one lane publishes the state lane series (`plan_lane_flush_seconds`,
  `plan_lane_flush_blocked_seconds`, the new `plan_lane_flush_touched_contexts_total`, and the others), under
  `component_id:dsd_pre_aggregation` and `lane:0`, where ADP published them only on lanes. The Datadog Agent's
  `aggregator.processed` for DogStatsD metrics, remapped from what entered the native `dsd_agg` component, stays:
  aggregation's kernel counts the events entering its step as `dsd_agg`'s `component_events_received_total` wherever it
  runs (`Recorder::StepInput`), and leaves it to the region's input where the region is `dsd_agg`, on lanes.
- **Left as is:** the room check counts every event of a batch as a new context, so a small, full table makes every
  batch wait for the pass; production tables have room to grow. The test-only `AdmittingStandIn` stays: the traces'
  `room: Some(0)` cases use it.
- **The N1 candidate (2026-10-21): the exit is not met** ([results](programmable-metrics-next.md#v7b-candidate-results),
  `test/scaling/results/v7b-20261021/`). Per gate, candidate / plan unless stated:
  - G1 (CPU per metric) passes at 100,000 contexts (0.94 to 1.02) and fails at 3,000: 1.115 steady at 0.3 CPUs, 1.144
    steady and 1.104 burst at 0.6 (limit 1.10). It passes at 0.05 and 1.2.
  - G3 (burst throughput) passes: 0.967 and 1.003 at 0.3 CPUs, 1.012 and 0.983 at 1.2.
  - G4 (peak memory) passes: at most 1.044.
  - G5 (shortfall, 100,000 contexts) fails at 0.05 CPUs (0.67 against native's 0.60) and at 0.3 (0.375 against
    native's 0.30, above the owner's 0.31). It passes at 1.2 (0.16).
  - G6 (admission after / before the flush, 100,000 contexts) fails at 0.3 CPUs (0.44, limit 0.85; plan 0.51) and at
    1.2 (0.73, limit 0.90; plan 0.65).

  The interleaving works: `plan_lane_flush_blocked_seconds` is 0 wherever drains completed, and no pass fell back to
  blocking. Two things remain. First, the CPU that a flush's drain, encoding, and new contexts take at a full quota
  falls in the 2 s G6 reads, for native too (0.61 and 0.66). Second, the pace is a floor, not a ceiling: a pass runs a
  step whenever no batch waits, so it completes in 0.07 to 0.6 s instead of spreading over a third of the interval. At
  0.3 CPUs the candidate's datagram pool stays at 129 buffers, against 5,708 for native, so G5 is section 4's question
  of where a burst waits.

## 7. Owner questions

Decided by the owner on 2026-10-21, all as recommended:

1. `FLUSH_SPREAD` is a third of the flush interval, with a step at least every 100 ms. In addition, a starved region
   prefers flushing to admitting: once a pass is behind its pace it runs ahead of admission until it completes, and a
   new pass never starts while one is pending ([3.7](#37-how-much-of-a-turn-drains)).
2. The fused component is named `dsd_pre_aggregation`.
3. Several state owners in one region come later.
4. The datagram pool is unchanged in V7b.
5. If the candidate meets G6 but misses G5 at 0.3 CPUs, V7b is done and G5's gap moves to N4, provided the shortfall
   is no worse than 0.31.

The questions as asked:

| # | Question | Options | Recommended default |
|---|---|---|---|
| 1 | How long a flush may take under saturation, `FLUSH_SPREAD` | a third of the flush interval (5 s at 15 s); a half; or a share of the region's time instead of a deadline | **A third**, with a step at least every 100 ms, tuned by the candidate runs at 0.3 and 1.2 CPUs. It delays aggregated points by up to `D` only while input never runs out; G6 needs a large flush spread over several seconds |
| 2 | The fused component's name | `dsd_pre_aggregation` (the slot `dsd_in` feeds today); `dsd_agg` (what `slot_name`'s rule gives, `topology.rs:123-145`); a new name such as `metrics` | **`dsd_pre_aggregation`.** The aggregation node keeps reporting as `dsd_agg` under its own name (V10), and only the region components' own series go, as invariant 6 says of fused components. `dsd_agg` would report the whole region's component series (health, CPU, memory) under the `component_id` that the remapper's `aggregator.*` rules select on |
| 3 | Several state owners in one region | in V7b; or later, when a program needs it | **Later.** The default program has one, and a second owner keeps V7a's sub-region, as today |
| 4 | The datagram pool | leave it; let it park more of a burst (blocking the decoders sooner); lower its ceiling | **Leave it** in V7b ([section 4](#4-the-datagram-pool-and-back-pressure)); decide in N4 with the candidate's columns |
| 5 | If the candidate meets G6 but misses G5 at 0.3 CPUs | V7b waits for the pool decision; or V7b is done and G5's gap moves to N4 | **Done, with G5's gap in N4**, provided the shortfall is no worse than plan's in N1 (0.31) |

## 8. Proposed amendments

The next free number is 203 ([status](programmable-metrics-status.md#spec-amendments-index)).

203. **State steps fuse, and R1's cost model is removed** (FRM, D; brings forward the stage 10 candidate "removing the
     aggregator isolation fence"; adopts 122 and withdraws 47; revises 200 and status invariants 4 and 5). Without
     lanes, a state owner is not a fence: fusion is maximal between fixed components, I/O hosts, and opaque hosts,
     whatever the costs, and a state owner runs in the region of its inputs when that region runs no other, else in a
     sub-region as amendment 200 says. With lanes, the frame isolates state owners as before. The default program's
     metrics path is one region.
204. **Lanes are opt-in** (EXE, D; reverses 157; 167 stays for `auto`). `lanes.mode` defaults to `off`. `auto` and
     explicit counts keep their meaning, with state owners isolated and flushes blocking.
205. **The interleaved flush** (STA, D; refines stage 6's decision 10, drains). A tick or boundary flush of an
     instance not on lanes is fixed when it starts and runs in steps while admission continues; a context admission
     reaches before the flush did is flushed first, so output, losses, and state equal those of the flush completed
     before the batches, apart from the order in which records leave. A batch that could be refused, or that needs the
     context table to grow, waits for the flush. Resets, final retirements, and shutdown still stop admission. A flush
     is due to finish within a third of its flush interval, and to run at least a step every 100 ms; it runs whenever
     the region has no input waiting, and once behind that pace it runs before input until it completes. A new flush
     never starts while one is pending. A downstream state owner counts a derived point's time only from the header-only
     batch its owner sends once a drain completed ([6.5](#65-as-built)).
206. **Admission while advancing** (EXE, C; refines the kernel contract). A kernel that says so may receive batches
     while its `advance_time` is yielded, never while a `control` or `finish` is; what it writes then is output of the
     yielded call. A region driver may have background work, which its host runs only while no batch waits.
