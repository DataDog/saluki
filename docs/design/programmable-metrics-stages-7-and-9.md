# Programmable metric processing: stages 7 and 9

**Status:** implementation design for stages 7 (resource generations) and 9 (partitioned execution) of [the
specification](programmable-metrics.md), written against `b01049372e` on branch `luke/programmable-metrics`, after
stages 1 to 6. Nothing here is built yet. The specification stays authoritative for semantics; where this design departs
from it, [proposed spec amendments](#10-proposed-spec-amendments) 88 to 128 record what and why. [The implementation
design](programmable-metrics-implementation.md) describes stages 1 to 6 as built, and [the status
document](programmable-metrics-status.md) records the state of the branch. Code references are repository paths with
line numbers at `b01049372e`. **(est.)** marks an estimate that a measurement in [the measurement
plan](#86-measurement-harness-and-what-is-measured-first) replaces.

## Contents

1. [Purpose, owner direction, scope, and non-goals](#1-purpose-owner-direction-scope-and-non-goals)
2. [Starting point](#2-starting-point)
3. [The physical model after stages 7 and 9](#3-the-physical-model-after-stages-7-and-9)
4. [Stage 7 design](#4-stage-7-design)
5. [Stage 9 design](#5-stage-9-design)
6. [Semantics and verification](#6-semantics-and-verification)
7. [Interfaces by crate](#7-interfaces-by-crate)
8. [Implementation plan](#8-implementation-plan)
9. [Assumptions](#9-assumptions)
10. [Proposed spec amendments](#10-proposed-spec-amendments)
11. [Risks](#11-risks)
12. [Open questions](#12-open-questions)

## 1. Purpose, owner direction, scope, and non-goals

**Purpose.** Implementation agents build stages 7 and 9 from this document. It fixes the physical model both stages
share, the data structures and invariants of each, the semantics partitioning must keep and how they are checked, the
interfaces by crate, and an implementation plan: an interfaces step per stage, parts built in parallel work trees, and
an integration step with a gate.

**Owner direction (binding, October 10, 2026):**

- Stage 8 (Remote Config) is out of scope. Stages 7 and 9 are implemented right after this design.
- **Stage 7:** outputs, recipients (endpoints), and encoder and forwarder settings become changeable live. Who may add a
  destination is settled by a configuration-driven change or by declaring endpoints apart from ordinary program
  definitions. Security need not be hardened yet, provided nothing is unrecoverable. The other questions (output
  fingerprints and staleness, queued and spilled payloads of a removed recipient, credential references, scoped
  registration and release, global budgets) take the path that least surprises operators and contributors, documented so
  it can be revisited ([assumptions](#9-assumptions)). The native forwarder is **defunctionalized**: the behavior it
  captures at build time (one I/O loop per resolved endpoint, fixed routing and retry policy) becomes plan data that a
  generic host interprets, with per-recipient prepare, commit, drain, and retire. The physical frame stays fixed;
  logical outputs and recipients are multiplexed over fixed encode and delivery hosts, and compression stays as parallel
  as today.
- **Stage 9:** the specification's stage 9 list is not binding. Partitioning is designed as a whole, from the state of
  the art and from this problem's constraints, to restore the parallelism that fusing native components into regions
  removed (stages 3 to 6), while adding as little coordination and communication as possible. If first-class parallelism
  makes R1's cost model unnecessary, it goes. Gate: scale with cores at least as well as legacy, with semantics
  unchanged as the reference interpreter checks them.
- Easily changed layers (surface syntax, the demo UI) get first passes behind narrow interfaces. Foundations are
  correct, robust, and reliable. The demo shows stage 7 and 9 features: adding an endpoint live and routing a policy to
  it, and per-lane activity.

**Scope, stage 7:** prepare, commit, drain, and retire for outputs and recipients inside fixed hosts; output bindings,
recipients, and encoder and forwarder settings as plan parameters; native configuration of endpoints, routing, failover,
metrics-only encoder keys, and forwarder applied live, the native set for logs, events, and traces through resource
transactions; a `resources` product on the stub channel, with pending and degraded artifacts; scoped registration and
release; admission of everything added after boot against the memory limit; the delivery-policy slot opened minimally;
the demo's endpoint scenario.

**Scope, stage 9:** producer lanes with admission at each lane, keyed state lanes with a repartition edge that costs no
hop, post-aggregation lanes, shared per-lineage rules where native semantics couple contexts, a multi-point reset rule,
removal of R1's cost model, a measurement harness with a Linux gate, and the demo's lane view.

**Non-goals:** Remote Config and a separate resource authority with audit (stage 8); adding or removing components,
edges, or slots after boot (the frame stays fixed, amendment 5); hardened endpoint authority (signed resources,
allowlists per credential); moving key groups between lanes (stage 10's partition movement); removing the aggregator
isolation fence (stage 10); hot-key splitting; running plan code inside the DogStatsD source's decoder tasks (a measured
option, [O10](#87-conditional-parts-measured-options)); changes to upstream sources beyond handing decoded buffers to a
lane.

**Symbols used throughout:**

| Symbol | Meaning | Default |
|---|---|---|
| W | worker threads of the primary runtime (`SALUKI_WORKER_THREADS`, else `available_parallelism().max(2)`) | host |
| D | producer lanes (pre-aggregation lanes), one per DogStatsD decoder up to `PRODUCER_LANES_MAX` (16); decoder `d` feeds lane `d mod D` | `min(decoders, 16)`, decoders `dogstatsd_workers_count`, else `max(vCPUs − 2, 2)` |
| P | state lanes of `dsd_agg` | 1 if W < 4, else `clamp(W / 2, 2, 8)` |
| M | post-aggregation lanes of `metrics_enrich` | `min(D, 8)` |
| K | key groups | 256 |
| B | events per routed buffer | `clamp(2,048 / P, 256, 1,024)` |
| S | builder shards of one output's lease | 1 |

**Terms.** "Host" alone never means a machine here. A **region host** is `saluki-core`'s task that runs a slot's
program; an **encode host** and a **delivery host** are the fixed components that multiplex leases and recipients
(stage 7); **host settings** (`HostBinding`) are a delivery or encode host's shared settings; **host facts** are what
ADP resolves from the process environment at boot; **host tags** are the tags enrichment adds.

## 2. Starting point

Stages 1 to 6 are committed: the metrics path always runs from the plan, configuration and policy artifacts install live
as generations, and DogStatsD aggregation runs as a kernel in the isolation slot `dsd_agg` with admission-time
bucketing, watermarks, and cuts. See [where things stand](programmable-metrics-status.md#where-things-stand), the [code
map](programmable-metrics-status.md#code-map), the invariants listed there, and the [stage 7 and stage 9 coupling
lists](programmable-metrics-status.md#deferred-work-by-stage), which this design resolves item by item.

The facts below, checked against the code, decide the design.

| # | Fact | Where | Consequence |
|---|---|---|---|
| F1 | The topology is frozen at spawn; dispatchers, senders, consumers, health handles, and admission points are created once. | `lib/saluki-core/src/topology/blueprint.rs:1229-1233`, `built.rs:177-363` | Live outputs and lane counts must live inside fixed components. |
| F2 | Health and resource groups cannot be released; a dropped health handle reads `Dead` and fails readiness. Metric handles are evicted 3 s after the last one drops. | `lib/saluki-core/src/health/mod.rs:293-336`, `lib/saluki-common/src/resource_tracking/groups.rs:255-320` | Health per fixed host; bounded, stable resource-group names; metrics per logical resource. |
| F3 | An admission point's tokio mutex (fair) is held across the stamp and the whole sequential fan-out, waits for room included; every DogStatsD decoder shares the one point `dsd_in.metrics`. | `lib/saluki-core/src/topology/interconnect/dispatcher.rs:98-145` | The serial point stage 9 removes. |
| F4 | The publication epoch of a cut is recorded lazily, at a point's first stamp after publication. | `lib/saluki-core/src/execution/admission.rs:288-334` | With a shared clock, a late point would record a post-reset epoch; the boundary must supply it. |
| F5 | A region is one task that drains only batches already waiting, up to `WORK_PER_TURN` (8), dispatching after each. | `lib/saluki-core/src/execution/region/host.rs:456-479` | Coalescing within one turn engages only under backlog. |
| F6 | A worker inherits its parent's resource group; the tracking allocator updates the group's atomics on every allocation, and charges a deallocation to the group that allocated it. | `lib/saluki-core/src/runtime/process.rs:123-131`, `lib/saluki-common/src/resource_tracking/stats.rs:4-30`, `allocator.rs:63-92` | Lane tasks need their own groups; frees of another task's allocations still write that task's group. |
| F7 | A group entry reads the thread's CPU time twice: a real system call of about 850 ns on Linux, nothing on macOS. Entries happen on every task poll and per kernel per buffer. | `stats.rs:126-150`, `groups.rs:93-114`, `lib/saluki-metric-plan/src/runtime/driver.rs:1565-1610` | Splitting batches multiplies a Linux-only cost; the gate needs Linux; CPU-time reads must be sampled. |
| F8 | The isolation driver clones its watermark tracker on every batch. | `lib/saluki-metric-plan/src/runtime/driver/isolation.rs:311` | One allocation per buffer, growing with the point count. |
| F9 | Aggregation assigns `last_seen` on every insert; with one FIFO point that equals the maximum. | `lib/saluki-metric-kernels/src/aggregate/state.rs:298` | Interleaved lanes would move it backward and expire counters early. |
| F10 | The node context limit is a check followed by an act: `node.used() >= limit`, then `global.try_acquire(1)`, then `node.try_acquire(1)` on a node budget that is always unlimited. | `state.rs:89-101`, `lib/saluki-metric-plan/src/runtime/state/mod.rs:66` | With several lanes a node limit below the global capacity can be exceeded by up to P − 1. |
| F11 | Each flush tick reads its own wall-clock second; phases start at each instance's `start + interval`. | `lib/saluki-metric-plan/src/runtime/state/rules.rs:271-285` | Lanes waking on either side of a second would flush at `s` and `s + 1`. |
| F12 | Idle-counter back-fill starts at the core's own `last_flush`, which advances only when the core is non-empty. | `state.rs:400-440`, `lib/saluki-metric-kernels/src/aggregate/mod.rs:625-637` | Empty lanes would back-fill too far; the rule must be per lineage. |
| F13 | `Cache::get` writes a shared hit or miss counter on every lookup; memo caches of one node share one identifier. | `lib/saluki-core/src/cache/mod.rs:326-336`, `lib/saluki-metric-plan/src/operators/dogstatsd.rs:661-668` | Per-lane caches need per-lane handles. |
| F14 | `dsd_stats_out` is always connected and drops every buffer unless a statistics request is running. | `lib/saluki-components/src/destinations/dsd_stats/mod.rs:112-158`, `bin/agent-data-plane/src/cli/run.rs:1152-1162` | A per-event clone and a single-task drop that every curve shares. |
| F15 | The datagram decoders share one receiver behind a tokio mutex held across `recv().await`; the context resolver writes shared memory on every resolve. | `lib/saluki-components/src/sources/dogstatsd/mod.rs:1720-1723`, `lib/saluki-core/src/cache/expiry.rs:126-161` | Upstream ceilings common to legacy and plan, measured first. |
| F16 | Each output is one encoder component with one request builder on a fixed 8-thread pool; encoders have only a default output. | `lib/saluki-components/src/encoders/datadog/metrics/mod.rs:547-583`, `built.rs:140-159`, `graph.rs:388-393` | One encode host per delivery host; one builder per output stream; pool sized from cores. |
| F17 | The forwarder's endpoint table and loops are built once; per-payload routing is already data. A removed additional endpoint keeps receiving with its last key; an added one is ignored. | `lib/saluki-components/src/common/datadog/io.rs:385`, `:528-597`, `api_key.rs:68-72`, `:331-353` | The table becomes live data; removal must stop delivery. |
| F18 | Plan outputs are resolved once from the startup configuration, and their fingerprints include the resolved recipients. | `bin/agent-data-plane/src/programmable_metrics/lowering/mod.rs:195-215`, `outputs.rs:326-354` | Lowering from the current configuration; a selector-based fingerprint. |
| F19 | A batch header carries `entry: Option<EntryId>`, and a region's exit op writes it. | `lib/saluki-core/src/execution/header.rs:76-96`, `driver.rs:1774-1781` | A logical output rides in an existing field. |
| F20 | The forwarder stops popping queued work at close; persisted retries probably contain `DD-API-KEY`, which the request mapper overwrites at dispatch; queue directories are a stable function of (component, raw URL, key index) for additional endpoints and (component, resolved endpoint) otherwise (code reading, untested). | `io.rs:798-899`, `:901-920` | Retirement drains; persistence and directory naming stay native, pinned first by tests of today's behavior. |
| F21 | V3 acceptance is exclusive per endpoint: an endpoint with V3 series takes no V2 series, and the reverse. | `lib/saluki-components/src/common/datadog/endpoints.rs:90-119`, `io.rs:802` | Payloads must name the recipients of their own protocol variant. |
| F22 | The region host's control mailbox can be served only inside `execution::region` (`into_parts` and the request fields are `pub(super)`), and only the region host sets the running flag. | `lib/saluki-core/src/execution/region/mailbox.rs`, `region/host.rs:373` | The mailbox moves to `execution::control` with a public receiving side. |
| F23 | The logs, events, service-check, and trace encoders read the compression, payload-size, and flush-timeout keys the metrics encoder reads, and are built once. | `bin/agent-data-plane/src/cli/run.rs:795-856` | Only metrics-only encoder keys can become live. |

## 3. The physical model after stages 7 and 9

Both stages keep one rule from stage 3: **the physical frame is fixed at boot, and everything that changes live is data
inside fixed components.** Stage 7 multiplexes logical outputs and recipients over fixed encode and delivery hosts.
Stage 9 multiplies the tasks inside three fixed slot components (lanes) without changing which components exist. Neither
stage adds a component, an edge, a health registration, or an admission point after boot.

### 3.1 What is fixed at boot, and why

| Fixed at boot | Value in ADP with DogStatsD | Why fixed |
|---|---|---|
| Components | the sources, as natively; the slots `dsd_pre_aggregation`, `dsd_agg`, `metrics_enrich`; the encode hosts `dd_metrics_encode`, `mrf_metrics_encode`, and `af_metrics_encode` (the last only when an `https` Cluster Agent endpoint resolves at boot); the delivery hosts `dd_out`, `mrf_dd_out`, and `cluster_agent_out` (same condition); `dsd_client_telemetry_out`; the taps `dsd_stats_out` and `dsd_debug_log_out` | F1, F2: components cannot be added, and their health cannot be released |
| Edges | the frame's edges; every fused slot that has an output downstream has one exit to every encode host | an output enabled later (failover) needs a physical path; idle exits cost nothing (stage 3) |
| Lane counts | D, P, M, from configuration and the host (see [Configuration](#513-configuration)); at most `MAX_LANES` (64) each, and the automatic counts at most what the gate measured | lanes are tasks with input channels and resource groups, created at spawn |
| Key-group table | K = 256 groups, contiguous ranges over the P state lanes | routing must be a pure function of the key for the life of the process |
| Admission point IDs | D lane points of `dsd_in.metrics`, one point per other fixed component output the plan reads, then P output points of `dsd_agg` | the cut protocol and the trackers address points by ID |
| Registrations | one health registration per component; resource groups per component, per lane, per kernel of a lane, per native output's lease, and one per host for declared outputs | F2: none can be released, so every name is bounded and stable |
| Thread pools | the primary runtime (W workers); the global pool sized `max(8, W)` (stage 9; 8 today) | runtimes are built before the topology |
| Storage and budgets | the retry storage root, the shutdown budget, the host facts, the memory grant | as natively |

**What changes live**, without a restart:

| Changes live | Unit | Commit | Retire |
|---|---|---|---|
| Programs, kernels, state bindings | generation | one schedule store; each admission point switches at its next stamp | reference counting on headers |
| Output bindings (protocol variants, series payload limits, recipient set) | lease, in an encode host | none: a batch names its lease | drained and released after its last user generation retires |
| Recipients (endpoint, credential reference, protocol, payload classes, delivery policy) | recipient, in a delivery host | none for metrics: a payload names its recipients; the native set for other payloads, every recipient's settings, and the recipients a removal withdraws are published by a resource transaction that follows the current configuration (4.10) | withdrawn at once (nothing reaches it after the publish); drained (at most `DRAIN_DEADLINE`, 30 s) after every lease naming it is released and the published native set no longer names it |
| Host settings (client profile, retry, concurrency) | host binding | published by a resource transaction | replaced in place |
| Credential values | live configuration view | not a plan change: rotation is a cell store | with the last recipient using the reference |

Changes that still need a restart: lane counts, the global pool size, the retry storage root, turning disk persistence
on or off (`forwarder_storage_max_size_in_bytes` crossing 0), the stop timeout, the Cluster Agent endpoint host fact,
the encoder keys the logs, events, service-check, and trace encoders share (F23, 4.3), and structure upstream of the
policy region that no binding makes live (4.3).

### 3.2 Lanes

A **lane** is a task inside a slot component that runs the slot's region program on an input channel of its own, with
its own driver, kernel instances, memos, state, and resource group. The lanes of one component share its component ID,
its health registration, its control mailbox (served by a **coordinator** task that fans control out and joins the
acknowledgments), and its dispatcher. Three slots have lanes:

| Slot | Lanes | Input | What a lane holds |
|---|---|---|---|
| `dsd_pre_aggregation` | D, one per decoder up to 16 | lane `d` receives the unstamped buffers of every decoder `i` with `i mod D = d` and of the stream connections assigned to `d` | the admission point `dsd_in.metrics#d` (the lane stamps), the fused pre-aggregation program (mapper through tag projection), open routed buffers |
| `dsd_agg` | P | every pre-aggregation lane's keyed exit, routed by context key | a share of every aggregation lineage (disjoint contexts), a tracker over the D points, its own output point |
| `metrics_enrich` | M | pre-aggregation lane `d`'s passthrough exit (lane `d mod M`); every `dsd_agg` lane's output and every other source, spread per buffer | the post-aggregation filter, enrichment, host tags, the policy decision, its memos |

Edges into lanes are one of four routing kinds, all chosen by the frame from the lane counts, never by the plan:

| Routing | From | Decided | Per event |
|---|---|---|---|
| fixed lane | a source output with one lane each (`dsd_in.metrics_<d>` to lane `d`) | at boot | nothing |
| keyed | a lane's exit into a state slot with P > 1 lanes | per event, by the partition key through the key-group table | one table load and one move |
| affinity | a lane's exit into a stateless slot with lanes | sender lane `mod` target lanes | nothing |
| spread | a state lane's time-driven output, or a source with one admission point, into a stateless slot with lanes | per buffer, the first lane with room in rotation from a per-sender counter | nothing |

Encode and delivery hosts have no lanes. An encode host runs one builder task per lease (one per output stream, as
natively), and stage 9 may give a lease S > 1 builder shards when measurement says so.

### 3.3 Logical outputs and recipients

- An **output** is one encoder stream that `emit(output)` delivers to (the specification's unit). Its resolved binding
  runs as a **lease** in the encode host paired with its delivery host. A batch names its lease in the header's existing
  `entry` field, which the region's exit op already writes (F19). A generation's program carries a lease table
  (`OutputId -> EntryId`) per slot.
- A **recipient** is one endpoint of one delivery host: its I/O loop, credential cell, retry queue, and persisted
  directory. Recipients are shared by every output whose binding names them (specification, Output bindings). A delivery
  host interprets a **recipient table**; a payload names its recipients by table slot.
- Generations reference leases and recipients. A lease drains after the last generation using it retires, and a
  recipient after every lease naming it is released, so the schedule store remains the only commit.

### 3.4 The frame

```text
UDS/UDP listeners ─► datagram queue ─► decoders (shared receiver, as natively; decoder d feeds lane d mod D)
stream connections (one task each, assigned at accept to the lane with the fewest live connections)
   │ unstamped buffers on dsd_in.metrics_<d>  ──┬─► dsd_stats_out (gated), dsd_debug_log_out (taps; no lock)
   ▼                                           │
dsd_pre_aggregation   lane 0 .. lane D−1        (lane d stamps as point dsd_in.metrics#d; SourceClock shared)
   │ keyed exit dsd_agg (context key → 256 groups → P lanes; held ≤ 5 ms) │ passthrough exit (affinity d mod M)
   ▼                                                                     │
dsd_agg               lane 0 .. lane P−1        (tracker over D points; per-lineage shared tick and flush records)
   │ time-driven output, spread per buffer                                │
   ▼                                                                     ▼
metrics_enrich        lane 0 .. lane M−1  ◄── otlp_in, checks_ipc_in, liveness_in, trace_sampler.metrics (spread)
   │ one exit per encode host; header.entry = lease ID
   ▼
dd_metrics_encode (front task → builder task per lease) ─► dd_out (recipient table: routing loop → recipient loops)
mrf_metrics_encode ─► mrf_dd_out          af_metrics_encode ─► cluster_agent_out          dsd_client_telemetry_out
```

### 3.5 The path of one batch from socket to recipient

An 8-vCPU host (W = 8, D = 6, P = 4, M = 6), DogStatsD defaults, one artifact that routes the `team:platform` family to
the declared output `resources_team_d` as well as to the main output.

1. **Socket.** The UDS datagram reader receives a datagram of 40 metrics into a pooled buffer and queues it on the
   shared datagram queue (unchanged).
2. **Decode.** Decoder 3 takes it under the receiver mutex, decodes, resolves contexts through the shared resolver, and
   appends to its 1,024-event buffer. When full, it dispatches on `dsd_in.metrics_3`. That output has no admission
   point, so no lock is taken: the dispatcher sends the buffer into `dsd_pre_aggregation` lane 3's channel, and gives
   `dsd_stats_out` a clone only while a statistics collection runs (otherwise only the tap's received count, P9.F).
3. **Admit.** Lane 3 receives the buffer and stamps it as the single stamper of `dsd_in.metrics#3`: its point lock
   (uncontended; only installer transactions share it), the source's shared clock (one short critical section), and one
   schedule load. The header becomes `{t_a, point #3, epoch, generation g}`.
4. **Pre-aggregation.** The lane runs generation `g`'s fused program on its own kernels: mapper, prefix, blocklist,
   timestamp split, tag projection. Timestamped metrics leave by the passthrough exit to `metrics_enrich` lane 3.
5. **Route.** At the keyed exit into `dsd_agg`, each remaining event's context key (after every quotient) picks a key
   group, `group = key >> 56`, and the group's lane, `owner[group]`. The event moves into that lane's open buffer, which
   coalesces events of this lane's batches with an equal generation, epoch, entry, and admission second, across
   batches, for at most `COALESCE_HOLD` (5 ms). A buffer is sent when it holds B events, when the second, generation,
   or epoch changes, or when the hold expires; state lanes with no open buffer get header-only progress every 100 ms of
   admission time, and at once on a generation or epoch change.
6. **Aggregate.** `dsd_agg` lane 1 observes point #3's header (an O(1) index), checks for a crossing, a reset window, or
   a gated tick, and admits the events into its instance of the lineage `(dsd_agg, 10 s)`. A new context takes one unit
   of the global budget and one of the node's; an existing one takes nothing.
7. **Flush.** Lane 1's deadline for tick `k` fires at the instant the lineage's shared tick record holds for it. The
   record gives the tick's second `s` (the first lane at `k`'s deadline recorded it); the gate waits for lane 1's own
   watermark to reach `s`; the shared flush log gives the idle-counter back-fill start once every lane resolved tick
   `k − 1`. The drain emits closed buckets in bounded steps, stamped with lane 1's output point, and spreads them over
   the six `metrics_enrich` lanes, one buffer at a time.
8. **Post-aggregation.** `metrics_enrich` lane 4 runs the post-aggregation filter, host enrichment, host tags, and the
   policy decision. Its exit to `dd_metrics_encode` writes the lease ID of the main output (5) on one buffer and of
   `resources_team_d` (7) on another.
9. **Encode.** The encode host's front task reads its input (fan-in from six lanes), looks up lease 5 and lease 7, and
   moves each whole buffer into that lease's builder channel. Each builder task, on the global pool, encodes and
   compresses, and stamps every payload with the recipients its lease resolved for the payload's protocol variant:
   lease 5's V2 series payloads with slots `{0, 2}` (the primary and a dual-ship endpoint), lease 7's with `{3}` (team
   D).
10. **Deliver.** `dd_out`'s routing loop sends each payload to its slots in order. Each recipient loop inserts the
    current credential from its cell at dispatch and sends the request; retries queue per recipient, as natively.

## 4. Stage 7 design

### 4.1 Decisions

1. **Fixed frame, multiplexed resources.** The frame holds three delivery hosts with their native IDs (`dd_out`,
   `mrf_dd_out`, `cluster_agent_out`) and one encode host per delivery host (`dd_metrics_encode`, `mrf_metrics_encode`,
   `af_metrics_encode`). `mrf_dd_out` and `mrf_metrics_encode` are built, idle, whenever the metrics pipeline is; the
   Cluster Agent pair whenever an `https` Cluster Agent endpoint resolves at boot (the host fact). An idle host declares
   only its minimum memory bound; its queue bound is admitted with its first recipient (4.12). Logical outputs and
   recipients are plan data that the installer prepares, commits, drains, and retires inside them.
2. **Leases.** An output's resolved binding runs as a lease in its encode host, with one request-builder task on the
   global pool, so compression is exactly as parallel as natively (one builder per output stream). The lease ID rides in
   `header.entry`. A binding change creates a new lease; the old one drains after its last generation retires.
3. **The commit is still one schedule store.** A batch names its lease and a payload names the recipients of its own
   protocol variant, so metrics need no commit step. State that outlives generations, the native recipient set for
   non-plan payloads (logs, events, service checks, traces, APM stats), host and recipient settings, and the recipients
   a removal withdraws, follows the current configuration through **resource transactions** (prepare the recipients a
   native set adds, then publish), which wait for no metrics prepare and no cut (4.10).
4. **The defunctionalized forwarder.** A delivery host interprets a recipient table (endpoint, credential reference,
   protocol, payload classes, delivery policy) with per-recipient prepare, publication of settings, drain, retire, and
   reopen. One new ordering mechanism, the **input barrier**, guarantees that a lease's final flush reaches a recipient
   before the recipient is released. Without a metrics pipeline, `dd_out` runs a static table built from the startup
   configuration: today's behavior.
5. **Who may add a destination:** native configuration, now applied live (endpoints, routing, failover, metrics-only
   encoder settings, and forwarder settings), and a separate `resources` product on the stub channel with its own
   revisions. Policy artifacts reference outputs by ID only. Resources artifacts may use only credentials the operator
   declares under a new Saluki-only key, never the native API keys.
6. **Fingerprints cover what an output's declaration says it delivers to**, not its resolved recipients: the main and
   failover outputs are roles, a routing group is its key, a declared output is its recipient list. Adding a dual-ship
   endpoint changes no fingerprint and no base, so artifacts keep running and are not stale.
7. **Removal by the resource authority wins, and fails closed.** An artifact in effect whose recorded output
   disappears, or now delivers elsewhere, keeps running with every emit to that output replaced by a counted drop, and
   reports `degraded` until the output returns or the artifact is rebased; a new artifact waits `pending` (output
   missing) or is rejected (output rebound); a kept last-known-good that uses a resource the latest lowering no longer
   declares is released. The delivery host enforces removal itself: the resource transaction that follows the change
   **withdraws** the removed recipients, and the host drops and counts every payload send to them, so removal never
   waits for, or depends on, a metrics generation committing. A removed endpoint therefore stops receiving, which fixes
   today's behavior (F17), and nothing an artifact kept out of an output returns to it.
8. **Retiring recipients:** queued work is attempted for up to 30 s, then closed as at shutdown. Persistence stays
   native: queue directories keep native names, so a recipient that returns adopts its backlog, and persisted retries
   keep the native format (stripping credentials at rest is a follow-up, [A18](#91-stage-7-assumptions)). Health stays
   per host; resource-group names are bounded; metric handles and the forwarder's per-domain telemetry caches are
   released when a recipient retires.
9. **Budgets stay native at boot; what changes after boot is admitted.** Per-recipient limits and the forwarder's
   declared bound are unchanged (a known underestimate). Everything a generation or a resource transaction adds after
   boot (leases, an idle host's queue bound, declared recipients, a recipient's bound raised in place, new kernel
   instances times their lanes) is admitted against the memory limit's headroom in the boot verification's own mode, as
   credits per resource that return when that resource ends, however it ends (4.12).
10. **Delivery policy, minimal:** a resources artifact can shrink a recipient's queue, turn off disk spill, and choose
    the closure at retirement. Nothing can raise a native bound.

### 4.2 Resource model and identity

| Resource | What it is | Identity | Declared by |
|---|---|---|---|
| Output | one encoder stream that `emit(output)` delivers to | `OutputId`: native IDs (`dd_metrics_encode`, `metrics_routing_encode_<i>`, `mrf_metrics_encode`, `af_metrics_encode`, `dsd_client_telemetry`), or `resources_<name>` | native configuration, or a resources artifact |
| Output binding | the output's resolved parameters: delivery host, selector, encoder parameters (protocol variants, compression, limits, flush timeout), resolved recipients per protocol variant, delivery policy, the telemetry identity of its encoder | the output plus a digest of the binding | lowering |
| Recipient | one endpoint of one delivery host: I/O loop, credential cell, retry queue, persisted directory; shared by every output naming it | native `<host>/<route>/<key index>/<endpoint>` (amendment 13, unchanged); declared `<host>/declared/<credential name>/<canonical endpoint>` | native configuration, or a resources artifact |
| Host binding | a delivery host's shared settings (client profile: proxies, TLS, timeouts, HTTP protocol; retry and concurrency) and an encode host's shared tuning | the host | lowering |

| Thing | Rule | Why |
|---|---|---|
| Native output IDs | unchanged, routing groups still positional (`metrics_routing_encode_<i>`) | node ID equals native component ID in the default program; renumbering is caught by the fingerprint |
| Declared output IDs | `resources_<name>`, an ordinary identifier (`name` at most 53 bytes); lowering reserves the prefix `resources_`, which no native output uses | never collides with a native output, now or later, and needs no change to the identifier grammar of the IR or to the surface syntax (`emit resources_team_d`); its encoder telemetry reports under the same string as `component_id` |
| Declared recipient IDs | the digest rule of amendment 13 for an unprintable or long endpoint | a URL or credential change is another recipient with another queue, as the specification's identity rule implies |
| Lease IDs | `EntryId` per encode host, allocated by the installer core, reused only after the lease retired; a draining lease is never reused | carried by every batch at no header cost |
| Recipient slots | `u16` per delivery host, allocated by the core, reused only after retirement; a failed recipient's next incarnation keeps its slot | payload routing indexes a table instead of comparing strings |
| Recipient incarnations | `u64` per recipient prepare, allocated by the core; `RecipientRef { slot, id, incarnation }` names one loop on one queue | a reopened recipient, the one that just retired on the same slot and ID, and a failed loop's replacement are told apart |
| Control addressing | every recipient message and acknowledgment, host events included, names a `RecipientRef`, and every lease message `(EntryId, LeaseKey)`; a host ignores and logs a message whose reference does not match its table, and acknowledges a prepare of the reference it already holds as done; the core ignores acknowledgments and events of an older incarnation | a release, drain, or prepare repeated after an installer worker restart never reaches a reused slot, entry, or a newer incarnation, and credits return once per incarnation |
| Queue directory | native: `generate_retry_queue_id` (`io.rs:901-920`), `{component}/{host}/{hash}`; a declared recipient hashes `(dd_out, canonical endpoint, credential name)` the same way | a recipient added live gets the directory a restart would give it, and one that returns adopts its backlog ([4.9](#49-queued-and-persisted-work)) |

### 4.3 Who declares resources

**Native configuration, live.** `Lowering::lower(current)` resolves outputs, recipients, and host bindings from the
current configuration instead of the startup one (F18). These keys become live:

| Area | Keys |
|---|---|
| Endpoints | `dd_url`, `site`, `additional_endpoints` (membership as well as keys), the Observability Pipelines Worker and Vector metrics override |
| Routing | `experimental.metrics_endpoint_routing.*` (groups appear, disappear, and change) |
| Multi-Region Failover | `multi_region_failover.{enabled, site, dd_url}`; the gateway's `enabled` binding becomes `Live` |
| Autoscaling failover | `autoscaling_failover.{enabled, metrics}`, `cluster_agent.{enabled, auth_token}`; the endpoint stays a host fact (an `https` endpoint resolves from `cluster_agent.url` or the Kubernetes service environment at boot, whether or not `cluster_agent.enabled` is set), and the recipient exists only while forwarding is enabled, a token is set, and the branch is requested |
| Forwarder | `forwarder_*` except the storage root and stop timeout, and `forwarder_storage_max_size_in_bytes` only between nonzero values (crossing 0 creates or removes a queue's directory, `io.rs:765-782`, which a live setting cannot); proxies; TLS settings; `skip_ssl_validation`; HTTP protocol; connection reset; validation interval; `allow_arbitrary_tags` (a request header every payload class gets, `middleware.rs:64`) |
| Encoder, metrics only | `use_v2_api.series`, `use_v3_api.*`, `serializer_experimental_use_v3_api.*`, `serializer_max_series_payload_size`, `serializer_max_series_points_per_payload`, `serializer_max_series_uncompressed_payload_size` |
| Stays restart-only | `serializer_compressor_kind`, `serializer_zstd_compressor_level`, `data_plane.serializer_zstd_compressor_level`, `serializer_max_payload_size`, `serializer_max_uncompressed_payload_size`, `flush_timeout_secs`: the logs, events, service-check, and trace encoders read them too and are built once (F23), so a live change would split behavior by payload type; `forwarder_storage_max_size_in_bytes` changing to or from 0 |

Structure that follows these keys (a routing group's branch, the failover branches) follows them live: the default
program gains or loses the branch, and artifacts see a stale base, which is the behavior the specification designs for a
newly configured additional endpoint or failover branch. The schema overlay's support levels change with them
(`schema_overlay.yaml:1669-1671` says new domains need a restart); the overlay and the generated reference change
together, as the config-system skill requires. Standalone mode never applies configuration updates
(`lib/agent-data-plane-config-system/src/system.rs:157-168`), so native liveness is exercised in connected mode and in
the live tests; the demo uses the resources product.

The Core Agent sends one partial update per key, and each applies as it arrives: a credential is a cell store at once,
an endpoint change a candidate. A change of `site` and `api_key` together therefore runs in two steps, and requests sent
between them pair the new endpoint with the old key, or the old endpoint with the new key, which a restart never shows;
a 403 is then dropped unless secrets are in use (`retry.rs:116`), as for any rotation that reaches the forwarder before
the intake knows the key. Delaying candidates would not help, since the cell store is not a candidate; a quiet period
over the configuration view itself is the remedy if this proves common ([A14](#91-stage-7-assumptions)).

**The `resources` product.** A second product on the stub channel, beside `policy`:

- **Separate revisions and authority** (specification: "Policy and resources are separate artifacts with separate
  revisions and authority"): its own revision high-water mark; in the demo, its own publish button and approval.
- **One config.** More than one config in the product rejects all of them, as for one scope of policy. An empty snapshot
  revokes every declared resource. No order between the two products is assumed.
- **Document** (`resources_version: 1`), a first pass behind a narrow type (`resources::ResourcesArtifact`):

  ```json
  {
    "resources_version": 1,
    "recipients": [
      {
        "name": "team_d",
        "host": "dd_out",
        "endpoint": "http://127.0.0.1:17105",
        "credential": "credentials/team_d",
        "protocol": { "v3_series": false, "v3_sketches": false },
        "delivery": { "queue_bytes": 4194304, "spill": "never", "on_retire": "attempt" }
      }
    ],
    "outputs": [ { "name": "team_d", "recipients": ["team_d"], "accepts": "any", "label": "Team D" } ]
  }
  ```

- **Validation** (`resources::validate`): identifiers, and output names short enough that `resources_<name>` is one
  (at most 53 bytes); at most `MAX_DECLARED_RECIPIENTS` (16) and `MAX_DECLARED_OUTPUTS` (16); at most
  `RESOURCES_ARTIFACT_SIZE` (64 KiB); endpoints parse as `http` or `https` URLs and are put in canonical form (lowercase
  scheme and host, default port removed, normalized path); the host accepts declared recipients (`dd_out` only in this
  version); credentials are in the `credentials/` namespace; an output's recipients are non-empty and on one host;
  recipient IDs are unique.
- **Envelope** `{revision, artifact_hash, artifact}`, hashed and checked as policy envelopes are. A missing `resources`
  member in a poll answer revokes, as for policy; a `resources` member that is not a list makes only that product
  unreadable for the poll (policy is still read). The stub protocol gains the product additively within
  `protocol_version` 1 ([the stub protocol](programmable-metrics-stub-protocol.md)), and documents that `pending` with
  a non-null error is a held state ("waiting for output X"), not a transient one; `ApplyState`'s documentation
  (`channel/state.rs:24-25`) changes with it.
- **One feed slot per product.** The installer's feed (`channel/feed.rs`) holds one watch slot per product, so a
  resources snapshot published right after a policy snapshot never overwrites it: `ArtifactFeed::changed() ->
  (ArtifactProduct, Arc<ArtifactSnapshot>)` returns the product whose slot changed, and `mark_unseen(product)` re-arms
  one slot. The stub publishes each product into its own slot on every poll.
- **Lowering input:** lowering becomes `(startup, current, facts, accepted resources, approved policy) -> plan`.
  Declared outputs and recipients enter the default program's declarations and the catalog with their labels; no
  default-program node emits to a declared output. A declared recipient whose credential is not configured is detected
  here, not at prepare: the resources config goes `pending` and lowering excludes it (4.8).

**Who does what for the resources product.** The core stays I/O-free; ADP validates and resolves:

1. On a resources snapshot, ADP checks the snapshot's shape (one config, transport hash, envelope) and runs
   `resources::validate`, then calls `core.on_resources(event)` with one `ResourcesEvent`:

   ```rust
   // saluki-metric-plan::install::resources
   pub enum ResourcesEvent {
       Revoked,                                                // an empty snapshot, or a missing `resources` member
       Refused { config: ArtifactConfigId, version: u64, reason: Reason },   // several configs, a bad hash or envelope
       Delivered {
           config: ArtifactConfigId, version: u64, revision: ResourcesRevision, artifact_hash: PlanHash,
           document: Result<ResourcesArtifact, Reason>,         // `Err`: the document failed validation
       },
   }
   ```

2. The core applies the revision rules of the policy product: its own high-water mark, acknowledgments kept per
   `ArtifactConfigId`, a redelivery recognized by its artifact hash, revocation. It keeps the accepted document and
   returns `Action::InstallDefaultProgram` when what lowering should see changed.
3. On every configuration change, before lowering, ADP resolves every credential the accepted documents reference
   against its configuration views and calls `core.credentials_changed(missing)` with the names that are not
   configured; the core re-evaluates `pending` from it, so a change is lowered once.
4. ADP lowers with `core.accepted_resources()`: the newest accepted revision that is neither rejected nor pending (the
   one installing, else the one active), or `None` once revoked. A newer revision that is refused, rejected, or
   pending leaves the revision installed before it in place, as a refused policy delivery does not hide what an
   earlier one installed (`install/core/artifacts.rs:35-37`): declared resources are removed only by a revocation, or
   when the installed revision itself becomes pending (its own credential removed).
5. The core records which resources revision each lowering, generation, and resource transaction carries, so it can
   report `installing`, `active`, and `retired`, and a prepare failure attributed to a declared resource (4.16) marks
   that revision `rejected`. `config_states()` returns the acknowledgments of both products.

| State | Set by | When |
|---|---|---|
| `rejected` | ADP (validation), or the core (a prepare failure a declared resource caused) | invalid document; a declared recipient that failed to prepare, with the slot's reason; the revision installed before it, if any, keeps running |
| `pending` | the core, from `credentials_changed` | a referenced credential is not configured; re-evaluated on every configuration change; the revision installed before it keeps running |
| `installing` | the core | accepted; the generation or resource transaction lowered with it waits, prepares, or retries (as policy's `pending` while it waits or prepares) |
| `active` | the core | the committed generation was lowered with this revision |
| `retired` | the core | a newer revision installed, or a revocation, and the last generation lowered with it retired |

**Policy artifacts** keep their shape: an artifact emits to an output by ID and records the output's fingerprint in its
base. It cannot declare outputs, bindings, recipients, credentials, or delivery policies; composition takes every output
declaration from the default program.
**Privilege summary.** A destination enters ADP only through operator-controlled native configuration or through the
resources product. A credential leaves the host only toward an endpoint native configuration names, or toward a declared
endpoint with a credential the operator explicitly made available to resources artifacts. Revoking the product withdraws
everything it declared at the resource transaction that follows, retires it within the drain deadline, and leaves behind
only its recipients' queue directories, as removing an endpoint from native configuration does (4.9). Signed resources
and endpoint allowlists per credential wait for stage 8.

### 4.4 Output bindings as plan parameters

The plan carries every resolved resource, so a change lowers into a new plan hash, goes through prepare and commit, and
appears in the diff and in `explain`. The plan crate stays protocol-agnostic: host parameters are typed `Params` that a
host-provided schema in ADP's catalog validates (`Catalog::host(&HostId) -> Option<HostSchema>`, new beside
`Catalog::output`, `validate/catalog.rs:68`).

```rust
// saluki-metric-plan::ir
pub struct OutputDecl {
    pub id: OutputId,
    pub accepts: PortType,
    pub fingerprint: PlanHash,                 // version 2, 4.5
    pub recipients: Vec<RecipientId>,          // resolved, sorted, deduplicated (as today)
    pub delivery: DeliveryPolicy,              // 4.13
    pub binding: Option<OutputBinding>,        // None for a fixed destination (client telemetry)
}
pub struct OutputBinding {
    pub host: HostId,                          // dd_out, mrf_dd_out, cluster_agent_out
    pub selector: Selector,
    pub encoder: Params,                       // protocol variants, compression, limits, flush timeout
    pub telemetry: NativeComponentId,          // the component_id its encoder telemetry reports under
    pub provenance: Provenance,                // native, or the resources revision; not in the lease's binding digest
}
pub enum Selector {
    Role(RoleName),                            // main, multi_region_failover, autoscaling_failover
    Group(GroupKey),                           // a routing group's key: a digest of its endpoint set
    Declared(Vec<RecipientId>),                // a declared output's recipients, sorted
}
pub struct Resources { pub hosts: Vec<HostBinding>, pub recipients: Vec<RecipientDecl> }   // plan member `resources`
pub struct HostBinding { pub host: HostId, pub settings: Params }
pub struct RecipientDecl {
    pub id: RecipientId,
    pub host: HostId,
    pub endpoint: String,                      // as configured (native) or canonical (declared)
    pub credential: CredentialRef,             // a reference, never a value (ir/id.rs)
    pub classes: PayloadClasses,               // MetricsOnly | All (4.7)
    pub protocol: Params,                      // V3 acceptance; request mapper kind (Datadog intake or Cluster Agent)
    pub delivery: DeliveryPolicy,              // a recipient's only knobs beyond its host's settings (4.13)
    pub provenance: Provenance,                // not compared when deciding whether a recipient was reconfigured
}
```

- **Typed parameters across crates.** `saluki-components` cannot read `Params` (it does not depend on the plan crate),
  so the typed specs live there and ADP decodes into them. I7 defines, in `saluki-components`: `EncoderSettings`
  (series and sketch modes, compressor kind and level, payload limits, flush timeout, `log_payloads`),
  `EncodeLeaseSpec { lease: EntryId, key: LeaseKeyDigest, telemetry_id, settings, recipients:
  Arc<ResolvedRecipients>, builders: NonZeroU8 }`, `RecipientSpec`, `RecipientProtocol`, `DeliverySettings`,
  `PayloadClasses`, `ResolvedRecipients` (a slot list per protocol variant), `RecipientSlot`, and the payload-metadata
  key that carries `ResolvedRecipients`; in ADP, one codec module, `programmable_metrics::resources::params`, owned by
  P7.5, which lowering encodes with and the installer decodes with, with a table of `Params` names and value types per
  member (`OutputBinding::encoder`, `HostBinding::settings`, `RecipientDecl::protocol`) and a round-trip test. Names
  are the typed fields' names in snake case; the schema that `Catalog::host` returns is generated from the same table,
  so validation, `explain`, and decoding cannot disagree.
- **Resolved recipients per variant.** An output's recipients and each recipient's V3 acceptance
  (`RecipientDecl::protocol`) give, per protocol variant the binding produces, the recipients that accept it (F21):
  the lease's `ResolvedRecipients` holds one slot list per variant, so no recipient filters a payload it was resolved
  for.
- **Credentials stay references** (specification, Lowering). Proxy URLs appear without user information, which becomes a
  credential reference (`native:proxy.http`, `native:proxy.https`). The proxy is built into the client
  (`proxy.rs:123-126`), so the delivery host follows each proxy reference's view itself: when the value changes, it
  builds a new client off its routing loop and swaps it in at a request boundary, keeping the old client and reporting
  the failure if the build fails. A proxy password rotation therefore applies without a plan change, like an API key
  rotation, and never enters a plan.
- **Hashing.** The plan hash covers `resources` and every `binding` with resolved values, provenance included (so
  `explain` shows it), so any change produces a candidate; an equal candidate is `Unchanged`, as today. A lease's
  binding digest (`LeaseKey`, 4.10) and the comparison that decides whether a recipient was reconfigured leave
  provenance out, so a resources revision whose content is unchanged rebinds no output and flushes nothing early.
- **Validation** adds the specification's capability-binding rule: every output binding names a host the catalog
  declares (`Catalog::host`), with parameters its schema accepts, and recipients the plan declares on that host; every
  recipient of an output accepts its protocol (a Cluster Agent recipient only V2 series of scalar kinds); declared
  outputs name declared recipients only.
- **Diff** lists outputs as reused, rebound, added, or removed (as today), recipients as reused, reconfigured, added, or
  removed, and hosts as reconfigured; `explain` shows each resource as reused or new, with provenance, as the
  specification requires. The export bundle's catalog lists declared outputs with labels, recipients, and fingerprints,
  but not host settings.
- **Frame and placement.** `FrameTarget::Output(OutputId)` splits into `FrameTarget::EncodeHost(HostId)` for bound
  outputs and `FrameTarget::Destination(OutputId)` for fixed destinations; `Frame::compute` takes the frame's hosts
  (`FrameHosts`) from ADP. An output places when its binding's host is a frame host and every slot that emits to it has
  an exit to that host's encode host (`compile/placement.rs:320-342`). The `ResourceGenerations` errors for "adding an
  output" and "changing an output's binding" go away; the ones for host components, physical edges, new exits, and
  isolation slots stay, with messages that tell structure a restart would add from structure only an artifact asks for
  ([amendment 107](#10-proposed-spec-amendments)). A direct edge from an admission point
  to an encode host is a frame error, since nothing would write the lease; ADP never builds one.
- **Region programs.** `emit(output)` compiles to `Op::Exit { exit, entry: EntrySource::Lease(OutputId) }` toward the
  output's encode host. The slot preparation carries the generation's lease table, and the region's program translation
  resolves `EntrySource::Lease` into the concrete `EntryId`, so a rebinding changes only the lease table of an otherwise
  unchanged program. Every lane of a slot (stage 9) translates the same table.

### 4.5 Fingerprints, base, staleness, removal, pending

**Fingerprint version 2.** An output's fingerprint is the digest of `{"v": 2, "host": <delivery host>, "accepts": <port
type>, "selector": <selector>}`:

| Output | Selector | Changes when |
|---|---|---|
| `dd_metrics_encode` | role `main` | never by recipients: dual-ship endpoints added or removed, a changed `dd_url`, a routing group's endpoints excluded or released |
| `metrics_routing_encode_<i>` | group key | the group at that position is another group (renumbering, an edited endpoint set) |
| `mrf_metrics_encode` | role `multi_region_failover` | never by its endpoint or key |
| `af_metrics_encode` | role `autoscaling_failover` | never by its endpoint or token |
| `resources_<name>` | the declared recipient IDs | the resources authority points it elsewhere |
| `dsd_client_telemetry` | destination (unchanged) | never |

The rule: **a fingerprint covers what an output's declaration says it delivers to.** A role delivers to whatever native
configuration resolves for it; an explicit list is part of the declaration. Encoder parameters, delivery policy,
recipient settings, and credentials are never part of it (as in amendment 41). Version 1 bases (stages 5 and 6) keep
composing while they are right: the catalog also gives each native output its version 1 digest, computed from the
current configuration with the stage 5 formula (`outputs.rs:326-354`), and a recorded fingerprint equal to it composes
as before the upgrade, with the note "rebase to fingerprint version 2" in the acknowledgment. A version 1 base that no
longer matches is rejected with the version 1 meaning ("output X now delivers elsewhere; rebase"), so a working policy
does not stop applying at the upgrade.

**Base and staleness.** The base hash covers the export's structure and the fingerprints of the outputs it emits to
(amendment 61). With version 2: a dual-ship endpoint added or removed makes an artifact neither stale nor invalid (it
follows the main output's new binding); a routing group added or removed, or a failover branch appearing, makes it
stale, and it keeps running; a declared output added changes nothing.

| Situation at composition | First installation (a new artifact) | Re-composition of the artifact in effect |
|---|---|---|
| Recorded output declared with the recorded fingerprint | composes | composes |
| Recorded output declared with another fingerprint | `rejected`: "output X now delivers elsewhere; rebase"; whatever ran keeps running | composes with every `emit X` replaced by a counted drop; `active`, degraded: "output X now delivers elsewhere; its emits are dropped; rebase" |
| Recorded output not declared | `pending`: "waiting for output X"; whatever ran keeps running | composes with every `emit X` replaced by a counted drop; `active`, degraded: "output X is no longer declared; its emits are dropped until it returns" |
| Emitted output not recorded, but declared or named `resources_<name>` | `rejected` (`UnrecordedOutput`: "record the output's fingerprint in `base.outputs`") | cannot happen: the artifact composed before |
| Emitted output neither recorded nor declared, other names | `rejected` (`UndeclaredOutput`, as today) | cannot happen |

**Removal fails closed.** The two re-composition rows depart from the specification's "the current generation keeps
running" ([amendment 91](#10-proposed-spec-amendments)): keeping the old generation would keep the removed destination
alive and receiving, and falling back to the default program would undo everything else the artifact does (an
exclusive routing would send its families back to the primary intake; its drops would deliver again), triggered even by
an unrelated configuration edit. Composition instead turns each affected `emit X` into a `drop` node with the same
node ID, whose loss counts as policy loss of that node, and the scope's status names the output and the reason. The
removed destination stops receiving, and nothing the artifact kept out of an output reaches it. When X returns with the
recorded fingerprint, the next re-composition restores the emits; a rebound X needs a rebase. When the routing groups
are renumbered (trace F), the reason names the output that now carries the recorded fingerprint ("output renumbered:
the recorded group is now `metrics_routing_encode_0`; rebase"). A removed recipient stops receiving metrics at the
resource transaction that withdraws it (4.10), whether a cut is in flight or the re-composition fails to prepare or
compose; the composition-level drop, which names the output in status, follows when the re-composition commits (after
an in-flight cut, at most `lcm` + 1 s later, 301 s at a 300 s width).

### 4.6 The multiplexed encode host

`MetricsEncodeHost` (`lib/saluki-components/src/encoders/datadog/metrics/host.rs`, new) is one encoder component per
delivery host, built from today's `DatadogMetrics` with its per-encoder state moved into a per-lease struct.

```text
region exits (any number of producers; stage 9: M lanes)
   │  EventsBuffer, header.entry = lease ID
   ▼
[host input, 128 buffers] ──► front task ──► lease table: Vec<Option<LeaseHandle>> indexed by lease ID
                                             LeaseHandle { key, shards: [mpsc::Sender<EventsBuffer>; S], recipients }
                                                │ whole buffers only, at most LEASE_QUEUE_BUFFERS (8) per shard
                                     builder task per shard (global pool), today's run_request_builder
                                                │ HostPayload::Payload, stamped with its variant's recipients,
                                                │ then HostPayload::LeaseClosed { lease, shard } after its last one
                                     front task ──► delivery host input
```

- **The front task** is today's `DatadogMetrics::run` loop generalized: it reads input buffers and the control mailbox,
  indexes the lease table by `header.entry`, and moves the whole buffer into one of the lease's shard channels. It does
  no per-event work. While it waits for room in a shard, it keeps draining the shared payload channel, as the native
  task does to avoid the two-channel deadlock (`metrics/mod.rs:596-631`), and serves its control mailbox and health:
  the wait is one `select!` over the shard's reservation, the payload channel, the mailbox, and liveness, so a prepare
  or a drain never waits behind data. As built, it also answers its mailbox and liveness while it waits for the
  delivery host to take a payload, since a slow recipient fills the delivery host's input (P7.2).
- **Shards.** `EncodeLeaseSpec::builders: NonZeroU8` is 1 in stage 7 (native payloads, byte for byte). With S > 1 (stage
  9, only on measurement), a buffer goes to the shard with the most free capacity (`Sender::capacity`), ties by
  rotation; payloads are independent requests, and payload boundaries are not observable (specification, Equivalence).
- **Each builder task** is today's `run_request_builder` (`metrics/mod.rs:686` onward) with its `EncoderSettings` from
  the lease's spec. It stamps every payload with the slot list its lease resolved for the payload's protocol variant
  (`ResolvedRecipients`, 4.4) in place of `Arc<MetricsEndpointRouting>` (`apply_endpoint_routing`, `:656-669`), keeps
  `MetricsPayloadInfo` for telemetry, and counts what the native encoder drops without counting (an oversized V2
  payload's recoverable errors, the TODO at `metrics/mod.rs:1015`) as `component_events_dropped_total{drop_reason:
  payload_too_large}` under the lease's `component_id`. After its last payload it sends `HostPayload::LeaseClosed {
  lease, shard }` on the shared payload channel.
- **Capacity.** The host input keeps the native encoder input's 128 buffers, counted at full-buffer size in the firm
  bound of the topology as natively; a shard channel holds `LEASE_QUEUE_BUFFERS` (8), the native events channel, counted
  at full-buffer size in the lease's admitted bound (4.12). With one output this is exactly what the native encoder
  held; with several, the host holds one 128-buffer input where natively each encoder had its own, so a slow output has
  8 buffers of its own plus the shared 128 before the regions block. When one lease is full, the front task waits and
  every output of the host waits with it. Natively the region's fan-out awaits each exit in turn, so one full encoder
  already stalls every output; overflow stays `wait`. **As built**, an encode host that has no lease at boot (an idle
  failover host) keeps the 128-buffer input of the topology: encoder inputs are not part of the bounds of the topology
  (`TopologyBlueprint::with_component_input_capacity` sizes only transforms' and destinations', which are), so the
  8-buffer input this paragraph proposed would save nothing in the boot bound and would only shrink burst slack. A burst
  that reaches several outputs of one host fills the shared input sooner than natively, where each output had an encoder
  and an input of its own: a flush to one routing group holds 64 batches in `dd_metrics_encode`'s input where the native
  routing encoder held 128 (`burst_tests`; release-noted).
- **Unknown lease.** A batch whose entry names no live lease, or a draining one, is counted as loss
  (`plan_loss_total{class=invalid}`) and logged as a wiring fault, once per entry value. Installer order (4.10) makes
  it unreachable. The lease table is a hash map by lease ID, not a vector: identifiers are allocated in rotation and
  reach every value over a process's life (P7.2).
- **Protocol variants follow the recipient set.** Lowering computes each binding's variants (V1 or V2 series, V2
  sketches, V3 series, V3 sketches) from its resolved recipients with the encoder's own functions (`requires_v3_series`,
  `requires_v2_series`, `metrics/mod.rs:354-378`), so a V3 recipient joining a V2-only output changes the binding and
  builds a new lease. Because acceptance is exclusive (F21), a payload carries its variant's slots, and a recipient
  loop applies the `MetricsPayloadInfo` filter (`io.rs:802-808`) only to payloads without a resolved set (the native
  set's and legacy payloads). A live change of a recipient's V3 acceptance then loses nothing: the old lease's V2
  payloads name the slots that accepted V2 when it was resolved, and the new lease's V3 payloads the slots that accept
  V3 now, whatever the loop's settings say at that moment. A metric of a kind for which the lease has no variant (a
  Cluster Agent output's sketches) is dropped and counted as `encoder_failure` under the lease's `component_id`;
  validation's capability rule keeps that unreachable (P7.2).

| Phase | Encode host does |
|---|---|
| Prepare | receives `LeasePrepare { lease, key, spec, built }`, whose builders and telemetry handles the installer built off the data path through the host's `EncodeLeaseFactory`; spawns its S builder tasks; acknowledges. A prepare into an entry another lease holds, or a draining one holds, is rejected. Nothing addresses the lease before the commit. |
| Commit | nothing: batches of a generation whose lease table names the lease start arriving |
| Drain | on `LeaseDrain { lease, key }`, sent only after every user retired: closes each shard channel by dropping its senders (FIFO with the buffers before it, and needing no room in a full shard), after the lease's last batch on that channel; each builder flushes every open payload (as its timer would), sends it, then sends its `LeaseClosed` marker, and exits. Each builder's sends are FIFO on the shared channel, and the front task awaits each dispatch, so once it has dispatched past every shard's marker every payload of the lease is in the delivery host's input; it acknowledges then. |
| Retire | drops the handles and frees the lease ID; the lease's series are evicted 3 s later unless a successor registers the same keys |
| Shutdown | on input end: closes every shard of every live and draining lease, keeps dispatching until the payload channel closes (the front task drops its own sender, kept for spawning, and every builder exits), and answers pending drains, and any message still in the mailbox, with `Stopped`; no lease is ever half prepared, since a prepare carries its built lease |

A binding change therefore costs one early flush of the old builder per affected output: smaller payloads once, nothing
lost. A metrics-only encoder-setting change such as `use_v2_api.series` replaces every lease of the host that way.

**Parity.** With one lease per native output and native settings, the host produces the payload bodies and metadata of
the native `DatadogMetrics` encoder for the same input, apart from the routing value's type. A differential test pins
it; the native encoder stays as the legacy path's reference.

### 4.7 The delivery host: the forwarder defunctionalized

The native forwarder captures its behavior at build (`io.rs:377-441`, `:511-602`). Each captured piece becomes a
constructor in a table that one generic `DeliveryHost` (`lib/saluki-components/src/common/datadog/delivery/`, new)
interprets:

| Native, captured at build | Delivery host, as data |
|---|---|
| `Vec<RoutableEndpoint>` from `build_routable_endpoints` (`config.rs:426-467`) | `RecipientTable`: slot to `RecipientEntry { id, spec, sender, state, credential cell, settings watch }` |
| `should_route_to_endpoint` and `matches_metrics_endpoint_routing` over raw strings (`io.rs:572-597`, `:621-655`) | per payload, its variant's `ResolvedRecipients` slots; without one, the published **native set** filtered by payload class with the native rule |
| request mapper factory (`middleware.rs:17-58`); Cluster Agent mapper (`forwarders/cluster_agent/mod.rs:196-223`) | `RecipientProtocol::{DatadogIntake { authorities, allow_arbitrary_tags, v3 }, ClusterAgent}`; the credential (API key or bearer token) is inserted from the recipient's cell at dispatch, for both kinds |
| retry policy, concurrency, timeouts, queue sizes from `ForwarderConfiguration` | `DeliverySettings` (the host binding) and a declared recipient's `DeliveryPolicy`, published to each loop through its `RecipientSettingsSender` and applied in place between requests: the loop's service and circuit breaker are built once, so a publication keeps an open backoff and the retry policy's error count; turning persistence on or off is not applied live (P7.1a) |
| `ApiKeyRefresher` bound to configured `(url, index)` positions (`api_key.rs:222-280`) | one credential cell per recipient, fed by the view its `CredentialRef` resolves to (4.8) |
| `ApiKeyValidator` over the startup set (`validation.rs:24-70`) | validation targets: the native `DatadogIntake` recipients of the table, following it (`RecipientValidationTargets`), asked about in a native forwarder's order (the primary endpoint, the alternate metrics intake, the additional endpoints) and stopping at the first key the intake accepts |
| one `HttpClient` (`io.rs:386-410`) | one client per host binding, built when a publication changes the client profile or a proxy credential it follows changes, off the routing loop, and switched for every loop and the validator through `HttpClient::switchable`; a failed build keeps the current client, is reported in status (`client_failure`, which ADP shows in `status.degraded` naming the host), and is built again at the next publication (P7.1b D2, R6) |

`DatadogForwarderConfiguration` and `ClusterAgentForwarderConfiguration` become configurations of the one `DeliveryHost`
(protocol defaults, telemetry names, which validation runs). `build_routable_endpoints` must allow an empty table (it
always pushes a primary today, `config.rs:429-432`).

**Routing.** The component's run loop becomes the routing loop, so control messages and payloads are ordered in one task
(today a second hop, component loop to the main I/O loop over `mpsc(8)`, `forwarders/datadog/mod.rs:181-221`). A send
to a recipient that waits for room is one arm of the same `select!`, so control and liveness are served while data
waits, and a prepare or a release, including the one that removes a stuck recipient, never times out behind it:

```text
select! {
  health, validation,
  control mailbox -> Prepare | Publish | Release | Reopen | Abort | AbortResources,
  pending send    -> the next slot of the payload being routed (sends stay in slot order, one at a time),
  payload input   -> transaction; route(payload) becomes the pending sends   (only while no send is pending)
}
```

- Metrics payloads from leases carry their variant's `ResolvedRecipients` and go exactly there, in slot order, awaited
  in turn (native head-of-line behavior), except to a **withdrawn** recipient (below). Lowering already applied the
  native rule to resolve them, including the metrics override replacing the primary for metrics
  (`metrics_recipients`, `config.rs:471-506`).
- **Withdrawal.** Each publish carries the recipients of this host that the current lowering no longer declares, by
  `RecipientId`. The routing loop drops every send of a payload to a withdrawn recipient, whatever lease or generation
  stamped it, and counts it in `plan_recipient_withdrawn_payloads_total{host}`; a later publish that no longer lists the
  recipient restores it. Removal is therefore enforced where delivery happens: a generation that never commits (an
  addition that keeps failing to prepare, a re-composition that fails, a cut in flight, a candidate queued behind
  another) cannot keep a removed destination receiving, and neither can a rollback. **As built** (stage 7 review), a
  resource transaction follows whenever the published withdrawn set differs from the table's, so a retired recipient's
  identity leaves it after the exit and never withdraws a later incarnation; and a generation that uses a recipient its
  host withdraws while the latest lowering declares it again (an endpoint added back before the transaction restoring
  it published, as when a newer change supersedes that transaction) passes its barrier only once the transaction in
  flight published, so its leases never send to a recipient their host drops everything for
  (`InstallerCore::pass_barrier`).
- Payloads without a resolved set (logs, events, service checks, traces, APM stats, any legacy encoder's) route over the
  published native set with `should_route_to_endpoint`, unchanged; a legacy `MetricsEndpointRouting` is still honored.
- **Payload classes.** Native recipients keep native classes (an additional endpoint receives every class, the metrics
  override only metrics). Declared recipients receive metrics only, so a resources artifact cannot divert logs or
  traces.
- Consequence: since `dd_out`'s native set follows configuration, a live change to `additional_endpoints` also changes
  where logs, events, and traces go, at the resource transaction that follows the change, which is what a restart does
  ([A14](#91-stage-7-assumptions)). It waits for no metrics prepare or cut, and a metrics prepare failure does not hold
  it back (4.10).

**API key validation and readiness.** Validation runs for native `DatadogIntake` recipients only, over the table as it
stands: declared recipients are never validated (their endpoint would otherwise receive `GET
<endpoint>/api/v1/validate?api_key=<credential>`, `validation.rs:325-340`) and never affect readiness; an invalid
declared credential is reported in status and `plan_recipients{state}`. A host whose table holds no native recipient
(an idle failover host) is ready and logs nothing; static mode keeps the native warning
(`validation.rs:207-211`). **As built**, a host whose native intake recipients have no usable key (a blank primary key,
or one not streamed yet in connected mode) is ready with the native warning too. Enabling Multi-Region Failover live
with a bad key makes the process not ready, as a restart with that configuration would.

**Per-recipient lifecycle:**

| Phase | Delivery host does |
|---|---|
| Prepare | receives `RecipientPrepare { recipient: RecipientRef, decl, built }` whose pieces the installer built through the host's `RecipientFactory` (credential cell bound to its view, request mapper, telemetry handles); checks that no live loop owns the recipient's directory (`RecipientPieces::queue_directory`), then opens its retry queue (`open_queue`), so a directory that cannot be used fails the prepare; spawns the I/O loop through `RecipientLoop::spawn` on the component's supervisor (not a scope of its own: P7.1b D15); inserts it, addressable only by slot; acknowledges. Opening a queue on disk is not read-only: it creates the directory (mode 0700), lists it by name and size, and removes files whose names are older than the age limit, reading no file contents; files beyond the size limit go when the next entry is persisted (P7.1a). So a queue is opened on a directory only while no loop owns it: the core prepares an identity's next incarnation only after the previous one's exit was reported, and the host fails a prepare whose directory a live loop still owns. **As built** (stage 7 review), a prepare whose directory a loop an abort dropped still owns waits, unanswered, until that loop exits, and proceeds then; an abort of its preparation rejects it. The core cannot tell when a dropped loop exits, and prepares the next incarnation right after an abort (a newer candidate cancelling the generation that created a recipient first, a restart for exits, a retry after a timeout): failing would reject a declared recipient's resources revision. An incarnation is created at most once: a create of an incarnation the host dropped, at or below the newest it created of that identity, fails the part (P7.4 R4); **as built**, the host remembers the newest incarnation of each identity it runs or holds, and of the last 128 identities it no longer does, so what it keeps stays bounded. A failure (client, directory, bounds, duplicate key) is the slot's failed acknowledgment, naming the recipient; a prepare of the reference the host already holds is acknowledged as done. |
| Publish | on `Publish { revision, native_set, settings, withdrawn }` from a resource transaction: swaps the native set, stores each recipient's settings version into its watch (the only path that changes a live recipient's settings, 4.10), replaces the withdrawn set, updates the validation targets |
| Drain | on `Release { recipients: [RecipientRef] }`, acknowledged once accepted: waits for the **input barrier**, removes the slots from routing, and retires each loop through its closer (`RecipientCloser::retire(deadline)`, `DRAIN_DEADLINE`). The loop keeps dispatching queued work, persisted entries included. A readiness error during a drain ends it as the deadline would, and the exit is `Retired` with what the close persisted and dropped (P7.1a). It exits when its queue and in-flight set are empty, or, at the deadline, closes as at shutdown: in-flight requests finish, queued work persists where disk is configured, otherwise it is dropped and counted; a transaction that cannot be persisted is dropped and counted, and the close goes on past it. A `Discard` closure drops what is queued in memory and keeps entries already on disk. The host keeps a draining recipient's sender, so that it can be reopened; dropping the closer closes the loop as at shutdown. At most `MAX_DRAINING_RECIPIENTS` (16) drain per host; one more closes the oldest early with its closure. |
| Retire | when the loop exits: drops the scope, the credential cell, the telemetry handles, and the per-domain cache entries it held (4.11); frees the slot; reports `RecipientExited { recipient, outcome: Retired(DrainReport { delivered, persisted, dropped, deadline_reached }) }` on the host's event channel |
| Reopen | `Reopen { creator, recipients: [RecipientSettings], reply }` (a declaration only, no built pieces) for a recipient declared again while its release waits at the barrier, its loop drains, or the close a retirement reaches (its deadline, a closure, an error while draining) has not become final: cancels the removal or the close, applies the new declaration, and the loop continues with the same queue and directory; replies `ReopenReply::Reopened`, and the cancelled release reports no exit. The host asks the loop's closer (`RecipientCloser::reopen`), which answers atomically with the loop's decision to close for good, and a reopen with no input left is refused. An abort that returns it to draining keeps the original deadline. If the loop's close is final, the host replies `ReopenReply::Exited`: the core waits for that incarnation's exit event and then prepares the next incarnation, whose queue adopts the directory (same identity, same directory, 4.9). |
| Fail | a loop that ends on an error outside a drain (`io.rs:853-858`) finishes its native closure (`io.rs:881-899`) and then reports `RecipientExited { recipient, outcome: Failed(reason) }`. Its slot stays allocated, and payloads naming it are dropped and counted, until the core's next incarnation for it, prepared into the same slot, is live (4.10), or until the core releases it: a released failed recipient frees its slot at once, and the payloads received before the release that name the slot are dropped and counted (a tombstone), so a recipient prepared into the slot meanwhile receives only payloads sent after the release; a loop that ends while its release waits at the barrier, and a recipient an abort drops, free their slots the same way. An abort that drops a failed recipient's next incarnation puts the failed marker back in the slot (P7.1b R1, P7.4 R4). **As built** (stage 7 review), a failed native recipient stays in the published native set while it waits for its backoff, so payloads routed over the native set are counted at its slot as natively, and a committed successor lets go of the failed incarnation it replaced; a payload that no recipient of the native set takes (no primary endpoint, for logs, events, service checks, and traces) is dropped and counted in `component_errors_total{error_type:http_send}`, and logged once per publication |

The drain-on-close mechanism exists only on the local branch `experiment/programmable-metrics-spike` (head
`e61e9b2ad6`, `lib/saluki-components/src/common/datadog/config.rs:279-372`, with the test
`queue_drain_dispatches_accepted_transactions_after_input_closes`); P7.1a ports it, since this clone may hold the only
copy.

**The input barrier.** A lease's last payloads enter the delivery host's input before the encode host acknowledges the
lease's drain (the `LeaseClosed` markers, 4.6), but the release arrives later on another channel, the control mailbox.
On `Release`, the routing loop records `target = received + input.queued()` and keeps routing to the releasing slots
until it has received `target` payloads and no send is pending: by FIFO on its single input, every payload enqueued
before the release has then been routed to every slot it names, one send at a time. This needs one accessor,
`Consumer::queued()` over `mpsc::Receiver::len` (tokio 1.53, `topology/interconnect/consumer.rs:24-44`), and no extra
message. A `debug_assert!` checks that no payload names a slot that holds neither a recipient nor the tombstone of one
that left after the payload was received.

**Process shutdown** keeps native closure: the input closes, every loop's close request becomes `Shutdown`, and loops
stop popping, finish in-flight work, and persist or drop, within the shutdown budget (`built.rs:641-663`). A loop
draining for retirement switches to shutdown closure. **A recipient loop that fails** stays native (later transactions
to it count as permanently failed), is reported in status and in `plan_recipients{state=failed}`, and reports its exit
on the event channel. The core marks it `Failed` and, with backoff (1 to 30 s), prepares its next incarnation into the
same slot through a resource transaction, which adopts the directory once the failed loop has exited. Leases keep
naming the slot, so recovery needs no plan change and no unrelated configuration edit.

**Host events.** The control mailbox is request and reply, so loop exits travel on a channel of their own:
`DeliveryHostConfiguration::with_events(mpsc::Sender<DeliveryEvent>)`, bounded at the table's size plus
`MAX_DRAINING_RECIPIENTS`, so a send never waits. Each incarnation's exit is sent exactly once, retired or failed. ADP
keeps the receiver in the installer's state outside the worker, as it keeps `acks`, so an exit reported during a worker
restart is not lost.

**Failover hosts.** `mrf_dd_out` is built with an empty table whenever the metrics pipeline is. Enabling Multi-Region
Failover is then a recipient prepare (`mrf_dd_out/primary/0/<url>`, credential `native:multi_region_failover.api_key`),
the MRF output's lease in `mrf_metrics_encode`, and the default program's MRF branch: no restart. `cluster_agent_out`
and `af_metrics_encode` are built, idle, whenever an `https` Cluster Agent endpoint resolves at boot (the host fact,
which no longer requires `cluster_agent.enabled` or a token), so setting `cluster_agent.enabled`, adding or rotating
`cluster_agent.auth_token`, and toggling autoscaling failover are live; the recipient exists only while all three
allow it. Its protocol keeps native behavior: path forced to `/series`, `DD-API-KEY` removed, the bearer token from the
recipient's credential cell inserted at dispatch (today the mapper fixes it when built,
`forwarders/cluster_agent/mod.rs:190-200`), no 403 retry, no validation, V2 series of scalar kinds only.

**Static mode.** Without a metrics pipeline no installer exists: `dd_out` runs a table built from the startup
configuration, with no control mailbox and native routing, which is exactly today's behavior for logs-only and
traces-only deployments. Every existing forwarder test runs against this mode. **As built**, static mode is the native
`TransactionForwarder` itself (its component loop, `io_loop` task, and endpoint loops), and only `::controlled(..)`
builds a delivery host; the host is pinned against it by
`a_host_booted_from_a_configuration_delivers_what_the_static_forwarder_delivers_for_it`. Folding static mode into the
host, and deleting `TransactionForwarder::spawn`, `run_io_loop`, and `EndpointSender`, is deferred (P7.1b D1).

### 4.8 Credentials

- **References, never values** (`CredentialRef`, `lib/saluki-metric-plan/src/ir/id.rs`):

  | Reference | Resolves to | Usable by |
  |---|---|---|
  | `native:api_key` | `shared.endpoints.api_key` (live) | lowering only |
  | `native:additional/<endpoint digest>/<index>` | `additional_endpoints[url][index]` (live) | lowering only |
  | `native:multi_region_failover.api_key` | the MRF key (live) | lowering only |
  | `native:cluster_agent.auth_token` | the bearer token (live) | lowering only |
  | `native:proxy.http`, `native:proxy.https` | the user information of a proxy URL (live) | lowering only |
  | `credentials/<name>` | entry `<name>` of `experimental.programmable_metrics.resources.credentials` (new Saluki-only key) | resources artifacts, and lowering |

- **The credentials key.** `experimental.programmable_metrics.resources.credentials` is one key whose value is a map
  from credential name to key, a new string-map value type in the overlay model (`config-overlay-model`, beside the
  `StringMapList` of `metric_allowlist`, `saluki_keys.rs:82`), with a JSON object as its environment form
  (`DD_EXPERIMENTAL_PROGRAMMABLE_METRICS_RESOURCES_CREDENTIALS='{"team_d":"..."}'`). ADP resolves no secrets itself:
  `ENC[...]` handles are resolved by the Core Agent in connected mode only, and a value that still reads `ENC[...]`
  when ADP sees it counts as not configured.
- **Liveness depends on the source, and one source is unverified.** Saluki-only keys reach ADP through the same merged
  map as Datadog keys (`saluki_only.rs:5-8`), and updates fold only the Agent's stream over a base fixed at boot
  (`system.rs:260-301`). The environment form lives in that base, so it never changes after boot: restart-only. From
  `datadog.yaml` in connected mode the key is live only if the Core Agent streams a key its schema does not publish,
  and resolves `ENC[...]` in it, which nothing in this repository shows. P7.5 pins which holds with a test against the
  Core Agent's configuration stream ([A36](#91-stage-7-assumptions)); if it does not stream the key, declared
  credentials are restart-only in every mode (rotation and removal included), every `ENC[...]` credential counts as
  not configured, and the overlay, the release note, and the live tests of 6.11 say so.
- **`CredentialSource`** (a `saluki-components` trait; ADP implements it over the configuration system's live views)
  resolves a reference to a view of `Option<MetaString>`. Each recipient's cell follows its view through one refresher
  per distinct reference, with the native `ApiKeyCell` store rules (trim; keep the last usable key on a blank or invalid
  update, `api_key.rs:68-92`). Rotation is not a plan change.
- **Removing a declared credential.** A `credentials/<name>` whose entry is absent from the map is not configured: the
  resources revision that references it goes `pending` at the next lowering, which withdraws and retires its
  recipients. An entry that is present but blank or invalid keeps the last usable key, as natively, and is reported in
  status. So removing a credential stops new payloads from using it at once, though a retiring recipient keeps sending
  its queued work with the last key until its drain ends (at most `DRAIN_DEADLINE`, less under a `discard` closure,
  `api_key.rs:63-92`); a transient bad value stops nothing.
- **F17 goes away**: an additional endpoint removed from configuration is a recipient that retires. Reordering keys
  within the key list of one URL keeps the native quirk: identity is by position, so the recipient at index 0, with its
  queued payloads (including, now live, its in-memory retries), continues under the key now at index 0, and data queued
  for one organization is delivered under the key of another. Recorded, release-noted, and not changed
  ([risk 13](#11-risks)).
- **Identity and conflicts.** At prepare the delivery host computes an in-memory key fingerprint (a keyed digest, never
  exported) per recipient. A declared recipient whose canonical endpoint and key fingerprint equal another live,
  non-draining recipient's on the same host fails its prepare ("duplicates recipient X"); a draining recipient never
  counts, so renaming a credential does not fail for the drain's 30 s. Native configuration wins over a declared
  recipient: a native recipient that duplicates a live declared one prepares, the conflict is reported in status and
  `plan_resource_conflicts`, and the declared recipient keeps running (each sends only its own outputs' data). A later
  rotation that makes two references resolve to one key is reported as a conflict, without stopping either (the
  specification's "reported as a conflict"). Native duplicates keep native behavior (amendment 13).
- **Unresolved credentials** are detected at lowering, never at prepare: the resources config goes `pending` with the
  credential's name, lowering excludes its recipients and outputs, and it is re-evaluated on every configuration change.
- **Secrets gate.** `SecretsGateRefresher` (`retry.rs:54-97`) stays per delivery host and covers every recipient of it.
- **Scoped release.** A retiring recipient drops its cell, and its refresher when no other recipient uses the reference.

### 4.9 Queued and persisted work

| Concern | Choice |
|---|---|
| Queued work of a retiring recipient | attempted until `DRAIN_DEADLINE` (30 s), then native shutdown closure; counted with the existing drop telemetry; summarized in the `DrainReport` |
| The encoder's final flush at retirement | the lease drains first and its payloads pass the input barrier before the recipient is released, so they are attempted like any queued work |
| Credentials at rest | native: a persisted retry holds the headers it was sent with, and the request mapper overwrites the credential at dispatch, so a rotated key is used after a restart too. Stripping credentials before persisting is a follow-up ([A18](#91-stage-7-assumptions)): the owner's direction does not ask for hardening yet, and it would change the persisted format of every payload `dd_out` carries. |
| Directory identity | native: `generate_retry_queue_id` (`io.rs:901-920`), `{component}/{resolved host}/{hash}` with the hash over (component, raw URL, key index) for additional endpoints and (component, resolved endpoint) otherwise; a declared recipient hashes (`dd_out`, canonical endpoint, credential name). The resolved host carries the data plane's version for a Datadog site (`<major>-<minor>-<patch>-<id>.agent.datadoghq.eu`), so an upgrade moves every Datadog endpoint's directory, as today (A19's follow-up). A recipient added live gets the directory a restart with that configuration would give it. A declared recipient renamed with the same endpoint and credential keeps the directory, which its predecessor may still own: that revision is rejected (P7.4 R3). **As built** (stage 7 review), a native recipient's identity names its endpoint as its directory does (`queue_endpoint`): an additional endpoint's as configured, any other's as resolved, without its root path and with the version label written as the `app` label it replaced, so no two identities share a directory, and a primary endpoint respelled live (a trailing slash, a default port, a site and the URL it implies) stays one recipient whose settings a publication changes. Version 1 fingerprints keep naming recipients as configured. |
| Orphaned directories | as natively: a removed recipient's directory stays, and is adopted, with its backlog, when a recipient of the same identity returns (a configuration flap, a resources redelivery after a restart); its files age out (`forwarder_outdated_file_in_days`) only while a loop runs on it. Removing an endpoint and restarting orphans it the same way today. A janitor for orphans is part of the follow-up ([A19](#91-stage-7-assumptions)). |
| A replaced endpoint | the old recipient (another ID, another directory) drains for at most 30 s beside its successor; each holds only its own bound |

F20's code-reading claims (credentials in persisted files, overwritten at dispatch; directory names as above) are
pinned by tests of today's behavior in P7.1a, so the follow-up starts from verified facts.

### 4.10 Installer integration

**The resource table** lives inside `InstallerCore`, so it survives worker restarts with the rest of the installer
state, and is I/O-free:

```rust
// saluki-metric-plan::install::resources
pub struct ResourceTable { hosts: BTreeMap<HostId, HostResources>, revision: ResourcesRevision, next_incarnation: u64 }
struct HostResources {
    leases: BTreeMap<EntryId, Lease>,                  // every lease, draining ones included
    live: BTreeMap<LeaseKey, EntryId>,                 // LeaseKey = (OutputId, binding digest), to its Live lease
    recipients: BTreeMap<RecipientId, Recipient>,      // one per identity; a successor waits for its exit
    published: Option<ResourcesRevision>,              // the native set, settings, and withdrawn set the host runs
    transaction: Option<InFlight>,                     // at most one resource transaction per host (below)
}
enum User { Generation(GenerationId), NativeSet }      // the published native set holds the recipients it names
struct Lease { key: LeaseKey, users: BTreeSet<GenerationId>, state: LeaseState }        // Preparing | Live | Draining
struct Recipient { reference: RecipientRef, users: BTreeSet<User>, decl: RecipientDecl, state: RecipientState }
    // Preparing | Live | Releasing | Draining | Failed; `successor`: declared again after its loop exited
pub struct RecipientRef { pub slot: u16, pub id: RecipientId, pub incarnation: u64 }
pub enum CreditKey {                                   // one entry of the resource budget's ledger (4.12)
    Lease(HostId, EntryId, u64), Recipient(HostId, RecipientRef), HostQueue(HostId), Instances(GenerationId, SlotId),
}
impl ResourceTable {
    /// Per host: leases to reuse or create, recipients to require (reused), reopen, or create, never to reconfigure
    /// (only resource transactions change a live recipient's settings), the lease table of every slot's preparation,
    /// and the credits the preparation adds, per resource (4.12).
    pub fn plan(&self, candidate: &Plan) -> ResourcePlan;
    /// The resource transaction a lowering needs: the native recipients its native set adds, then what each host
    /// publishes (the native set, every recipient's settings, native and declared, and the recipients it withdraws),
    /// with the credits a bound raised in place adds.
    pub fn transaction(&self, lowered: &Resources) -> Option<ResourceTransaction>;
}
```

A generation whose plan binds an output exactly as a `Live` lease does (equal `LeaseKey`, provenance excluded) reuses
that lease, so unchanged outputs keep their builders, open payloads, and flush timers across installs (specification:
"Generations reuse a resource only when its stream, batching, and lifecycle contracts permit it"). A `Draining` lease is
never reused: an equal binding declared again gets a new lease, under a new `EntryId`, beside it. A lease's users are
the live or kept generations whose plan uses it; a recipient's are the generations whose plan declares it, and the
published native set when it names the recipient. A failed recipient is never reused: its next incarnation is prepared
into the same slot once the failed loop's exit is reported (4.7), and a recipient declared again after its loop exited
waits as a `successor` the same way.

```text
lowered(configuration)              every lowering, independent of the metrics candidate below
  a resource transaction per host whose native set, recipient or host settings, withdrawn set, or failed recipients
  differ from what it published; at most one in flight per host, for the newest lowering:
    PrepareResources { revision, hosts: [HostPrepare { host, create, reopen }] }   the recipients the native set adds
       each host acknowledges within PREPARE_TIMEOUT: resources_prepared(revision, host, result), else AbortResources
    PublishResources { revision, hosts }        native sets without the additions that failed, settings, withdrawn sets
    ReleaseRecipients { host, recipients }      at once, for recipients the publish left without a user
propose(candidate)                  the metrics generation, as in stage 6
  Prepare { generation, slots, hosts: [HostPrepare { host, leases, create, reopen, requires }] }
     each acknowledges: prepare_acknowledged(generation, PrepareSite::{Slot, Host, Budget { resource }}, result)
     barrier: every acknowledgment within PREPARE_TIMEOUT (10 s), else Abort { generation, slots, hosts }
  Commit { generation }               or, for a cut: ArmCut, WatchCut, then FinalizeCut or CancelCut (stage 6)
retired(g)
  ReleaseLeases { host, leases }                leases no live or kept generation uses  -> lease_released(..)
  ReleaseRecipients { host, recipients }        once no lease names them and no user holds them
host events (4.7)                   recipient_retired(RecipientRef, DrainReport), recipient_failed(RecipientRef, reason)
whenever a creation ends            ReleaseCredits { credits }  (retired, released, aborted, refused, or superseded)
```

- **Resource transactions** carry only state that outlives generations: the native set for non-plan payloads, host
  settings, every recipient's settings (native and declared), and the withdrawn set. A transaction prepares the
  recipients its native set adds (a declared recipient, which no native set names, is created by the generation that
  first declares it), then publishes whatever those prepares returned: an addition that failed is left out of
  the native set, reported, and retried with backoff (1 to 30 s) by a later transaction, while the removals and the
  withdrawn set publish regardless, so a failed addition never keeps a removed destination receiving. A removal
  publishes first (the `NativeSet` user drops at the publish, and a recipient left without a user is released right
  after it, since the host handles its mailbox in order), and a recipient still named by a generation's lease is
  released once none uses it. A transaction never waits for a cut or a metrics prepare, and a metrics prepare failure
  does not hold it back, so a logs dual-ship endpoint follows its configuration within one prepare.
- **Withdrawal.** The withdrawn set of a host is every recipient of its table that the latest lowering does not declare,
  native or declared (4.7). Metrics to a removed recipient therefore stop at the transaction that follows the change,
  not when a generation without it commits: one edit that removes endpoint A and adds an endpoint B that fails to
  prepare, a re-composition that fails to compose, an artifact still preparing when its resources are revoked, and a
  rollback to a plan that used A all leave A withdrawn. The last payloads of A's old leases are dropped and counted
  rather than delivered, which is the removal's meaning.
- **Only transactions reconfigure.** A live recipient's settings (protocol, `DeliveryPolicy`, and its host's settings)
  change only through a resource transaction, versioned by `ResourcesRevision`: a generation's prepare creates,
  reopens, or requires recipients and never reconfigures one, so an aborted generation or a cancelled cut leaves no
  setting behind, and a generation lowered earlier cannot overwrite settings a newer transaction published.
- **One transaction per host at a time, for the newest lowering.** A newer lowering replaces a transaction that has not
  published: `AbortResources { revision, hosts }` releases what it prepared that the newer one does not need, and
  returns its credits. A host that does not acknowledge `PrepareResources` within `PREPARE_TIMEOUT` gets
  `AbortResources`, and the transaction is retried with backoff. Prepares and publishes are idempotent per revision and
  `RecipientRef`. `ResourcesRevision` (`u64`) grows with every publication, and a host ignores an older one. A short
  window in which a generation's payloads meet older settings is harmless: settings change how a recipient sends,
  never which payloads reach it (that is the resolved sets' and the withdrawn set's, 4.6, 4.7).
- **Prepare** builds every new lease's builders and every new recipient's pieces in the installer task, off the data
  path, through the factories each host exposed at build. A recipient prepare opens its persisted directory as 4.7
  describes and builds a client; it reads no file contents. A generation's `HostPrepare` lists in `requires` every
  recipient its leases name that it neither creates nor reopens; the host fails the acknowledgment when one is not
  `Live`, so a generation never commits over a recipient whose transaction failed to prepare it, and it is retried
  after that transaction is. Before dispatching a `Prepare` or a `PrepareResources`, ADP acquires the credits the plan
  lists that it does not hold yet (4.12); a `Strict` refusal is the failed acknowledgment of
  `PrepareSite::Budget { resource }`, which names the resource, so the core attributes it (4.16).
- **Abort** releases only what the aborted generation created, and returns those credits; a recipient it reopened goes
  back to draining with its original deadline, so repeated prepares and aborts cannot keep a drain from ending.
- **Commit** is unchanged and publishes nothing.
- **Retirement is two-phase per host**: leases, then recipients, because a recipient may be released only after the last
  payload addressed to it has been routed (the encode host's drain acknowledgment, then the input barrier), so no
  payload ever names a slot that was freed and reused.
- **Cuts.** A cut generation's leases and recipients are prepared with its `Prepare`, like any generation's, and nothing
  is published for it: metrics payloads name their recipients, and the native set follows resource transactions. A
  cancelled cut therefore leaves nothing behind: `Abort` releases what its generation created, and no native set names a
  recipient only that generation held. Leases switch with the generation a batch carries, and time-driven output belongs
  to the lineage's newest committed generation (amendment 69), so flushes after a commit use the new leases: the
  specification's "changed destination" trace (`retain`, flushes go to `B`). A native endpoint removed while a cut is
  in flight stops receiving everything at the resource transaction, which withdraws it; the cut's candidate waits as in
  stage 6 (amendment 79, at most `lcm` + 1 s, 301 s at a 300 s width), and the recipient drains after it.
- **The overlap budget** (4 live generations) bounds leases to four per output plus those draining, and recipients to
  the union of four plans' declarations, the published native set, and at most 16 draining per host.
- **Last-known-good.** A kept generation is live for resource purposes: its leases and recipients stay prepared, so a
  rollback is still one store. At every commit, and whenever a lowering arrives (`on_default_program`, which every
  configuration change and resources event leads to), the core keeps a generation (the one it replaces or the one kept
  before) and a cut's rollback plan only while every output its plan declares is declared by the latest lowered plan
  with an equal fingerprint, and every recipient its plan declares is declared there with the same protocol;
  otherwise it releases the generation, or drops the rollback plan in favor of the re-composition. Additions therefore
  keep it, and a removal, a rebinding, or a protocol flip releases it as soon as it is lowered, so a rollback never
  resurrects a removed destination ([amendment 100](#10-proposed-spec-amendments)), and the withdrawn set covers the
  time before the core sees the lowering. A rollback publishes nothing: the native sets, settings, and withdrawn sets
  stay those the resource transactions published from the current configuration, and until the re-composition
  (`InstallDefaultProgram`, as today) commits, metrics follow the kept generation's bindings, so an endpoint added
  since then misses them for that time.

**The ADP side.** `Installer` (`programmable_metrics/installer.rs`) gains the hosts' `ControlSender`s, created at boot
with `execution::control::control_mailbox` like the regions' (take-once receivers passed to the host builders through
the `MetricsWiring` hooks; `dd_out` is built in `run.rs` for every pipeline, so the plan hook supplies its mailbox),
and the receiving ends of their event channels (4.7). Boot waits for every host's `running()` as it does for regions;
hosts mark their mailbox running when they start (F22). `InstallerCore::new` returns the boot generation's resources
(lease IDs, slots and incarnations, the first published revision), and the topology builds the hosts with them, so
`dd_out` routes logs from its first payload. Resource actions are queued and performed one at a time, like every other
action, but a release is not awaited: ADP sends `Release`, which the host acknowledges once accepted, and the core
advances when the host's event channel reports the exit (`recipient_retired`), so a 30 s drain never stalls a commit
or a candidate. `PlanStatus` gains `resources: { declared: { state, revision, installed_revision }, outputs,
recipients: [{ id, host, state, incarnation, draining_since }], pending, degraded, conflicts, withdrawn }`, also on the
stub poll.

### 4.11 Scoped registration

| Registry | Choice | Release |
|---|---|---|
| Health | per host only, fixed at boot (F2: nothing unregisters a component, and nothing needs to) | none needed |
| Resource groups | per host (the component group, as natively); per native output name for its lease (bounded by the native output set), below the encode host's identity (`<host identity>.leases.<output>`), through `runtime::worker(..).with_resource_group(name)`, which I7 adds to `saluki-core` (additive, no behavior change); one group per encode host shared by every declared output (`<host identity>.resources`). Groups are flat statistics: a host's bytes are its group's and those below it | none: bounded names (F2) |
| Internal metrics | per lease (encoder telemetry under the output's native `component_id`) and per recipient (queue, retry, transaction series) | handles dropped at retire, evicted 3 s later; a successor that registers the same keys continues the series |
| Forwarder caches | the per-domain caches (`data_points_sent_by_domain`, `data_points_dropped_by_domain`, the retry-queue gauges by domain) and `SharedTransactionQueueTelemetryInner::per_endpoint` become reference-counted by domain; `per_endpoint` is keyed by recipient ID, which fixes two keys on one URL overwriting each other, and the per-domain gauge sums the recipients on the domain. `http_errors_by_code` is keyed by status code (`telemetry.rs:104`), bounded, and stays. | a per-domain entry goes with its last recipient, so a retired recipient's last queue size stops counting in totals; a domain that returns more than 3 s later restarts its running-total gauges (`component_data_points_sent_total`, remapped as `points.sent`) from 0, as after a restart |
| Validation targets | native `DatadogIntake` recipients, following the table | at retire |
| Credentials | a cell per recipient, a refresher per reference | at retire, and with the reference's last user |
| Diagnostics | the HTTP inspection layer per recipient | at retire |
| Memory bounds | 4.12 | credits returned once per resource, when it ends, however it ends |
| Tasks | native names for native recipients (`endpoint_io_loop_<host>`); `recipient_io_loop_<declared name>` for declared ones (at most 16, names sanitized); a lease's builder per shard, below the encoder's task (`<host>.encoder.request_builder`, where the native one was `<host>.request_builder`). The component's `io_loop` task and the per-endpoint `api_key_refresher_*` tasks disappear: the routing loop does their work, its credential followers being futures inside it | end with the task |

**The specification's exit gate, "resources reclaimed after repeated installs":** after N add-and-remove cycles of a
recipient and an output, the task count, live metric handles, forwarder cache entries, credential refreshers, scopes,
budget credits, and the hosts' allocator group live bytes return to their baseline ([6.11](#611-stage-7-tests) lists the
test).

### 4.12 Budgets

| Budget | Choice | Why |
|---|---|---|
| Per-recipient limits (`forwarder_retry_queue_payloads_max_size`, the high-priority buffer, `forwarder_storage_max_size_in_bytes`) | native: each recipient has its own bounds | the specification's "configured limits ... mean the same thing" holds: a value means the same for every recipient however many outputs feed it |
| A delivery host's boot bound | unchanged for a host with boot recipients: one queue per host (`forwarders/datadog/mod.rs:140-166`); an idle host (no recipient at boot) declares only its minimum | correcting it to per recipient would make existing multi-endpoint configurations fail strict boot verification (about 320 MiB per recipient); a known underestimate. An idle `mrf_dd_out` counting its queue would add about 335 MiB to every deployment's boot bound |
| The resource budget | one `StateBudget` per process, separate from the context budget, with capacity `grant − boot firm total + boot resource bounds`; the boot generation then acquires its own resource bounds, so every release returns what was acquired (`budget.rs:63-77` treats releasing more as a bug) | boot verification stays exactly native; later changes are checked against the same grant |
| What a prepare admits | its new leases (two `Metric` arrays of `max_metrics_per_payload` per shard, `metrics/mod.rs:502-524`, plus S × `LEASE_QUEUE_BUFFERS` full buffers); an idle host's native queue bound while it has any live or draining recipient (acquired with the first, returned when the last retires: what a restart with that configuration would count); each declared recipient's own bound (its `queue_bytes`, else the native retry queue bound, plus the high-priority buffer × the largest compressed payload); the generation's new kernel instances and memos times their lanes (each operator's declared bounds, `ConfiguredOperator::specify_bounds`; retained instances add nothing) | the status document's "bounds cover the boot program only" closes: every later generation's memory is counted |
| Growth in place | a resource transaction that raises a live recipient's bound (a declared `queue_bytes`, or `forwarder_retry_queue_payloads_max_size`, every declared recipient's default) admits the growth before it publishes (`ResourceTransaction::admitted`); a shrink returns its credits after the publish | a bound raised live is counted as a restart would count it |
| Mode | the boot verification's own (`lib/saluki-app/src/accounting.rs:81-120`, which P7.5 changes to return the verified grant, firm total, and mode): `Strict` refuses the prepare, `Permissive` warns and proceeds, `Disabled` or no limit skips | a live change that adds nothing old resources do not free succeeds when a restart with the same configuration would; overlap (below) is the exception |
| Overlap | a rebound output's old and new leases, and a replaced kernel's old and new instances, hold credits until the old one retires | the specification's "overlapping generations draw from a declared, bounded budget"; near the limit, a live change can be refused where a restart, which has no overlap, would pass |
| Declared resources | at most 16 recipients and 16 outputs; a declared recipient's queue at most the native bound (4.13) | a resources artifact adds at most 16 recipient queues within native bounds and 16 leases, each admitted like a live native change |
| Draining recipients | at most `MAX_DRAINING_RECIPIENTS` (16) per host; one more closes the oldest draining recipient early with its closure | flapping configuration cannot grow them without bound |
| Artifact memory | `ARTIFACT_MEMORY` (64 MiB) stays a validation limit per instance, which is lane-independent; the real total, times lanes, is what the prepare admits | validation and plan hashes stay free of lane counts |

**The credit ledger.** The core computes the credits per resource, `ResourcePlan::admitted` and
`ResourceTransaction::admitted` as `Vec<(CreditKey, u64)>` (`CreditKey` names a lease, a recipient incarnation, an idle
host's queue, or a generation's kernel instances in one slot, 4.10), from the compiled program and the resource table.
ADP holds the budget and a ledger of the credits it holds, in the installer's state outside the worker. Before
dispatching it acquires, with `try_acquire`, every listed credit it does not hold yet, so an action repeated after a
worker restart acquires nothing twice; a refusal names its `CreditKey` in `PrepareSite::Budget { resource }`. It
releases on `Action::ReleaseCredits`, which the core emits whenever a creation ends, however it ends: a lease released,
a recipient incarnation retired or failed, a generation's instances retired, and whatever an `Abort`, an
`AbortResources`, a refused or failed prepare, a cancelled cut, or a superseded transaction created. Default-program
candidates that retry every 1 to 30 s therefore never accumulate credits, and every credit returns exactly once.

### 4.13 The delivery-policy slot

Open to resources artifacts, mapping onto native mechanisms only:

```rust
// saluki-metric-plan::extension
pub enum DeliveryPolicy { Native, Bounded(BoundedDelivery) }      // Bounded: declared recipients only
pub struct BoundedDelivery {
    pub queue_bytes: Option<u64>,     // in-memory retry queue bound, at most the native configured bound
    pub spill: Spill,                 // Native (as configured) | Never
    pub on_retire: Closure,           // Attempt (until the drain deadline) | Persist | Discard
}
pub enum Overflow { Wait, EvictOldest }   // outputs: Wait; recipients: EvictOldest (the native retry queue)
pub enum Closure { Native, Attempt, Persist, Discard }
```

A dual-ship destination added for an experiment should not take the primary's memory, should not fill the disk, and may
not deserve 30 s of retries when removed: these three knobs cover that. `queue_bytes` above the native bound and spill
for a host without storage are validation errors. `explain` shows each recipient's policy; the diff shows a policy
change as a reconfiguration, applied in place at a resource transaction's publish (a shrinking queue evicts its oldest
entries, counted as `queue_limit` drops; a growing one is admitted first, 4.12). Priorities, other overflow rules, and
reactive policies stay reserved (stage 10). Delivery queues take no credits from a budget in stage 7, though the
specification has them do so from stage 7: they keep native limits, and a declared recipient's bound is admitted at
prepare (4.12; [amendment 102](#10-proposed-spec-amendments)).

### 4.14 Pending and degraded

- **Pending:** a new policy artifact whose base records an output that is not declared (4.5), as when the policy is
  published before the resources that declare its output; a resources config whose credential is not configured.
  Pending is a held state with a reason, not a transient one (the stub protocol documents it).
- **Degraded:** the artifact in effect, re-composed after an output it records was removed or rebound: it runs with
  those emits dropped and counted (4.5), and its acknowledgment is `active` with the reason. A native configuration
  that cannot be re-resolved (a recipient that keeps failing to prepare, 4.16) is reported as `status.degraded` with the
  reason: this is what the specification means by "reports a degraded state" for an invalid re-resolution.
- **Re-evaluation:** on every default-program candidate, that is, every configuration change and every resources
  snapshot. `on_default_program` re-composes the policy in effect (restoring its emits when the output returns) and
  each pending artifact, newest revision per scope first; one that now composes proceeds as a new installation.
- **Holds nothing:** a pending artifact has no generation and counts against no overlap budget. Its revision counts as
  seen (amendment 54). A newer artifact for its scope supersedes it; revocation removes it. Its acknowledgment is
  `pending` with the reason ("waiting for output `resources_team_d`").
- **Bounded:** at most one pending artifact per scope (the newest); the core keeps the document, as it keeps the policy
  in effect.

### 4.15 Telemetry continuity

| Series | Before | After | Release note |
|---|---|---|---|
| Encoder telemetry (`component_events_dropped_total{drop_reason:encoder_failure}`, V3 serializer metrics) of `dd_metrics_encode`, `metrics_routing_encode_<i>`, `mrf_metrics_encode`, `af_metrics_encode` | per native encoder | per lease under the same `component_id` and `component_type:encoder`, continuous across rebinding, summed over shards | no |
| Interconnect, health, and task series of `dd_metrics_encode`, `mrf_metrics_encode`, `af_metrics_encode` | per encoder | per encode host: `dd_metrics_encode`'s now counts every output of `dd_out` | yes |
| Interconnect, health, and task series of `metrics_routing_encode_<i>` | per encoder | gone, as fused transforms' and gateways' went in stages 3 and 4 | yes |
| `metrics_enrich` sent per exit (`output:metrics_routing_encode_<i>`) | per output | per encode host (`output:dd_metrics_encode`) | yes |
| Forwarder series keyed by `domain` and `endpoint` | per forwarder | unchanged for native recipients; per-domain caches now release (4.11). **As built** (stage 7 review), each clone of a host's switchable client keeps one copy of the client it switched to, with its per-endpoint handles, from request to request: before, every request took a fresh copy, so `network_http_requests_*` and their remapped `transactions.*` series reset within 3 s of each request | the fix |
| `mrf_dd_out`, `mrf_metrics_encode` for operators without failover | absent | health series present and ready; interconnect series registered when the topology is built; queue and lease series with the first recipient or lease | yes |
| `cluster_agent_out`, `af_metrics_encode` | present only when autoscaling failover was requested at boot | present, idle, whenever an `https` Cluster Agent endpoint resolves at boot, which is most node Agents in Kubernetes | yes |
| The forwarders' `io_loop` task, and their `api_key_refresher_<endpoint>` tasks | one per forwarder, and one per endpoint key | gone where a delivery host runs (with a metrics pipeline): the component loop is the routing loop, and the credential followers are futures inside it; unchanged in static mode | yes |
| The metrics encoder's `request_builder` task | `<encoder>.request_builder` | `<encode host>.encoder.request_builder`, one per lease shard | yes |
| `metrics_routing_encode_<i>` encoder series | per routing group | per position: after a group is removed, `metrics_routing_encode_0` continues with the former group 1's data; the installer logs the renumbering with both group keys | yes |
| `component_events_dropped_total{drop_reason:payload_too_large, intentional:false}` of an encode host's leases | uncounted (a TODO) | counted per lease | new |
| `component_data_points_sent_total{domain}` (`points.sent`) | a running total for the process | restarts from 0 when a domain returns more than 3 s after its last recipient retired | yes |
| Declared outputs | none | encoder telemetry under `component_id:resources_<name>` | new |
| Plan-level | | by each delivery host, under its `component_id`: `plan_recipients{host, state}` (`active`, `draining`, `failed`), `plan_recipient_retirements_total{host, outcome}` (`drained`, `persisted`, `deadline`, `discarded`; as built, `persisted` counts a close that wrote work to disk, such as a `persist` closure, which `drained` counted before), `plan_recipient_withdrawn_payloads_total{host}`, `plan_resource_conflicts{host}`; by each encode host: `plan_loss_total{class:invalid}`; by the installer: `plan_live_leases{host}`, `plan_resource_budget_bytes{state}` (`granted`, `used`) | new |

The Agent remaps key on names and on `domain` and `endpoint`, never on encoder component IDs
(`bin/agent-data-plane/src/state/metrics/rules/serializer.rs`, `transaction.rs:10-50`), so remapped output is unchanged.
Forwarder and encoder telemetry remain the source of truth for what a drain delivered or dropped; `retired`
acknowledgments summarize it (specification, Lifecycle).

### 4.16 Failure and rollback

"Nothing unrecoverable" means every state this stage adds can be undone by revocation, a configuration change, or a
restart, and what it persists is what native configuration would persist for the same endpoints.

| Failure | Behavior | Recovery |
|---|---|---|
| A native resource fails to prepare (invalid proxy, unreadable directory, a strict bounds refusal) | in a resource transaction: the addition is left out of the native set and retried, while the transaction's removals and withdrawals publish; in a generation: the barrier fails (directly, or through `requires`), `Abort` everywhere, and the active generation keeps running, with removed recipients already withdrawn; default-program candidates and resource transactions retry with backoff (1 to 30 s); `status.degraded` names the resource and the reason; artifacts composed with it are not rejected (they compose again with the next candidate) | fix the configuration |
| A declared resource fails to prepare (unreadable directory, a duplicate key, a strict bounds refusal), attributed by the slot's failed acknowledgment, which names the recipient, or by `PrepareSite::Budget { resource }` | the core marks that resources revision `rejected` with the reason and asks for a lowering without it (the revision installed before it, if any, keeps running), so native configuration changes keep applying; policies emitting to outputs only it declares go `pending` (new) or degraded (in effect); no retry. A budget refusal by an artifact's kernel instances is that artifact's prepare failure, never the resources revision's | republish a corrected resources artifact |
| An unresolved credential | never a prepare failure: detected at lowering; the resources config is `pending` and excluded (4.8) | configure the credential |
| A host does not acknowledge in time | as any slot: abort, retry | automatic |
| A builder task panics | the encode host fails and the process ends, as a native encoder failure does (amendment 1) | restart; a panic a declared resource causes recurs after the restart, when the resources are redelivered, until the product is revoked |
| A recipient loop ends on an error | native: later transactions to it fail permanently; reported on the host's event channel, in status, and in `plan_recipients{state=failed}` | automatic: the core prepares its next incarnation into the same slot with backoff (1 to 30 s), which adopts the directory once the failed loop exited |
| A resource transaction is superseded, or its host does not acknowledge in time | `AbortResources` releases what it prepared that the newer lowering does not need, and its credits | automatic: the transaction for the newest lowering runs, retried with backoff |
| An artifact generation fails under supervision | rollback to last-known-good (one store), or to the default program if the keep rule released it | automatic |
| The resource authority removes a resource an artifact uses | the artifact in effect keeps running with emits to it dropped and counted, degraded (4.5); a new one is `pending` | the resource returns, or the artifact is rebased |
| The installer worker panics mid-action | restarts with the core's state; at most the action in progress repeats (prepares are idempotent per generation or revision and `RecipientRef`, releases per `RecipientRef`, which hosts check; credits per `CreditKey`; host events wait in the channel) | automatic |
| ADP restarts | persisted queues are adopted by native directory naming; declared resources return with the stub's redelivery; meanwhile the default program runs and policies that use declared outputs are pending | automatic |
| A mistaken resources artifact | it can send metrics (not logs or traces) to a declared endpoint with an operator-declared credential; revoking the product withdraws everything it declared at the next resource transaction and retires it within the drain deadline | revoke |
| Disk full while a retiring recipient persists | dropped and counted, as at native shutdown | none needed |

### 4.17 Worked traces

**A. A dual-ship endpoint added through native configuration (connected mode).** The Core Agent sends
`additional_endpoints: {https://dual.example.com: [k2]}`. The installer lowers the current configuration: `dd_out` gains
recipient `dd_out/additional/0/https://dual.example.com` (classes `All`, credential `native:additional/<digest>/0`), and
the main output's binding gains it. First, a resource transaction: `dd_out` prepares slot 3 (cell, directory adopted
if present, telemetry, loop), then publishes a native set with slot 3, so logs and traces dual-ship at once. Then the
metrics generation: the main output's selector is the role `main`, so every fingerprint and base is unchanged and an
artifact in effect re-composes, neither stale nor invalid; `dd_metrics_encode` prepares lease 5 for the main output
(variants and resolved sets recomputed); `metrics_enrich` gets the new lease table with an unchanged program. Commit:
one store. The previous generation retires within milliseconds; lease 1 drains with one early flush to the old
recipient set and is released.

**B. That endpoint removed again.** The reverse: a resource transaction publishes a native set without slot 3 and
withdraws its recipient, so logs, traces, and metrics stop going there at once; lease 5's payloads that still name slot
3 are dropped there and counted. The metrics generation without it commits, and a kept last-known-good that used slot 3
was released when the core saw the lowering. Lease 5 drains; the input barrier routes its last payloads (slot 3's sends
dropped); the recipient, held by nothing now, is released; its loop drains its queue for at most 30 s and retires; its
series disappear 3 s later; its directory stays, as natively, and is adopted if the endpoint returns.

**C. A declared endpoint and a policy routed to it, policy first.** Policy revision 7 emits to `resources_team_d` with
the fingerprint the editor computed: not declared, so `pending`, and whatever ran keeps running. Resources revision 1
validates; lowering adds the declared output and recipient; the default program's export is unchanged (its hash too).
`on_default_program` re-composes the pending policy, which now composes, and proposes one candidate: a new recipient
(its queue bound admitted against the headroom), a new lease, new region programs. After the commit Team D receives the
platform family; resources and policy are both acknowledged `active`. An empty resources snapshot then drops the
declared output: the resource transaction that follows withdraws team D's recipient, so team D stops receiving at
once; the policy in effect re-composes with its emit to `resources_team_d` turned into a counted drop, `active` and
degraded, while the primary keeps receiving what the policy sends it; the team D lease drains, and its recipient
retires. Publishing the resources again restores the emit.

**D. V3 series turned on live for one endpoint.** `use_v3_api.series.endpoints` gains the dual-ship endpoint of trace
A. Its recipient's protocol now accepts V3 series and not V2 series (F21), so the main output's binding produces both
and lease 5 is replaced by lease 6, whose resolved sets give V2 series to `{0}` and V3 series to `{3}`; fingerprints
and bases do not change. Lease 5's last V2 payloads still name `{0, 3}`, and slot 3's loop sends them, since a payload
with a resolved set is not filtered (4.6); lease 6's V3 payloads reach slot 3 even when they beat the resource
transaction that publishes the recipient's new protocol. Continuous-input counts stay exact per recipient.

**E. Multi-Region Failover enabled live.** `mrf_dd_out`, idle since boot, prepares its recipient, admitting the host's
native queue bound (about 335 MiB at the defaults) against the headroom in the boot verification's mode;
`mrf_metrics_encode` prepares the MRF lease; the default program gains the MRF branch, so artifacts become stale (and
keep running, not mirroring, until rebased). After the commit, mirroring follows the gateway's live bindings.

**F. A routing group removed while an artifact emits to the next group.** Group 0 goes; group 1 becomes index 0. The
artifact emitted to `metrics_routing_encode_1` (recorded key K1); that output no longer exists, so the re-composition
turns its emits into counted drops and the artifact runs degraded, its status naming the reason (output renumbered: the
recorded group is now `metrics_routing_encode_0`; rebase). Until the author rebases, the artifact's selection for group
K1 is dropped, visibly and counted, rather than the whole scope falling back to the default program
([A6](#91-stage-7-assumptions) records the trade-off of positional IDs).

### 4.18 Alternatives considered

| Alternative | Why not |
|---|---|
| A dynamic Saluki topology (components added and removed live) | 2,000 to 4,000 lines in `saluki-core` touching every component type's lifecycle, readiness, and memory accounting, and it still needs live recipients inside `dd_out`, which every pipeline shares (F17) |
| Native encoder components for native outputs, one multiplexing host for new outputs only | two encoder paths, and native outputs would still not be live |
| One builder task for all outputs of a host | serializes compression across outputs: a regression for multi-output configurations |
| Folding `mrf_dd_out` and `cluster_agent_out` into `dd_out` as recipients | changes forwarder identity, health, and the persisted queue path for every failover user; separate routing loops keep a failover region's trouble out of `dd_out` |
| Fingerprints over resolved recipients (today's rule) | a dual-ship endpoint would reject every artifact emitting to the main output (`compose.rs:964-972`) |
| The current generation keeps running when its resource is removed (the specification's text) | keeps a removed destination alive and receiving |
| The default program runs when a resource an artifact uses is removed | fails open: everything else the artifact does is undone (exclusive routing back to the primary, drops delivered again), even after an unrelated configuration edit |
| Publishing the native set at the commit of the generation that resolved it | ties logs, events, and traces to the metrics installer: a cut delays an endpoint change by up to 300 s, and a metrics prepare failure blocks it |
| Stripping credentials at rest, version-stable directory names, upgrade adoption, and a janitor, in stage 7 | not needed to make outputs live, changes the persisted format of every payload on `dd_out`, and orphans every old directory once; recorded as a follow-up (A18, A19) |
| Per-lease payload channels instead of the `LeaseClosed` marker | the front task would select over a changing set of channels; one shared channel with an in-band marker keeps native's single drain loop |
| Per-recipient firm bounds | correct, but existing multi-endpoint configurations would fail strict boot verification |

## 5. Stage 9 design

### 5.1 Strategy and decisions

Every metrics aggregator that scales on one host partitions by series key on the producer side, gives each partition its
own state, and runs no exchange stage between them (the Datadog Agent's DogStatsD time samplers are the closest case).
This design does the same within the stage 3 frame: **producer lanes** run the fused pre-aggregation program in
parallel, one per decoder, each its own admission point; the lane routes every event by its final context key, at the
exit that already leads into aggregation, to one of P **state lanes**; **post-aggregation lanes** take the state lanes'
output and passthrough. Contexts are disjoint across state lanes, so every per-context rule is exact; the few native
rules that couple contexts (the tick's flush time, the idle-counter back-fill start, the flush count, the breach
warning, the shutdown time) become small records shared by the lanes of one lineage instance, touched a few times per
tick. Parallelism then comes from lanes, so R1's cost model goes.

| # | Decision | Adds per event | Per new context | Per flush tick |
|---|---|---|---|---|
| D1 | **Producer lanes.** `dsd_pre_aggregation` runs D lanes, lane `d` fed by every decoder `i` with `i mod D = d` and by the stream connections assigned to it at accept. The source only hands decoded buffers over ([5.2](#52-producer-lanes-and-admission)). | 0 | 0 | 0 |
| D2 | **Admission at the lane.** Lane `d` is admission point `dsd_in.metrics#d`, the only task that stamps it, so its lock covers the stamp only; the lane drives its own heartbeats. | 0 (per batch: an uncontended lock, one short shared-clock section) | 0 | 0 |
| D3 | **One clock per source** (`SourceClock`): one `latest`, one epoch, one reset decision for the D points; an affected point's publication epoch comes from the cut boundary. | 0 | 0 | 0 |
| D4 | **The keyed exit is the repartition edge.** At the exit into `dsd_agg`, after every quotient, each event goes by its context key through K = 256 fixed key groups to a state lane: no hop, no exchange stage ([5.3](#53-routing-key-groups-and-coalescing)). | about 2 ns to route and 5 ns to move (est.), only when P > 1 | 0 | 0 |
| D5 | **Coalescing** at keyed exits: per target lane, consecutive events with equal generation, epoch, exit entry, point, and admission second share one buffer of at most B events, held across batches and across idle input for at most `COALESCE_HOLD` (5 ms). | 1/B of a send once buffers fill within the hold (at saturation); below that, at most one send per target lane per 5 ms | 0 | 0 |
| D6 | **State lanes.** `dsd_agg` stays one component and runs P lanes, each owning a disjoint share of every lineage; one exact global budget and an exact, bounded node count ([5.5](#55-state-lanes)). | 0 | 2 compare-and-swap operations, as today; no shared gauge write | 0 |
| D7 | **Shared lineage rules**: per lineage instance, a tick record by ordinal (the instants, each tick's second, which lanes resolved it, a reset mark), a flush log (back-fill start, flush counted once), a breach flag, a shutdown time. No barrier, no merge, no message. | 0 | 0 | 2 short lock sections per lane |
| D8 | **A reset window** per state lane, with instances per (lineage, epoch), the exact final old-epoch watermark at every lane, a shared reset mark that stops old-epoch ticks in every lane at once, and a successor record that every lane's instances of a lineage join at the window's close, replaces "a reset at one of several points drains everything" ([5.6](#56-time-watermarks-progress-ticks-resets-shutdown)). | 0 | 0 | 0 (work only on a reset) |
| D9 | **Post-aggregation lanes**: `metrics_enrich` runs M lanes; passthrough by affinity, flush output and single-task sources spread per buffer. The isolation fence stays (its removal is stage 10). | 0 | 0 | 0 |
| D10 | **Lanes in `saluki-core`**: a slot component with lanes has one bounded channel per lane, a coordinator for control and health, and a resource group per lane ([5.4](#54-lanes-in-saluki-core-kernels-and-caches)). | 0 | 0 | 0 |
| D11 | **No shared telemetry handle is written per event on a lane's data path**; kernel and cache counters accumulate per instance and publish deltas every second. | removes shared writes | removes 3 shared gauge updates | 0 |
| D12 | **Sampled CPU-time accounting**: a resource-group entry always switches the group, but reads the thread's CPU time on one entry in N (N = 16 on Linux), scaling the delta; entries known to be long always read it. | removes about 1.7 µs per kernel per buffer on Linux, 15 of 16 times | 0 | 0 |
| D13 | **R1's cost model is removed**: off whenever `lanes.mode` is `auto`, deleted once the gate passes ([5.10](#510-removing-r1s-cost-model)). | removes hops | 0 | 0 |
| D14 | **Lane counts fixed at boot** from Saluki-only keys with host-derived defaults ([5.13](#513-configuration)). | 0 | 0 | 0 |

Everything with one lane per slot is stage 6 behavior, and each part ships behavior-neutral at one lane, with a
measurement of its own ([8.5](#85-stage-9-integration-increments-and-gate)).

### 5.2 Producer lanes and admission

**The source hands buffers over and nothing else.** In plan mode the DogStatsD source is built `with_metric_lanes(D)`:
its metrics output `metrics` is replaced by `metrics_0` to `metrics_<D−1>`, with no admission point on them. Datagram
decoder `d` dispatches on `metrics_<d mod D>` (D is the decoder count up to `PRODUCER_LANES_MAX`, 16, 5.13). A stream
connection (UDS stream, TCP, named pipe) takes, when accepted, the lane with the fewest live connections (a counter
per lane in its listener, incremented at accept and decremented at close, ties by lowest index), and dispatches on that
lane's output for its lifetime, so a connection's batches stay in order (one producer, one FIFO channel) and a few heavy
long-lived connections do not land on one lane by accept order. The taps `dsd_stats_out` and `dsd_debug_log_out` are
connected to every lane output; each decoder clones its own buffers to them, outside any lock, so the clone work is
unchanged in total and runs in parallel (and with the tap gate of P9.F, `dsd_stats_out` gets clones only while a
collection runs). No plan code enters the source, and the decoder count keeps its native default. With `lanes.mode:
auto` and one decoder, the source still uses `metrics_0` and an owned stamper, and R1 is off (5.10); `off` is the stage
6 layout.

**The lane stamps.** `dsd_pre_aggregation` lane `d` owns the admission point `dsd_in.metrics#d` through an
`OwnedAdmission`. For every input batch with an unstamped header, it calls `stamp` before running the program:

```rust
// saluki-core::execution::admission
pub enum Stamper {
    Shared,   // today: several tasks stamp; the lock is held across the stamp and the whole fan-out
    Owned,    // one task stamps and sends in program order; the lock covers the stamp only
}
impl AdmissionPointConfig {
    pub fn with_source_clock(self, clock: Arc<SourceClock>) -> Self;
    pub fn owned(self) -> Self;                          // Stamper::Owned; heartbeat recipients must be empty
}
/// The stamping side of an owned admission point, held by the one task that admits through it.
pub struct OwnedAdmission { point: Arc<AdmissionPoint> }
impl OwnedAdmission {
    pub async fn stamp(&self, header: &mut BatchHeader);          // lock, stamp, unlock
    pub fn heartbeat_due(&self) -> Option<Instant>;               // last stamp + period, if heartbeats are enabled
    pub async fn stamp_heartbeat(&self) -> Option<BatchHeader>;   // re-checks idleness under the lock
}
```

- **FIFO without the fan-out lock.** One task stamps and then sends in program order, so every channel downstream of the
  lane receives its batches in stamp order, which is what the stage 6 lock gave. Installer transactions still take the
  lock, and a stamp waits for them; a transaction waits at most for a stamp in progress, never for a fan-out blocked on
  a full channel, so arming over D points does not time out under backpressure.
- **Heartbeats move into the lane.** A separate heartbeat task sending on the lane's channels would break FIFO without
  the fan-out lock. The lane host arms a heartbeat timer at `heartbeat_due()` (last stamp + period) and, when it fires,
  re-checks it: if a stamp moved it, the host re-arms the timer at the new instant (one reset per firing, none per
  batch); if the point stamped nothing for the period, the host stamps a header-only batch and processes it, and the
  driver forwards it on its progress exits, as it forwards any heartbeat today. An idle point's next stamp therefore
  comes one period after its last, as stage 6's `run_heartbeats` gives (`admission.rs:589-607`), never up to two. The
  period is stage 6's: `min(1 s, smallest boot flush interval downstream)`.
- **Resets reach every lane at once, and no wake is lost.** The check is level-triggered: after every batch the lane
  compares `SourceClock::epoch()` (an atomic mirror) with the epoch of its own last stamp, and stamps a heartbeat at
  once when the clock is newer, so its first new-epoch header follows its last old-epoch batch within queueing delay.
  Only while its input is idle does the lane also wait on the clock's `watch::Receiver<u32>`, created once per lane and
  kept pinned, so no waiter is registered per batch and a reset that happens while the lane is busy is seen at its next
  batch (a `Notify::notify_waiters` would store no permit for a lane that was not waiting).
- **Admission time moves one queue later.** The queue from decoder to lane now precedes admission, so `t_a` is taken
  when the lane receives the batch, not when the decoder dispatched it. Queueing in that channel no longer holds the
  watermark back, and a batch waiting there across a tick is bucketed after the tick, as native aggregation, which reads
  its clock when it processes a batch, would ([amendment 109](#10-proposed-spec-amendments)). Nothing between a decoder
  and its lane reads a header: the taps are host destinations.
- **Status and cuts.** `status_within` reads under the lock, which only stamps hold, so polling never waits behind a
  blocked send. `AdmissionPointStatus::passed_cut` and `last_admitted_at` use the point's **own** last stamp
  (`PointState::last`), not the shared clock's latest.

**One clock per source.** The D points share one `SourceClock`:

```rust
// saluki-core::execution::clock
pub struct SourceClock {
    clock: std::sync::Mutex<AdmissionClock>,   // one latest, one epoch, one reset decision per source
    epoch: AtomicU32,                          // a mirror of the clock's epoch, read without the lock
    resets: tokio::sync::watch::Sender<u32>,   // the epoch, for lanes that wait while idle
}
impl SourceClock {
    pub fn new(clock: AdmissionClock) -> Arc<Self>;
    pub fn stamp(&self) -> ClockStamp;         // max-latched as today; on a reset, updates the mirror and the watch
    pub fn latest(&self) -> Option<AdmissionTime>;
    pub fn epoch(&self) -> u32;                // the mirror
    pub fn wall_now(&self) -> AdmissionTime;
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u32>;
}
// PointState: clock: ClockRef::{Own(AdmissionClock), Shared(Arc<SourceClock>)}, last: Option<AdmissionTime>
```

Per-point clocks would be wrong twice over. A backward jump near the threshold could be absorbed by one point and reset
another, whose `latest` differs by milliseconds, so the reset window below would never close (one point never changes
epoch). And a point that first sees a cut after another point's reset would record the post-reset epoch as its
publication epoch (F4) and stamp the new timeline `old`. With one clock there is one reset decision, and for the points
a cut affects, the publication epoch is the one the boundary recorded under every lock: `observe_cut(new)` takes
`new.cut().and_then(|cut| cut.epoch_of(id))`, falling back to the clock's epoch at first observation for points the
boundary does not record (sources that feed no state, unchanged) ([amendment 110](#10-proposed-spec-amendments)). The
clock costs one short critical section per stamp, at most D stamps of up to 1,024 events per millisecond at saturation
(est. 30 to 50 ns each). M10 measures it; if it contends, the fallback is per-lane clocks sharing one epoch atomic (a
lane that detects a reset advances the epoch with a compare-and-swap, and every lane adopts a newer epoch at its next
stamp), with the boundary-epoch rule unchanged.

**Upstream of the lanes nothing changes**: the datagram receiver's mutex (F15) and the shared context resolver remain,
as in legacy. Both are measured first (M2), because they decide where every curve, legacy's included, flattens;
replacements are conditional parts ([O4, O5](#87-conditional-parts-measured-options)).

### 5.3 Routing, key groups, and coalescing

**The key.** `Context::key().as_u64()` of the event at the keyed exit. Every kernel that renames a context or changes
its tags recomputes the key (`lib/saluki-core/src/data_model/event/metric/context/mod.rs:89-95`, `:477`), so at the exit
into the isolation slot, after the mapper, the namespace prefix, and the tag projection, it is the key aggregation's
table uses: context equality is key equality (`context/mod.rs:517-521`). Every event one table entry would merge, 64-bit
collisions included, reaches one lane. The Agent routes before its tag filterlist and can emit two points for one
filtered series and bucket; this design cannot. The key comes from `OperatorDef::partition_key(&self) ->
Option<PartitionKey>`, `PartitionKey::Context` for `aggregate`; a state owner without one runs its slot with one lane.
The key is process-local (`context/hash.rs:129-144`): nothing persists it, and fixtures assert only that each context
takes exactly one lane.

**Key groups.** K = 256 (`limits::values::KEY_GROUPS`). `group = (key >> 56) as usize`, the high byte, which equals
`fastrange` over the key's upper half for K = 256; aggregation's table re-hashes the key with its own random state, and
R4's memo mixes it, so neither correlates with the group. `owner: [u16; 256]` with `owner[g] = g × P / 256`: contiguous
ranges, fixed at boot. The indirection costs one L1 load and keeps stage 10's partition movement (a moved group is a cut
at a bucket boundary) possible; stage 9 never moves a group.

**The keyed exit.** `ExitRouting::Keyed(Arc<KeyGroups>)` is a property of the exit, not a program step, so programs and
their fingerprints do not change. When `Op::Exit` hands a register to a keyed exit, the driver makes the two passes a
decision makes (`runtime/driver.rs:1866-1985`): count per lane, then move each event into its lane's open buffer.
Events keep their order within a lane. A register whose events all map to one lane `j` moves whole through rule 1
below: it becomes `open[j]` when none is open (no copy), appends when `open[j]`'s key is equal, and otherwise follows
`open[j]`, which is sent first; it never overtakes an open buffer, so `t_a` never decreases at the state lane. With P =
1 the exit is `ExitRouting::One` and buffers move whole, as in stage 6.

**Coalescing.** Splitting a 1,024-event batch P ways would make every per-buffer cost at the state lanes (the host's 70
ns, the channel, the tracker, the header's reference count, a wake of the state lane, and on Linux CPU-time reads) P
times larger. A producer lane is fed one-to-one by its decoders, and the fused pre-aggregation program (about 250 ns per
metric) costs less than decoding and resolving, so at the decoders' saturation the lane's input is idle after nearly
every batch: coalescing must hold buffers across idle input to engage at all (F5). Per keyed exit and target lane, the
driver keeps:

```rust
// saluki-metric-plan::runtime::driver::keyed
struct KeyedTarget {
    open: Option<OpenBuffer>,
    last_sent: Option<SentMark>,          // the header of the last buffer actually sent to this lane
}
struct OpenBuffer {
    buffer: EventsBuffer,                 // capacity B; its header takes the generation once, when it opens
    key: CoalesceKey,
    opened_at: Instant,
}
struct CoalesceKey {
    generation: GenerationId, epoch: u32, entry: Option<EntryId>, point: AdmissionPointId, second: u64,
}
struct SentMark { generation: GenerationId, epoch: u32, admitted_at: AdmissionTime }
```

`key(h)` of a call with header `h` uses the entry the exit writes (`Op::Exit`'s entry, `driver.rs:1774-1781`), not
the region input's, so it is comparable with an open buffer's.

1. **Append.** Events for lane `j` from a call with header `h` append to `open[j]` when its key equals `key(h)`; the
   open buffer's admission time becomes `h.t_a`, the latest stamp (its generation, taken when it opened, is equal by the
   key, so later calls copy only the `Copy` admission time, with no reference-count traffic). Otherwise `open[j]` is
   sent first and a new one opened. A full buffer (B events) is sent.
2. **Key changes and header-only input.** After every call, an open buffer whose key differs from `key(h)` is sent (the
   second, generation, or epoch moved on). When the call's input was header-only (a heartbeat or forwarded progress),
   every open buffer is sent.
3. **Hold.** Otherwise open buffers stay open across calls and across idle input, until `opened_at + COALESCE_HOLD`
   (5 ms). `RegionDriver::end_turn(turn, input_idle, exits) -> Option<Instant>`, called by the host after its
   `WORK_PER_TURN` loop, sends the buffers whose hold expired and returns the earliest remaining deadline; the host arms
   a timer for it only when its input is idle with buffers open, so no timer is reset per batch under backlog.
4. **Finish**: before `finish` returns, every open buffer is sent, and then the exit's last header goes header-only to
   every target lane whose `last_sent` is older (on a `One` exit toward a state owner with several upstream points,
   likewise), so every state lane's tracker holds each point's true last stamp when its input closes (5.6).

At saturation buffers fill within the hold: at W = 8 (D = 6, P = 4, B = 512) a producer lane taking 1.6 M events per
second fills each target's buffer in about 1.3 ms (est.), so a routed buffer carries B events and per-buffer costs at
the state lanes fall to 1/B per event. Below saturation a target lane gets at most one buffer per 5 ms per producer
lane, plus one per second boundary. `plan_lane_routed_events_total` over `plan_lane_routed_buffers_total` shows the
fill in both regimes.

Coalescing is exact. Aggregation reads only `secs(t_a)` (`lib/saluki-metric-kernels/src/aggregate/kernel.rs:321`) and
allowed bucket widths are whole seconds; a cut's `T` is a whole second, and the generation is part of the key anyway;
the tracker sees a real, non-decreasing stamp of the lane; the tick gate floors the watermark, and a tick cannot fall
between two batches of one second, since `floor(W) >= s` cannot change between equal seconds; nothing else reads
sub-second `t_a` before state (`host_tags` runs after it). What coalescing can change is *when* a batch arrives relative
to a tick's deadline: by at most `COALESCE_HOLD`, which is amendment 86's class ([amendment
114](#10-proposed-spec-amendments)). The hold adds no latency the gate measures: the keyed exit feeds only a state
owner, whose output leaves on ticks, and the paced p99 sentinel travels the passthrough exit. `B = clamp(2,048 / P, 256,
1,024)` (`limits::values::ROUTE_BUFFER_EVENTS` is 2,048), so open buffers hold at most `P × B` events per producer lane:
2,048 at any automatic P (at most 8), 4,096 (1.3 MiB) at an explicit P = 16. Holding buffers up to `PROGRESS_PERIOD`,
for the case where M6 still shows routed buffers below B/2 at the state lanes, is a measured option
([O2](#87-conditional-parts-measured-options)).

**Progress per target lane.** Stage 6's `forward_progress` marks an exit written when a call emitted to it and records
the call's header as sent (`driver.rs:1199-1228`), which holds only because every written buffer carried the call's
header. With coalescing a sent buffer can carry an older header (rule 2), so progress is judged against what was
actually sent. After the call's sends, for every target lane `j` with no open buffer, the exit compares `h` with
`last_sent[j]` as this call left it, and sends a header-only copy of `h` (entry cleared) when `h` is header-only and
`h.t_a > last_sent[j].t_a`, or `h`'s epoch or generation differs from `last_sent[j]`'s, or `h.t_a >= last_sent[j].t_a +
PROGRESS_PERIOD` (100 ms). A generation change therefore reaches every state lane at once (today progress forwards only
on an epoch change or after 100 ms, `driver.rs:1216-1219`), so a cut's crossing never waits for the throttle, a reset
window closes within queueing delay, and a heartbeat's watermark advance is never lost. A lane with an open buffer gets
no copy of `h`: the buffer carries the header of its last append, so until a later call's progress rule or a heartbeat
reaches that lane, its view of the point lags as it does in stage 6, bounded by the throttle and the heartbeat period,
not by `COALESCE_HOLD`.

**The final old-epoch latest.** At the start of a call whose header `h` is in a newer epoch than the previous header
this exit handled (`prev`), before routing any of `h`'s events, the exit sends every open buffer (each holds `prev`'s
epoch or an older one), then a header-only copy of `prev` to every target lane `j` whose `last_sent[j]` is older than
`prev`, and only then routes `h`, applying the progress rule to it after its sends. No lane therefore receives an `e +
1` buffer before the final `e` header, even when the call's first `e + 1` events fill a buffer (rule 1) or move a
whole register, and every state lane holds the exact final old-epoch latest of every point, which the reset window's
drain time needs (5.6). The rule applies equally to an `ExitRouting::One` exit toward a state owner with more than one
upstream point (P = 1 with D > 1, increment 9.3): stage 6's `forward_progress` sends only a throttled `h`
(`driver.rs:1199-1226`), so on an epoch change it first sends `prev` header-only when the last header it forwarded or
wrote is older. With one upstream point nothing changes. An older-epoch header after a newer one from the same point is
then a wiring fault: `WatermarkTracker::observe` compares epochs with `epoch_newer` and returns `Observation::Fault`
(with a `debug_assert!`), and the batch counts as invalid loss, where today any epoch difference reads as a reset
(`rules.rs:135-139`).

### 5.4 Lanes in `saluki-core`, kernels, and caches

**A slot component with lanes.** `TransformBuilder::input_lanes(&self) -> NonZeroU16` (default 1) is read when the
transform is added, as outputs are. The interconnect builds one bounded channel per lane, each holding the capacity
`with_component_input_capacity` gives per lane, and the transform's context hands one consumer to each lane
(`TransformContext::take_lane_inputs() -> Vec<EventsConsumer>`). Edges into a component with lanes name their routing:

```rust
// saluki-core::topology
pub enum LaneEdge { Fixed(u16), Keyed, Affinity, Spread }
impl TopologyBlueprint {
    pub fn connect_components_with(&mut self, from: &str, to: &str, edge: LaneEdge) -> Result<&mut Self, GenericError>;
}
/// Which lane of the target a send goes to, by the edge's routing.
pub enum LaneRoute<'a> {
    Target(u16),             // keyed: the lane the driver chose
    From(u16),               // affinity: the sending lane, taken modulo the target's lanes
    Spread(&'a mut u16),     // spread: the sender's own rotation counter, advanced by the send
}
impl EventsDispatcher {
    /// Sends to one lane of the named output's target; a `Fixed` edge ignores the route.
    pub async fn dispatch_named_to_lane(
        &self, output: &str, route: LaneRoute<'_>, buffer: EventsBuffer,
    ) -> Result<(), GenericError>;
}
// saluki-core::execution::region
impl ExitSink { pub fn emit_to_lane(&mut self, exit: ExitId, lane: u16, buffer: EventsBuffer); }
```

`Spread` tries the target's lanes in rotation from the sender's counter (`try_reserve`) and awaits only when every lane
is full, so a lane that is momentarily full does not hold a buffer while others have room; the counter belongs to the
sending lane, or, for a shared stamper (OTLP, checks, liveness, the trace sampler), sits under its admission lock, as
its fan-out does today, so nothing shared is written. `Affinity` sends to `lane mod target lanes`; `Keyed` takes the
lane the driver chose through `ExitSink::emit_to_lane`. The keyed exit's sub-buffers of one call are sent with
`try_send` first, and the full ones awaited afterward, still before the next call, so one full state lane does not delay
the call's sub-buffers for every lane after it; FIFO per channel is unchanged.

**The region host with lanes.** `RegionHostBuilder::with_lanes(lanes)` makes the factory `Fn(&BuildContext, LaneContext)
-> D`, with `LaneContext { index: u16, count: u16 }`, and `with_lane_admission(Vec<AdmissionPointConfig>)` gives lane
`i` its owned point. At run, the host spawns one lane task per lane and becomes the **coordinator**:

- **Control**: it owns the mailbox. `Prepare`, `Abort`, and `Release` requests split per lane (`trait LaneSplit { fn
  split(self, lanes: u16) -> Result<Vec<Self>, Self>; }`, implemented by `RegionControl`, which carries one kernel set
  per lane), go to each lane's own control queue, and are acknowledged once: after every lane accepted, or at the first
  rejection. The installer core still sees one acknowledgment per slot. `Kernel` requests are not split (their message
  is an opaque `Box<dyn Any>` with a reply called once, `runtime/driver.rs:354-370`): `KernelControl::lane:
  Option<u16>` names the lane, and the coordinator routes each request to it (`None` is rejected in a slot with lanes).
  ADP's snapshot bridge sends one request per lane and concatenates the entries, which the contexts API already takes as
  a `Vec` (`bin/agent-data-plane/src/dogstatsd_contexts/api.rs:103`).
- **Health**: one registration per component. `Health::live()` answers a probe inside one future
  (`health/mod.rs:73-80`), so `saluki-core` gains `Health::next_probe() -> Probe` and `Probe::respond()`: the
  coordinator takes each probe, forwards it to every lane, and responds when all answered; a lane answers from its
  select loop, also while it waits for a send.
- **Supervision**: a lane task that ends, or a driver that returns `Fail`, fails the component, and the topology with
  it, as any component exit does today (intensity 0). Lanes are never restarted on their own. A generation's failure
  reaches every lane: the shared panic ledger (5.5) holds each artifact generation's mode in an atomic, which every
  lane's `live_index_for` reads (`driver.rs:1133-1147`), so a generation that exhausts its budget in one lane stops
  being processed in all of them, and its rollback is requested once.
- **Resource groups**: each lane task is spawned with `runtime::worker(name, fut).with_resource_group(name)` (added in
  I7), which registers `<component group>.lane_<i>` instead of inheriting the parent's group (F6); a lane's kernels
  account under `<slot>.lane_<i>.<node>`. With one lane, every name is today's.
- **Dispatch**: lanes share the component's dispatcher behind an `Arc`; FIFO holds per lane.

**Kernels are `Send` but not `Sync`** (`lib/saluki-core/src/execution/kernel.rs:115-175`), so each lane owns its
instances. The installer builds them on a blocking thread (`spawn_blocking`) instead of inline in its task
(`bin/agent-data-plane/src/programmable_metrics/installer.rs:1443-1520`), and lane `i` receives its own kernel set in
its share of the `Prepare`. To keep D builds cheap, `KernelSpec::with_shared(prepare, build)` computes a kernel's
immutable part once per spec (behind a `OnceLock<Arc<_>>`) and builds each instance around it: the mapper shares its
compiled profiles and takes a fresh result cache and 64 KiB interner per lane; the tag projection shares its rules and
takes a fresh `ContextMemo`; decisions already share compiled predicates. A lane never builds a kernel on its data path,
except after a panic (`rebuild_kernels`, as today) and for a reset window's empty instance (5.6).

**Caches and memos are per lane, at the configured size.** The mapper's result cache and resolver and the tag
projection's `ContextMemo` memoize pure functions, so per-lane copies change no output. Each lane gets the configured
size: `dogstatsd_mapper_cache_size` and the mapper's interner, the tag projection memo, and each R4 decision memo (96
KiB per generation) are per lane, so their memory is the lane count times the configured value, and the firm bound
grows with it (5.12 gives the formula; the overlay documentation and the release note say "per producer lane"). Dividing
them would keep the total but cut every lane's hit rate whenever the working set exceeds a lane's share, since
datagrams are not context-affine; M11 measures per-lane hit rates and decides whether to share instead. One native
timing dependence becomes per lane: the mapper leaves a metric unmapped when its interner is full and heap allocation
is off (`mapper/mod.rs:276-279`), and which metrics that hits depends on which lane's interner filled.

**The telemetry rule.** No shared handle is written per event on a lane's data path. The audit in the first part lists
every write in the pre-aggregation, aggregation, and post-aggregation kernels and in `Cache`, with its frequency:
per-event writes become per-instance accumulations published as deltas every second onto the existing, unlabeled
handles (`CacheBuilder::with_accumulated_telemetry`, `Cache::publish_telemetry`; `Kernel::publish_telemetry`, a default
no-op method the region driver calls from its once-a-second `publish`), so series shapes do not depend on the lane
count, and per-lane data appears only in the `plan_lane_*` series (5.11); per-buffer writes (consumer and dispatcher
counters, the dispatcher's two `Instant::now` calls) are allowed and listed with their rates; remapped series keep their
handles.

**Sampled CPU-time accounting.** `ResourceGroupToken::enter` keeps switching the thread's current group on every entry
(the allocation attribution), but reads `CLOCK_THREAD_CPUTIME_ID` only when a thread-local `xorshift64*` draw selects
the entry, with probability 1/N, and attributes `delta × N`. Sampling is random, not periodic, so nested entries (a task
poll around a kernel call) cannot alias; nested entries are still counted in both groups, as today. Entries known to be
long and rare, which uniform sampling would report as 0 or N times their cost in a scrape interval (a state lane's drain
steps, `finish`, the installer's builds), use `ResourceGroupToken::enter_measured`, which always reads.
`resource_tracking::set_cpu_time_sampling(NonZeroU32)` sets N process-wide; ADP sets 16 on Linux at startup, or the
value of `SALUKI_CPU_TIME_SAMPLING`, so measurements can compare at equal N; the default is 1, today's behavior.
Resource-group CPU series become unbiased estimates; nothing else changes ([amendment
126](#10-proposed-spec-amendments)). The sampler takes a CPU clock that tests replace (`test-util`), so its tests run on
macOS, where thread CPU time is not read (`stats.rs:146-150`). This part precedes any split of batches.

### 5.5 State lanes

`dsd_agg` stays one component: one component ID, one health registration, one control mailbox, the bounds path
`dsd_agg.aggregate_context_limit`, and every `component_id:dsd_agg` series the Agent's `aggregator.*` remaps read. It
runs P lanes. Each lane runs today's isolation driver (`runtime/driver/isolation.rs`) with:

- a tracker over the D lane points of `dsd_in.metrics`, indexed by `point − first lane point` in O(1) (today a linear
  `find`, `rules.rs:117-157`), and without the per-batch clone (F8): `Observation::EpochSwitched` carries the point's
  previous epoch, and `WatermarkTracker::in_publication_epoch_with(cut, point, epoch)` evaluates the predicate as it was
  before the observation;
- one instance per lineage and admission epoch (more than one only inside a reset window, one per open epoch, 5.6),
  holding the contexts whose key group the lane owns; the instances of one logical lineage instance across lanes share
  one `LineageLanes` record (below);
- its own output point (`SlotKind::Isolation::output_points[lane]`) and output latch;
- its own drain cursor and stash; drains stay bounded (`DRAIN_STEP_EVENTS` 1,024, `DRAIN_STEP_CONTEXTS` 4,096).

**Budgets.** One global `StateBudget` and one node counter per state-owner node, shared by every lane, as stage 6 shares
them across lineages (`aggregate/mod.rs:286-312`). The check-then-act on the node (F10) becomes one bounded atomic
operation, taken first, as native checks the node first (`state.rs:93`):

```rust
// saluki-core::execution::budget
impl StateBudget {
    /// Takes `units` if `used + units <= limit`, the smaller of `limit` and the capacity (one compare-and-swap loop).
    pub fn try_acquire_below(&self, units: u64, limit: u64) -> bool;
}
// saluki-metric-kernels::aggregate::state::ContextLimit::try_admit, with a shared budget:
//   node.try_acquire_below(1, limit) else refuse;
//   global.try_acquire(1) else { node.release(1); refuse }
```

The global capacity and every node limit are then exact at any lane count: the limit is never exceeded, and a context is
refused only while its node is at its limit or the global budget is full. Taking the node unit first means a node at
its limit never holds a global unit, even transiently, so it cannot make another node's context see a full global
budget. Which contexts win at the limit becomes the race of compare-and-swap operations across lanes, which the
specification calls capacity behavior. A new context costs two compare-and-swap operations (as today), now contended
across P lanes under churn, and an existing context costs none. The node counter cannot be skipped when the current
limit does not bind: a later generation can lower the node's limit under `retain`, and its check needs every context
counted. The three shared gauge updates per new or removed context (`aggregate/telemetry.rs:66-87`) become
per-instance integers published as deltas every second. M4 measures the budget under churn (the re-insertion burst
after a flush, F9's shape at 1M contexts); if it contends, per-batch reservations or credit cells replace the direct
operations behind the same `ContextBudget` interface ([O1](#87-conditional-parts-measured-options)). The admission
policy stays `AdmissionPolicy::FirstCome` (`extension/mod.rs:106-113`): the specification's credit splitting inside
the admission-policy slot is "no split" ([amendment 118](#10-proposed-spec-amendments)).

**`last_seen` is a maximum** (`state.rs:298`): `aggregated.last_seen = aggregated.last_seen.max(timestamp)`. With one
point it is identical (F9); with D points interleaving at a lane, assignment could move it backward and expire a counter
early ([amendment 117](#10-proposed-spec-amendments)).

**Shared lineage records.** Natively, five rules couple the contexts of one aggregation: the flush time of a tick (one
wall-clock read), the idle-counter back-fill start (`last_flush`, which advances only on a non-empty flush, F12), the
flush count (`aggregator.number_of_flush`), the breach warning (once per episode), and the shutdown close time. One
`LineageLanes` record per logical lineage instance, (lineage, epoch), makes the time rules node-wide, and one
operator record per lineage, shared by its records of every epoch, makes the flush rules node-wide. Ticks are
identified by their ordinal `k` (the k-th tick of the logical instance), never by an `Instant`, which a skip or an
interval change would move apart across lanes:

```rust
// saluki-metric-plan::runtime::state::lanes
pub struct SlotLanes {                                // one per slot with lanes
    lineages: Mutex<HashMap<LogicalInstance, Weak<LineageLanes>>>,
    resets_counted: AtomicU32,                        // the newest epoch whose reset was counted (5.11)
}
pub struct LogicalInstance { key: LineageKey, created_by: GenerationId, epoch: u32 }
    // `created_by`: the lineage's creator, never a batch's generation; `epoch`: of the source clock (5.6)
pub struct LineageLanes {
    first_at: OnceLock<Instant>,                      // the instant of ordinal 1 (below)
    ticks: Mutex<TickRecord>,
    shutdown_at: OnceLock<AdmissionTime>,
    share: Arc<dyn LaneShare>,                        // the lineage's operator record: aggregate::LaneShared
}
struct TickRecord {
    recent: VecDeque<SharedTick>,                     // the last TICK_RECORD_DEPTH (4) ordinals
    resolved_through: Vec<Option<u64>>,               // per lane, the newest ordinal it resolved; None: not a member
    reset: Option<ResetMark>,                         // set by the first lane to open a reset window (5.6)
}
struct SharedTick { ordinal: u64, at: Instant, second: u64, next_at: Option<Instant>, grace_counted: bool }
struct ResetMark { target: u32, cutoff: u64 }  // no ordinal after `cutoff` fires on an instance holding this epoch

// saluki-core::execution::kernel (generic: which shared time-driven call of a lineage a call is, among lanes)
pub struct LaneTick { pub epoch: u32, pub flush: LaneFlush, pub lane: u16 }
pub enum LaneFlush { Ordinal(u64), Closing }  // a tick, or the instance's reset drain, final retirement, or shutdown
pub trait LaneShare: Send + Sync + 'static {
    fn join(&self, epoch: u32, lane: u16, through: u64);   // the lane holds an instance in `epoch` from now on
    fn leave(&self, epoch: u32, lane: u16);                // it holds none: Empty at every later ordinal
    fn skipped(&self, epoch: u32, ordinal: u64, lane: u16); // resolved without a pass (skip, drop, mark): Empty
}
impl Turn<'_> {
    pub fn with_lane_tick(self, tick: LaneTick) -> Self;   // a second per-call slot, beside `with_param`
    pub fn lane_tick(&self) -> Option<LaneTick>;
}

// saluki-metric-plan::operators: every lane's instance of one lineage is built against one cell
impl KernelBuildContext {
    pub fn with_lanes(self, lanes: LaneBinding) -> Self;   // LaneBinding { cell: Arc<LaneCell>, lane: u16, lanes: u16 }
    pub fn lane_share<T: LaneShare>(&self, make: impl FnOnce(u16) -> T) -> Option<Arc<T>>;   // get or create
}

// saluki-metric-kernels::aggregate::lanes
pub struct LaneShared {                               // implements LaneShare
    flushes: Mutex<FlushLog>,                         // per epoch: members, and per recent flush each lane's outcome
    breached: AtomicBool,
}
pub enum Resolution { Flushed(u64), Empty }   // a lane's outcome: a flush at F holding contexts, or none
pub enum Previous { Known(Option<u64>), Unresolved }
pub struct FlushRecord { pub first: bool, pub previous: Previous }
impl LaneShared {
    /// Records this lane's outcome of `tick`. `first` is true for the first lane to record a flush holding contexts
    /// at it; `previous`, the latest flush time before it at which any lane held contexts in its epoch, is known once
    /// every member lane resolved the ordinal before (for a closing flush, the newest ordinal any lane resolved).
    pub fn resolve(&self, tick: LaneTick, outcome: Resolution) -> FlushRecord;
    pub fn try_breach(&self) -> bool;                                   // false to true: the caller warns
    pub fn clear_breach(&self) -> bool;                                 // true to false: the caller logs the recovery
}
```

| Rule | Lane does | Coordination |
|---|---|---|
| Tick instants | ordinal 1 is at the record's `first_at`; ordinal `k + 1` at `next_at` of `k`, which the first lane to resolve `k` sets, under the record's lock, to `at_k + interval`, with the interval its resolution moves its deadline by: the stamping generation's for a fire, as stage 6's completion does (`isolation.rs:1311-1320`), the instance's for a skip or a drop. Every lane, one that skipped `k` included, takes its next deadline from the record, so a skip, an interval change, or a late lane never moves a lane off the record's instants; a lane gated at `k` waits on its own gate, as stage 6 does, and takes `k + 1`'s instant once `k` resolved. `first_at` is set once: at boot, the first lane's start plus the interval, which every lane's `start_isolated` reads (stage 6 starts each instance at its own `now`, `isolation.rs:288-298`); for a cut's `New` lineage, the replaced record's current deadline (the instant of its newest ordinal no lane has resolved, else its newest `next_at`), as stage 6's `insert_instance` takes the replaced instance's scheduled or gated deadline (`isolation.rs:1057-1080`); for a reset's successor, the predecessor's current deadline, as a stage 6 reset keeps the instance's schedule. An instance that joins a record later takes its next deadline from the record, never a burst of past ones | once per instance, and per tick |
| The tick's second | at its deadline for ordinal `k`, under the record's lock: the first lane at `k` sets `second = floor(wall.now())`, and later lanes take it; a lane whose `k` is older than the four ordinals the record keeps is a straggler: it uses its own second and counts `plan_lane_tick_straggler_total`. The gate is then `TickGate::due_at_second(bound, W_lane, second, last_F)`; grace expiry stays per lane (`F = floor(W_lane)`), counted by `plan_lane_tick_divergence_total`, and in `plan_state_tick_grace_expired_total` once per ordinal, by the first lane whose grace expired (`grace_counted`) | one lock per lane per tick |
| Resolution | a lane resolves ordinal `k` in one critical section of the record: it checks the reset mark (an ordinal after the cutoff is forbidden on an instance holding the mark's epoch) and sets `resolved_through`. That section is the point at which a fire takes effect (taken when the tick is queued: at its deadline, at the gate's release, or at a grace expiry, never when the second is taken), a skip, or a drop, so a mark's cutoff, the newest ordinal any lane resolved, always covers an ordinal a lane queued, and a queued drain step never fires past it (a `debug_assert!` re-checks when the step starts). The kernel's `LaneShared::resolve`, at the pass, or `LaneShare::skipped`, follows. A lane that skips `k` while its instance holds contexts (a window still open, 5.6) counts `plan_lane_tick_divergence_total` | one lock per lane per tick |
| Back-fill start | a lane flushing at `k` starts idle-counter zeros at `align(previous)`, the latest flush time before `k` at which **any** lane held contexts, in this epoch, once every member lane resolved `k − 1`; until then (a lane still gated at `k − 1`), it uses its own `last_flush`, counts `plan_lane_tick_divergence_total`, and the arrival replay excludes the case. Every closed pass takes its start this way, the closing ones too: a reset drain, a cut's final retirement, and a shutdown flush call `begin_flush_with(F, previous)` with `previous` as of the newest ordinal any lane resolved, so no lane back-fills from its own `last_flush` at a cut, a reset, or a shutdown (`kernel.rs:143-160` starts all three with the core's own `begin_flush`) | one lock per lane per tick |
| Flush count | the first lane to record a flush holding contexts, at an ordinal or at the instance's closing flush, counts `aggregate_flushes_total`: once per non-empty ordinal and once per non-empty closing flush, however many lanes flush it late or at a grace expiry; a straggler beyond the four ordinals the log keeps never counts, since it cannot know whether its ordinal was counted, and telemetry parity excludes it; every lane adds its own `aggregate_flushed_total` | none |
| Breach warning | a rejection calls `try_breach`; the winner logs "Context limit reached"; any lane's pass that ends below both budgets calls `clear_breach`, and the winner logs the recovery | one atomic on the slow path |
| Shutdown close time | per record: the first lane to finish sets `shutdown_at = max(its tracker's max latest in the record's epoch, wall.now())` (`WatermarkTracker::max_latest_in`), and the others use `max(shutdown_at, their own max latest in that epoch)`; a newer or older epoch's latest never enters it | once |
| Reset mark | the first lane to open a reset window sets the old-epoch record's `ResetMark { target, cutoff }`, with the newest ordinal any lane resolved as `cutoff` (5.6); the reset itself is counted once per (slot, epoch), through `SlotLanes::resets_counted`, by the first lane to open a window into it, whether or not it holds instances | once per reset |

**Membership and the kernel side.** A lane is a member of a record while it holds an instance in it, and the driver
keeps membership equal in both records: it joins the lane (`resolved_through` and `LaneShare::join`, both at the
record's newest ordinal) when it binds an instance to the record, and leaves it (`None`, `LaneShare::leave`) when the
instance is freed, retires, or moves to a successor (5.6). A lane that is not a member counts as `Empty` at every
ordinal, so a lane without instances never holds `previous` back. The kernel learns which shared flush a call is from
the turn's `LaneTick`, which the driver sets on every time-driven call of an instance with lanes; the aggregate kernel
resolves it in its `LaneShared`, passes `previous` to `begin_flush_with`, and counts the flush when `first`. Every
lane's instance of one lineage is built against one `LaneCell`: the installer creates it with the `New` binding's
kernel sets, and the aggregate factory gets or creates the lineage's `LaneShared` in it through
`KernelBuildContext::lane_share`; a reset's fresh instance and a panic's rebuilt one are built against the same cell,
so they share the lineage's `LaneShared`, keyed by epoch inside, and never an older record's flush log.

`AggregateCore::begin_flush_with(now, start_from: Option<u64>)` takes the back-fill start from outside; with one lane it
equals the core's own `last_flush`, so the legacy core and the frozen references are untouched. `SlotLanes` is looked up
only when an instance is bound to a record (a `New` binding applied by a lane, a reset window's fresh instance or close)
and a record is dropped with its last member. A straggler beyond the four ordinals the record keeps, or a lane gated
past the next ordinal, takes the per-lane behavior and is counted, as single-core native under the same stall bursts
its ticks (amendment 86's class); the exclusions are listed in the [exactness ledger](#61-the-exactness-ledger).

**Per-kind semantics** under lanes are in the [exactness ledger](#61-the-exactness-ledger). Arrival order at a lane
decides gauges, first-seen metadata, and kind conflicts, as arrival order at `dsd_agg` decides them natively: natively
the D decoders race for one mutex, and now their lanes' streams interleave at a state lane. Per-connection order and
order within a batch are kept exactly.

**Supervision.** A panic in a state lane's kernel restarts that lane's instance empty (what it held counted as `invalid`
loss); other lanes keep their state, which is less loss than native's whole-aggregator restart. The rebuilt instance
starts as if it had never flushed: its first flush adds no zeros, as native's restarted aggregator, and it reads
`previous` only from its second flush on; panics are outside equivalence. Panic budgets belong to the artifact
generation across lanes (one `PanicLedger` shared by the slot's lanes, touched only on a panic, holding each
generation's mode, 5.4), so an artifact's allowance (3 per 60 s, 5 in all) does not grow with P, and a generation that
exhausts it stops in every lane. Under the default program (budget zero) a lane panic fails the component, as natively.

### 5.6 Time: watermarks, progress, ticks, resets, shutdown

**Watermarks.** `U(dsd_agg)` is the D lane points of `dsd_in.metrics`. Point `d` reaches pre-aggregation lane `d` only,
and lane `d` reaches state lane `j` through one channel, so there is exactly one path per (point, state lane), FIFO
holds on it, and the tracker's `debug_assert!` that `t_a` never decreases within an epoch keeps holding
(`rules.rs:142-148`). Each state lane's `W` is the minimum over the D points of the latest `t_a` it received from each;
the min rule is unchanged. The frame's `StateOwnerPoints` check becomes "every upstream point belongs to one source
clock" ([amendment 127](#10-proposed-spec-amendments)); `StateOwnerPaths` stays, counted over the slot graph.

**Progress and heartbeats** follow 5.2 and 5.3: at most 10 header-only buffers per second per (producer lane, state
lane), and only toward state lanes with no open buffer; heartbeats from idle lanes (at most one per second per lane,
forwarded to every state lane); and at once on a reset or a generation change, preceded on a reset by the final
old-epoch header. With D = 14 and P = 8 the throttle and the heartbeats give at most 1,120 header-only buffers per
second (est. a few hundred nanoseconds each with sampled accounting: well under 1% of a core), none while data flows;
each commit adds one header per (producer lane, state lane), D × P = 112 at that size, which the gate's live change
every 50 ms makes 2,240 per second while it runs.

**Coupling through `W`.** A producer lane blocked on a full state lane `k` sends nothing to lane `j` either, so `j`'s
watermark stalls, its tick gates, and after the 2 s grace it fires at `floor(W)`. Closure is late, never wrong, as along
one path natively; `plan_lane_watermark_lag_seconds` shows it.

**Clock resets: the reset window.** With one clock per source, a backward jump beyond `CLOCK_RESET_THRESHOLD` (300 s) is
one reset into epoch `e + 1`. Every lane sees the new epoch at its next batch, or at once while idle (5.2), and stamps a
heartbeat. A state lane still receives each point's epoch change at a different time, with old-epoch batches from
slower points arriving after the first new-epoch one. The rule lives in `runtime::state::rules::ResetWindow`, which the
driver and the interpreter share:

```rust
// saluki-metric-plan::runtime::state::rules
pub enum Observation {
    Ok,
    Fault,                                    // also an older-epoch header after a newer one from the same point
    /// The point's header is in a later epoch than its previous one: `previous` closed at `final_latest`.
    EpochSwitched { point: AdmissionPointId, previous: u32, final_latest: AdmissionTime, pre: Option<AdmissionTime> },
}
impl WatermarkTracker {
    pub fn observe(&mut self, header: &BatchHeader) -> Observation;
    /// The watermark of the instances holding `epoch`: per point, its latest in `epoch` (its current one, or the final
    /// latest it recorded when it switched away); `None` if a point has not reached `epoch` yet. A point that went past
    /// `epoch` without stamping in it is left out of the minimum.
    pub fn watermark_in(&self, epoch: u32) -> Option<AdmissionTime>;
    pub fn max_latest_in(&self, epoch: u32) -> Option<AdmissionTime>;   // the latest any point stamped in `epoch`
    pub fn switched_epoch(&self, epoch: u32) -> bool;   // every point's current epoch is `epoch` or newer
}
pub struct ResetWindow { older: Vec<u32>, target: u32 }   // the epochs not yet closed at this lane, oldest first
pub enum WindowStep {
    None,
    Opened { older: u32, target: u32 },
    Route(EpochSide),
    Closed { epochs: Vec<(u32, Option<AdmissionTime>)> },  // each closed epoch, oldest first, with its W_pre
}
pub enum EpochSide { Older(u32), Target }
impl ResetWindow {
    pub fn step(window: &mut Option<Self>, tracker: &WatermarkTracker, header: &BatchHeader) -> WindowStep;
}
/// Epochs wrap: `a` is newer than `b` when `(a.wrapping_sub(b) as i32) > 0` (today's code compares only for equality).
pub fn epoch_newer(a: u32, b: u32) -> bool;
```

Per point the tracker keeps its current `PointProgress` and, for each earlier epoch not yet closed at this lane, the
final latest it recorded when the point switched: at most `MAX_OPEN_EPOCHS` (4) per point. A point whose first stamp
after epoch `e` is in `e + 2` (two resets while it was blocked) records nothing for `e + 1`, and `watermark_in(e + 1)`
is the minimum over the points that stamped in it. A reset that would leave a fifth earlier epoch open at a lane (five
resets before every point switched, a clock flapping by more than 300 s that often) closes the oldest epoch at once with
its watermark so far, counted by `plan_lane_reset_overflow_total`; a later batch of that epoch has no instance, counts
as invalid loss, and the arrival replay excludes the case.

1. **Identity.** Instances are per lineage and epoch: `LogicalInstance` is the lineage's key and creator (never a
   batch's generation) with the source clock's epoch, which is global (one `SourceClock`), so every lane's instances of
   one lineage and epoch share one record, whatever order the lanes see the resets in. A lane binds a `New` instance to
   its own newest epoch when it applies the binding: its window's target while one is open, otherwise its current epoch.
2. **Open** at the first header of a newer epoch from any point at this lane. For every older-epoch logical instance
   the lane holds, it takes the record's lock and, if no lane has yet, sets `ResetMark { target, cutoff }` with the
   newest ordinal any lane resolved as `cutoff` (a later reset moves the target, never the cutoff); then it resolves,
   on its own instance, every ordinal up to the cutoff it has not resolved: their deadlines have passed, since another
   lane resolved them at the same instant, and one the gate holds is dropped, as stage 6 drops a gated tick at a reset
   (`isolation.rs:350-358`). From then on no lane fires an ordinal after the cutoff on an instance holding that epoch's
   data (5.5, Resolution); a lane whose own window is not open yet resolves such an ordinal as a skip. Every instance
   holding older-epoch data becomes reset-retiring, whatever created it, a cut's new instance included. When the
   opening header also closes the window (every point is already in the newer epoch, as always at D = 1), the lane
   closes it (rule 5) right away, before routing the header's batch, which then goes to the instance that continues:
   D = 1 builds no fresh instance, and at P = 1 it is stage 6 exactly.
3. **Fresh instances.** The first newer-epoch **data** batch for a lineage that has no instance in that epoch at this
   lane gets a fresh, empty instance: a new slab entry built from the lineage's kernel spec and build context (so
   against the lineage's `LaneCell`, 5.5), as `insert_instance` builds a `New` binding's instance
   (`isolation.rs:1054-1090`), never `rebuild_instance`, which replaces a kernel in place (`kernels.rs:136-154`) and
   would destroy the retiring state. It joins the record `(key, created_by, e + 1)`, creating it if no lane has, with
   `first_at` from its predecessor's record (5.5). It inherits its predecessor's bindings: its users in every live
   generation (each generation binding the predecessor's lineage binds it too) and the `Retiring { owner, by }` mark of
   a cut that retires the lineage, so it crosses as the lineage does and is never freed with its data counted as
   invalid loss (`isolation.rs:916-938` frees an unbound instance that is not retiring). A header-only first newer
   header opens the window without a fresh instance.
4. **During the window** a batch goes to the instance of its own epoch. Bindings name lineages, never instance
   handles, and resolve per (key, from, epoch), instances created later included: a `Retain { key, from }` in a prepare
   (`install/core.rs:191-202`) binds every epoch's instance of `from`'s lineage, each batch routed by its epoch; `New`
   creates its instance in the lane's newest epoch (rule 1); a cut's `Retire` marks every epoch's instance of `from`,
   present and later; `Release` releases all of them. Both sides draw on the global budget, so a context seen on both
   is held once per open epoch for the window's length (capacity behavior). The fresh instance cannot tick:
   `watermark_in(e + 1)` is `None` until every point switched, and a deadline meanwhile is skipped as stage 6 skips a
   tick with no watermark, resolved `Empty`, and counted as divergence when the instance holds contexts (5.5).
5. **Close** when `switched_epoch(target)`: each older-epoch instance, oldest epoch first, runs the stage 6 reset drain
   (a closing flush at `floor(W_pre)` with `W_pre = watermark_in(e)`, the minimum of the points' final old-epoch latest
   times, its back-fill start from the flush log, 5.5, then everything it holds), stamped in its output epoch; then the
   lane's output epoch increments, and each lineage at this lane continues in the successor record `(key, the
   lineage's created_by, e + 1)`, which the first lane to need it creates: through its fresh instance when one exists
   (the drained one leaves its record and is freed), otherwise through the drained instance itself, which the drain
   left empty and as if it had never flushed, as stage 6's continues (it leaves the older record and joins the
   successor at its newest ordinal), unless that drain was its final retirement (a cut-retiring instance, below). The
   joined instances tick from the successor's instants, and the reset mark, which applies only while an instance holds
   the older epoch's data, no longer concerns them. `W_pre` is exact at every lane, not the lane's lagging view,
   because every producer lane forwards its final old-epoch header to every state lane before anything newer (5.3).

Properties: with one point the window opens and closes on one header, which is stage 6 exactly. The window holds
aggregated state, not stashed input: a stash would grow while a point blocked on another lane holds its new header back,
and refusing to read would deadlock (producer lanes block on full state lanes). The window is lane-count invariant:
every lane drains at the same `W_pre`, the reset mark makes every lane fire the same old-epoch ordinals, those up to
the cutoff, so the arrival replay places the reset between ordinals `cutoff` and `cutoff + 1`, and every lane's
instances of a lineage continue in one successor record however many lanes received newer-epoch data before their
window closed (at low cardinality, most lanes receive none). One case stays outside, counted by
`plan_lane_tick_divergence_total` and excluded as a grace expiry is: an ordinal up to the cutoff that one lane fired and
another, still gated, dropped at its opening.

**With a cut in flight**, a reset after publication stamps `new` (amendment 42), and with D points a point can also
stamp `new` in epoch `e`, at `t_a >= T`, before the reset (`admission.rs:63-74`), so the cut's new instance may already
hold epoch-`e` data. Instances per lineage and epoch cover both: every instance holding epoch-`e` data reset-drains at
close, the cut-retiring old instance at `min(W_pre, T)` when `W_pre` is in the publication epoch (its reset drain is
also its final retirement), and the cut's new instance at `floor(W_pre)`, not clamped to `T`, since it does not retire
by the cut. While the old lineage's instance is reset-retiring, `switched(new)` does not cross it: its crossing waits
for the window's close, where its reset drain is its final retirement, so its last flush time does not depend on
whether a lane receives a point's `e + 1` header or another point's `new` header first, and equals stage 6's at one
point. The first epoch-`e + 1` data of the new lineage always gets a fresh instance, which takes the new lineage's
binding at close; the first epoch-`e + 1` data of the old lineage gets a fresh instance that inherits the cut's
`Retiring` mark (rule 3) and crosses at `T`. The fresh instance is built on the data path, which amends stage 6's "no
rebuild on the data path" for this rare event: an empty table, no I/O, no compilation, with credits and gauges
continuous through the shared budget and per-instance accumulation ([amendment 115](#10-proposed-spec-amendments)).

**Shutdown.** Each lane finishes when its input closes (every producer lane dropped its senders, after forwarding its
last header to every state lane, 5.3), with the shared `shutdown_at` of each record (5.5), taken from that record's
epoch only, so no bucket is flushed in one lane and discarded in another, and after a jump back the new timeline's open
buckets are discarded at the new timeline's time, not closed at the old one's. The bucket open at the close time is
discarded as `shutdown` loss, as natively (amendment 63). A lane that finishes inside a reset window closes the window
first (each older-epoch instance reset-drains at its watermark so far, which is exact, since every point's last header
reached it, oldest first), then runs the shutdown drain on the successor instances.

### 5.7 Generations, prepare, commit, retirement, cuts

- **Commit is unchanged**: one schedule store; each of the D lane points switches at its next stamp, and every lane runs
  a batch by its header's generation. Each routed buffer carries one generation (it is in the coalescing key).
- **Prepare.** The installer sends one `Prepare` per slot carrying one kernel set per lane (built on a blocking thread
  from shared specs, 5.4) and, for `dsd_agg`, per-lane state bindings (`Retain`, `New`, `Retire`, `Release`), which name
  lineages, never instance handles, so they also bind instances a reset window creates later (5.6); for a `New` lineage
  it creates one `LaneCell` that every lane's new instance is built against, so they share the operator's `LaneShared`
  (5.5). Each lane binds its new instance to the record of its own newest epoch when it applies the binding (get or
  create through `SlotLanes`, with `first_at` from the replaced record). The coordinator splits, each lane applies at a
  batch boundary (control is served before input, as today), and the slot acknowledges once. The core's barrier,
  `PREPARE_TIMEOUT` (10 s), and the overlap budget are unchanged. Per change: D + P + M lane prepares behind the same
  three slot acknowledgments as today, and the new instances' declared bounds, times the lanes that build them, admitted
  against the resource budget before the `Prepare` is sent (4.12).
- **Retirement** waits for every lane by itself: a generation token retires when the last header referencing it drops,
  in any lane. `Release` fans out to every lane.
- **Cuts** (`installer/cut.rs`). `Frame::affected_points` is the D lane points. Arming is one transaction over their
  locks in ID order; each lock is held only for a stamp, so the arm waits at most for stamps in progress, never for a
  send. `T = cut_time(max(clock.latest(), clock.wall_now()), lcm, 1 s)` from the shared clock; the `ClockAhead` check
  reads the shared clock once; the boundary records the clock's epoch for each of the D points (all equal, since every
  lane is locked). Watching reads D statuses concurrently every 100 ms, irrevocable at the first `new` stamp;
  finalization stores `Steady(new)` once every point passed by its own last stamp. Cancellation locks every point, other
  sources first and the lanes last, as stage 6 orders it.
- **Crossing per state lane.** Each state lane's tracker reaches `switched(new)` when every point's `new` header reached
  it (forwarded at once on the generation change, 5.3); that lane then makes its retiring instance's final flush at `T`
  (a closing flush, its back-fill start from the flush log, 5.5) and admits the crossing batch. Lanes cross
  independently. A retiring instance that is reset-retiring does not cross on its own: its reset drain at the window's
  close is its final retirement (5.6). Values stay exact: a `new` batch has `t_a >= T` (or a post-reset epoch) and an
  `old` one `t_a < T`, so the two instances hold disjoint buckets. Units do not: with D points, between the first and
  the last point's switch at lane `j`, `new` batches from the points that switched are admitted into the new instance
  (`isolation.rs:430-446`) while `old` batches from the others still feed the retiring one (`isolation.rs:366-391` waits
  for every point), so a context on both sides holds two global units and two node units at lane `j` for that spread,
  longer under backpressure. With a reset inside the spread, a lane can also hold the context in the new lineage's
  instances of both epochs: at most one instance per (lineage, open epoch), for the switch spread or the window, so up
  to `MAX_OPEN_EPOCHS + 1` instances per lineage at one lane. That is capacity behavior: near the limit a context may be
  refused during the spread, at lane `j` or at another lane. The new instance counts such contexts while a retiring
  instance exists (one lookup into the retiring instance per new context, only during the spread) in
  `plan_lane_crossing_overlap_contexts`. Stashing `new` batches until the crossing would avoid the overlap, but the
  stash is unbounded while a point is blocked behind another lane. Stage 6's "the two lineages never hold contexts at
  once" and "a cut near the limit loses nothing" hold at D = 1 ([amendment 128](#10-proposed-spec-amendments)).
- **Time-driven output generation** is chosen per lane by `stamping_generation` from that lane's tracker
  (`rules.rs:438-462`). Between irrevocability and the last crossing, one lane may stamp a tick `new` while another
  still stamps it `old`, as a native commit in the middle of a flush split one flush; each context's points go through
  one generation ([amendment 119](#10-proposed-spec-amendments)).

### 5.8 Flush, post-aggregation, and what stage 9 needs from stage 7

**No flush barrier.** Lanes flush independently at the shared second, in bounded drain steps, yielding between steps.
They do not stall independently, though. All P lanes fire at the same second, a draining lane reads no input
(`region/host.rs:423`, `poll_input` needs no call pending), and every producer lane routes to every state lane, so the
producers block on the first full state-lane channel: ingest stalls for the slowest lane's drain. The drain is also
bound by its output: one builder per output (S = 1) sits behind about 393 buffers of slack (the post-aggregation lanes'
inputs, 257 in all; the encode host's input, 128; the lease's shard channel, 8). At N contexts the tick's stall is
therefore about `max(N / P × 213 ns, N × c_builder − slack)` (est.), where `c_builder` is the builder's cost per metric:
at 1M contexts and P = 8 the drain alone is about 27 ms instead of 0.21 s, but whenever the builder dominates the stall
equals legacy's, whose aggregator blocks on the same encoder. M8 splits each lane's flush into drain time and time
blocked on its output (`plan_lane_flush_seconds` and `plan_lane_flush_blocked_seconds`) and records decoder drops at the
tick; O6 (builder shards) triggers on M8 showing a stall dominated by builder backpressure as well as on M3, and B3's
rationale for the automatic P is revisited after M8. Staggering lanes inside a tick is semantically neutral (the second
comes from the tick record) and is a measured option, off by default ([O8](#87-conditional-parts-measured-options)):
the Agent flushes its samplers one after another and waits on each, which this design does not.

**Post-aggregation lanes.** `metrics_enrich` runs M lanes (default `min(D, 8)`): the post-aggregation filter, the client
telemetry tap, host enrichment, host tags, and the policy decision. Its inputs: passthrough from pre-aggregation lane
`d` to lane `d mod M` (affinity, whole buffers); each state lane's time-driven output spread per buffer, its rotation
starting at offset `j`, so one tick's burst divides over M lanes; the other sources spread per buffer (5.9). Nothing
downstream holds state, so nothing needs FIFO there: `host_tags` compares each batch's `t_a` with a fixed deadline, and
the decision, the tap, and the encoders do not depend on order. A state owner downstream of a spread edge is a frame
error naming stage 10. The isolation fence stays: running post-aggregation inside the state lanes would let a stateless
artifact kernel's panic empty an aggregation instance (`isolation.rs:1182-1205` charges a panic to the instance whose
step ran), and it is stage 10's fence removal.

**What stage 9 needs from stage 7's encode host**, all part of the stage 7 design above:

1. **Fan-in from any number of producers**, resolving the lease from `header.entry` per buffer, never per event (4.6).
   Lanes need no registration with encode hosts: every lane runs the same generation's program and writes the same lease
   IDs, and a lease drains only after its generations retired, which already waits for every lane's last batch.
2. **Whole-buffer forwarding in the front task**, with no per-event work, so it is one hop per buffer and not a serial
   point per event. At 10M passthrough metrics per second in 1,024-event buffers that is about 10,000 buffers per second
   (est. 1 to 2 ms of one core per second).
3. **Builder shards** (`EncodeLeaseSpec::builders`, 4.6): one builder per output is one core per output. Stage 9 keeps
   one shard (native payloads and compression ratio) until M3 shows the main output's builder saturating at the gate's
   rates; then S > 1 for that output, at the cost of up to S partial payloads per flush
   ([O6](#87-conditional-parts-measured-options)). The drain closes all shards.
4. **A global pool that grows with cores.** `WorkerPoolConfiguration::Dedicated { threads }` replaces the fixed 8
   (`built.rs:140-159`); ADP sets `max(8, W)`. With more than eight leases, or S > 1, eight threads would cap
   compression on a large host.
5. **Bounds per shard** (the re-encode arrays and the shard channel's 8 full buffers) are admitted at prepare with the
   lease (4.12); telemetry per lease is summed over shards under the output's native `component_id`; health stays per
   host.

Sending from region lanes straight into a lease's shard channels, skipping the front task, is a measured option
([O7](#87-conditional-parts-measured-options)). Nothing in the delivery host depends on lanes: payload volume is native.

### 5.9 Sources fed by one task

`otlp_in` (one converter task), `checks_ipc_in` (one dispatch loop with one-event buffers), `liveness_in`, and the trace
sampler keep one shared-stamper admission point each. None feeds state: they send no heartbeats, and no cut affects
them. Their output into `metrics_enrich` becomes a spread edge: one buffer to the first lane with room, in rotation from
a counter under the point's lock (a plain counter), where a native fan-out would clone to all M. Their own production
stays one task, as in legacy, so their throughput is at least legacy's; spreading adds post-processing parallelism.

### 5.10 Removing R1's cost model

R1's pipelining recovered the pipeline parallelism that fusion removed (`compile/frame.rs:40-63`). With lanes it has no
job: fusion becomes maximal between fences, and slots follow fence depth only (`frame.rs:1394-1400`).

- **Off whenever lanes are on.** `FrameOptions { pipelining: bool }` is an input of `Frame::compute`; ADP passes `false`
  whenever `lanes.mode` is `auto`, at any D (one decoder included), and `true` with `lanes.mode: off`. So no part builds
  lanes for R1's pipelined slots.
- **Deleted after the gate** passes with lanes on (part P9.7): `REGION_COST_FLOOR`, `Slot::cost`, `Frame::cost_budget`,
  `Layout::budget`, `FusionVerdict::Pipelined`, the layout's cost and staging section and the stage term of slot
  grouping (`frame.rs:40-63`, `:99-102`, `:304-306`, `:384-394`, `:547`, `:849-857`, `:1189-1190`, `:1259-1397`); the
  operator estimates only R1 reads (`OperatorDef::cost` and its default, `ConfiguredOperator::cost`, the mapper, prefix,
  timestamp split, tag filterlist, host enrichment, and `tee` costs, `decision_cost`); the explain cost lines and
  `pipelined` text. About 250 lines plus tests and golden files.
- **Kept for R4**, moved into decision compilation with its documentation: the `split` estimate, `evaluation_cost`,
  `leaves_cost`, their constants, and `MEMO_COST_FLOOR` (25 ns, `compile/decision.rs:95`). R4 reads a split's cost
  through `ConfiguredOperator::cost` today (`compile/program.rs:1049-1055`), so that call moves into decision
  compilation in the same part. R7's static costs are independent.
- **Visible changes:** the slots `dsd_prefix_filter_region`, `dsd_no_agg_split_region`, and `dsd_post_agg_filter_region`
  disappear with their component, health, and task series (the stage 3 and 4 release notes advertised them; no Agent
  remap reads them); `dsd_agg`'s exit is always `metrics_enrich`; region input capacities are recomputed by the stage 5
  rule from fused paths; placement's one-hop pass-through limit disappears, so a pre-aggregation artifact's aggregation
  under mapper profiles places; R5a's "declines to move a filter across an R1 cut" becomes unreachable. Amendment 47 is
  withdrawn ([amendment 122](#10-proposed-spec-amendments)).
- **Tests replaced:** the cost-model tests (`compile/tests.rs:798-1036`, `runtime/frame_tests.rs:1132-1162`,
  `operators/tests.rs:1523-1578`) give way to `property_test_frames_fuse_maximally_between_fences` (slots follow fences
  only, decisions stay whole, one path per point and state lane), and `compile/isolation_tests.rs:61-124` and ADP's
  topology and telemetry pins change with the slot names.

The residual cost: a single saturated producer lane runs its whole pre-aggregation chain on one task, where legacy
pipelines it over five. That is one stream connection; datagram traffic spreads over D lanes. Before R1, one scripted
feed through one point ran at 0.68 to 0.72 of legacy on four workers. A real connection also decodes in its own task,
which pipelines with the lane, and may be bound by decoding; M1 measures it, and the gate has a one-connection case
([Q1](#12-open-questions)).

### 5.11 Telemetry, identity, and resource accounting

| Series or registration | With lanes |
|---|---|
| Component IDs, `component_type`, health (`dsd_pre_aggregation`, `dsd_agg`, `metrics_enrich`) | one per slot, unchanged; a stuck lane reads as a stuck component |
| `component_events_received_total{component_id:dsd_agg}` (`aggregator.processed`) | lane consumers share the handle (a per-buffer write); sub-buffers sum to the same events; header-only buffers count 0 |
| The 17 aggregation kernel series (`aggregator.*` remaps) | per-instance accumulation under `component_id:dsd_agg`, published as deltas: gauges and counters sum (each context is in one lane); `aggregate_flushes_total` once per non-empty ordinal and closing flush, by the first lane to record it; the breach log once per episode |
| `plan_clock_resets_total`, `plan_state_tick_grace_expired_total` (counted per isolation slot, `runtime/telemetry.rs:35-40`) | a reset is counted once per (slot, epoch), by the first state lane to open a window into it, through `SlotLanes::resets_counted`, whether or not that lane holds instances (a reset mark is per record, so it cannot count); a grace expiry once per ordinal, by the first lane whose grace expired (`SharedTick::grace_counted`) |
| Region driver counters (`plan_node_events_total`, port events, loss) | plain integers per lane, published each second as deltas, as today per region |
| `plan_state_watermark_lag_seconds{component_id}` | the coordinator publishes the maximum over lanes |
| `plan_state_lineages{component_id}` | logical lineage instances, not instances times P |
| Cache series | native identifiers and label sets: each lane accumulates and publishes deltas onto the unlabeled handles, so series shapes are the same at every lane count and no remapped series sees a new label |
| `dsd_in`'s output | `metrics` becomes `metrics_0` to `metrics_<D−1>`: `component_events_sent_total{component_id:dsd_in}` and the interconnect series carry `output:metrics_<d>`, and the admission points in `/programmable-metrics/status` and `explain` are `dsd_in.metrics#d`; the remaps read only `component_events_received_total{component_id:dsd_in}` (`state/metrics/rules/dogstatsd.rs:127-131`), so they are unchanged; release-noted |
| Resource groups | `topology.primary.transforms.<slot>.lane_<i>` per lane, kernels under `<slot>.lane_<i>.<node>`; with one lane, unchanged |
| Bounds | the context-limit state bounds once, under `dsd_agg.aggregate_context_limit` (the budget is global); lane-local bounds (buffers, kernels, memos, open routed buffers) per lane |
| Context snapshots | the snapshot bridge sends one request per lane (`KernelControl::lane`, 5.4) and concatenates the entries (the API already takes a `Vec`, `bin/agent-data-plane/src/dogstatsd_contexts/api.rs:103`) |

New series, bounded by `MAX_LANES` per slot, for the demo and diagnosis: `plan_lane_events_total{component_id, lane}`,
`plan_lane_batches_total`, `plan_lane_busy_seconds_total` (the host's call time from the turn clock),
`plan_lane_input_depth` (gauge), `plan_lane_routed_buffers_total`, `plan_lane_routed_events_total`,
`plan_lane_progress_total`, `plan_lane_stamp_wait_seconds_total` (time waiting for the point's lock, timed only when
`try_lock` fails, so nonzero only during transactions), and for state lanes `plan_lane_contexts`,
`plan_lane_watermark_lag_seconds`, `plan_lane_flush_seconds` (histogram), `plan_lane_flush_blocked_seconds`
(histogram: the part of a flush spent waiting for room downstream), `plan_lane_crossings_total{kind: cut | reset}`,
`plan_lane_crossing_overlap_contexts` (gauge), `plan_lane_tick_divergence_total`, `plan_lane_tick_straggler_total`,
`plan_lane_reset_overflow_total`, and `plan_lane_misrouted_total`, which must stay 0.

### 5.12 Memory

The firm bound grows by the per-lane kernel, memo, and buffer bounds times `lanes − 1`. At the automatic counts (5.13),
the growth over one lane per slot, at default cache sizes (est.):

| Item | Per lane | 4 vCPUs: D 2, P 2, M 2 | 8: D 6, P 4, M 6 | 16: D 14, P 8, M 8 | 32 and 64: D 16, P 8, M 8 |
|---|---|---|---|---|---|
| Aggregation state | partitioned; the total is unchanged, bounded by the global budget | 0 | 0 | 0 | 0 |
| Region input capacity | stateless slots `ceil(native / lanes)` buffers per lane; state lanes `ceil(native × 1,024 / (P × B))` buffers of capacity B, so the allocated events and the firm interconnect bound keep their native totals | 0 | 0 | 0 | 0 |
| Pre-aggregation kernels, per producer lane | the mapper's result cache at `dogstatsd_mapper_cache_size` and its 64 KiB interner, the tag projection memo: about 0.2 to 0.7 MiB at the defaults | 0.2 to 0.7 MiB | 1 to 4 MiB | 3 to 9 MiB | 3 to 11 MiB |
| Decision memos, per post-aggregation lane | 96 KiB per memoized decision per generation, up to 4 generations | 0.4 MiB | 1.9 MiB | 2.6 MiB | 2.6 MiB |
| Open routed buffers, per producer lane | `P × B × 336 B` = 672 KiB (at most 2,048 events at any automatic P), allocated only while open; none with one state lane | 1.3 MiB | 3.9 MiB | 9.2 MiB | 10.5 MiB |
| Drain steps, per state lane | about 380 KiB | 0.4 MiB | 1.1 MiB | 2.6 MiB | 2.6 MiB |
| Reset window | one instance per (lineage, open epoch) per lane during the window (two for one reset, at most `MAX_OPEN_EPOCHS + 1`), bounded by the global budget | 0 normally | 0 | 0 | 0 |
| Total (est.) | | about 2 to 3 MiB | 8 to 11 MiB | 17 to 23 MiB | 19 to 27 MiB |

With configured cache sizes, a producer lane's kernel row is `mapper_cache_bound(dogstatsd_mapper_cache_size) + 64
KiB + the memo bound`, times D: an operator who set `dogstatsd_mapper_cache_size: 100000` on a 16-vCPU host pays it 14
times. Lane counts are fixed at boot, so the boot verification counts all of it. **In `Strict` mode the automatic
counts fit the grant:** if the boot bound with automatic lanes exceeds the grant while the stage 6 layout fits, ADP
lowers the automatic M, then P, then D (decoder `d` still feeds lane `d mod D`), down to one lane each, logs the choice,
and refuses to boot only when `lanes.*` was set explicitly, naming the keys in the error. A unit test computes the firm
bound at W = 64. M9 checks RSS on the 250,000-context SMP gate at one lane and with defaults.

### 5.13 Configuration

| Key | Meaning | Default |
|---|---|---|
| `dogstatsd_workers_count` (native) | the decoder count, and therefore D, the producer lanes, up to `PRODUCER_LANES_MAX` (16); its overlay documentation says it now also sets D and the default of M | `max(vCPUs − 2, 2)`, unchanged |
| `experimental.programmable_metrics.lanes.mode` (new, Saluki-only) | `auto`, or `off`: one lane per slot and the source's single `metrics` output with its dispatcher-side point; until P9.7, also R1's cost model (the stage 6 layout), and after it maximal fusion at one lane per slot, which runs one stream at 0.68 to 0.72 of legacy on four workers (5.10) | `auto` |
| `experimental.programmable_metrics.lanes.state` (new) | P; 0 is automatic | 0: 1 if W < 4, else `clamp(W / 2, 2, 8)` |
| `experimental.programmable_metrics.lanes.post` (new) | M; 0 is automatic | 0: `min(D, 8)` |
| `dogstatsd_pipeline_count`, `dogstatsd_pipeline_autoadjust` (native) | still `support: none`; the overlay's reason becomes "ADP partitions aggregation by lanes (`experimental.programmable_metrics.lanes.*`)" | — |

Every value is read at startup and changes at restart (`SALUKI_KEYS`, beside `artifact_channel`). An explicit P or M
above `MAX_LANES` (64) is capped with a warning. The decoder count itself is never capped: with more decoders than
producer lanes, decoder `d` feeds lane `d mod D`, and a lane stamps whatever it receives, so several decoders per lane
are fine. The automatic counts stop at what the gate measured
(8.5): P and M at 8, D at `PRODUCER_LANES_MAX` (16), which a gate run at W = 32 raises to `MAX_LANES`. The automatic P
is a first pass the sweep replaces (M5, the scaling curves), whose objective is worker time per metric at equal
throughput, not throughput alone. In `Strict` memory mode the automatic counts shrink to fit the grant (5.12). The
Agent's pipeline keys are not reused: their default (one pipeline) would turn state lanes off, and their automatic
adjustment changes the decoder count. New limits, each with a decision-log note: `KEY_GROUPS` (256), `MAX_LANES` (64),
`PRODUCER_LANES_MAX` (16), `ROUTE_BUFFER_EVENTS` (2,048), `COALESCE_HOLD` (5 ms), `TICK_RECORD_DEPTH` (4),
`MAX_OPEN_EPOCHS` (4), `LANE_FLUSH_STAGGER` (0), `CPU_TIME_SAMPLING` (16, ADP on Linux).

### 5.14 Failure and supervision

- A lane is a region host's loop: panics are caught per call, and `reset_after_panic` applies per lane, with the panic
  ledger shared per artifact generation (5.5).
- A lane that stops answering its control queue fails the barrier (abort everywhere) and keeps its preparations alive,
  as a region does today; that pins `Prepare`'s strong `GenerationRef` for the lanes affected.
- A blocked state lane blocks every producer lane once its channel fills (an all-to-all edge), as a stalled `dsd_agg`
  blocks everything natively; the difference is that key skew decides which lane is slow. `plan_lane_input_depth` and
  `plan_lane_contexts` show skew; key-group movement (stage 10) is the remedy, and M5 measures skew first.
- A stream connection whose lane's channel is closed (the component failed) stops, as a connection does when its
  component fails.
- A generation that fails in one lane (its panic budget exhausted) fails in every lane, through the shared panic
  ledger's per-generation mode (5.4), and its rollback is requested once.

### 5.15 Coordination added and removed

Against stage 6 as built.

| Unit | Added | Removed |
|---|---|---|
| **Per event** | at a keyed exit with P > 1 only: one table load and one move of the `Event` (at most 336 bytes), about 7 ns (est.); no shared write anywhere on a lane's data path | per-event shared cache counter writes (accumulated per lane); the deep clone for `dsd_stats_out` while no collection runs (the tap gate, P9.F) |
| **Per input batch** (pre-aggregation lane) | the lane's stamp: an uncontended point lock and one shared-clock critical section (est. 30 to 50 ns) | the shared `dsd_in.metrics` mutex held across the stamp, the tap clones, and every send, contended by D decoders (F3); the tracker clone per batch at the state slot (F8) |
| **Per routed buffer** (B events at saturation; below it, at most one per 5 ms per producer and state lane) | one channel send into a state lane's channel (D producers per channel), with the dispatcher's shared counter and two `Instant::now` calls; a wake of the state lane when it was parked (microseconds of parking and waking on Linux); a buffer allocated on the producer's core and freed on the state lane's, whose allocator write lands in the producer lane's group (F6); one increment and one decrement of the generation's reference count, one `Arc` that every lane of every slot touches (taken once when the buffer opens, 5.3) | — |
| **Per new context** | the global and node compare-and-swap operations, now contended across P lanes under churn | three shared gauge updates per new and per removed context |
| **Per flush tick** | per state lane: one tick-record lock and one flush-log lock; the breach flag only on a rejection or recovery; no barrier, no merge, no message | the 0.21 s serial drain at 1M contexts (P drains in parallel), though the stall stays bound by the builder (5.8) |
| **Progress** | at most 10 header-only buffers per second per (producer lane, state lane) without data, and immediately on a generation or epoch change (two on an epoch change: the final old-epoch header, then the new one); heartbeats only from idle lanes; per commit, D × P headers (2,240 per second at D = 14, P = 8 during the gate's change every 50 ms) | — |
| **Per configuration change** | D + P + M lane prepares inside three slot acknowledgments; kernels built per lane from shared specs on a blocking thread, their bounds times lanes admitted against the resource budget (4.12) | — |
| **Per cut** | an arm over D locks, each held only for a stamp; D status reads per poll; P independent crossings, each with a spread in which a context may hold units in both instances | — |
| **Per reset** | one reset decision per source; one heartbeat per lane; the final old-epoch header per (producer lane, state lane); one reset mark per lineage instance; per state lane one fresh instance per lineage that receives newer-epoch data during the window; one successor record per lineage, which every lane's instance joins at its close | — |
| **Per task poll and kernel call** (Linux) | — | 15 of 16 CPU-time system call pairs (D12) |

### 5.16 Alternatives considered

| Alternative | Why not |
|---|---|
| Plan code inside the decoder tasks (each decoder runs its lane's program inline) | saves one hop per buffer but puts plan kernels, control, and supervised calls into the DogStatsD source across the `saluki-components` and plan boundary (est. 2,500 to 3,000 lines in `saluki-core`), and ties pre-aggregation parallelism to the decoder count, which regresses a 4-vCPU host unless a native default changes; kept as a measured option ([O10](#87-conditional-parts-measured-options)) |
| Keep the one `dsd_in.metrics` point and give slots lanes behind it | keeps the mutex held across every send, the serial point at 8 to 16 decoders (est.); its own contingency, a lane-striped point, amounts to per-lane points |
| Lanes as separate components (`dsd_agg_lane<i>`) | changes component IDs, health names, and remaps, and needs a telemetry alias for shared series |
| A separate exchange stage after decode | one more hop per event and a second repartition after the quotient kernels |
| Partial aggregation per producer, merged at the flush | multiplies memory and inserts by up to D, the context limit loses its meaning, gauges, metadata, and kinds need order keys, and every tick needs a merge barrier |
| One shared concurrent table | atomics per entry, cache-line traffic on hot contexts, flushes concurrent with inserts; contradicts one owner per lineage |
| `SO_REUSEPORT` sockets as partitions | UDP on Linux only, UDS datagram has one socket, and it gives client affinity, not context affinity |
| Routing by the key at decode | the mapper, prefix, and tag projection re-key contexts afterwards; the Agent's choice, which can emit two points for one filtered series and bucket |
| Last-writer-wins gauges by `(t_a, point)` | needs an order key per point per bucket (16 bytes or more) to make deterministic a race native ADP already has; the candidate oracle bounds the race instead |
| Splitting the context limit per lane, or credit cells by default | per-lane limits change the limit's meaning under skew; credit cells hold units idle; both kept as the measured fallback behind the budget interface |
| A barrier at each tick | the shared tick record gives the same flush time with one lock per lane and no waiting |
| Stashing new-epoch input at a reset | unbounded while a point is blocked behind another lane, and a refusal to read deadlocks |
| Coalescing only within one host turn, or only under backlog, or none | a lane that outpaces its decoder rarely has two batches in one turn, so sub-buffers of 1,024/P events would multiply per-buffer costs in exactly the regime the gate measures; the chosen hold works across batches and idle input |
| Stashing `new` batches at a cut's crossing | removes the units overlap of a crossing, but the stash is unbounded while a point is blocked behind another lane |
| Lane counts that change live | registrations cannot be released (F2) and state would have to move; a restart re-lowers from scratch |
| Thread per core with lanes and state in one task | makes the task graph cyclic (bounded queues deadlock without non-blocking sends), and a flush would stall decoding |

### 5.17 Worked paths

**A configuration change (a live tag filterlist edit).** The installer lowers, compiles, and places into the unchanged
frame with lanes; the diff is stateless (`retain` for `dsd_agg`). Only the tag projection needs new instances: its
spec's shared rules are compiled once and six instances are built on a blocking thread. One `Prepare` per slot: six
kernel sets for `dsd_pre_aggregation`, `Retain` bindings for each of `dsd_agg`'s four lanes, unchanged programs for six
`metrics_enrich` lanes; three acknowledgments after sixteen lane acceptances. Commit: one store; each lane point stamps
`g + 1` from its next batch. Generation `g`'s token drops with its last header in any lane, and `Release` fans out to
every lane.

**A cut (an artifact changes the window from 10 s to 30 s).** Prepare carries, per state lane, `Retire { from }` for the
old lineage and `New { key }` with that lane's empty instance, built against the new lineage's `LaneCell`; the first
lane to apply the binding creates the new `LineageLanes` record, its `first_at` the old record's current deadline, which
the four new instances share. Arm: one transaction over the six lane locks in ID order, each held only for a stamp; `T`
= the next 30 s boundary at least 1 s after `max(clock.latest, wall)`; the boundary records the clock's epoch for each
point; publish; release. Each lane stamps `new` from the first stamp with `t_a >= T`, and its progress carries the
generation change to every state lane at once. Between the first and the last point's `new` header at state lane 0,
`new` batches go to its new instance while `old` ones still feed the retiring one, so a context both sides see holds two
units there for those milliseconds (`plan_lane_crossing_overlap_contexts`). State lane 0 reaches `switched(new)` when
all six points delivered a `new` header, makes its old instance's final flush at `T`, releases its units, and admits;
lane 1 does the same, perhaps 50 ms later. When all six points passed, `FinalizeCut` stores `Steady(new)`.

**A clock reset (the wall clock steps back 600 s).** Lane 2's next stamp on the shared clock detects the jump: epoch
`e + 1`; the other five lanes see the clock's new epoch at their next batch, or at once while idle, and stamp heartbeats
in `e + 1`. Before routing anything newer, each producer lane's keyed exit sends its open `e` buffers and its last `e`
header to every state lane that has not seen it, then the `e + 1` one. State lane 0 receives point 2's first `e + 1`
buffer and opens its window: it sets the lineage record's reset mark (cutoff: the newest tick any lane resolved), its
instance becomes reset-retiring, and the buffer goes to a fresh instance with the record `(key, created_by, e + 1)`.
From now on no lane fires a later tick on the epoch-`e` instance. Old-epoch buffers still queued from points 0, 1, 3, 4,
5 go to the retiring instance; their `e + 1` headers then arrive. When the last point switched, lane 0 runs the reset
drain at `floor(watermark_in(e))`, the same second every lane computes, emits everything held, increments its output
epoch, and continues on the fresh instance, which joins the successor record. Lane 1, which received no `e + 1` data for
the lineage before its own window closed, does the same on its own schedule, and its drained instance itself joins the
same successor record, so both lanes tick the successor's ordinals together.

## 6. Semantics and verification

"Unchanged semantics" has three classes, each checked differently:

1. **Partitioning itself adds no nondeterminism.** For a fixed arrival order at every lane, the output is the same at
   every lane count, bit for bit, and equal to the interpreter's: routing, budgets below capacity, the shared lineage
   records, ticks, coalescing, the values of crossings, and reset windows fall in this class. The cases where a lane's
   timing decides an outcome (a grace expiry, a lane still gated when another needs its tick resolved, a tick one lane
   fired and another dropped at a reset's opening, a lane that skipped a tick while holding contexts in an open window)
   are counted by `plan_lane_tick_divergence_total` and excluded, as amendment 86 excludes a grace expiry.
2. **The producer race is native.** Natively D decoders race for one mutex, so two decoders' batches carrying one gauge
   reach aggregation in an order the client does not control. With lanes the same race is the interleaving of lane
   streams at a state lane. Order within a batch and per connection is kept exactly.
3. **Capacity behavior** (which contexts win at the limit, and the units a context holds in more than one instance,
   one per lineage and open epoch, during a cut's crossing or a reset window) is reported, not promised
   (specification, Equivalence).

The specification's stage 9 ordering promise (inputs with the same pre-mapping context go to the same worker) cannot be
kept without context-affine decoding (a datagram holds many contexts, and any decoder may take it), and native ADP never
kept it. This design keeps the order native has: per lane, per connection, within a batch ([amendment
108](#10-proposed-spec-amendments)).

### 6.1 The exactness ledger

| Property | Stage 6 | With lanes | Verified by |
|---|---|---|---|
| Grouping by context after the mapper, prefix, and tag filterlist | one core keyed by the post-quotient key; colliding contexts merge into the first inserted | routed by `Context::key()` at the keyed exit, so a context and its collisions share one lane | routing assertion at every admit; fixture X6; trace-order exactness |
| Counter, rate (equal interval), set, histogram, distribution values | merged in arrival order | per context, in that lane's arrival order | trace-order and arrival-replay exactness; under interleaving, sums within 1e-9, set unions, histogram sample multisets, sketch bins |
| Gauge last value | last arrival, which is the latest stamp (one point, one FIFO) | last arrival at the lane; exact per point and per connection; across points the native race | maximal-candidate oracle (6.3); fixture X4 |
| First-seen metadata | first insertion | first arrival at the lane | candidate check |
| Kind or rate-interval replacement | the incoming value replaces the value set; counted as policy loss (amendment 82) | last arrival wins, the loss counted | the emitted kind is one of the per-point last kinds; loss balance |
| `last_seen` and counter expiry | assigned per insert (`state.rs:298`), equal to the maximum with one point | a maximum: identical with one point | fixture X5; kernel property test |
| Context limit (count) | the global and node budgets (`state.rs:89-114`), check then act on the node (F10) | bounded node acquire, then the global compare-and-swap: never exceeded at any lane count, counting held units | capacity oracle; loom model |
| Which contexts win at the limit | first come in stamp order | the compare-and-swap race across lanes (capacity behavior) | capacity oracle; equivalence below capacity only |
| Breach warning and recovery | once per core per episode (`kernel.rs:341-347`) | once per lineage instance across lanes | fixture X2; loom model |
| Idle-counter zeros and back-fill start | from `align(last_flush)`, which advances only on a non-empty flush | from the lineage's latest non-empty flush in any lane, per epoch, once every member lane resolved the previous ordinal, for ticks and for closing flushes (reset drains, final retirements, shutdown) alike; otherwise the lane's own `last_flush`, counted and excluded | fixture X1 with cut, reset, and shutdown cases; idle-counter property test at 8 lanes with one lane delayed past an interval |
| Flush time of a tick | one wall-clock read | the shared tick record's second, by ordinal; expired graces and stragglers per lane, counted and excluded | lane tick agreement assertion; `plan_lane_tick_divergence_total`, `plan_lane_tick_straggler_total` |
| `aggregate_flushes_total` (`aggregator.number_of_flush`) | once per non-empty tick and per non-empty closing flush | the same, by the first lane to record a flush, whatever later lanes or lanes whose grace expired record; a straggler beyond the record's depth never counts (excluded from telemetry parity) | fixture X3; telemetry parity |
| Shutdown close time | `max(latest t_a, wall)`; the bucket open then is discarded | one per lineage instance, set by the first lane to finish, from that instance's epoch only | trace T-L6; the shutdown-inside-a-window fault test |
| R5b emission suppression | per instance, per flush, with the stamping generation's predicate | per lane, the same rule; suppressed counts add up | the R5b property test with lanes |
| Bucket by `t_a` | one `t_a` per batch | per routed buffer; coalescing merges only one second | coalescing property test; trace-order exactness |
| Watermark and closure | minimum over one point on one path | minimum over D points, one path per (point, state lane) | the monotonicity assertion per (point, lane); tick parity |
| Clock resets | one point resets; the core drains and continues empty (amendment 64) | one decision per source; instances per lineage and epoch; a reset window per state lane, draining at the exact final old-epoch watermark; a shared reset mark, so every lane fires the same old-epoch ticks; at the close every lane's instances of a lineage join one successor record | multi-point reset property test; traces T-L2, T-L3, and stage 6's trace 11 at D = 3, P = 2 |
| Cuts | armed under one point's lock; crossing on that point; the two lineages never hold one context at once | armed over D lane locks (stamp only); each state lane crosses on its own, a reset-retiring instance at its window's close; values exact; between the first and the last point's switch at a lane, or during a reset window, a context may hold units in one instance per (lineage, open epoch) (capacity behavior, amendment 128) | cut property test; trace T-L1; `plan_lane_crossing_overlap_contexts` |
| Time-driven output generation | one rule per drain | per lane; during a cut's window one lane may stamp `old` and another `new` | transition traces |
| Admission time | stamped at the source's dispatcher, before the pre-aggregation queue | stamped at the lane, after it (amendment 109) | bucket property tests extended to the lane stamp |

### 6.2 Reference interpreter changes

- **Lanes are admission points of one source sharing one clock.** `TraceEvent::Admit { source, lane: u16, at, batch }`
  (lane 0 for a source with one point). A `t_a` earlier than the source's latest by more than the threshold is one reset
  for every point of the source; within the threshold it is ill-formed, as today (amendment 81).
- **Shared rules from `runtime::state::rules`:** the reset window (`ResetWindow`, with `watermark_in`,
  `switched_epoch`, and `epoch_newer`) replaces "a reset at one of several points drains everything"
  (`interpret/mod.rs:169`), with instances per lineage and epoch, and the window's opening as the point after which no
  old-epoch tick fires; `last_seen` is a maximum; the crossing is over every lane point.
- **No lanes inside the interpreter.** It stays the lane-less oracle: one instance per state owner, delivery in trace
  order. The shared lineage records are designed so that the union of the lanes equals that one instance. The
  interpreter shares one output latch per set of upstream points, where each state lane stamps with its own output point
  and latch; the oracles therefore compare delivered records (output, context, kind, timestamp, value), never output
  points or latch values, and two distinct isolated owners over one set of points stay unsupported, as in stage 6.
  (Stage 6's transition traces also compare each record's flush time, which the lanes give exactly under their mapping,
  6.7.) `AdmissionPointSnapshot`'s optional heartbeat recipients stay deferred.
- **Candidate mode** (`Interpreter::with_candidates`): for every emitted gauge point it records the last value each
  point admitted to the bucket, and for every entry the kinds and metadata admitted while it lived, carried with the
  emitted record through the downstream program to the outputs (`equivalence::Candidates`).
- **Arrival replay** (`interpret::replay::ArrivalReplay`, `test-util`): from the per-state-lane logs a harness records
  (each admitted routed buffer with the header the lane observed, each header-only observation, each tick fired with its
  flush time), it builds one trace: per scheduled tick, every lane's events before its own firing go before one tick
  event, and its events after go after; each routed buffer is a separate admit at the header the lane saw (one second,
  one bucket); a reset's opening goes between the mark's `cutoff` ordinal and the next. Every old-epoch admit of a
  source goes before that source's first newer-epoch admit (they feed disjoint instances, and the old-epoch instance
  fires no tick after the cutoff), so the interpreter, which infers resets from jumps in `t_a`, sees one reset per jump,
  never an old-epoch admit after a newer one read as a second jump. A consistent merge always exists, since flush times
  never decrease and contexts are disjoint. A replay is valid only when every lane fired each ordinal at the same flush
  time and took its back-fill start from the shared log; otherwise (a grace expiry, a straggler, an unresolved previous
  ordinal, a tick dropped at a reset's opening in one lane only, a tick skipped by a lane holding contexts, a reset
  overflow beyond `MAX_OPEN_EPOCHS`) the case is counted and excluded, as amendment 86 excludes a grace expiry.

### 6.3 Oracles

1. **Exact** (`equivalence::compare`): outputs equal, floats bit for bit. Used for trace-order delivery and for arrival
   replays.
2. **`compare_partitioned(expected, actual, candidates, tolerance)`** for seeded schedules against the original trace in
   candidate mode. Groups are `(output, context, kind, timestamp)`; metadata leaves the group key. Point counts per
   group must be equal.

   | Kind | Check |
   |---|---|
   | Counter, rate | sum within 1e-9, relative (pairwise per bucket, amendment 31) |
   | Set | unions equal |
   | Histogram | sample multisets with weights equal; count, min, max, and percentiles exact; sum and average within tolerance |
   | Distribution | bins, count, min, and max exact; sum and average within tolerance, after `property_test_sketch_merge_order` confirms the merge order assumption |
   | Gauge | **maximal candidate**: the value equals the last value one of the contributing points admitted to the bucket, and exactly that point's last when one point contributed. "Any admitted value" would accept a wrong first value. |
   | Metadata | one of the candidates |
   | Kind conflict across points | the emitted kind is one of the per-point last kinds; values not compared; the loss balance must hold |

3. **`check_capacity(trace)`** under pressure, from the budget operations a harness records (`test-util`): held units at
   most the limit after every admit, counting a context once per instance that holds it (one per lineage and open epoch
   during a crossing's spread or a reset window); every refusal justified by the refused operation itself, from the
   `used` value its own compare-and-swap observed (recorded inside the loop, since recordings from several threads are
   not in the order the operations took effect), with outstanding node units counted as held (a unit acquired and not
   yet returned on a global refusal still occupies the node); emitted contexts are a subset of admitted ones; delivered
   points plus capacity loss plus policy loss equal admitted points.

### 6.4 Property tests

Release mode, named `property_test_*`, in the crate that owns the code.

| Test | Generates | Checks |
|---|---|---|
| `property_test_lanes_equal_interpreter_in_trace_order` | random pre-aggregation scope plans (mapper, projection, `aggregate` with expiry), multi-point traces with idle gaps, low and high cardinality; D in {1, 2, 4}, P in {1, 2, 3, 4, 8}, M in {1, 3}; open buffers flushed before every trace tick | exact against the interpreter |
| `property_test_lanes_equal_interpreter_in_arrival_order` | the same plans; seeded schedules: interleaved deliveries respecting FIFO per channel, per-channel delays, the coalescing hold, the progress throttle | exact against the arrival replay |
| `property_test_lane_count_invariance` | one trace, one fixed schedule that fixes, per context, the order its events from several points reach the state slot, identically at P = 1 and P = k (at P = 1 the exit holds nothing, so the schedule, not coalescing, decides the order); cuts and resets included | bit-equal outputs at P = 1 and P = k, ticks at the same seconds across a cut and a reset; equal deltas of `aggregate_flushes_total`, breach logs, R5b counts |
| `property_test_lanes_under_interleaving` | seeded schedules | `compare_partitioned` against the interpreter in candidate mode on the original trace |
| `property_test_idle_counters_across_lanes` | 1 to 3 counters over 8 lanes, so most lanes are empty at most ticks; gaps; expiry 0 and 300 s; one lane delayed by more than one interval; a cut, a reset, and a shutdown between ticks | exact at P = 1 and against the interpreter, closing flushes included (no idle zeros a single core would not emit); the delayed case falls back, is counted, and is excluded from the replay |
| `property_test_multi_point_reset` | resets with straggler points, in-flight old-epoch buffers, a point's last old-epoch batch routed to one state lane only, the first `e + 1` call filling B for one lane, ticks due inside the spread of window openings, windows that close before any `e + 1` data reaches a lane, low cardinality (most lanes without fresh instances), a reset inside a window, a point skipping an epoch, a reset during a cut whose new instance already holds epoch-`e` data, with both arrival orders of a point's `e + 1` header and another point's `new` header, shutdown inside a window | exact in trace order; exact in arrival replay whenever every lane resolved each ordinal up to the reset mark's cutoff alike (otherwise counted and excluded); every lane drains at the same `W_pre`; after the close every lane's instances tick the successor's ordinals; window rule identical in driver and interpreter |
| `property_test_cut_with_lanes` | cuts with delay schedules so lanes cross at different times; a reset during a cut | exact values; a context is held by at most one instance per (lineage, open epoch) of one lane, and more than one only between the first and the last point's switch at that lane or inside a reset window; the ID invariant |
| `property_test_partitioned_capacity` | contexts beyond limits 0 to 3 and 1,000, node limits below the global capacity, churn; P in {2, 4, 8} | `check_capacity` |
| `property_test_keyed_exit_routes_each_context_to_one_lane` | random keys and P | one lane per key; contiguous table; order kept within a lane |
| `property_test_coalescing_is_exact` | buffer streams crossing seconds, generations, epochs, entries, with idle input and expired holds; the first `e + 1` call filling B for one lane | equal output with and without coalescing, for aggregation, the tracker, and the gate; every state lane's observed watermark per point equals the one without coalescing after each call's sends, within `COALESCE_HOLD` for lanes with an open buffer; no lane receives an `e + 1` buffer before the final `e` header. A unit test pins the progress case: an open buffer of second `s − 1` for lane `j`, then a generation change with no data for `j`, delivers the buffer and then the new header |
| `property_test_frames_fuse_maximally_between_fences` | random plans and lane policies | slots follow fences only; decisions whole; one path per (point, state lane) |
| `saluki-metric-kernels`: `property_test_last_seen_is_max_under_reordering`, `property_test_lane_flush_log_matches_one_core`, `property_test_bounded_node_acquire_never_exceeds_the_limit` | kernel-level | against the frozen reference with one lane |
| `saluki-core`: `property_test_owned_admission_keeps_stamp_order`, `property_test_source_clock_resets_once_per_jump` | stamps and sends from one task with transactions interleaved; jumps near the threshold from several points | per-channel stamp order; one epoch per jump |

### 6.5 Concurrency: loom models and a multi-thread stress run

The harnesses above run lane calls one at a time, so they never exercise a race inside a shared record. Each shared
record therefore gets a `loom` model. Loom 0.7 has no `OnceLock`, its `Notify` is not tokio's, and a tokio mutex cannot
be modeled, so the models transcribe each protocol with loom primitives, as `saluki-core`'s metrics model does
(`observability/metrics/mod.rs:1189-1206`), over the same code where it is plain `std` synchronization:
`saluki-metric-kernels` and `saluki-metric-plan` gain a `loom` feature with `crate::sync` shims (`Mutex`, atomics, and
`OnceLock` as `Mutex<Option<T>>` under `cfg(loom)`), and `make test-loom` adds both crates to the two it runs
(`Makefile:619-623`):

| Record | Model | Property |
|---|---|---|
| `SourceClock` | 3 stampers across a backward jump beyond the threshold, with the epoch mirror | exactly one reset; every later stamp in the new epoch; `latest` never decreases within an epoch; the mirror never shows an epoch the clock has not reached |
| `StateBudget::try_acquire_below` with the global budget | 3 lanes admitting new contexts of one node and one of another node, limit below capacity | never above the node limit or the capacity; the node unit returned on a global refusal; a node at its limit never holds a global unit |
| `TickRecord` | 3 lanes firing one ordinal in any order, one skipping it, one opening a reset window; a lane that takes an ordinal's second at its deadline, gates, and fires after another lane set the mark; a lane joining the successor record while others tick it | one second and one next instant per ordinal; every lane's next deadline from the record; no ordinal after the mark's cutoff fires on an instance holding the mark's epoch; the cutoff covers every ordinal a lane queued; a joining lane never holds `previous` back |
| `LaneShared::resolve` | 3 lanes resolving consecutive ordinals, one lagging, one leaving and one joining between ordinals, and a closing flush | exactly one `first` per non-empty ordinal and closing flush; `previous` is known only once every member lane resolved the ordinal before, and is then the latest earlier flush time |
| Breach flag | rejections and pass ends interleaved | one warning and one recovery per episode |
| `shutdown_at` | 3 lanes finishing | one close time |
| `OwnedAdmission` with a transaction | a stamp and an arm interleaved, with the point's lock modeled as a `std` mutex | a transaction never sees a half-written stamp; no stamp while it holds the lock |

**ADP stress run** (`programmable_metrics::topology::lane_stress_tests`): the test topology with D = 4, P = 4, M = 2 on
a 4-worker runtime, 30 s of datagram traffic over 10,000 to 100,000 contexts near the context limit, a live install
every second, and a width cut every 10 s. Checks: `check_capacity` over the recorded budget operations,
`plan_lane_misrouted_total` zero, the canary balance (every admitted canary point delivered once, or counted as
capacity loss, which a crossing near the limit may cause), and no debug assertion.

### 6.6 Differential tests against legacy

- The ADP test topology with four scripted producers, one per lane point, against legacy's one point and one aggregator;
  P in {1, 2, 4} and M in {1, 4}, across the lowering matrix's DogStatsD rows, with and without gateways;
  `compare_partitioned` (legacy delivers in its mutex order, the plan in lane order).
- Tick parity at P = 4 with one scripted producer: exact, idle zeros and expiry included (the stage 6 tick parity test
  with lanes).
- Telemetry parity: every `aggregator.*` remap equal to legacy at P = 4 (flush count once per non-empty tick, gauges
  summed).
- The demo check runs once with `--state-lanes 2`; its low-cardinality traffic over two lanes exercises the shared flush
  log every tick, and its canary balance catches lost or doubled points (8.8).

### 6.7 Transition traces

Stage 6's traces 1 to 15 replay through the harness at D = 3 and P = 2 with their hand-written expectations under trace
order (trace 14 still needs two sources and stays interpreter-only). Each stage 6 trace stamps one point, so the
mapping is fixed: the trace's `i`-th stamp (an admit or a heartbeat) is stamped at point `i mod 3`, and mirrored at
the same `t_a` as a heartbeat on the other two points, right after it and before the trace's next event. Every state
lane's watermark then equals the single point's latest after each event, so ticks, gates, expired graces, `W_pre`, and
cut crossings happen at the stage 6 instants and seconds, and a reset is one jump on the shared clock. The
expectations' flush times are compared too: each lane's output latch is max-latched over flush times that never
decrease within an output epoch, and every lane's latch resets at the same window closes, so every record's stamp
equals the single latch's. New traces, at both levels (`interpret/transition_tests.rs`, `runtime/transition_tests.rs`):

| Trace | What it pins |
|---|---|
| T-L1 | a cut with state lane 1 delayed by three progress periods: lanes cross separately; values equal P = 1; a context seen on both sides of lane 1's spread holds two units until its crossing |
| T-L2 | a reset with one idle point (its heartbeat closes the window) and one point with five queued old-epoch buffers, whose last one goes to state lane 0 only: lane 1 learns its final latest from the forwarded header and drains at the same second as lane 0; a tick due between the two lanes' openings fires in neither; stage 6's trace 11 at D = 3, P = 2, whose mapping closes every lane's window before any `e + 1` data arrives: the drained 5 s instance of the lane owning `c` joins the successor record, takes `c = 2`, and delivers `(2, 3_606_000, "c rate/5s [3600:2]")` at the successor's first tick |
| T-L3 | a reset after a cut's publication: new-epoch data reaches a fresh instance of the new lineage, and the old lineage's reset drain is its final retirement, in both arrival orders of a point's `e + 1` header and another point's `new` header; a reset after one point already stamped `new` in epoch `e`: the cut's new instance reset-drains at `floor(W_pre)`, and the first `e + 1` data gets a fresh instance, which takes the new lineage's binding; stage 6's queued trace 11 at D = 3, P = 2, the cut prepared while both batches wait: the fresh old-lineage instance inherits the cut's retiring mark, crosses at 3,610.5, and delivers `(1, 3_610_000, "c rate/10s [3600:1]")` |
| T-L4 | a prepare in which one of sixteen lanes rejects: every lane aborts; the generation is never stamped |
| T-L5 | a panic in state lane 1 under an artifact: only lane 1's contexts are lost; the panic budget counts once |
| T-L6 | shutdown with lanes finishing 200 ms apart across a bucket boundary: one close time, one consistent discard |
| T-L7 | revocation while armed, D = 4: cancellation over every point |

### 6.8 Fault tests

In ADP: arming while one producer lane is blocked on a full state lane (the arm completes within one attempt, since the
lock covers stamps only); a cut under sustained backpressure (arming succeeds within `CUT_ARM_ATTEMPTS`); a state lane
panic under the default program fails the component, and under an artifact budget it continues; a producer lane panic; a
blocked encoder shard, so one lease waits and the region lanes back up; shutdown with open routed buffers loses nothing;
a stalled lane reads dead to liveness; a lane task that ends fails the component; a stream connection whose lane failed
stops; an artifact generation that exhausts its panic budget in one lane stops in every lane; shutdown inside a reset
window closes it first, drains the older epoch at the exact final watermark although a point never switched (its last
header forwarded at finish), and, after a jump back from 7,201 to 3,600, closes the new timeline at its own time, not
at 7,201; a `Strict` grant too small for the automatic lanes boots with fewer lanes and logs it, and
refuses to boot when `lanes.*` was set explicitly.

### 6.9 Hand-written fixtures

Independent of the code under test.

| Fixture | Content |
|---|---|
| X1 | lane 0 holds counter `a`; lane 1 is empty at ticks 15 and 30; counter `b` arrives in lane 1 at 31: `b`'s zeros start at `align(30)`, not lane 1's never. The same for a closing flush: 10 s buckets, 15 s ticks; lane 0 holds `a` and flushes at 30 and 45; lane 1 last held contexts at 15, and `b` arrives in lane 1 at 47; a cut with `T = 60` retires the lineage (or a reset drains it, or shutdown closes it): lane 1's closing flush starts `b`'s zeros at `align(45)`, so it emits a zero for `b` at 50 only, never at 10, 20, or 30 |
| X2 | 4 lanes, limit 3, 5 new contexts at once: exactly one warning; recovery after the flush |
| X3 | `aggregate_flushes_total` +1 per tick with 4 lanes; +0 when every lane is empty |
| X4 | gauge `g` from points 0 and 1 in one bucket: the last value of one of them; never point 0's first value when it sent two |
| X5 | counter `c` from point 1 at 100, then a delayed buffer from point 0 at 99: `last_seen` 100, expiry at 400, not 399 |
| X6 | `x{env:a,pod:1}` and `x{env:a,pod:2}` with `pod` stripped by the tag filterlist: one series and one point per bucket at any P (the Agent's duplication, pinned against) |
| X7 | shutdown discard with lanes: the bucket open at the shared close time is discarded in every lane |

### 6.10 Runtime assertions

- `debug_assert!` and `plan_lane_misrouted_total`: an admitted event's key maps to the admitting lane.
- The `t_a` monotonicity assertion per (point, state lane) within an epoch.
- The budget assertions of `state.rs` (`check_bounds`), with the node limit exact.
- The lanes' flush times agree for one ordinal unless the grace expired or a lane straggled (`debug_assert!` plus
  `plan_lane_tick_divergence_total` and `plan_lane_tick_straggler_total`); no old-epoch ordinal after a reset mark's
  cutoff fires.
- A context is held by at most one instance per (lineage, open epoch) of one lane, and by more than one only during a
  crossing's switch spread or a reset window (at most `MAX_OPEN_EPOCHS + 1` per lineage).
- No payload names a retired recipient slot (stage 7, 4.7).
- Antithesis `always!`: a context is in at most one lane per lineage instance (debug builds, through the routing check).

### 6.11 Stage 7 tests

**Plan crate:** fingerprint version 2 with hand-derived digests per selector (a dual-ship addition leaves every
fingerprint and base unchanged; renumbering changes a routing output's; a declared output's changes with its
recipients), and version 1 bases accepted while they match; composition, every row of 4.5's table, pending in both
product orders, re-composition with dropped emits (degraded) and their restoration when the output returns; resources
validation and canonicalization fixtures, hand-written; `ResourceTable` (reuse by binding digest without provenance,
never of a draining lease, two-phase release order, abort releases only what it created and keeps a reopened
recipient's original deadline, reopen of a draining recipient, a reopen answered `Exited` followed by the next
incarnation only after the exit event, a failed recipient's next incarnation in the same slot, acknowledgments of an
older incarnation ignored, resource transactions ordering prepare before publish for additions and publish before
release for removals, a failed addition that still publishes the transaction's removals and withdrawals, a superseded
transaction aborted, a transaction timeout, `requires` failing a generation over a recipient whose transaction failed,
the keep rule when a lowering removes a resource the kept generation or the cut's rollback plan uses or flips a
recipient's protocol, a newer refused or pending resources revision that leaves the installed one in place, the
credits per resource, each released once by `ReleaseCredits` however its creation ends) and
`property_test_resource_table_never_releases_what_a_generation_uses` (random candidates, resource transactions and
their supersession, commits, aborts, cuts and their cancellations, failures, recipient failures, worker restarts that
repeat an action, and retirements, across several scopes, never release a lease or recipient a live or kept generation
or the published native set uses, never keep a generation or rollback plan that uses a recipient or output the latest
lowering does not declare, withdraw every recipient the latest lowering does not declare, release every credit exactly
once, and eventually release everything else); frame and
placement (exits to every encode host, outputs placing by host, the remaining capability errors with their two
messages); explain golden files with the resources section.

**`saluki-components` and `saluki-io`:** the encode host routes by lease; a lease's drain flushes with its own recipient
set; **the drain race**: a `LeaseDrain` while the shared payload channel still holds the lease's payloads acknowledges
only after they are dispatched (the `LeaseClosed` markers); a prepare and a drain served while the front task waits for
room in a full shard; an unknown lease is counted; builder channel backpressure; per-variant resolved sets; the
shutdown row of 4.6; a **differential test**: one lease per native output against the native `DatadogMetrics` on the
same buffers gives identical payload bodies and metadata. The delivery host: table add and remove; routing by resolved
slots (unfiltered) and by the native set with classes (filtered); **withdrawal** (sends to a withdrawn recipient
dropped and counted, whatever lease stamped them, and restored by a later publish); **the input barrier** (a release
racing a lease's last payloads, with a send still pending, routes them all); control served while a send waits for a
full recipient; a retiring loop dispatches queued work (the spike's
`queue_drain_dispatches_accepted_transactions_after_input_closes`, generalized); the deadline persists or drops and
counts; reopen during a drain and while a release waits at the barrier, and `Exited` after the loop ended; messages
with a stale `RecipientRef` ignored, and a repeated prepare of the held reference acknowledged; a prepare for a
directory a live loop still owns fails; a failed loop reports its exit once, and payloads to its slot are dropped and
counted until its next incarnation is live; at most 16 draining recipients; a settings swap at a request boundary; a
client rebuild, and one driven by a proxy credential change; credential rotation and conflict reporting, with draining
recipients excluded; Cluster Agent token rotation inserted at dispatch;
validation for native recipients only, an idle host ready without a warning, a declared credential never affecting
readiness; static mode passes every existing forwarder test. F20's claims pinned by tests of today's behavior (a
persisted file holds the key it was sent with, the mapper overwrites it at dispatch, directory names follow
`generate_retry_queue_id`). Telemetry caches release a per-domain entry with its last recipient and keep
`http_errors_by_code`. Tests use an explicit test root store and run with `--test-threads 4`, and ADP loads platform
roots once per process and reuses them across client rebuilds, since loading them under parallel load on macOS takes
longer than `PREPARE_TIMEOUT`.

**ADP:** live configuration in connected mode (`ConfigUpdate::Partial`, as `installer/live_tests.rs` does): add and
remove an additional endpoint (it receives from the commit, stops after its drain, and the artifact is neither stale nor
invalid); add a routing group (stale artifact, the default program delivers the selection); enable Multi-Region
Failover; enable autoscaling failover and set `cluster_agent.enabled` and a token after boot; flip
`use_v3_api.series.endpoints` for one endpoint under continuous input; change series payload limits, forwarder timeout,
concurrency, proxy, and the proxy password; remove an endpoint in the same edit that adds one which fails to prepare
(the removed one stops at the transaction while the candidate retries). Continuous-input counts are exact across every
change (no loss, no duplication), per recipient, by recording intakes, a removed recipient's up to the transaction that
withdraws it. A recipient loop that fails is replaced without a configuration change. A test against the Core Agent's
configuration stream pins whether `experimental.programmable_metrics.resources.credentials` from `datadog.yaml` changes
live in connected mode, `ENC[...]` values included (A36). **Non-metrics delivery:** logs, events, and traces follow a
live `additional_endpoints` add and remove with no loss, including while a cut is in flight and while a metrics prepare
fails. The resources product: declare, route, revoke (degraded, then restored), pending in both orders, a duplicate, an
unresolved and a removed credential, a declared recipient that fails to prepare (rejected, re-lowered without it, native
changes still apply), redelivery idempotency, restart with redelivery; resources and policy snapshots published back to
back (neither lost). **Fault tests:** a blocked recipient during retirement (deadline, then persisted or dropped and
counted); a prepare failure per resource kind (the current generation keeps running); a host's prepare timeout; a worker
restart during a resource prepare and during a two-phase release; shutdown during a drain; disk full during a
retirement's persist; a generation failure with and without a kept last-known-good after a removal, with two scopes; a
cancelled cut whose generation adds an additional endpoint. **Reclamation:** 1,000 cycles of adding and removing a
recipient and an output (native and declared): tasks, metric handles, cache entries, refreshers, scopes, budget credits,
and resource-group live bytes return to baseline. Telemetry parity: remapped output equals that of the legacy topology
for unchanged configurations. The differential against the legacy topology, with routing groups and failover added to
its configuration matrix: identical delivered series multisets per recipient.

**Correctness suites:** Panoramic runs (the `/panoramic` skill) of `dsd-events`, `dsd-service-checks`, `dsd-forwarding`,
`dsd-forwarding-burst`, `otlp-traces`, and `otlp-metrics` against the `main` image as baseline (`--image-override
baseline=<main image>`), since `dd_out` is rewritten under every pipeline and the in-process suites check metrics only.

## 7. Interfaces by crate

`[7]` and `[9]` mark the stage. `saluki-core` and `saluki-common` stay free of metric and Datadog knowledge.

| Crate | Stage | Interface | Notes |
|---|---|---|---|
| `saluki-common` | [9] | `resource_tracking::set_cpu_time_sampling(NonZeroU32)`; `ResourceGroupToken::enter` reads CPU time on a sampled entry only (thread-local `xorshift64*`, probability 1/N, delta scaled by N); `ResourceGroupToken::enter_measured` always reads; a CPU clock that tests replace (`test-util`) | default N = 1 is today's behavior |
| `saluki-core` | [7] | `execution::control` (moved from `execution::region::mailbox`, which re-exports it): `control_mailbox`, `ControlSender`, `ControlMailbox::{into_receiver, mark_running}` (public), `ControlReceiver<C>::recv() -> Option<(C, ControlReply)>`, `ControlReply::{ack, reject(GenericError), panicked(String)}`, `ControlError`; the region host ported to it; `topology::interconnect::Consumer::queued(&self) -> usize`; `runtime::worker(..).with_resource_group(name)`; `execution::EntryId` documentation: an encode host's entries are lease IDs, stable for the lease's life | additive; F22. Health, supervision, and topology are otherwise untouched by stage 7 |
| `saluki-core` | [9] | `execution::clock::SourceClock` (epoch mirror and `subscribe`); `AdmissionPointConfig::{with_source_clock, owned}`, `Stamper`, `OwnedAdmission::{stamp, heartbeat_due, stamp_heartbeat}`; `PointState::last` and the boundary-epoch rule in `observe_cut`; `StateBudget::try_acquire_below` (P9.F) | the epoch rule equals today's with one point |
| `saluki-core` | [9] | `TransformBuilder::input_lanes`; `TransformContext::take_lane_inputs`; `LaneEdge`, `LaneRoute`, and `TopologyBlueprint::connect_components_with`; `EventsDispatcher::dispatch_named_to_lane(output, LaneRoute, buffer)` with spread by first lane with room; lane input bounds (`TransformBuilder::input_buffer_events`, default 1,024); the gated tap sender (P9.F) | one bounded channel per lane |
| `saluki-core` | [9] | `RegionHostBuilder::{with_lanes, with_lane_admission}`, `LaneContext`, `trait LaneSplit`, the coordinator; `ExitSink::emit_to_lane`; `RegionDriver::end_turn(&mut self, turn, input_idle: bool, exits) -> Option<Instant>` (default `None`); `health::{Health::next_probe, Probe::respond}`; `Kernel::publish_telemetry` (default no-op, P9.F); `execution::kernel::{LaneTick, LaneFlush, LaneShare}` and `Turn::{with_lane_tick, lane_tick}` (5.5); `WorkerPoolConfiguration::Dedicated { threads }`; `cache::{CacheBuilder::with_accumulated_telemetry, Cache::publish_telemetry}` (P9.F) | additive; with one lane, today's host |
| `saluki-io` | [7] | `RetryQueue` and `PersistedQueue`: `set_max_size_bytes`, `set_disk_limits` (live settings between nonzero disk sizes) | persistence format and directory naming unchanged; turning persistence on or off stays restart-only |
| `saluki-components` | [7] | `common::datadog::delivery::{DeliveryHost, DeliveryHostConfiguration::with_events, RecipientFactory, RecipientRef { slot, id, incarnation }, DeliveryControl::{Prepare, Publish { revision, native_set, settings, withdrawn }, Release, Reopen { recipients, decls }, Abort, AbortResources { revision }}, ReopenReply::{Reopened, Exited}, DeliveryEvent::RecipientExited { recipient, outcome }, RecipientExit::{Retired(DrainReport), Failed(String)}, CloseRequest::{None, Retire { deadline }, Shutdown}, DrainReport, CredentialSource}`; the shared typed specs (I7): `EncoderSettings`, `EncodeLeaseSpec`, `RecipientSpec`, `RecipientProtocol`, `DeliverySettings` (each proxy as a URL without user information and a credential reference the host follows), `PayloadClasses`, `ResolvedRecipients` (per variant), `RecipientSlot`, the payload-metadata key, `HostPayload::{Payload, LeaseClosed { lease, shard }}` | `ResolvedRecipients` rides in payload and transaction metadata, `#[serde(skip)]`; every recipient message, acknowledgment, and event names a `RecipientRef`, every lease message (`EntryId`, `LeaseKey`) |
| `saluki-components` | [7] | `encoders::datadog::metrics::host::{MetricsEncodeHost, EncodeLeaseFactory, EncodeControl::{Prepare, Drain}}`; `MetricsRecipient` gains classes and protocol (in I7); the `encode_host` bench | `builders` is 1 in stage 7 |
| `saluki-components` | [9] | DogStatsD source `with_metric_lanes(D)`: outputs `metrics_<d>`, decoder `d` on lane `d mod D`, stream connections to the lane with the fewest live connections | conditional: the datagram queue (O4) |
| `saluki-metric-kernels` | [9] | `aggregate::lanes::{LaneShared, Resolution, Previous, FlushRecord}`, `LaneShared` implementing `saluki-core`'s `LaneShare`, `LaneShared::resolve(LaneTick, Resolution)`; `AggregateCore::{with_budget(.., lanes: Option<Arc<LaneShared>>), begin_flush_with}`, with every closed pass (tick, reset drain, final retirement, shutdown) of an instance with lanes starting from the turn's `LaneTick`; `last_seen` as a maximum; `ContextLimit::try_admit` with the bounded node acquire first; per-instance accumulation of the aggregation gauges (`AggregateTelemetry::accumulating`, published through `Kernel::publish_telemetry`); the mapper's and tag projection's shared immutable parts for `KernelSpec::with_shared` | reference property tests at one lane |
| `saluki-metric-plan` | [7] | `ir::{OutputBinding, Selector, Resources, HostBinding, RecipientDecl, PayloadClasses, HostId}`, the reserved `resources_` output prefix, a schema version bump; `extension::{DeliveryPolicy::Bounded, BoundedDelivery, Spill, Overflow::EvictOldest, Closure}`; `resources::{ResourcesArtifact, validate, canonical_endpoint, fingerprint}`; `channel`: the `resources` product (`ResourcesSnapshot`, its own high-water mark and acknowledgments), one feed slot per product (`ArtifactFeed::changed() -> (ArtifactProduct, Arc<ArtifactSnapshot>)`, `mark_unseen(product)`); `validate::Catalog::host`, and a catalog output's optional version 1 digest (`legacy_fingerprint`) | no change to the identifier grammar or the surface syntax |
| `saluki-metric-plan` | [7] | `compose`: dropped emits for removed or rebound outputs (`ComposeOutcome::Degraded`), `ComposeError::{PendingOutput, ReboundOutput}`, version 1 bases, the outcomes of 4.5; `install::resources::{ResourceTable, ResourcePlan, ResourceTransaction, ResourcesRevision, RecipientRef, CreditKey, ResourcesEvent}`, `PrepareSite::{Slot, Host, Budget { resource: CreditKey }}`, `HostPrepare { host, leases, create, reopen, requires }`, `Action::{PrepareResources, PublishResources, AbortResources, ReleaseLeases, ReleaseRecipients, ReleaseCredits}`, `InstallerCore::{resources_prepared, lease_released, recipient_retired, recipient_failed, on_resources(ResourcesEvent), credentials_changed, accepted_resources}`, the keep rule at every lowering; `compile::{FrameHosts, FrameTarget::{EncodeHost, Destination}}`, placement by host, `EntrySource::Lease` and lease tables in `PrepareRegion` and `runtime::code`; the `MissingCapability` messages; `explain` and `diff` resources sections; limits `MAX_DECLARED_RECIPIENTS` (16), `MAX_DECLARED_OUTPUTS` (16), `RESOURCES_ARTIFACT_SIZE` (64 KiB), `LEASE_QUEUE_BUFFERS` (8), `MAX_DRAINING_RECIPIENTS` (16) | |
| `saluki-metric-plan` | [9] | `compile::frame::{LanePolicy { dogstatsd: u16, state: u16, post: u16 }, FrameOptions { pipelining: bool }, KeyGroups, ExitRouting::{One, Keyed, Affinity, Spread}}`; `Frame::compute(plan, registry, namer, &FrameHosts, &LanePolicy, FrameOptions)`, which computes the lane layout in I9; `AdmissionPointInfo::lanes`, `Slot::lanes`, `Exit::routing`, `SlotKind::Isolation::{output_points, upstream}`; `OperatorDef::partition_key`, `PartitionKey`; `KernelSpec::with_shared`; `KernelBuildContext::{with_lanes, lane_share}`, `LaneBinding`, `LaneCell`; `PrepareRegion` with one kernel set and binding list per lane, bindings by lineage; `KernelControl::lane` | lane layout never enters the plan hash or the logical explain |
| `saluki-metric-plan` | [9] | `runtime`: `RegionConfig::with_lane(LaneContext, Arc<SlotLanes>)`, keyed exits with coalescing, the final old-epoch header, and per-lane progress; `state::lanes::{SlotLanes, LineageLanes, LogicalInstance, ResetMark}`; `state::rules::{ResetWindow, WindowStep, EpochSide, Observation::EpochSwitched, epoch_newer}`, `WatermarkTracker::{watermark_in, max_latest_in, switched_epoch, in_publication_epoch_with}`, `TickGate::due_at_second`; `harness::{LanedFrameHarness, DeliverySchedule::{TraceOrder, Seeded(u64), Delay { point, lane, by }}}` (`test-util`) | the interpreter and the driver share `rules` |
| `saluki-metric-plan` | [9] | `interpret`: `TraceEvent::Admit { source, lane, at, batch }`, `Interpreter::with_candidates`, `replay::ArrivalReplay`; `equivalence::{Candidates, compare_partitioned, check_capacity}`; lanes per slot and routing per exit in the physical explain; R1's cost model removed (P9.7); limits `KEY_GROUPS`, `MAX_LANES`, `PRODUCER_LANES_MAX`, `ROUTE_BUFFER_EVENTS`, `COALESCE_HOLD`, `TICK_RECORD_DEPTH`, `MAX_OPEN_EPOCHS`, `LANE_FLUSH_STAGGER` | |
| `agent-data-plane` | [7] | `programmable_metrics::resources` (from `outputs.rs`): `NativeResources::resolve(current, startup, facts, declared)` producing output bindings, recipients, and host bindings, fingerprint versions 2 and 1; `resources::params`: the `Params` codec for the typed specs; `credentials.rs`: `AdpCredentialSource` over configuration views; `host_facts.rs`: the Cluster Agent endpoint fact without `enabled` or a token; `catalog.rs`: declared outputs with labels, host schemas, each native output's version 1 digest; `topology.rs`: the fixed hosts (idle MRF, Cluster Agent by fact) with mailboxes and `FrameHosts`; `installer.rs`: resource actions and transactions, the resource budget and its credit ledger, the hosts' event channels, `ResourcesEvent` and `credentials_changed` from snapshots and configuration changes, status; `channel_stub.rs`: both products, one slot each; `api.rs`: status additions; `bindings.rs`: the newly live keys (its "liveness follows native behavior" comment revised, amendment 104); `telemetry_profiles.rs`: lease identities; `lib/saluki-app/src/accounting.rs`: the verified bounds returned; `agent-data-plane-config`, `config-overlay-model` (a string-map value type), `agent-data-plane-config-system/src/saluki_only.rs`, `schema_overlay.yaml`, and the generated `docs/agent-data-plane/configuration/configuration.md`: the credentials key and the support levels of the newly live keys | |
| `agent-data-plane` | [9] | `LaneCounts`, `lane_counts(&SalukiConfiguration, workers, grant) -> LaneCounts` (with the `Strict` fit); `PlanTopology` with lanes (D source outputs, slot components with lanes, lane edges, lane admission configurations, per-lane capacities, heartbeat periods); per-lane prepares with builds on a blocking thread; the snapshot bridge's request per lane; `plan_lane_*` telemetry; CPU-time sampling (`SALUKI_CPU_TIME_SAMPLING`) and the global pool size at startup; overlay entries for `dogstatsd_pipeline_*` and `dogstatsd_workers_count`; the measurement harness through the decoder pool; the stress test | |
| `programmable-metrics-demo` | [7] | the Team D intake, the `resources` product in the stub, the Resources tab, editor fingerprints for declared outputs, the Recipients panel, steps S8 to S11 | a first pass behind the stub protocol and the export catalog |
| `programmable-metrics-demo` | [9] | new flags `--decoders`, `--state-lanes`, `--post-lanes` (`--traffic-rate`, `--contexts`, and `--aggregate-window-secs` exist); the Lanes panel; steps S12 to S14 in a second launch; pre-aggregation scope support in the headless check | reads the `plan_lane_*` series through the telemetry viewer |

## 8. Implementation plan

### 8.1 Order

Stage 7 lands first, then stage 9, with stage 9's measurements and behavior-neutral foundations overlapping stage 7:

| Phase | Work, in parallel work trees | Base | Ends with |
|---|---|---|---|
| A | I7 (stage 7 interfaces); P9.0 (measurement harness and probes on today's code); P9.F (stage 9 foundations, neutral at one lane) | `b01049372e` | I7 merged; P9.0 merged onto I7 before phase B starts, since the stage 7 gate extends the same measurement file; first measurements M1, M2, and M4 to M8, with M3 on today's `run_request_builder` |
| B | P7.1a, P7.1b, P7.2, P7.3, P7.4 in parallel against I7; P7.5 and P7.6 against fakes; P9.F continues | I7 | I7-Z: stage 7 integrated, gated, committed; P9.F merged into it or right after |
| C | I9 (stage 9 interfaces, with the lane layout computed for real); then P9.1a, P9.1b, P9.2, P9.3a, P9.3b, P9.4 in parallel; P9.5 once P9.1a, P9.1b, and P9.2 merged; P9.6 against P9.5 | the stage 7 commit | I9-Z: increments 9.2 to 9.5 enabled one by one, the gate, P9.7 (R1 deleted), committed |
| D | conditional parts O1, O2, and O4 to O10, each only on the evidence its measurement names (O4 and O5 move into phase C when M2 shows a ceiling below the gate's top W, 8.5) | the stage 9 commit | each its own small commit |

Why this order: stage 9's encode-host requirements (5.8) are met by stage 7's encode host, so stage 9 builds on it
rather than on fakes; the two stages touch the same frame, installer, and ADP topology files, so building them
concurrently would collide; and P9.0 and P9.F touch only files stage 7 does not (benches and tests, the kernels crate,
`saluki-common`'s resource tracking, `saluki-core`'s cache, budget, kernel trait, and dispatcher, the isolation
driver's tracker, the interpreter's observation handling), apart from one call in the region driver's `publish`
(`runtime/driver.rs`), where I7 edits other functions and P9.F rebases over it. That is the stage-overlap practice the
earlier stages used ([how the work was done](programmable-metrics-status.md#how-the-work-was-done)).

Each part: implement, review through several lenses, fix, with focused tests while iterating; every fix mutation-checked
by hand; semantic fixtures hand-written; golden files only detect change. Each integration step merges, regenerates
golden files, runs [the stage gate](programmable-metrics-status.md#the-stage-gate) and the demo check, and updates this
document, the implementation design, the status document, and the release notes.

**Disk and builds.** Each work tree builds its own `target/` (this tree's `target/release` is 67 GB, with 397 GB free),
so at most four parts build at once; parts build with `CARGO_INCREMENTAL=0` and `-p` for the crates they own, run
release-mode property tests only for those crates, and delete their `target/` when they finish. Phase B's seven parts
and P9.F therefore run in two waves: P7.1a, P7.1b, P7.3, and P7.4 first (the parts others' real tests need), then P7.2,
P7.5, P7.6, and P9.F.

**Sizes.** Stage 3's five serial parts took about six hours ([how the work was
done](programmable-metrics-status.md#how-the-work-was-done)); parts here are sized in changed lines (est.), and none is
larger than about 2,000, which is why P7.1, P9.1, and P9.3 are split, with the interfaces between their halves in I7
and I9.

### 8.2 Stage 7: interfaces step and parts

**I7 (short, serial, about 1,500 lines):**

- the IR types and schema version, the `resources_` prefix, and the `Catalog::host` method;
- the core's actions and events (`PrepareSite`, `HostPrepare`, `PrepareResources`, `PublishResources`,
  `ReleaseLeases`, `ReleaseRecipients`, `on_resources`, `accepted_resources`, `resources_prepared`);
- the per-product artifact feed (`channel/feed.rs`) and the stub protocol additions; `ResourcesArtifact`;
- `saluki-core`: the control mailbox moved to `execution::control` with `ControlReceiver`, `ControlReply`, and a public
  `mark_running`, and the region host ported to it (F22); `Consumer::queued`; `with_resource_group`;
- `saluki-components`: the hosts' control messages and factory traits (`DeliveryControl`, `EncodeControl`,
  `RecipientFactory`, `EncodeLeaseFactory`, `CredentialSource`), the shared typed specs of 4.4, `HostPayload`, the
  recipient loop's interface between P7.1a and P7.1b (`RecipientLoop::spawn(spec, pieces) -> RecipientHandle { sender,
  close, exited }`), and the `MetricsRecipient` field changes (`endpoints.rs`, which P7.1b then owns);
- ADP: the `Params` name table and the codec module's skeleton (`programmable_metrics::resources::params`);
- `EntrySource::Lease` implemented for real, with one lease per native output and lease tables in `PrepareRegion` (no
  behavior change: native encoders ignore `entry`), so P7.4 can start without P7.3's compile output;
- fakes of each host: `saluki-metric-plan::install::testing::{FakeEncodeHost, FakeDeliveryHost}` for the core's tests
  and `bin/agent-data-plane/src/programmable_metrics/test_hosts.rs` for ADP's.

Exit: the workspace compiles, every existing test passes, and behavior is unchanged (no host is controlled yet).

| Part | Scope and files | Size (est.) | Tests | Exit evidence |
|---|---|---|---|---|
| P7.1a recipient loops and persistence | `lib/saluki-components/src/common/datadog/{io.rs, retry.rs, validation.rs, telemetry.rs, transaction.rs}`; `lib/saluki-io/src/net/util/retry/` (live limits); the spike's drain-on-close port | 1,500 | close requests and the drain to the deadline, a loop's exit outcome (retired or failed) reported once, live retry limits (persistence turned on or off stays restart-only), validation for native recipients only, per-domain cache release; F20's claims, and the trimming and outdated-file removal a queue does when opened, pinned by tests of today's behavior | every existing forwarder test passes in static mode |
| P7.1b delivery host | `lib/saluki-components/src/common/datadog/{delivery/, config.rs, api_key.rs, endpoints.rs, middleware.rs, protocol.rs, proxy.rs}`, `forwarders/{datadog, cluster_agent}/` | 2,000 | table, routing by slots and by native set, withdrawal, the barrier race with pending sends, control during a blocked send, reopen and `Exited`, `RecipientRef` checks, the event channel, prepares serialized per directory, a failed slot's next incarnation, `AbortResources`, settings swap, client rebuild (and on a proxy credential change), rotation and conflicts, Cluster Agent token at dispatch, static mode | every existing forwarder test passes in static mode |
| P7.2 encode host | `lib/saluki-components/src/encoders/datadog/metrics/{host.rs, mod.rs}`; `lib/saluki-components/benches/encode_host.rs` | 1,500 | routing by lease, the drain race with `LeaseClosed`, control during a full shard, unknown lease, shard backpressure, per-variant sets, shutdown, the differential against `DatadogMetrics` | identical payload bodies and metadata; the bench within its threshold (8.3) |
| P7.3 plan crate | `lib/saluki-metric-plan/src/{ir/, extension/, resources/, channel/ (beyond the feed), compose.rs, compile/{frame.rs, placement.rs, program.rs}, runtime/{code.rs, harness.rs}, validate/, export*, explain/, install/diff.rs, limits.rs}`, `schema/`, `fixtures/` | 2,000 | fingerprint digests (both versions), composition outcomes with dropped emits, validation and canonicalization fixtures, the capability-binding rule, frame and placement, explain golden files | golden files regenerated and reviewed; schemas current |
| P7.4 installer core | `lib/saluki-metric-plan/src/install/{resources.rs, core.rs, core/artifacts.rs}` | 2,000 | `ResourceTable` unit tests; `property_test_resource_table_never_releases_what_a_generation_uses`; incarnations and successors; resource transactions (one per host, supersession, timeout, `AbortResources`, withdrawal, failed additions); `requires`; pending and degraded re-evaluation; `ResourcesEvent` and revision retention; the keep rule at every lowering; the credit ledger and `ReleaseCredits` | the property test and every existing installer test pass |
| P7.5 ADP | `bin/agent-data-plane/src/programmable_metrics/{resources.rs (from outputs.rs), resources/params.rs, credentials.rs, host_facts.rs, catalog.rs, topology.rs, installer.rs, channel_stub.rs, api.rs, bindings.rs, telemetry_profiles.rs, lowering/, test_hosts.rs}`, `cli/run.rs` (the MRF warning text); `lib/saluki-app/src/accounting.rs`; `lib/agent-data-plane-config`, `lib/datadog-agent/config-overlay-model`, `lib/agent-data-plane-config-system/src/saluki_only.rs`, `schema_overlay.yaml`, `docs/agent-data-plane/configuration/configuration.md` (generated) | 2,000 | against the host fakes in its work tree: lowering, credentials and `credentials_changed`, `ResourcesEvent`, the resource budget's credit ledger across worker restarts, the hosts' event channels, status; at I7-Z: live configuration, resources product, fault, reclamation, telemetry parity, differential, the connected-mode credentials key (6.11, A36) | the work-tree tests and the overlay check pass; the real tests at I7-Z |
| P7.6 demo | `bin/programmable-metrics-demo/src/{intake/, stub/, ui/, scenario/, launcher/, checks.rs}` | 1,000 | against the stub and a recorded ADP status; S8 to S11 (8.8) at I7-Z | the headless check passes S0 to S11 twice, at I7-Z |

Dependencies: P7.1a, P7.1b, P7.2, P7.3, and P7.4 build and test in their own work trees against I7; P7.5 and P7.6
test against the fakes and run their real tests at I7-Z, as do the cross-part tests (the input barrier with a real
encode host, the non-metrics delivery test, the Panoramic suites).

**I7-Z** merges the parts, replaces the fakes, runs the live, fault, reclamation, differential, correctness, and
measurement tests, regenerates golden files, runs the demo check, and writes the release notes (8.9).

#### Changes after I7 started

A review that ran after I7 began changed these I7 interfaces. Each is a type or signature change that
I7's fix step applies, or the integration step if I7 already merged; the parts named then build on the changed form.
Sections 4.2, 4.3, 4.7, 4.10, 4.12, and 7 give the full text.

| # | What changed | I7 item it changes | Absorbed by |
|---|---|---|---|
| C1 | Recipient addressing: `RecipientRef { slot, id, incarnation }` (`saluki-components` `delivery::RecipientRef`; the plan crate's `install::resources::RecipientRef`, which ADP converts) replaces `(slot, RecipientId)` in `RecipientPrepare`, `Release`, `Reopen`, `HostPrepare`, `ReleaseRecipients`, and `recipient_retired`; the core allocates an incarnation per recipient prepare | the hosts' control messages; the core's actions and events | I7's fix step (types); P7.1b (a host ignores a mismatched reference and acknowledges a repeated prepare of the one it holds); P7.4 (allocation, ignoring older incarnations) |
| C2 | `DeliveryControl::Reopen { recipients: [RecipientRef], decls }` carries no built pieces; its reply is `ReopenReply::{Reopened, Exited}`; `Release` is acknowledged when accepted, and its outcome arrives as an event (C3) | `DeliveryControl` | I7's fix step; P7.1b; P7.4 (after `Exited`, the next incarnation only once the exit event arrived) |
| C3 | Host events: `DeliveryHostConfiguration::with_events(mpsc::Sender<DeliveryEvent>)`, `DeliveryEvent::RecipientExited { recipient: RecipientRef, outcome: RecipientExit::{Retired(DrainReport), Failed(String)} }`; the new core event `InstallerCore::recipient_failed(RecipientRef, reason)`, and `recipient_retired(RecipientRef, DrainReport)` fed from the channel; `RecipientLoop::spawn`'s `exited` resolves with the `RecipientExit` | the hosts' control messages; the core's events; the recipient loop interface between P7.1a and P7.1b | I7's fix step; P7.1a (exit outcome); P7.1b (the channel); P7.4 (`Failed`, the next incarnation in the same slot with backoff); P7.5 (the receivers kept in the installer's state outside the worker) |
| C4 | `DeliveryControl::Publish { revision, native_set, settings, withdrawn: BTreeSet<RecipientId> }`, and `PublishResources` carries each host's withdrawn set; `ResourceTable::transaction` covers declared recipients' settings and withdrawal | `DeliveryControl`; `PublishResources` | I7's fix step; P7.1b (drop and count sends to withdrawn recipients); P7.4 |
| C5 | `HostPrepare { host, leases, create, reopen, requires: [RecipientRef] }` (was `{ host, leases, recipients, reopen }`); a generation's prepare never reconfigures a recipient | `HostPrepare` | I7's fix step; P7.4 (`plan()` lists `requires`, drops reconfiguration); P7.1b (fails the acknowledgment when a required recipient is not live) |
| C6 | `Action::AbortResources { revision, hosts }` and `DeliveryControl::AbortResources { revision }`; one transaction per host, superseded by the newest lowering, timed out after `PREPARE_TIMEOUT` | the core's actions; `DeliveryControl` | I7's fix step; P7.4; P7.1b; P7.5 (dispatch and timeout) |
| C7 | `InstallerCore::on_resources(ResourcesEvent)` with `ResourcesEvent::{Revoked, Refused { config, version, reason }, Delivered { config, version, revision, artifact_hash, document: Result<ResourcesArtifact, Reason> }}` (was `on_resources(revision, Result, missing_credentials)`), and `InstallerCore::credentials_changed(missing)`; `accepted_resources()` keeps the installed revision while a newer one is refused, rejected, or pending | `on_resources`, `accepted_resources` | I7's fix step; P7.4 (revision rules per `ArtifactConfigId`, revision retention); P7.5 (builds the event from the snapshot; calls `credentials_changed` before lowering) |
| C8 | `PrepareSite::Budget { resource: CreditKey }` (was a unit variant); `CreditKey`; `ResourcePlan::admitted` and `ResourceTransaction::admitted` as `Vec<(CreditKey, u64)>`; `Action::ReleaseCredits { credits }` | `PrepareSite`; the core's actions | I7's fix step; P7.4 (the ledger's entries and releases); P7.5 (acquisition idempotent per key, held across worker restarts) |
| C9 | `DeliverySettings` carries each proxy as a URL without user information plus a credential reference, which the delivery host follows through `CredentialSource`, rebuilding and swapping its client on a change | the shared typed specs | I7's fix step (the field, and its row in the `Params` name table); P7.1b (the rebuild); P7.5 (the codec) |
| C10 | The privileged status route lists recipient incarnations as a top-level `recipients` member: the installer's live entries (`ResourceTable::recipients()`, joined with each host's `DeliveryHostStatus`) first, then the last 16 retired after a release, with `last_drain`; `GET /programmable-metrics/resources` serves the resources view | the status route | P7.5 (retired entries, the resources route); I7-Z (live entries, and retirements only of incarnations the core released); P7.6 reads it |
| C11 | `RecipientHandle { sender, close: RecipientCloser, settings: RecipientSettingsSender, exited }`; `RecipientCloser::{retire, shutdown, reopen, current}`; `RecipientLoop::{prepare, spawn, name, into_task}`; `RecipientPieces::{queue_directory, open_queue}`; `RecipientHandle::detached` for doubles; `RecipientValidationTargets` | the recipient loop interface between P7.1a and P7.1b | P7.1a; I7-Z (the router's settings publication, closer, queue opening, and validation targets) |
| C12 | `HttpClient::switchable` and `HttpClientSwitch` (`saluki-io`); `DeliveryHostConfiguration::{new, with_events, with_drain_deadline, factory, status}`, `BootPublication`, `DeliveryHostStatus` with `client_failure`; `Datadog`/`ClusterAgentForwarderConfiguration::controlled` | the delivery host's construction | P7.1b; P7.5 (wiring); I7-Z (`client_failure` in `status.degraded`) |
| C13 | `Frame::compute_with_hosts` and `FrameHosts`; `Approval::{in_effect, upgraded}`; `ScopeComposition::{outcome, legacy_outputs, notes}`; `ComposeError::{PendingOutput, ReboundOutput}`; `Plan::{binding_digest, binds_alike}`; `Catalog::{declares_hosts, refuses}`; `CatalogOutput::legacy_fingerprint` | the frame, composition, and catalog interfaces | P7.3; P7.4 (in effect, upgrades, pending); P7.5 (the catalog); I7-Z (the core's wiring) |
| C14 | `InstallerCore::{held_resources, accepted_revision}`; `ResourceTable::{recipients, lease_states, created_first}`; `ResourceBounds::recipient(decl, settings)` (the host's settings give an undeclared bound); `Live::map` (`agent-data-plane-config`) | the core's resource accessors and bounds | P7.4; P7.5; I7-Z |

#### Stage 7 as built

What the parts and the integration step changed from sections 4 and 7, beyond the corrections folded into those
sections (marked "as built" there). Each item names the test that pins it and the design note it comes from (P7.2 note
6, P7.1b R3, and so on: the notes each part kept with its work, outside this repository, whose substance this table
carries); the amendments it implies are 129 to 138 of [section 10](#10-proposed-spec-amendments).

| Area | As built | Why, and the test |
|---|---|---|
| Encode host (4.6) | A builder's failure (error, panic, cancellation) fails the host explicitly, through a guard per shard | with one shared payload channel the native signal (its channel closing) does not exist; stricter than native, as 4.16 intends (P7.2 note 6) |
| Encode host (4.6) | One construction path: `DatadogMetricsConfiguration::encoder_settings()` and `StreamBuilders::build` build the native encoder's builders and every lease's; `RequestBuilder::new` became synchronous; additional tags are a host-level input | parity of settings by construction; the differential checks what differs (P7.2 notes 9, 11, 14) |
| Encode host (4.6) | `LEASE_QUEUE_BUFFERS`, `DRAIN_DEADLINE`, and `MAX_DRAINING_RECIPIENTS` exist in `saluki-components` and the plan crate | `saluki-components` cannot depend on the plan crate; `the_hosts_limits_are_the_plans` pins them equal |
| Recipient loops (4.7) | Close requests through `RecipientCloser`: a reopen cancels a retirement's close until the final flush begins, answered atomically; reopens are counted, so a reopen followed by a release is never lost; a reopen with no input is refused; a dropped closer is a shutdown | the review's races (P7.1a notes 17 to 19; a 500-race stress test) |
| Recipient loops (4.7) | One concurrency semaphore per loop that every publication resizes, and no polling of the service while it owes permits | a rebuilt limit overshot by every request in flight, and a lowered one did not hold under a backlog (P7.1a notes 9, 20; `a_lowered_concurrency_limit_holds_while_work_is_queued`) |
| Recipient loops (4.9, 4.11) | Per-domain telemetry released with the domain's last registration; the shared retry-queue series keyed by recipient; a transaction a queue refuses or cannot persist dropped and counted, and the flush goes on past it | static-mode fixes too: two keys on one URL no longer overwrite each other's queue size, and a failed flush no longer loses the rest uncounted (P7.1a notes 15, 22) |
| Delivery host (4.7) | Exits of recipients an abort drops are reported; a publication refuses a native set naming a recipient not held live or failed; only a newer incarnation of the same identity takes a failed slot; a publication commits exactly its own revision's creations and reopens, and what its native set names | P7.1b D4 to D6, R2 |
| Delivery host (4.7, 4.8) | A withdrawn recipient is neither a duplicate nor a conflict | renaming a credential would otherwise always fail, since the old recipient is still live, withdrawn, when the new one prepares (P7.1b R3) |
| Delivery host (4.6, 4.7) | A hosted recipient loop never filters by protocol; the host decides which recipients take each payload; `Metadata::recipients` is gone | a live V3 flip made the loop's stale filter drop native-set metrics (P7.1b R4; `a_recipient_takes_every_payload_its_host_routes_to_it_whatever_variants_it_accepts`, and the ADP V3 flip test) |
| Delivery host (4.7) | A native-set member resolves to the incarnation its slot holds under the same identity; validation follows the table through `RecipientValidationTargets`, in a native forwarder's order, once per change | P7.1b R5; the order and the single pass were found at I7-Z by the telemetry parity test (`native_intake_recipients_are_validated_once_in_the_order_a_forwarder_validates_its_endpoints`) |
| Delivery host (4.8) | Credential followers are futures in the routing loop, one per reference; `CredentialSource` views come from `Live::map` | P7.1b D13, D14; P7.5 note 7 |
| Plan crate (4.4, 4.5) | `Frame::compute_with_hosts`; exits to every encode host after the boot edges' targets, in `FrameHosts` order; a fixed destination writes no lease; an exit toward an encode host needs a lease table | P7.3 notes 1 to 4, R6 |
| Plan crate (4.5, 4.14) | `ComposeError::{PendingOutput, ReboundOutput}`; `ScopeComposition::{outcome, notes}` carry the degraded reasons and the rebase note, which the core shows as the `active` acknowledgment's `error` | P7.3 note 8; the core's wiring at I7-Z (`in_effect`, `upgraded`, notes) |
| Plan crate (4.4) | `Catalog::declares_hosts` and `Catalog::refuses` | the plan crate reads no protocols, and an exported catalog carries no host schemas (P7.3 note 11) |
| Plan crate (4.3) | Endpoint canonicalization (WHATWG URLs; user information, query, and fragment refused, never echoed) | P7.3 note 12, R4 |
| Installer core (4.10) | `ResourceTable::plan` is pure; leases per host keyed by `LeaseKey { output, binding }` with `Plan::binding_digest` (recipients' hosts and protocols included); identifiers and slots allocated in rotation | P7.4 notes 1, 2; P7.3 R1 |
| Installer core (4.10) | One rule for every preparation: create, co-create, require, reopen, name a failed slot, or wait; generations create native recipients a transaction left out; one transaction in flight for all hosts | replaces "a generation `requires` every recipient the transaction adds, and fails if the transaction failed" (P7.4 notes 6, 9) |
| Installer core (4.10, 4.16) | A host's failed part holds nothing at once; attribution of a host failure to a resources revision by text, only to what the part creates first; the last 64 parts that hold nothing are remembered for late acknowledgments | P7.4 notes 10, 16, 17, R3; structured failures deferred (below) |
| Installer core (4.10) | Each wait keeps its own delay on the caller's one timer (`core/retry.rs`) | a failed recipient's 1 s retry rode on a candidate's 30 s (P7.4 R7) |
| Installer core (4.12) | `ResourceBounds { lease, recipient(decl, settings), host_queue }`; `Permissive` warns and holds a credit at zero bytes; a failed permissive verification yields zero headroom | P7.4 note 18; P7.5 note 12 |
| ADP (4.3, 4.8) | Native recipients by configured position: a key position stays a recipient while its URL lists it, whatever its key holds; a new position joins as native construction decides at startup | key rotation must not change the plan (P7.5 R2) |
| ADP (4.4) | The Cluster Agent host exists when its endpoint resolves at boot, whether forwarding is enabled or not; every host gets `dd_out`'s delivery settings | P7.5 notes 5, 6 |
| ADP (4.10) | A retirement is listed (and its provenance kept) only when the exit ended an incarnation the core was releasing: held before the exit and not after | a drop by an abort, or a loop that ended on its own, is no retirement (P7.5 R4; `an_endpoint_added_and_removed_live_receives_the_metrics_events_logs_and_traces_routed_to_it_once_each`) |
| ADP (4.14, 4.16) | `status.degraded` names each delivery host whose client could not be built | P7.5 R6; `delivery_settings_and_a_proxy_changed_live_reach_the_running_recipients_without_loss_or_duplication` |
| ADP (4.15) | A routing group's renumbering is logged with both group keys | `a_removed_routing_group_renumbers_the_outputs_after_it_each_named_with_both_groups` |
| Delivery host (4.7), review | A prepare whose directory a loop an abort dropped still owns waits for that loop's exit; an abort of its preparation rejects it | the next preparation after an abort was refused 20 times in 20 with real loops, and a declared recipient's refusal rejected its resources revision for good (`a_preparation_on_a_directory_a_dropped_loop_owns_waits_for_its_exit_and_an_abort_ends_the_wait`, `with_real_loops_an_identitys_next_incarnation_prepared_right_after_an_abort_waits_for_the_dropped_loop`; the core with fakes that model it, `a_declared_recipients_next_incarnation_waits_for_the_loop_a_cancelled_preparation_dropped_and_rejects_nothing`, and the property test) |
| Delivery host (4.7), review | The created-once memory covers the identities the host runs or holds and the last 128 others; a committed incarnation drops the failed one it replaced; a payload no native-set recipient takes is counted; a `persist` closure counts as `outcome:persisted` | the per-identity map grew without bound, the replaced chain nested at every failure, payloads vanished uncounted without a primary, and a persisted queue counted as drained (`a_host_remembers_what_it_created_only_of_the_identities_it_runs_and_the_latest_it_ran`, `a_failed_incarnation_its_committed_successor_replaced_is_let_go`, `a_payload_no_recipient_of_the_native_set_takes_is_counted_as_dropped`, `a_retirement_is_counted_by_what_became_of_the_work_the_recipient_held`) |
| Validation (4.7), review | A host with native intake recipients but no usable key is ready with the native warning | a blank primary key made `dd_out` ready silently (`a_host_warns_that_no_key_is_usable_only_once_it_holds_a_native_intake_recipient`) |
| `saluki-io` (4.7, 4.15), review | A clone of a switchable client keeps one copy of the switched client, taken again only once the switch changes | every request took a fresh copy, whose per-endpoint telemetry the registry evicted 3 s later, so `network_http_requests_*`, `transactions.*`, and `forwarder_transactions_*` never accumulated; the parity test missed it because its recorder never evicts (`a_switchable_clients_clone_keeps_its_endpoint_telemetry_from_request_to_request_until_it_is_switched`) |
| Installer core (4.7, 4.10), review | At the end of every event, a transaction follows a host's published withdrawn set that differs from what the table and the latest lowering withdraw (`withdrawal_differs`); a generation using a recipient its host withdraws while the latest lowering declares it waits for the transaction in flight to publish; a failed native recipient stays in the native set during its backoff | a retired identity stayed withdrawn and covered its next incarnation, and a generation could commit over a recipient its host still withdrew (`a_retired_endpoints_withdrawal_is_dropped_by_a_publication`, `a_generation_using_an_endpoint_added_back_while_it_drains_commits_once_the_transaction_restoring_it_published`, `a_recipient_a_generation_of_an_older_lowering_creates_after_a_newer_publication_is_withdrawn_at_once`, and the property test's world checks: a committing or arming generation uses no recipient its host withdraws while the latest lowering declares it; no stale withdrawal once no transaction is in flight; no resources revision rejected for a directory race) |
| Installer core, delivery host (4.7), review | A failed incarnation's next one that fails before it is committed keeps the one it replaced, which takes the slot back when nothing holds either (`Replaced` is a chain in the core; the router walks `Entry::replaced`) | found by the property test: an abort of both successors left the slot empty under the leases naming it (`a_failed_recipient_keeps_its_slot_when_its_next_incarnation_fails_too_and_every_later_preparation_aborts`, `a_failed_recipient_takes_its_slot_back_after_its_next_incarnation_failed_too_and_both_successors_are_aborted`) |
| ADP (4.3, 4.9), review | Native recipient identities by `queue_endpoint`; version 1 fingerprints by the configured endpoint | respelling `dd_url` live made a second primary that waited forever for the first one's directory while the first was withdrawn, cutting the primary off from all data (`the_primary_endpoint_respelled_live_keeps_receiving_every_batch_event_log_and_trace_once`, `a_native_recipients_queue_endpoint_tells_two_endpoints_apart_exactly_when_their_queue_directories_differ`) |
| ADP (4.14), review | A configuration change whose preparation keeps failing sets `last_error`, and so the status summary's degraded reason, until it is in effect | the failures showed only in the logs (`an_endpoint_that_cannot_be_prepared_is_retried_while_the_endpoint_its_edit_removes_stops_at_the_transaction`) |
| ADP (6.11), review | The reclamation test counts the budget against a memory limit, cycles a declared recipient and output beside the native endpoint, gives each cycle a domain of its own, and counts the metric handles the process holds | the budget was uncounted, so its check could not fail, and identity-keyed leaks never grew; a queue registration, a domain registration, a credential follower, or a budget credit left behind now fails it (mutation-checked) |

**Deferred from stage 7**, each with its reason (the status document's deferred work lists them too):

- Folding static mode into the delivery host (P7.1b D1): static mode is the native forwarder, which keeps every
  forwarder test meaningful; the host is pinned against it.
- Loading platform roots once per process across client rebuilds (P7.1b D17, 6.11's last sentence): builds run off
  the routing loop, so a slow load delays only the switch; a shared root store belongs in `saluki-io`'s builder.
- A structured prepare failure naming the recipient (P7.4 R3(b)), and a declared recipient's directory including its
  name (R3(a)): both change other parts' interfaces; until then a rename that keeps endpoint and credential is
  rejected while its predecessor owns the directory. Since the review, a directory a dropped loop owns makes a prepare
  wait rather than fail, so text attribution meets only failures a declaration causes.
- Tests 6.11 lists that the review did not add: a routing-group add and remove cycle in the reclamation test, and the
  non-metrics delivery of a live `additional_endpoints` change while a cut is in flight (the failed-prepare case now
  checks events, logs, and traces); per-cache counts beyond what the metric handles and the hosts' live bytes show
  (the router's tombstones, which its unit tests cover).
- A delivery host's validator keeps the per-endpoint telemetry of every domain it validated for the host's life, as a
  native forwarder's does for its fixed set: bounded by the domains a process ever validates, which stop at the first
  accepted key.
- Growth in place (4.12): a recipient's credit keeps its creation's bytes; the design (`CreditKey::Resized`,
  `PublishResources::admitted`) is P7.4 R6's. Queue bounds are capped at the host's in any case.
- 4.13's validation errors for a declared `queue_bytes` above the native bound and for spilling without storage: the
  plan crate cannot read host settings and ADP's resources validator does not see them; at run time the recipient's
  bound is capped at the host's (`ForwarderConfiguration::for_recipient`), so the declaration cannot exceed it.
- The idle encode host's 8-buffer input (4.6): rejected, see 4.6 as built.

### 8.3 Stage 7 gate

- The specification's: **fault tests pass** (4.16 and 6.11); **resources reclaimed after repeated installs** (the
  1,000-cycle reclamation test).
- Behavior equal to legacy where configuration is unchanged: the differential and telemetry-parity suites.
- Continuous-input tests across every live change: no loss and no duplication, per recipient.
- The measurement tests (plan against legacy, one and four workers, with two routing groups and Multi-Region Failover)
  within the existing tolerances (7% throughput, CPU, and allocations; 10% p99), with two changes P9.0 makes first: CPU
  is the process's, not only the primary runtime's (the global pool's workers, where lease builders run, and the
  blocking threads' CPU time around kernel builds, or `getrusage` with the generator in a child process), and the
  measurement topology keeps real encoders (native `DatadogMetrics` for legacy, `MetricsEncodeHost` for the plan)
  feeding a delivery that discards, so the encode host's front task and builders are measured.
- The `encode_host` bench: one lease within 3% per metric of native `DatadogMetrics`, with the front task's cost per
  buffer recorded.
- Non-metrics delivery: the Panoramic correctness suites of 6.11 against the `main` image, and the ADP test in which
  logs, events, and traces follow live endpoint changes.
- The stage gate commands, Vale on the changed documents and crates, and the demo check S0 to S11. **As built**, the
  gate runs Vale on every file the stage changes or adds, `make test-measurement`, and `make
  test-integration-targets` (every crate's `tests/` targets, which `make test` skips); CI runs the last two as jobs of
  their own.

### 8.4 Stage 9: foundations, interfaces step, and parts

**P9.0, measurement harness and probes** (phase A, merged onto I7 before phase B; no production code beyond probes
behind a `measurement` feature):

- `bin/agent-data-plane/src/programmable_metrics/topology/measurement_tests.rs` gains a feed through the **real
  DogStatsD source**, so the decoders, the receiver mutex, the resolver, and admission are in the path (the stage 3
  feed bypasses them): a generator thread pool sends pre-encoded payloads over UDP on loopback (datagrams up to 8 KiB)
  and over UDS streams on macOS, where UDS datagram sockets cap datagrams at 2 KiB with a 4 KiB receive buffer
  (`net.local.dgram.maxdgram`, `net.local.dgram.recvspace`) and would measure the kernel's socket buffer, and over UDS
  datagram socket pairs on Linux. Legacy and plan in one harness, at W in {1, 2, 4, 8, 16}, with
  `dogstatsd_workers_count` set to `max(2, W − 2)` explicitly (the native formula reads vCPUs, not workers). CPU is the
  process's, and the topology keeps real encoders (8.3).
- Benches: `lib/saluki-components/benches/dogstatsd_codec.rs` (decode and resolve per metric, threads 1 to 8 on shared
  resolvers), `lib/saluki-components/benches/encode_host.rs` started on today's `run_request_builder` (M3; P7.2 reruns
  it on the host), `lib/saluki-core/benches/state_budget.rs` (compare-and-swap under churn), and lock-wait histograms in
  `DispatchTarget::send` and the datagram receiver behind the feature.
- `test/scaling/` (new): a compose file running the release image (`make build-adp-image-release`) under `--cpus N
  --cpuset-cpus 0-(N−1)` with `SALUKI_WORKER_THREADS=N`, lading and an HTTP blackhole as intake each pinned to disjoint
  CPU sets (`--cpuset-cpus`, cores N and up), lading on a shared volume for the UDS socket (a `lading.yaml` from an SMP
  case), and scrapes of ADP's `:5100` and lading's `:9229`; legacy is the `main` image at the merge base (`b940bf1f8b`),
  since the legacy path exists only under `cfg(test)` on this branch. The host needs at least `W_max + 4` homogeneous
  cores (20 for W = 16). Before gating, an A/A series (legacy against legacy) per host and per W sets the tolerance, and
  the spread of the plan's rounds from p25 to p75 must fit inside it. The plan image also runs
  at `SALUKI_CPU_TIME_SAMPLING=1`, so B, O2, and the automatic P are decided at equal N, since the `main` image does not
  sample; the cross-image comparison decides gate items 1 to 4. A `make scaling-curves` target runs it. Docker on this
  Mac runs a Linux VM whose vCPUs float over performance and efficiency cores: it can check F7 and M6, not homogeneous
  scaling.

**P9.F, foundations** (phase A and B; each change behavior-neutral at one lane, in files stage 7 does not touch):

| Change | Files | Tests |
|---|---|---|
| `last_seen` as a maximum | `lib/saluki-metric-kernels/src/aggregate/state.rs` | `property_test_last_seen_is_max_under_reordering`; frozen-reference properties unchanged |
| bounded node acquire, node first | `lib/saluki-core/src/execution/budget.rs`, `aggregate/state.rs` | `property_test_bounded_node_acquire_never_exceeds_the_limit`; its loom model |
| per-instance accumulation of the aggregation gauges, and `Kernel::publish_telemetry` | `aggregate/telemetry.rs`, `aggregate/kernel.rs`, `lib/saluki-core/src/execution/kernel.rs`, the region driver's `publish` (`runtime/driver.rs`) | telemetry parity at one lane |
| sampled CPU-time accounting | `lib/saluki-common/src/resource_tracking/{groups.rs, stats.rs}` | unbiased-estimate test with a deterministic fake CPU clock (it runs on macOS); `enter_measured` always reads; default N = 1 unchanged |
| accumulated cache telemetry and the telemetry audit | `lib/saluki-core/src/cache/mod.rs`, the kernels' cache users | the audit table checked in as a test that lists per-event writes |
| the tap gate (formerly O3): while no statistics collection runs, the dispatcher skips the clone and the send to `dsd_stats_out` and adds the buffer's event count to the tap's received counter, so its series keep their native values | `lib/saluki-core/src/topology/interconnect/dispatcher.rs`, `lib/saluki-components/src/destinations/dsd_stats/mod.rs` | the tap's series equal today's with and without a collection; M7 measures the saving |
| the tracker clone removed, with 5.6's `Observation::EpochSwitched` shape | `lib/saluki-metric-plan/src/runtime/{driver/isolation.rs, state/rules.rs}`, `interpret/{installation.rs, mod.rs, drains.rs, state.rs}` | every stage 6 runtime, interpreter, and transition test |
| `KernelSpec::with_shared` | `lib/saluki-metric-plan/src/operators/mod.rs`, mapper and tag projection specs | built instances equal; shared part computed once |

**Changes after P9.F started.** Two rules of the same review reach P9.F's work in progress; neither changes behavior
at one lane:

1. `WatermarkTracker::observe` compares epochs with `epoch_newer` and returns `Observation::Fault` for an older-epoch
   header after a newer one from the same point (a `debug_assert!`, the batch counted as invalid loss), where today any
   epoch difference reads as a reset (`rules.rs:135-139`). One FIFO path never delivers it, so stage 6 never sees it.
   It is part of the `Observation::EpochSwitched` change.
2. The budget's `test-util` recording, which `check_capacity` reads (6.3), records the `used` value each
   compare-and-swap of `try_acquire_below` and `try_acquire` observed, and a node unit acquired but not yet returned
   counts as held. It is part of the bounded node acquire change; P9.4 consumes it.

**I9 (short, serial, on the stage 7 commit, about 1,500 lines):** every [9] signature of section 7 with stub bodies,
except the lane layout, which `Frame::compute` computes for real from `LanePolicy` (lane points, per-lane output points,
exit routing), so P9.3 and P9.4 can build frames with lanes (`Frame`'s fields are private, `compile/frame.rs:538-548`,
and `FrameHarness` takes `Frame::compute`'s output); `FrameOptions` threaded through; the interfaces between the halves
of P9.1 (the lane task's control queue and probe forwarding) and of P9.3 (`KeyedExit`, the state lane's admission API,
and the shared panic ledger's hooks in `runtime/driver.rs`, so that P9.3a alone edits that file); the kernel side of the
lineage records (`LaneTick`, `LaneFlush`, `LaneShare`, `Turn::with_lane_tick`, `KernelBuildContext::{with_lanes,
lane_share}`, and the aggregate factory's call to `lane_share` in `operators/aggregate.rs`, before P9.2 edits that
file), so P9.3b's driver and kernel halves meet at a fixed interface; I9 starts after P9.F merged; host fakes for P9.5
in `bin/agent-data-plane/src/programmable_metrics/test_hosts.rs`; the source's `with_metric_lanes(1)` equal to today.
Exit: the workspace compiles, and every existing test passes at D = P = M = 1, which is stage 6 behavior (with stage 7).

| Part | Scope and files | Size (est.) | Tests | Exit evidence |
|---|---|---|---|---|
| P9.1a lanes in the topology | `lib/saluki-core/src/topology/{blueprint.rs, graph.rs, built.rs, interconnect/{dispatcher.rs, consumer.rs}}`, `components/transforms/`, `runtime/builder.rs` (the pool size) | 1,500 | lane channels and edges, `LaneRoute` dispatch with spread by first lane with room, per-lane capacities and bounds, the pool size | the topology suites pass, with lanes and at one lane |
| P9.1b coordinator and admission | `lib/saluki-core/src/execution/{clock.rs, admission.rs, region/{host.rs, driver.rs}}`, `health/mod.rs` | 1,800 | owned admission (stamp order, transactions, heartbeats on period boundaries, the level-triggered reset check), `SourceClock`, the coordinator (split and joined acknowledgments, kernel requests by lane, probe forwarding, failure), `ExitSink::emit_to_lane`, `end_turn`'s deadline; loom models for the clock and owned admission | `property_test_owned_admission_keeps_stamp_order`, `property_test_source_clock_resets_once_per_jump`, loom clean |
| P9.2 frame and compile | `lib/saluki-metric-plan/src/compile/{frame.rs, placement.rs, program.rs}`, `operators/{mod.rs, aggregate.rs}`, `explain/`, `fixtures/explain/` | 1,200 | routing per exit, one path per (point, state lane), `partition_key`, `pipelining: false`, placement with lanes; golden files of the physical explain at `LanePolicy { 2, 2, 2 }` | `property_test_frames_fuse_maximally_between_fences`; golden files reviewed |
| P9.3a keyed exits | `lib/saluki-metric-plan/src/runtime/{driver.rs (`RegionConfig::with_lane`, progress), driver/keyed.rs}` | 1,200 | coalescing with the hold, per-lane progress, the final old-epoch header sent before anything newer is routed (keyed and `One` exits), the last header forwarded at finish, whole-register moves through rule 1 | `property_test_coalescing_is_exact`, `property_test_keyed_exit_routes_each_context_to_one_lane` |
| P9.3b state lanes | `lib/saluki-metric-plan/src/runtime/{driver/isolation.rs, state/{lanes.rs, rules.rs}, supervision.rs (the shared panic ledger, whose hooks in `driver.rs` I9 adds)}`; `lib/saluki-metric-kernels/src/aggregate/{lanes.rs, mod.rs (`begin_flush_with`), kernel.rs (the turn's `LaneTick` on every closed pass, `first`, `try_breach`)}`, after P9.F's change to `kernel.rs` merged | 2,000 | the O(1) tracker, the tick record by ordinal with `first_at`, membership, and the fire's critical section, the flush log for ticks and closing flushes, breach, close time per epoch, the reset window with the mark, per-epoch instances, inherited bindings, deferred crossings, and successor records, the crossing overlap count, the shared panic ledger; loom models for the shared records | runtime state tests with lanes; `property_test_lane_flush_log_matches_one_core` |
| P9.4 semantics | `lib/saluki-metric-plan/src/{interpret/, equivalence.rs}`, `interpret/replay.rs`, `runtime/harness.rs` (`LanedFrameHarness`, `DeliverySchedule`), transition tests at both levels | 2,000 | interpreter lane traces, the shared reset window, candidate mode, the arrival replay (old-epoch admits first), `compare_partitioned`, `check_capacity` (from each operation's observed `used`); fixtures X1 to X7; in its work tree against hand-built lane logs; every 6.4 property and traces T-L1 to T-L7 and stage 6's at D = 3, P = 2 under 6.7's point mapping at I9-Z | the oracles' own tests in its work tree; all of 6.4 and 6.7 at I9-Z |
| P9.5 ADP and DogStatsD | `lib/saluki-components/src/sources/dogstatsd/mod.rs`; `bin/agent-data-plane/src/programmable_metrics/{topology.rs, installer.rs, snapshot_bridge.rs, telemetry.rs, telemetry_profiles.rs}`, `lib/agent-data-plane-config/src/domains/` (the `lanes.*` keys), `lib/agent-data-plane-config-system/src/saluki_only.rs`, `schema_overlay.yaml`, `cli/run.rs` | 1,500 | lane counts and keys with the `Strict` fit, the source's lane outputs and stream lane assignment, `PlanTopology` with lanes, installer fan-out with blocking builds, the snapshot requests per lane, `plan_lane_*` telemetry, capacities; differential, lifecycle, stress, fault, and measurement tests with D producers | 6.5, 6.6, 6.8 pass; telemetry parity; lifecycle leaves registrations as they were |
| P9.6 demo | `bin/programmable-metrics-demo/src/{launcher/, ui/, scenario/, traffic.rs, telemetry/, checks.rs}` | 800 | flags, the Lanes panel, S12 to S14 in a second launch, pre-aggregation scope support in the check | the headless check passes S0 to S14 twice |
| P9.7 R1 removal | `compile/{frame.rs, decision.rs, program.rs, tests.rs, isolation_tests.rs}`, `operators/{mod.rs, routing.rs, dogstatsd.rs, enrich.rs, tests.rs}`, `explain/{physical.rs, text.rs}`, `testing/fixtures.rs`, `runtime/frame_tests.rs`, ADP `topology.rs` and its tests, benches | 800 | 5.10's replacements; the memo floor's tests and the split estimate move to decision compilation | runs only after the gate passed; the gate rerun after it |

Dependencies: P9.1a, P9.1b, P9.2, P9.3a, P9.3b, and P9.4 run in parallel against I9; P9.5 starts once P9.1a, P9.1b, and
P9.2 merged, and P9.6 against P9.5. Exit evidence that needs other parts (P9.4's properties against the real drivers,
P9.5's stress and differential tests) is produced at I9-Z.

### 8.5 Stage 9 integration increments and gate

**I9-Z** merges P9.1a to P9.6 with every lane count at 1 (stage 6 behavior, which the full suite pins), then enables
the increments one at a time; each is behavior-neutral until enabled and ships with its own measurement:

| Increment | Enables | Measures | Risk retired |
|---|---|---|---|
| 9.2 | post-aggregation lanes: M > 1, spread and affinity edges, the global pool `max(8, W)` | post-aggregation throughput; flush-to-encoder latency at 1M contexts; OTLP and checks throughput | lane hosts, control fan-out, per-lane telemetry, where no time protocol applies |
| 9.3 | producer lanes with one state lane: D lane points, admission at the lane, `SourceClock`, owned heartbeats, the multi-point tracker and reset window, cuts over D points, R1 off | pre-aggregation scaling (the gate's main item); stamp and clock contention (M10); the one-connection case (M1) | the multi-point time protocol, with state otherwise unchanged |
| 9.4 | state lanes: P > 1, keyed exits with coalescing, partitioned `dsd_agg`, shared lineage records, snapshot fan-out | admit scaling, flush latency, budget contention (M4), skew (M5), per-buffer cost on Linux (M6) | partitioned state |
| 9.5 | defaults on (`lanes.mode: auto`), CPU-time sampling on Linux; then the gate; then P9.7 and the gate again | the gate below | R1's coupling |

**The gate**, on Linux (the per-buffer costs partitioning multiplies are Linux-only system calls, F7), for each
configuration (defaults; a mapper profile; gateways; 100,000 and 1M contexts; histogram-heavy; passthrough-heavy, where
the encoder is the serial point for legacy and plan alike; one decoder with `lanes.mode: auto`; 70% of saturation; about
500 UDS stream connections) and each W in {1, 2, 4, 8, 16}, and 32 where the host allows, with lanes automatic, medians
of five alternating rounds, each within the host's A/A tolerance:

1. saturated throughput `T_plan(W) >= 0.93 × T_legacy(W)`;
2. scaling at least as good as legacy's: `T_plan(W) / T_plan(1) >= 0.93 × T_legacy(W) / T_legacy(1)`;
3. process CPU time per metric at most `1.07 ×` legacy's, and paced p99 at most `1.10 ×` legacy's (the stage 3
   tolerances), including the phase with a live filterlist change every 50 ms, whose D + P + M builds per change are
   counted, with allocations reported;
4. one saturated stream connection at least `0.93 ×` legacy, or the owner accepts the gap ([Q1](#12-open-questions));
5. semantics: every test of section 6 passes, and `plan_lane_misrouted_total` stays 0 in every run;
6. the existing SMP throughput and RSS gates at 4 vCPUs do not regress with automatic lanes (M9).

Absolute throughput per core is reported beside the ratios. If M2 shows the datagram receiver or the resolver capping
both curves below the gate's top W, items 1 and 2 would pass on two flat curves that say nothing about lanes: O4 and O5
then land in phase C, and items 1 to 3 are decided in process, where legacy runs the same source code and gets the
same fixes (the container legacy, the `main` image, cannot). Until a run at W = 32 passes, the automatic counts stay at
what W = 16 measured (P and M at most 8, D at most 16, 5.13). The in-process curves on this Mac (6 performance and 12
efficiency cores) are reported with that caveat and gate only the macOS-independent items (5 and the shape of the
curves at W up to 4); items 1 to 4 are decided on Linux ([Q2](#12-open-questions)).

### 8.6 Measurement harness and what is measured first

On today's code, before the interfaces harden (P9.0), in this order; each row decides something:

| # | What | How | Decides |
|---|---|---|---|
| M1 | Decode and resolve cost per metric; one stream connection's throughput, legacy against the plan | `dogstatsd_codec` bench; one UDS stream connection saturated in process | whether removing R1 regresses a single connection (Q1) |
| M2 | Waits on the `dsd_in.metrics` mutex and the datagram receiver mutex, and resolver throughput by threads, with busy workers | the real decoder pool at D in {1, 2, 4, 8}; lock-wait histograms; one and 50 metrics per datagram | O4 (receiver queue), O5 (resolver front cache); where both curves flatten |
| M3 | Encode and compress cost per metric at zstd level 3, series and sketches, 1M contexts at one flush | the `encode_host` bench on today's `run_request_builder` (P9.0), rerun on the host by P7.2 | S per lease (O6) |
| M4 | Budget compare-and-swap under churn | P threads re-inserting 1M contexts after a flush, P in 1 to 16 | O1 (reservations or credit cells) |
| M5 | Key skew, and the automatic P | contexts and events per lane and per group for lading's DogStatsD generators at 3,000, 100,000, and 1M contexts and power-law traffic, P in {2, 4, 8, 16}; worker time per metric at equal throughput per P | the automatic P (by worker time per metric at equal throughput, not throughput alone); whether group movement is ever needed |
| M6 | Per-buffer fixed cost on Linux | CPU-time reads, consumer and dispatcher counters, header reference counts, wakes, cross-core frees, allocator writes per group, at 1,024 and 128 events per buffer, sampled and not; routed-buffer fill at saturation, at 70% of it, and with about 500 UDS stream connections | N for CPU-time sampling; B; O2; per-decoder resource groups if allocator writes contend |
| M7 | The tap's cost | decoder time cloning for `dsd_stats_out`, and the destination task's time dropping, at saturation, with the tap gate (P9.F) and without | that the gate saves what it should |
| M8 | Flush storm | decoder drops and p99 at a tick with 1M contexts and P lanes ticking together; each lane's flush split into drain time and time blocked on output | O8 (stagger); O6 when builder backpressure dominates; B3's rationale |
| M9 | RSS | the 250,000-context SMP gate at one lane and with defaults; the firm bound computed at W = 4, 8, 16, 32, and 64 | memory acceptance; the `Strict` fit |
| M10 | `SourceClock` contention (after 9.3) | stamp wait at D in {2, 8, 16} | per-lane clocks with a shared epoch, if it contends |
| M11 | Per-lane memo hit rates | mapper and tag filterlist hits per lane under datagram traffic, at the default and at large configured cache sizes | per-lane caches at the configured size kept, or shared |

Then the **scaling curves**: in process and in containers, legacy against plan, saturated and paced, mapper profile and
defaults, with gateways: throughput and throughput per core, process CPU time per metric, allocated bytes, p99, RSS,
lane balance, and flush latency per tick.

### 8.7 Conditional parts (measured options)

Each lands only on the evidence named, as a small part with its own tests, and each keeps its interface unchanged.

| # | Option | Evidence that triggers it | Notes |
|---|---|---|---|
| O1 | Per-batch budget reservations (one compare-and-swap per up to 64 new contexts, unused units returned at the end of the batch), or credit cells per lane with stealing at the limit | M4: budget operations above 5% of state-lane time under churn | the limit stays exact; refusals while units sit reserved are capacity behavior, bounded by P × 64 units for one batch's duration; the context gauges count held contexts, not `global.used()`, and idle cell credits stay out of the breach and `is_below` decisions |
| O2 | A longer coalescing hold: keep open buffers up to `PROGRESS_PERIOD` instead of `COALESCE_HOLD` | M6: routed buffers below B/2 at the state lanes after the 5 ms hold, and per-buffer cost above 5 ns per event there after sampling | widens amendment 114's bound to 100 ms |
| O3 | (landed in P9.F as the tap gate, which keeps the tap's series native) | — | — |
| O4 | Datagram receiver without the mutex: a lock-free queue (`crossbeam_queue::ArrayQueue` with a `Notify`), or per-decoder queues fed by the reader with `try_send` | M2: decoders wait on the receiver more than 10% of the time | benefits legacy equally |
| O5 | Resolver front cache per decoder (4,096 direct-mapped entries keyed by `ContextKey`) | M2: resolver throughput flattens below the gate's rates | removes the shard lock, the expiry push, and the counters on hits; the `Arc` clone stays |
| O6 | Builder shards S > 1 for the main output | M3: one builder saturates at the gate's rates; or M8: the tick's stall is dominated by builder backpressure | up to S partial payloads per flush |
| O7 | Region lanes send straight to a lease's shard channels through a shared lease table | the front task shows in profiles | removes one hop per buffer; the lease lifecycle is unchanged |
| O8 | Flush stagger `LANE_FLUSH_STAGGER`, lane `j` ticking `j × stagger / P` later within the second | M8: socket drops at the tick | semantically neutral; widens amendment 86's exclusion by the stagger |
| O9 | Route `Metric` rather than `Event` in keyed buffers | M6: the move dominates the keyed exit | halves the bytes moved (est.) |
| O10 | Decoder-inline lanes: the decoder task runs its lane's program, removing the decoder-to-lane hop | profiles show the hop, and a 4-vCPU run shows no regression with the native decoder count | the larger change rejected in 5.16, kept behind the same lane type |

### 8.8 Demo extension

The demo stays a first pass behind the stub protocol and the telemetry viewer.

**Stage 7:**

- A fourth recording intake, **Team D**, on port 17105 (`--team-d-port`), not in `datadog.yaml`; its column reads "not
  declared" until a resources artifact declares it. The launcher writes
  `experimental.programmable_metrics.resources.credentials: {team_d: demo-team-d-key}` into the generated
  `datadog.yaml`, so the credential exists but nothing uses it.
- A **Resources** tab: a JSON editor seeded with the team D template, **Publish resources** (its own approval) and
  **Revoke resources**, and the resources config's acknowledgment timeline. The stub serves the `resources` product with
  its own revision counter and records its acknowledgments.
- The policy editor lists declared outputs (from the export catalog, with labels) beside native ones and fills
  `base.outputs` for every emitted output: from the catalog when declared, otherwise computed from the resources draft
  (`resources::fingerprint`), so the pending case can be shown.
- A **Recipients** panel: each recipient with its host, state (`active`, `draining`, `retired`), queue bytes, and last
  drain report.

A visible change in about 30 seconds: publish the team D resources; add a branch to the policy `tee`, `split(tag("team")
== "platform") { true -> emit resources_team_d; false -> drop }`; publish. Within about 3 s the platform family appears
in the Team D column while the primary column is unchanged.

**Stage 9:**

- Launcher flags `--decoders N --state-lanes N --post-lanes N` (written as `dogstatsd_workers_count` and the `lanes.*`
  keys), so lanes are visible and few on a laptop whose defaults (18 cores on this Mac) give D = 16, P = 8, M = 8;
  `/programmable-metrics/status` gains a `lanes` section (per slot: lanes, routing per exit, per-lane crossing state).
- A **Lanes** panel: one row per lane of each slot with events per second, busy fraction, input depth, and, for state
  lanes, contexts and watermark lag; each state lane's ticks are marked, so lanes are seen flushing together without a
  barrier, and crossings are marked at a cut. A skew figure (largest over mean contexts per state lane).
- Burst traffic: the existing `--traffic-rate 20000 --contexts 5000` loads the lanes and shows the keyed split.

**New headless check steps**, after S6 (S7 stays manual). Lane counts are fixed at boot, so S12 to S14 run in a second
ADP launch within the same check, with `--decoders 2 --state-lanes 2 --post-lanes 3 --aggregate-window-secs 10` (the
check's first launch keeps its 1 s window). S13 needs the `dogstatsd_pre_aggregation` scope, which the check does not
cover today; P9.6 adds it, with acknowledgment and visibility waits of at least the cut's `lcm` plus one window (40 s
for 10 s to 30 s).

| Step | Edit | What it proves |
|---|---|---|
| S8 | publish the team D resources, then the routing policy | adding an endpoint live and routing a policy to it; the primary and team B and C columns unchanged |
| S9 | revoke the resources | the policy stays `active` and degraded, its emit to team D dropped and counted, the primary unchanged; team D stops receiving, its recipient drains and retires; S8's publish of the resources again restores the emit |
| S10 | publish the routing policy first (pending), then the resources | pending in both orders; the policy installs by itself |
| S11 | pause the team D intake, then revoke | the drain reaches its deadline; the drop count is visible and matches the forwarder's telemetry |
| S12 | run with `--state-lanes 2`; a counter whose lane is empty at a tick | the canary balance over two lanes (no lost or doubled point) and fixture X1's shape end to end |
| S13 | a width change from 10 s to 30 s with `--state-lanes 2` | per-lane crossings shown; every count once in its bucket |
| S14 | S8's endpoint scenario with `--post-lanes 3` | team D receives from every post-aggregation lane: each lane's sends to `dd_metrics_encode` are nonzero, and the lease's count equals the intake's |

### 8.9 Release notes and documents

- **Stage 7:** the newly live native keys and their support levels, and the encoder keys that stay restart-only
  because other encoders share them; `dd_out`'s logs, events, and traces following live endpoint changes through
  resource transactions, and the two-step transient when `site` and `api_key` change together; the telemetry changes
  of 4.15 (the forwarders' `io_loop` task, routing-encoder series by position, the new `payload_too_large` drops,
  per-domain running totals restarting when a domain returns); the idle failover hosts, including the Cluster Agent pair
  for every node whose Cluster Agent endpoint resolves, and the MRF warning that no longer asks for a restart (`run.rs`
  says "Restart ADP after configuring the static MRF endpoint settings"); the smaller burst slack of outputs that share
  an encode host (routing groups beside the main output; an idle host keeps its full input, 4.6 as built); version 1
  fingerprints accepted while they match; removal of a used output failing closed
  (degraded), and a removed endpoint receiving nothing after the resource transaction that withdraws it, its last
  metrics payloads dropped and counted; proxy password rotation applied live; `forwarder_storage_max_size_in_bytes`
  live only between nonzero values; declared recipients never validated; the native key-reordering quirk now reachable
  live; the `resources` product and the credentials key (experimental), with its liveness as the connected-mode test
  found it (A36).
- **Stage 9:** the lane keys and the new meaning of `dogstatsd_workers_count`; the slot names R1's removal retires;
  per-lane resource groups and accounting paths; `dsd_in`'s outputs `metrics_<d>` and points `dsd_in.metrics#d`; the
  `plan_lane_*` series; resource-group CPU series sampled on Linux; admission time stamped at the lane; per-lane input
  capacity, so a burst on one stream connection gets its lane's share of the slack, which revises the stage 3 note's
  promise that DogStatsD "stops reading its socket no sooner than before"; `metrics_enrich`'s received count always
  including the post-aggregation filter's drops once R1 goes; mapper and tag projection caches at the configured size
  per producer lane; the automatic counts shrinking to fit a `Strict` grant; `last_seen` as a maximum (invisible with
  one point); the global pool size; `dogstatsd_pipeline_*` still unsupported.
- **Documents:** the stub protocol gains the `resources` product (additive within `protocol_version` 1) and the
  meaning of `pending` with an error; the implementation design gains stage 7 and stage 9 sections as built; the status
  document records both stages, moves this document's open items into its decisions table, and records the stage 7
  follow-up (credentials at rest, version-stable directories, upgrade adoption, a janitor for orphans; A18, A19).

## 9. Assumptions

Each was chosen on the owner's behalf for the least operator and contributor surprise, and says when to revisit it.

### 9.1 Stage 7 assumptions

| # | Assumption | Choice | Why it minimizes surprise | Revisit when |
|---|---|---|---|---|
| A1 | Physical structure | a fixed frame; logical outputs and recipients multiplexed in fixed hosts | no change to any component's lifecycle, health, or readiness; `dd_out` needs in-host recipients anyway | a non-metrics product needs dynamic components |
| A2 | Failover delivery hosts | keep `mrf_dd_out` and `cluster_agent_out`; build the MRF pair idle with the metrics pipeline, the Cluster Agent pair whenever an `https` Cluster Agent endpoint resolves at boot; idle hosts declare only their minimum bound | failover users keep forwarder identity, health, and queue paths (directory naming is native); enabling either failover becomes live; the boot bound grows by a few MiB, not a forwarder's 335 MiB | idle cost matters, or a merged host proves simpler |
| A3 | Encode hosts | one per delivery host, named after that host's main native encoder | encoders have one output; the busiest native series keep their names | stage 9 or later shards encoders across hosts |
| A4 | Builder tasks | one per lease (S = 1) on the global pool | exactly native compression parallelism and payloads | M3 shows one output near one core |
| A5 | Logical output identity on batches | the lease ID in `header.entry` | no header growth; the exit op already writes it; lanes need nothing | entries are needed for something else at encode hosts |
| A6 | Output IDs | native IDs unchanged (routing groups positional); declared `resources_<name>`, a reserved prefix | native IDs and telemetry stay; declared IDs never collide and need no syntax change; a renumbering degrades an artifact with a reason naming the new position | live routing edits turn out common (key-based IDs) |
| A7 | Fingerprints | the selector: roles, group keys, declared recipient lists | dual-ship additions change nothing for artifacts; pointing an explicit list elsewhere is caught | an artifact needs to pin the main output's endpoints |
| A8 | Removal of a used resource | the resource transaction that follows withdraws a removed recipient, and its delivery host drops and counts every payload to it, its old leases' last payloads included; the artifact in effect keeps running with its emits to the removed output dropped and counted (degraded) once its re-composition commits; a new artifact is `pending` | the operator's removal takes effect at once, whatever happens to metrics generations, and nothing the artifact kept out of an output reaches it; recovery is automatic when the output returns | authors would rather fall back to the default program, or operators expect a removed endpoint to get the final flush of data accepted before the removal |
| A9 | A rebound output | a new artifact is `rejected`; the artifact in effect degrades, its emits to the output dropped; a rebase is needed | an output that now means something else never receives a policy written for its old meaning | never expected |
| A10 | Recipient identity | native for configured endpoints (amendment 13); host, credential, and canonical URL for declared ones | native queues and duplicates unchanged; a declared URL change gets a fresh queue | stage 8 introduces resource identities of its own |
| A11 | Who may declare | native configuration, or the `resources` product; policy artifacts never | separate authority, as the specification says, without stage 8 | stage 8 (resource authority, audit) |
| A12 | Credentials for declared recipients | only `credentials/<name>` the operator declared; never the native keys | no artifact can point a native key at a new host | dual-shipping with the primary key is needed (an opt-in alias) |
| A13 | Declared recipients' reach | `dd_out` only; metrics only; the host's client profile; declared outputs only | a resources artifact cannot divert logs or traces or extend native outputs | per-recipient proxies, or declared recipients on native outputs, are needed |
| A14 | Non-metrics routing on `dd_out` | follows the native set that resource transactions publish from the current configuration, independent of metrics generations and cuts; each partial update applies as it arrives | the endpoint list means the same for every payload, as after a restart, and a metrics cut or prepare failure never delays it; a `site` and `api_key` change pairs old and new briefly | the logs and traces owners want restart semantics, or partial updates need a quiet period |
| A15 | Retiring recipients | attempt queued work up to `DRAIN_DEADLINE` (30 s), then native shutdown closure | the specification's default closure; nothing is lost beyond native shutdown | the deadline needs its own decision-log entry or setting (amendment 20) |
| A16 | Declared again while draining | reopen the same loop and queue; once the loop exited, a new incarnation, whose queue opens only after the exit is reported | no duplicate loops, never two queues on one directory | never expected |
| A17 | Without a metrics pipeline | `dd_out` static, native | logs-only deployments see no change | live endpoints are wanted without metrics |
| A18 | Persisted credentials | native: retries persist with their headers; the mapper overwrites the credential at dispatch | the owner does not ask for hardening yet; no persisted-format change for logs, events, and traces | a follow-up strips `DD-API-KEY` and `Authorization` at rest (recorded in the status document) |
| A19 | Queue directories | native naming (`generate_retry_queue_id`); orphans stay, as after a restart that removes an endpoint | a live change gets the directory a restart would; no upgrade migration | the follow-up adds version-stable names, upgrade adoption, and a janitor that deletes orphans by age |
| A20 | Per-recipient limits | native | the configured value means what it meant | a global delivery budget is designed |
| A21 | The forwarder's declared bound | unchanged (one queue per host) for hosts with boot recipients; an idle host declares only its minimum | existing configurations keep passing strict verification, and idle failover hosts add almost nothing | accounting moves to measured in-flight bytes |
| A22 | Live admission of bounds | everything a prepare adds (leases, an idle host's queue bound, declared recipients, new kernel instances times lanes) checked against the memory limit in the boot verification's mode | a live change that adds what a restart would count is admitted when the restart would pass, except near the limit during overlap | never expected |
| A23 | Delivery-policy slot | minimal: queue bytes (shrink only), no spill, retirement closure | only native mechanisms; nothing can raise a bound | richer policies (stage 10) |
| A24 | Last-known-good | kept generations hold their resources; at every commit and every lowering, a generation (the replaced one or the one kept before) and a cut's rollback plan are kept only while the latest lowered plan declares every output (with equal fingerprints) and recipient (with the same protocol) they use; a rollback publishes nothing | rollback stays one store; a removed destination is never resurrected | rollback after resource changes proves important |
| A25 | Pending | re-evaluated on every configuration change and resources snapshot; holds no generation; one per scope | artifact order is not guaranteed, as the specification says | Remote Config delivery semantics (stage 8) |
| A26 | Resources configs | one config in the product; an empty snapshot revokes; a newer revision that is refused, rejected, or pending leaves the installed one running | mirrors the policy product's rules: a mistyped credential name never tears down what runs | several teams need separate resources configs (a merge with conflict rules) |
| A27 | Health | per host only | readiness semantics unchanged | per-recipient readiness is wanted |
| A28 | Resource groups | native output names for leases; one shared group per host for declared outputs | bounded names; no leak | resource groups gain release |
| A29 | An encoder setting change | replace every affected lease (one early flush each) | simple, lossless, rare | frequent tuning changes |
| A30 | A forwarder setting change | applied in place at a resource transaction's publish, the only path that changes a live recipient's settings; the storage root fixed at boot, and turning persistence on or off restart-only | no duplicate loops; queues keep identity; an aborted generation leaves no setting behind | the storage root must move live |
| A31 | A recipient loop that fails | native closure, reported on the host's event channel; the core prepares its next incarnation into the same slot with backoff (1 to 30 s) | the failure is visible and recovers by itself, without a plan change or an unrelated configuration edit, and no two loops share its directory | a restart policy with its own limits is designed |
| A32 | Standalone mode | native configuration not live; the resources product works | standalone never applied updates | configuration file reload in standalone mode |
| A33 | Cluster Agent liveness | the endpoint is a host fact; `cluster_agent.{enabled, auth_token}` and autoscaling failover are live inputs | the pair exists for every Cluster Agent user, idle and cheap, so turning failover on never needs a restart | the idle pair's series surprise operators |
| A34 | Encoder keys shared with other encoders | restart-only | one meaning for every payload type | the logs, events, and trace encoders become live too |
| A35 | A declared resource that fails to prepare | its resources revision is rejected and lowering continues without it (with the revision installed before, if any) | a lower authority can never block native configuration changes | never expected |
| A36 | The credentials key's liveness | live from `datadog.yaml` in connected mode only if the Core Agent streams keys its schema lacks; restart-only from the environment; a P7.5 test against the Agent's stream decides, and the overlay and release note follow it | the doc promises nothing the configuration system has not been shown to do | the Agent's stream carries Saluki-only keys, or a Saluki-only source becomes live |

### 9.2 Stage 9 assumptions

| # | Assumption | Choice | Why it minimizes surprise | Revisit when |
|---|---|---|---|---|
| B1 | Lane counts | fixed at boot; a change needs a restart | registrations and interconnects are created at spawn; a restart re-lowers from scratch | partition movement (stage 10) |
| B2 | Producer lanes | one per decoder up to 16 (D), decoder `d` feeding lane `d mod D`; stream connections to the lane with the fewest live connections; the source only hands buffers over | no plan code in the source; the native decoder default stays; a few heavy connections do not share a lane by accident | profiles show the decoder-to-lane hop (O10); a gate run at W = 32 lifts the cap |
| B3 | State lanes | P = 1 below 4 workers, else `clamp(W / 2, 2, 8)` | multi-lane state code runs by default on typical hosts; capped at what the gate measures, with `P × B` at most 2,048; admission costs about 33 ns per event against about 250 ns for pre-aggregation, so more lanes mostly add split costs | the sweep (M5, by worker time per metric at equal throughput) and M8 give a better rule |
| B4 | Post-aggregation lanes | `M = min(D, 8)` | passthrough by affinity keeps buffer counts native; flush bursts spread; memos and memory stay bounded on large hosts | profiles show `metrics_enrich` lanes idle or saturated |
| B5 | Keys | Saluki-only `experimental.programmable_metrics.lanes.*`; the Agent's `dogstatsd_pipeline_*` stay unsupported | the Agent's keys have another meaning and default (one pipeline; automatic adjustment changes the decoder count) | operators ask for Agent key parity |
| B6 | Admission time | stamped when the lane receives a batch, one queue later than stage 6 | closer to native aggregation, which reads its clock when it processes a batch | a consumer needs the dispatch time |
| B7 | Clock | one `SourceClock` per source behind a short mutex | one reset decision; the simplest correct design | M10 shows contention (per-lane clocks with a shared epoch) |
| B8 | Order of gauges, first metadata, and kinds across producers | arrival order at the lane | the race native already has at the admission mutex; per-connection order kept | exact last-stamp order is required (a sequence per point) |
| B9 | Budgets | exact global compare-and-swap and a bounded node acquire per new context | the limit keeps its meaning at any lane count | M4 shows contention (O1) |
| B10 | Caches and memos | per lane at the configured size, telemetry accumulated onto the native handles | no shared write per lookup; memoized functions are pure; each lane's hit rate is native's; series shapes do not depend on lanes | M11 shows per-lane misses or memory cost more than sharing |
| B11 | Flush stagger | none | lowest latency; drains already yield between steps | M8 shows socket drops at the tick (O8) |
| B12 | Coalescing | per target lane, held across batches and idle input, sent on key change, size, header-only input, or after 5 ms | full buffers at saturation, where the gate measures; at most 5 ms added before a tick, never on the passthrough path | M6 shows buffers below B/2 after the hold (O2) |
| B13 | CPU-time accounting | sampled 1 in 16 on Linux, unbiased | removes the cost splitting multiplies; series stay meaningful | exact per-call CPU attribution is required |
| B14 | Key groups | 256, contiguous ranges, never moved | one table load; leaves stage 10's movement open | M5 shows skew that only movement fixes |
| B15 | The `dsd_stats_out` tap | gated: cloned only while a collection runs, its received count kept (P9.F) | native series unchanged; no per-event clone while nobody collects | never expected |
| B16 | Upstream of the lanes | the datagram receiver mutex and the shared resolver unchanged | native, and shared with legacy, so the gate compares like with like | M2 shows them capping both curves (O4, O5) |
| B17 | Encode builders | S = 1 per lease | native payloads and compression ratio | M3 shows a builder saturating (O6) |
| B18 | The global pool | `max(8, W)` threads | compression never capped below today's 8 | a host shows contention between the pool and the primary runtime |
| B19 | The reset window's fresh instance | built in the lane from the lineage's spec and build context, an empty table, only for a lineage that receives newer-epoch data while the window is open; otherwise the drained instance continues into the successor record | bounded memory, no stash, no deadlock; at one point no build at all; resets are rare | resets turn out frequent |
| B20 | Panics | a state lane loses only its instance; the panic budget is shared per artifact generation | less loss than native; an artifact's allowance does not grow with P | never expected |
| B21 | R1 | off whenever `lanes.mode` is `auto`; deleted after the gate | no lanes built for pipelined slots; the escape hatch keeps stage 6 until deletion | never expected |
| B22 | Accounting paths | per-lane resource groups and kernel groups; unchanged names with one lane | no contended counters for a lane's own allocations; bounded names. Frees of upstream allocations still write the allocating group (F6), and every decoder shares `dsd_in`'s group | M2 and M6 show allocator writes contending (per-decoder groups) |
| B23 | Per-lane input capacity | `ceil(native / lanes)` buffers for stateless slots; events-equivalent for state lanes | burst slack and firm bounds keep their native totals | bursts into one lane show loss earlier than natively |
| B24 | Health and identity | one component, one health registration per slot | `component_id:dsd_agg` and every `aggregator.*` remap unchanged | per-lane readiness is wanted |
| B25 | Units during a crossing | a context may hold units in both instances of a lane between the first and the last point's switch, and in one instance per (lineage, open epoch) when a reset falls inside the spread | no stash, no unbounded memory; values stay exact | M4 or the stress run shows refusals at the crossing that matter |
| B26 | Lanes in a tight `Strict` grant | automatic counts shrink to fit; explicit ones refuse to boot | an upgrade does not stop a deployment that fits today | operators want lanes to refuse instead |

## 10. Proposed spec amendments

Proposals for the specification's text and decision log, continuing [the implementation design's
list](programmable-metrics-implementation.md#proposed-spec-amendments) (1 to 87). Nothing in the specification is edited
without its author. Theme and kind follow [the status document's
index](programmable-metrics-status.md#spec-amendments-index): themes EXE, FRM, NAT, IR, INS, ART, TEL, STA, VER; kinds C
(clarifies), N (matches native), D (changes the design or a rule), L (defers).

**Stage 7:**

88. **Fixed frame for resources** (FRM, D). Delivery hosts and their encode hosts are fixed at boot by kind: Datadog
    with any pipeline; failover, idle, with the metrics pipeline; the Cluster Agent whenever its endpoint resolves at
    boot (a host fact), whether it is enabled and its token being live inputs. Outputs and recipients are logical
    resources inside them; an idle host declares only its minimum bound. Extends amendment 5.
89. **Leases** (INS, D). An output binding runs as a lease keyed by output and binding; generations reference leases; a
    lease drains and retires after the last live or kept generation using it retires. The commit stays one schedule
    store; addressing is data, and a payload names the recipients of its own protocol variant.
90. **Output fingerprints cover the selector** (ART, D; supersedes 41): `{host, accepts, selector}`, with roles for the
    main and failover outputs, the group key for routing groups, and the declared recipients for declared outputs.
    Recipient changes under a role neither invalidate nor stale an artifact. A version 1 base still composes while it
    equals the version 1 digest of the current configuration.
91. **Removal by the resource authority wins, and fails closed** (ART, D). When the artifact in effect is re-composed
    after an output it records disappeared or was rebound, every emit to that output becomes a counted drop and the
    artifact runs degraded until the output returns or it is rebased; a new artifact whose recorded output is missing
    is pending, and one whose output was rebound is rejected. A removed recipient stops receiving at the resource
    transaction that withdraws it, enforced by its delivery host, whatever becomes of metrics generations. Replaces
    "the current generation keeps running" for these cases; falling back to the default program was rejected, since it
    undoes everything else the artifact does.
92. **Pending and degraded** (ART, C). Pending covers new artifacts whose recorded outputs are not declared, and
    resources configs whose credentials are not configured; it is a held state with a reason. Degraded covers the
    artifact in effect with dropped emits, and native configuration that cannot be re-resolved (the specification's
    "reports a degraded state"). Both are re-evaluated on every configuration change and resources snapshot; a pending
    artifact holds no generation; one per scope. An emitted output that the base does not record is rejected at once.
93. **Resources artifacts** (ART, D). A separate `resources` product with its own revision mark and one config,
    declaring recipients (endpoint, credential reference, protocol, delivery policy) and outputs; an empty snapshot or a
    missing member revokes; policy artifacts reference outputs only. A newer revision that is refused, rejected, or
    pending leaves the installed one running. A declared recipient that fails to prepare rejects its revision, and
    lowering continues without it, so a resources artifact never blocks native configuration.
94. **Declared recipient identity** (NAT, C). `<host>/declared/<credential>/<canonical endpoint>`; native identity stays
    (amendment 13); a declared recipient equal to another live, non-draining one by canonical endpoint and key
    fingerprint fails its prepare; native configuration wins over a declared recipient, with the conflict reported; an
    equality a rotation causes is reported.
95. **Credentials** (IR, C). References name native positions (lowering only) or operator-declared credentials
    (`credentials/<name>`, the only ones resources artifacts may use, from one string-map key); a proxy's user
    information is a credential reference, which the delivery host follows live by rebuilding its client. An absent
    declared credential makes its revision pending; a blank one keeps the last usable key, as natively. Persisted
    retries keep the native format in this stage.
96. **Retirement closure for recipients** (INS, C). Queued work is attempted until the drain deadline, then native
    shutdown closure applies; a recipient declared again while it drains is reopened, keeping its original deadline,
    and one declared again after its loop exited is a new incarnation, opened only after the exit is reported. A loop
    that fails is replaced by its next incarnation, with backoff, without a plan change.
97. **The delivery-policy slot, first opening** (EXE, D). Resources artifacts may shrink a recipient's queue, forbid
    spilling, and choose its closure at retirement (`attempt`, `persist`, `discard`); outputs keep `wait`; no policy
    exceeds a native bound. This replaces the specification's "requires policy authority" for recipients: the
    recipient's declaring authority (resources) sets it.
98. **Retry queue identity stays native** (NAT, N). A recipient's directory is the native function of its identity, so
    a live change gets the directory a restart would; persisted credentials, version-stable names, and orphan removal
    are a recorded follow-up.
99. **Declared output IDs** (IR, C). Declared outputs are `resources_<name>`, a prefix native output IDs never use, in
    the existing identifier grammar of the IR; native output IDs are unchanged.
100. **Last-known-good and resources** (INS, D). A kept generation keeps its resources prepared. At every commit and
     every lowering a generation, the one replaced or the one kept before, and a cut's rollback plan, are kept only
     while the latest lowered plan declares every output (with an equal fingerprint) and every recipient (with the same
     protocol) they use; a rollback publishes nothing, and the native sets and withdrawn sets stay the current
     configuration's.
101. **Payload classes and resource transactions** (NAT, C). Native recipients keep native payload classes (additional
     endpoints receive every class); declared recipients receive metrics only; the native recipient set for non-plan
     payloads, host and recipient settings, and the withdrawn recipients follow the current configuration through
     resource transactions (prepare what is added, then publish, removals and withdrawals even when an addition failed;
     publish before releasing what is removed), one per host at a time, for the newest lowering, which wait for no
     metrics prepare and no cut. Only resource transactions change a live recipient's settings.
102. **Budgets for resources** (EXE, C). Delivery limits are per recipient, as natively, and delivery queues take no
     credits in stage 7 (the specification has them do so from stage 7; deferred). Everything a prepare adds after boot
     (leases, an idle host's queue bound, declared recipients, a recipient's bound raised in place, a generation's new
     kernel instances times their lanes) is admitted against the memory limit in the boot verification's mode, as
     credits per resource that return exactly once when the resource ends, however it ends; overlap counts both sides.
     Reads the Concurrency section's "delivery queue bytes ... are global" (its words) as meaning the same for every
     recipient.
103. **Encoder streams per binding** (EXE, C). An output has one encoder stream per live binding (and per builder shard,
     amendment 125); a rebinding flushes the old stream early. "Each output has exactly one encoder stream" holds per
     binding.
104. **Encoder and forwarder settings are plan parameters** (IR, D). Host bindings carry the resolved settings; a change
     prepares (and can fail) and applies like any other; the storage root, the stop timeout, and the encoder keys other
     encoders share stay startup-only. This revises amendment 3 ("liveness follows native") for the listed keys, which
     natively need a restart; `bindings.rs`'s documentation says so.
105. **Standalone mode** (NAT, N). Native configuration does not change there; resources artifacts do.
106. **`dd_out` without the plan** (NAT, N). Without a metrics pipeline the delivery host is static and native.
107. **Capability numbering** (FRM, C). After stage 7 the remaining `ResourceGenerations` errors (host components,
     physical edges, new exits, isolation slots) say one of two things: structure that follows native configuration
     needs a restart, since the frame is fixed at boot; structure only an artifact asks for (a new exit to an isolation
     slot, demo step S5d) is not in the frame built from the default program, and a restart does not add it, so the
     message names it unsupported until stage 10. The interpreter's stage 7 errors about state in host components move
     to stage 10 ("removing the aggregator isolation fence"). The pins in `compile/tests.rs`, `interpret/tests.rs`, and
     `installer/lifecycle_tests.rs` change with them. Settles the status document's open item on capability stage
     numbers.

**Stage 7, from the parts and the integration step:**

129. **Renumbered outputs reject new artifacts** (ART, C; refines 91 and 92). A new artifact whose recorded output is
     not declared, but whose recorded fingerprint another routing group's output now carries, is rejected naming that
     output (rebase), not held pending; the artifact in effect drops its emits with that reason.
130. **Who asks for missing structure** (FRM, C; refines 107). A missing structure is the artifact's when the node it
     is found at, or an end of the edge, has artifact provenance, or when a composed candidate leaves a host component
     out ("requires stage 10 frame changes"); otherwise native configuration's ("needs a restart").
131. **Bound outputs carry their own fingerprint rule** (IR, C; refines 90). Validation rejects a bound output whose
     fingerprint is not the version 2 digest of its binding, so a lowering bug cannot hand artifacts a fingerprint that
     does not cover the selector.
132. **Version 1 bases upgrade when they compose** (ART, C; refines 61 and 4.5's version 1 rule). From a version 1
     base's first composition on, each output it records with a version 1 fingerprint is compared by the version 2
     fingerprint the output had then, so a dual-ship endpoint added under a role leaves it composing. The upgrade lives
     in the installer and is lost at a restart, when the version 1 rule applies again.
133. **A waiting artifact is the newest of its scope** (ART, C; refines 92). Any newer authentic revision for its scope
     supersedes it, whatever becomes of that revision, and so does a snapshot that names its scope in several configs;
     a stale or unreadable delivery does not.
134. **Native recipients by configured position** (NAT, C; refines 95). Membership never depends on a key's value: a
     configured key position stays a recipient while its endpoint lists it, whatever the key holds; a new position joins
     as native construction decides at startup (a blank or duplicate key, or one that cannot be sent, is no recipient).
135. **Burst slack per encode host** (EXE, D; amends stage 3's burst-slack promise where outputs share a host). The
     outputs of one encode host share its 128-buffer input, so a burst that reaches several of them (the main output
     and a routing group) backs up to its source sooner than when each had an encoder of its own.
136. **Plan-level telemetry has owners** (TEL, C). Delivery hosts report `plan_recipients`,
     `plan_recipient_retirements_total`, `plan_recipient_withdrawn_payloads_total`, and `plan_resource_conflicts`
     under their `component_id`; encode hosts their invalid loss; the installer `plan_live_leases` and
     `plan_resource_budget_bytes`. One series has one writer.
137. **Withdrawal precedes the generation that drops a recipient** (INS, C; refines 91). Payloads routed to a removed
     recipient after the transaction that withdraws it are dropped and counted, including those the old generation's
     leases build before the generation without it commits; exactness for a removed recipient holds up to that
     transaction. An added recipient receives from the first batch of the generation that binds it.
138. **Retirements are releases** (INS, C; refines 96). A retired acknowledgment, and a listed retirement with its drain
     report, belong to an incarnation the installer released; an incarnation an abort dropped, or a loop that ended on
     its own (a failure), is not a retirement.

**Stage 9:**

108. **Admission lanes and their order** (EXE, D; with 110 and 127, supersedes 84). A source output may be a group of
     admission points (lanes), fixed at boot, each stamped by one task; a stream connection belongs to one lane for its
     life. Order is preserved within a batch, per lane, and so per connection; across lanes the native producer race
     remains. This replaces the specification's rule that inputs with the same pre-mapping context go to the same
     worker, which needs context-affine decoding and native ADP never had.
109. **Admission at the lane** (EXE, D). A lane stamps the batches it receives, so admission time is taken after the
     queue in front of the lane; its point's lock covers the stamp only (one stamper keeps per-channel FIFO), and the
     lane drives its own heartbeats and sees resets level-triggered. Refines amendment 7 for sources with lanes.
110. **One admission clock per source** (EXE, D). The points of one source share one clock and epoch, so a reset is one
     decision per source; an affected point's publication epoch is the one the cut boundary records.
111. **The keyed exit is the repartition edge** (FRM, D). In the last stateless region before a state owner, after every
     quotient, the exit routes each event by the owner's partition key (a function of its reducer key: the context key
     for `aggregate`) through 256 key groups fixed at boot; no exchange stage, no extra hop. A state owner without a
     partition key runs one lane.
112. **Lanes** (FRM, D). A slot may run on N lanes inside one component (one ID, one health registration, one control
     mailbox); lane counts are fixed at boot and are runtime layout, never plan content: plans, hashes, and the logical
     explain do not depend on them, and the physical explain shows lanes and routing (its golden files at one fixed
     policy). Per-lane input capacity divides the native capacity, so burst slack and the firm bound keep their native
     totals.
113. **Watermarks and progress under lanes** (STA, C). FIFO holds per (point, lane path); a state lane's watermark is
     the minimum over its points; progress is judged per (exit, target lane) against the last buffer actually sent,
     forwarded at once on an epoch or generation change; an epoch change first sends the open buffers and the point's
     final old-epoch header to every state lane, before anything newer, and finishing forwards the last header.
114. **Coalescing into a state lane** (STA, D). Consecutive events of one lane with equal generation, epoch, exit entry,
     point, and admission second may share a buffer under the latest of their stamps, across batches; a buffer waits at
     most `COALESCE_HOLD`. Exact for state owners that read admission seconds; amendment 86's exclusions extend to a
     batch whose arrival coalescing delays across a tick's deadline, by at most the hold.
115. **The reset window** (STA, D). Per state lane, instances are per lineage and epoch: older-epoch input feeds the
     older instances and newer-epoch input fresh ones, built as new instances, never by rebuilding in place; every
     older instance, a cut's new one included, reset-drains at the minimum of the points' exact final old-epoch times
     once every point switched, and a cut-retiring one crosses only there; a shared reset mark stops old-epoch ticks in
     every lane at once; at the close every lane's instances of a lineage join one successor record. Replaces the rule
     that a reset at one of several points drains everything, and amends stage 6's choice of no rebuild on the data path
     for the window's empty instance.
116. **Node-level aggregation rules under lanes** (STA, D). Per lineage instance, ticks by ordinal: one first instant,
     one instant and one flush time per tick; the idle-counter back-fill start is the latest non-empty flush in any
     lane, once every member lane resolved the previous tick, for ticks and closing flushes alike; the flush is counted
     once per non-empty tick or closing flush; the breach warning fires once per episode; one shutdown close time,
     from the instance's own epoch.
117. **`last_seen` is the latest admission second seen** (STA, N): a maximum, identical with one admission point.
118. **The context limit under lanes** (STA, D). One global budget and exact per-node counts (a bounded acquire, node
     first) keep every limit's meaning at any lane count; which contexts win across lanes is capacity behavior. The
     admission-policy slot stays `FirstCome`: the specification's credit splitting is no split at all, R5b's suppression
     does not change admission, so its proof holds, and `ContextBudget` is the slot's seam for O1. Per-batch
     reservations or credit cells are a measured fallback, refusing while units sit reserved only as capacity behavior.
119. **Time-driven output per state lane** (STA, C). Each state lane stamps its output at its own output point and
     latch, with the stamping generation from its own tracker; during a cut, one tick may be stamped `old` in one lane
     and `new` in another, and each context's points go through one generation.
120. **Spread edges** (FRM, C). A stateless slot downstream of the last state owner may receive buffers in rotation,
     from sources with one point and from state lanes, since nothing downstream needs FIFO; a state owner downstream of
     a spread edge is unsupported until stage 10.
121. **Panic budgets across lanes** (INS, C). A panic budget belongs to the artifact generation across lanes, so it does
     not grow with the lane count, and a generation that exhausts it stops in every lane; a state kernel's panic loses
     only its lane's instance.
122. **R1's cost model is removed** (FRM, D; withdraws amendment 47). Fusion is maximal between fences, and lanes give
     parallelism; R4 keeps its memo floor and split estimate.
123. **Verification of partitioned execution** (VER, C). In trace-order delivery, and in a replay of each state lane's
     arrival order, outputs equal the reference interpreter's exactly at every lane count, except where a lane's timing
     decided an outcome (counted and excluded); under interleaved delivery, per-kind checks with maximal gauge
     candidates, histogram sample multisets, sketch bins, and metadata candidates; a capacity oracle over held units
     (held at most the limit, refusals only while full, loss balance). Refines the stage 9 reordering row of
     Verification and amendment 31.
124. **The stage 9 gate** (VER, D). On Linux, at 1, 2, 4, 8, and 16 workers, and 32 where available: saturated
     throughput at least 0.93 of legacy's, scaling (throughput at W over at 1) at least 0.93 of legacy's, process CPU
     time and paced p99 within the stage 3 tolerances, one saturated stream connection at least 0.93 of legacy or an
     accepted gap, and every section 6 test passing; automatic lane counts stop at what the gate measured.
125. **Encoder builder shards** (EXE, C). An output's lease may run S builder shards, chosen per whole buffer; payloads
     are independent requests, so payload boundaries stay unobservable.
126. **Telemetry under lanes** (TEL, C). A slot with lanes keeps one component identity; kernel and cache series
     accumulate per instance and publish deltas onto the native handles, so sums are native and the flush count is once
     per non-empty tick; no shared handle is written per event; per-lane series are bounded by `MAX_LANES`;
     resource-group CPU time may be sampled, unbiased.
127. **Several sources into one state owner** (FRM, L). A state owner fed by points of more than one source clock stays
     unsupported (stage 10), as does one fed through a spread edge; `StateOwnerPoints` then requires one source clock.
128. **A cut's crossing under lanes** (STA, D; amends 70 for more than one upstream point). Each state lane crosses when
     every point's `new` header reached it, and a reset-retiring instance at its window's close; between the first and
     the last point's switch at a lane, or during a reset window, a context may hold units in one instance per
     (lineage, open epoch) of that lane (capacity behavior), while values stay exact, since the instances hold buckets
     on either side of `T` and of the reset. Amendment 70's "the two lineages' contexts never overlap"
     holds with one upstream point.

## 11. Risks

**Stage 7:**

1. **No measurement of encode plus compress exists** (F16). One builder per output is native parity by construction, but
   the bench may show the main output already near one core at realistic rates, which would pull S > 1 into stage 7.
   Mitigation: P7.2 runs the bench before integration; shards are already in the lease's shape.
2. **`dd_out` is rewritten under every pipeline**: logs, events, and traces depend on the delivery host. Mitigation:
   static mode is the same code path with a fixed table; every existing forwarder test runs against it; the native
   routing rule is kept verbatim for payloads without a resolved set.
3. **The input barrier** is the one new ordering argument. It relies on single-consumer FIFO and on the encode host
   acknowledging a lease's drain only after it dispatched past every shard's `LeaseClosed` marker, which FIFO per
   sender on the shared payload channel makes sufficient. Mitigation: the drain race and barrier race tests, and a
   debug assertion that no payload names a retired slot.
4. **Telemetry changes** (4.15) are visible to anyone charting `metrics_routing_encode_<i>` interconnect or health
   series, or `metrics_enrich`'s per-output sends, and the idle Cluster Agent pair appears on most Kubernetes nodes.
   Precedent exists (stages 3 and 4); remapped output is unchanged.
5. **Removal wins, and fails closed**, departing from the specification's text: the artifact in effect keeps running
   with its emits to a removed output dropped (amendment 91, Q3). When the removal is a renumbering, data the artifact
   meant for a group that still exists is dropped, counted and named in status, until the author rebases. A removed
   recipient is withdrawn at the resource transaction, so the last payloads its old leases still address to it are
   dropped and counted rather than delivered.
6. **Positional routing-output IDs** make live routing edits degrade or reject artifacts that emit to a later group
   (trace F). Key-based IDs would avoid it but break "node ID equals native component ID"; revisit if live routing
   edits are common.
7. **Native persistence is kept** (A18, A19): persisted retries hold the key they were sent with, and a removed
   recipient's directory stays until a recipient of its identity returns, as after a restart today. Nothing changes for
   a downgrade. Mitigation: the follow-up is recorded with F20's facts pinned by tests.
8. **Idle failover hosts** cost two components each, a few tasks, health series, and about 2.7 MiB of boot bound for
   operators who never use failover.
9. **Bounds stay an underestimate** for multi-endpoint configurations, as today; everything after boot is admitted.
10. **The keep rule at every commit** narrows rollback after a resource removal to the default program.
11. **Native configuration liveness is exercised only in connected mode**; the demo shows the resources path.
12. **Prepare latency** of a recipient with a large persisted backlog depends on listing, not reading, its directory;
    P7.1a verifies that `PersistedQueue` reads no file contents at construction.
13. **The native key-reordering quirk** becomes reachable live: removing the first key of an additional endpoint's URL
    moves the index-0 recipient, with its queued payloads, to the next key's organization, so data queued for one
    organization is delivered under the key of another. Recorded and release-noted, not changed; retiring a positional
    recipient whose key fingerprint moves to another position is the remedy if it matters.
14. **Resource transactions are a second installer path** beside generations, now carrying recipient settings and the
    withdrawn sets too. Mitigation: one transaction per host at a time, superseded by the newest lowering and aborted
    on a timeout; metrics never depend on them for delivery (payloads name their recipients), a generation names the
    recipients it requires, and the resource-table property test interleaves both, with worker restarts and recipient
    failures.

**Stage 9:**

15. **Per-buffer costs that only Linux pays** (F7). Splitting batches multiplies CPU-time system calls that macOS skips
    entirely, so every in-process number on this Mac leaves out the largest risk to the saving. Mitigation: sampled
    accounting lands in P9.F, before any split; M6 measures on Linux; the gate is decided on Linux (Q2).
16. **Upstream ceilings shared with legacy**: the datagram receiver mutex and the shared writes of the context resolver
    may flatten both curves around 6 to 12 cores (est.). The gate is relative to legacy, so it can pass on two flat
    curves that say nothing about lanes. Mitigation: M2 first; absolute throughput per core reported; if a ceiling
    falls below the gate's top W, O4 and O5 land in phase C and items 1 to 3 are decided in process, where legacy gets
    the same fixes (8.5).
17. **One saturated stream connection** runs its whole pre-aggregation chain on one lane once R1 goes, where legacy
    pipelined it over five tasks (0.68 to 0.72 of legacy before R1, with a scripted feed). Mitigation: M1; the gate's
    one-connection case; Q1.
18. **The reset window is new protocol**, and resets are rare in production, so bugs would hide. It depends on four
    mechanisms working together: per-epoch instances, the final old-epoch header sent before anything newer, the
    shared reset mark, and the successor record that every lane's instances join at the close.
    Mitigation: one rule in `runtime::state::rules` shared by the driver and the interpreter; the multi-point reset
    property with ticks inside the spread and a reset during a cut, traces T-L2 and T-L3, the clock's and the tick
    record's loom models; with one point it reduces to stage 6 exactly.
19. **Customer-visible zeros depend on the shared flush log**, and a lane still gated at the previous tick makes another
    lane fall back to its own back-fill start. Mitigation: the "every lane resolved the previous ordinal" rule, fixture
    X1, the idle-counter property at eight lanes with a delayed lane, exactness in arrival replay, the divergence
    counter, and demo step S12, whose low cardinality exercises the rule every tick.
20. **Key skew**: a slow state lane blocks every producer lane once its channel fills (all-to-all edges), and skew
    decides which lane is slow. Mitigation: M5 before the automatic P is fixed; per-lane contexts and depth in
    telemetry; stage 10's group movement is the remedy.
21. **Budget contention under churn** (the re-insertion burst after a flush at 1M contexts). Mitigation: per-instance
    gauges remove three of five shared writes per new context; M4; O1.
22. **The encoder stays the fan-in**: at high cardinality or passthrough-heavy traffic, one builder per output bounds
    flush delivery for legacy and plan alike. Mitigation: M3; O6; the gate includes a passthrough-heavy configuration.
23. **Memory times lanes** for kernels, memos, caches at their configured sizes, and open buffers (5.12: about 8 to 11
    MiB at 8 vCPUs, 19 to 27 MiB at 32 or more with the defaults). Mitigation: bounds per lane at boot; automatic
    counts capped and shrunk to fit a `Strict` grant; M9 on the SMP RSS gate and the firm bound at W up to 64.
24. **Visible identity changes**: R1's slots disappear, resource groups and accounting paths gain lanes, CPU series are
    sampled on Linux, `t_a` is stamped at the lane. Mitigation: release notes; remaps unchanged and tested; with one
    lane names are unchanged.
25. **Interpreter and runtime divergence.** Mitigation: shared rules (tracker, gate, window, cut time, stamping); exact
    arrival replay at every lane count; transition traces at both levels.
26. **Coalescing and the tick.** A held buffer can reach a state lane after a tick's deadline that it would have beaten
    (by at most 5 ms), within amendment 86's class (amendment 114). Below saturation the hold sends one buffer per 5 ms
    per producer and state lane, so per-buffer costs there stay above 1/B per event; M6 measures both regimes.
27. **Scheduling.** Tokio's work stealing moves lane tasks between threads, so table locality and NUMA effects are
    unmeasured; pinning is a later, measured option. This Mac's 6 performance and 12 efficiency cores make curves above
    6 workers noisy.
28. **Two stages, shared files.** Stage 7 and stage 9 both change the frame, the installer, and ADP's topology.
    Mitigation: stage 7 integrates first; stage 9's overlapping parts (P9.0, P9.F) touch disjoint files.
29. **Units held twice during a crossing.** Near the context limit, a cut's crossing spread at a lane holds a context
    in two instances (one per lineage and open epoch when a reset falls inside it), so contexts may be refused for
    those milliseconds, longer under backpressure (amendment 128).
    Mitigation: `plan_lane_crossing_overlap_contexts`; the capacity oracle counts held units; the stress run cuts every
    10 s near the limit.
30. **The gate host.** Items 1 to 4 need a Linux host with at least 20 homogeneous cores for W = 16, and an A/A series
    before gating (Q2). Without one, the stage 9 gate cannot be called, though implementation proceeds.

## 12. Open questions

**No question blocks the design or its implementation.** One (Q2, a gate host) needs the owner before the stage 9
gate can be called; the others are confirmations of documented defaults, each of which can be revisited.

| # | Question | Blocks | Default until answered |
|---|---|---|---|
| Q1 | If M1 shows one saturated stream connection below 0.93 of legacy once R1's pipelining is gone, does the owner accept the gap, or keep a narrow pipelining fallback for lanes fed by a single connection? Decoding may dominate, in which case the question disappears. | nothing now; it arrives with M1 and is gate item 4 | build without a fallback; measure |
| Q2 | **The gate needs a Linux host with at least 20 homogeneous cores** (W = 16 plus four for lading and the blackhole, on disjoint CPU sets), and 36 for W = 32. Can the owner provide or approve one? Docker on this Mac runs a VM whose vCPUs float over performance and efficiency cores, so it can check F7 and M6 but not homogeneous scaling, and on macOS the CPU-time reads that partitioning multiplies do not exist. | **calling the stage 9 gate** (not implementation) | P9.0 builds the container harness and its A/A series; in-process curves are reported with the caveat; the automatic lane counts stay at their caps until a gate run passes |
| Q3 | Confirm amendment 91: when its authority removes or rebinds an output the artifact in effect uses, the artifact keeps running with its emits to that output dropped and counted (degraded), instead of the specification's "the current generation keeps running" (which keeps the removed destination receiving) or a fallback to the default program (which undoes the artifact's other routing and drops); and that a removed recipient stops receiving at the resource transaction that withdraws it, the last payloads its old leases address to it dropped and counted rather than delivered (A8). | nothing | as designed |
| Q4 | Confirm amendments 108 and 109: across producers, a gauge's last value, first metadata, and kind conflicts follow arrival order at a lane (the race native has at its admission mutex); admission time is taken at the lane, one queue later than stage 6. | nothing | as designed |
| Q5 | Confirm assumption A14: a live change to `additional_endpoints` also changes where logs, events, and traces go, through a resource transaction that waits for no metrics cut or prepare, as a restart would. | nothing | as designed |
| Q6 | Accept decision-log entries for the new limits that change behavior: `COALESCE_HOLD` (5 ms), `KEY_GROUPS` (256), `MAX_LANES` (64), `PRODUCER_LANES_MAX` (16), `ROUTE_BUFFER_EVENTS` (2,048), `TICK_RECORD_DEPTH` (4), `MAX_OPEN_EPOCHS` (4), CPU-time sampling (16), `LEASE_QUEUE_BUFFERS` (8), `MAX_DRAINING_RECIPIENTS` (16), and `DRAIN_DEADLINE` (30 s) now applied to recipients (already open under amendment 20). | nothing | the values above, each with a note in `limits.rs` |
