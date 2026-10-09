# Programmable metric processing: implementation design

**Status:** working design for implementing [the specification](programmable-metrics.md), stages 1 through 6, plus a
local demo. The specification stays authoritative for semantics. This document fixes code layout, interfaces, and the
decisions the specification leaves open. Where the code contradicts the specification, [proposed spec
amendments](#proposed-spec-amendments) records what we do and why. Revision 2 incorporates a five-lens design review.

## Contents

1. [Scope](#scope)
2. [Code layout](#code-layout)
3. [Execution contracts in `saluki-core`](#execution-contracts-in-saluki-core)
4. [Physical frame and region programs](#physical-frame-and-region-programs)
5. [The plan crate](#the-plan-crate)
6. [Installation](#installation)
7. [ADP integration](#adp-integration)
8. [Demo](#demo)
9. [Stage plan](#stage-plan)
10. [Stage 6 outline](#stage-6-outline)
11. [Proposed spec amendments](#proposed-spec-amendments)
12. [Risks](#risks)

## Scope

In scope: stages 1–6 and a demo built on stage 5. The demo boots ADP on the programmable substrate running the default
program, then edits that program live through a demo server that stubs the artifact channel and shows the effect in a
telemetry viewer.

Out of scope: stage 7 resource generations (new outputs, recipients, encoder settings as plan parameters), stage 8
Remote Config, stage 9 partitioning, stage 10 extensions. The extension-point **types** for those stages exist from
stage 1, and only native defaults are accepted.

Easily changed layers get first-pass implementations behind narrow interfaces: the surface syntax
(`parse(&str) -> Result<Parsed, Diagnostics>`, `print(&PolicyArtifact, &PrintOptions) -> String`) and the demo UI (an
HTTP/JSON API to the demo server). Neither is linked into ADP.

## Code layout

```text
lib/saluki-core/src/execution/        generic execution contracts (no metric or Datadog knowledge)
  header.rs        BatchHeader and its field types                                        [types 1, wired 2]
  clock.rs         AdmissionClock: max-latched wall clock, reset epochs                   [2]
  admission.rs     AdmissionPoint (per component output), AdmissionSchedule, heartbeats  [2]
  generation.rs    GenerationId, GenerationToken (retirement notification), GenerationRef [types 1, wired 2]
  kernel.rs        Kernel trait, Turn, Budget, Progress, DrainPolicy                      [1]
  region/          RegionDriver trait, RegionHost transform, control mailbox, supervision [3]

lib/saluki-metric-kernels/            native metric semantics, extracted once          [1: names, matcher; 2]
  intake_name.rs   moved from bin/agent-data-plane/src/components/metric_name.rs
  matcher.rs       SortedSetMatcher, the sorted string-set matcher (exact and prefix; raw bytes or intake names),
                   and compact_prefixes, the one prefix compaction
  mapper/          DogStatsD MetricMapper core (from saluki-components), also the mapper's kernel           [2]
  prefix/          NamespacePrefix (from ADP's dogstatsd_prefix_filter)                                     [2]
  timestamp_split/ TimestampSplit and its functions (from dogstatsd_no_agg_split), with its native telemetry   [2]
  tag_projection/  TagProjection, its rules and context memo (from tag_filterlist), with its native telemetry [2]
  host_enrichment/ HostEnrichment (metrics unchanged) and HostTags (from host_tags)                       [2]
  selection/       name sets for gateways and post-aggregation filtering: RawNameSet, IntakeNameAllowlist,
                   histogram_statistic_entries, is_scalar_series (from metric_filter, the autoscaling gateway,
                   and dogstatsd_post_aggregate_filter)                                                     [2]
  aggregate/       AggregateCore: the state machine and its run-loop decisions, with a resumable flush
                   (moved unchanged in 2, t_a and watermarks in 6)
  params/          plan-owned parameter types (no agent-data-plane-config types): mapper profiles, namespace prefix,
                   tag rules and value rules (with their validation), histogram statistics                      [1]
  testing.rs       `test-util`: metric strategies, shared with the plan crate's `testing`                  [2]
  <kernel>/reference.rs, <kernel>/tests.rs   frozen pre-extraction copies and kernel-vs-copy property tests  [2]

lib/saluki-metric-plan/               IR, contracts, compiler, interpreter, installer core
  model/ ir/ canonical.rs predicate/ operators/ validate/ interpret/ equivalence.rs explain/ limits.rs extension/
  fixtures/        hand-written default-program plans (with and without the no-aggregation pipeline)       [1]
  compile/  frame.rs (physical frame), placement, rewrites R1-R7, region programs, decisions   [3, 4, 6]
  runtime/  MetricRegionDriver implementing saluki-core RegionDriver                           [3]
  install/  diff (with correspondence), generation lifecycle, installer core (no I/O)         [3, 5]
  channel/  artifact channel types                                                             [5]
  compose.rs, export.rs                                                                        [5]
  testing/  `test-util`: metric strategies, fixtures, harnesses                               [1+]

lib/saluki-metric-plan-syntax/        surface syntax: parser, printer, spans                    [demo]
                                      for tools only: no crate that runs plans may depend on it

bin/agent-data-plane/src/programmable_metrics/   (compiled only for tests until stage 3 runs it)
  host_facts.rs bindings.rs catalog.rs outputs.rs telemetry_profiles.rs    [2, 4]
  lowering/        Lowering::{new, lower}, and its tests: the configuration matrix, a hand-written
                   every-branch fixture, the structural comparison with the legacy blueprint, and a
                   run of the lowered program through the reference interpreter                         [2, 4]
  topology.rs installer.rs                                                  [3, 5]
  channel_stub.rs api.rs                                                    [5]

bin/programmable-metrics-demo/        demo server, embedded UI, recording intakes, load        [demo]
```

Dependency direction:

```text
saluki-core <- saluki-metric-kernels <- saluki-metric-plan <- saluki-metric-plan-syntax <- programmable-metrics-demo
                      ^      ^                  ^
                      |   saluki-components  <-  agent-data-plane
                      |                             |
                agent-data-plane-config-system  <---+
```

- `saluki-metric-kernels` depends on `saluki-core`, `saluki-common` (the native fast hash collections),
  `saluki-metrics`, `metrics`, `saluki-antithesis` (the moved state machine keeps its assertions), `stringtheory`,
  `regex`, `smallvec`, `ddsketch`, `foldhash`, `hashbrown`, `snafu`, and `tracing`, and on `proptest` under its
  `test-util` feature. `saluki-components` depends on it, so the legacy mapper, aggregation, and autoscaling gateway
  transforms call the extracted cores instead of keeping copies; so does ADP for its own transforms. It stays light so
  that `saluki-components` does not rebuild on compiler edits.
- `agent-data-plane-config-system` depends on it for one function: it validates `metric_tag_value_allowlist` at load
  with the kernels crate's `params::validate_tag_value_rule_keys`, the one implementation of that rule, and words the
  errors in terms of its keys. The configuration model (`agent-data-plane-config`) depends on neither.
- `saluki-metric-plan` adds `serde`, `serde_json`, `schemars` (with `derive`), `tokio` (`sync`), and `saluki-tls` (for
  FIPS-consistent SHA-256). It does not depend on `saluki-components` or `agent-data-plane-config`. Operators never read
  configuration; ADP converts typed configuration into the kernels crate's parameter types.
- Legacy components stay where they are as thin adapters over the kernels until the stage 3 plan-driven topology
  replaces them. They then become test-only differential references (`#[cfg(test)]`), after the non-metric parts of the
  legacy builder functions are split out (construction rule 4).

## Execution contracts in `saluki-core`

Documentation in this module must not reference Datadog. Types are defined in stage 1 so the kernel API does not churn
when stage 2 wires them.

### Batch header

```rust
#[derive(Clone, Debug, Default)]
pub struct BatchHeader {
    /// Admission time `t_a`, stamped once at the admission point. `None` means unstamped.
    pub admitted_at: Option<AdmissionTime>,
    /// The admission point that stamped `t_a`.
    pub admission_point: Option<AdmissionPointId>,
    /// The admission point's clock epoch; it increments on every clock reset.
    pub epoch: u32,
    /// The generation that processes this batch within its segment, stamped at the admission point.
    pub generation: Option<GenerationRef>,
    /// Set by a region when dispatching to another region: the consumer's logical entry under the batch's generation.
    pub entry: Option<EntryId>,
}
pub struct AdmissionTime(u64);        // nanoseconds since the Unix epoch
pub struct AdmissionPointId(u32);
pub struct EntryId(u16);
```

- `FixedSizeEventBuffer` carries `header` with `header()`, `header_mut()`, `set_header()`, `with_header()`, and
  `take_header()` (stage 2). `Clone` copies it, sharing the generation. `Default` and `with_capacity` are unstamped.
  `clear()` also resets the header, so a recycled buffer never keeps a generation alive. The header (40 bytes) is
  stored inline, so `size_of::<EventsBuffer>()` grows from 40 to 80 bytes; the interconnect bounds already count one
  struct per channel slot plus `N` events per in-flight buffer, and heartbeats allocate no event storage.
- **On the kernel path the header travels beside the buffer, never inside it.** A host takes the header out of the
  input buffer (`take_header()`) before calling a kernel, passes it to `Turn::new`, and stamps every buffer it drains
  from `PortOutputs` with the header that output carries (buffers created for pushed events start unstamped). Kernels
  read the header only through `Turn::header()`; `input.header()` is unstamped during the call, so a kernel that
  clears and refills its input cannot unstamp anything.
- `FixedSizeEventBuffer::with_capacity(n)` (n ≤ N) for exits sized to their contents (landed in stage 1, because
  kernel port buffers use it), and an order-preserving `retain(FnMut(&mut Event) -> bool)` for kernels (stage 2).
  `remove_if` and `extract` keep `swap_remove_back`, because legacy references use it.
- An empty, stamped buffer is header-only: a **heartbeat** from a source or a **progress** batch from a region.
- `Dispatchable` gains `fn header_mut(&mut self) -> Option<&mut BatchHeader> { None }`; event buffers implement it.

### Admission points and schedules

- An **admission point** is a component **output**. The blueprint marks it:
  `TopologyBlueprint::with_admission_point(output: impl AsRef<str>, AdmissionPointConfig)`, where `output` is a
  component output ID (`"dsd_in.metrics"`, or `"src"` for a default output) and
  `AdmissionPointConfig::new(id, mode, schedule: ScheduleHandle, clock_reset_threshold)` takes optional
  `.with_heartbeat(period)` and `.with_wall_clock(W: WallClock)` (default `SystemWallClock`; `ManualWallClock` under
  `test-util`). The threshold is a parameter, not a `saluki-core` constant, because the plan crate's limits table owns
  its value (`limits::values::CLOCK_RESET_THRESHOLD`). The blueprint rejects, with actionable errors, an invalid output
  ID, a missing component or output, a payload output (only event outputs of sources, decoders, and transforms
  qualify), an output marked twice, a reused admission point ID, and a heartbeat period outside `MIN_HEARTBEAT_PERIOD`
  (1 ms) through `MAX_HEARTBEAT_PERIOD` (1 hour): a shorter one sends a heartbeat on every timer tick while idle, and a
  much longer one overflows the driver's deadline. The bindings flow
  through the build state and `BuiltTopology` into `ComponentInterconnects`, which, after attaching every edge's sender,
  creates one `Arc<AdmissionPoint>` per marked output and calls
  `Dispatcher<EventsBuffer>::set_admission_point(&OutputName, Arc<AdmissionPoint>)`. That call captures the output's
  senders as `WeakSender`s and returns an owner token that the output's dispatcher keeps for as long as it lives.
  Attaching a sender to the output afterwards, or attaching one point to two outputs, is an error, so the heartbeat
  sender list cannot drift from the data path's.
- `mode = Admit` (sources) stamps unstamped buffers and sends already stamped ones as they are. `mode = Readmit`
  (opaque hosts) always overwrites `admitted_at`, `admission_point`, `epoch`, and `generation`, and clears `entry`, so
  nothing crosses an opaque host.
- **Clock** (`AdmissionClock`, `execution/clock.rs`): `t_a = max(previous t_a, wall clock)`. A backward jump larger
  than the threshold, measured from the latched `t_a`, is a reset: the epoch increments (wrapping, so epochs compare for
  equality) and `t_a` restarts from the wall clock. `stamp() -> ClockStamp { admitted_at, epoch, reset }`. A point logs
  a warning on every reset.
- Stamping happens in `DispatchTarget::send`, under one `tokio::sync::Mutex` per admission point held across stamp and
  the sequential fan-out. Every downstream channel of that output therefore sees `t_a` in FIFO order, even when several
  tasks share one dispatcher (the DogStatsD decoder pool). The graph is acyclic, so the mutex cannot deadlock.
  `BufferedDispatcher` flushes and `dispatch_one` go through the same path. Outputs without an admission point send
  buffers unchanged; ADP marks no output in stage 2, so the legacy topology runs exactly as before.
- Under the same mutex the point loads the shared `AdmissionSchedule` and chooses the batch's generation:

  ```rust
  pub enum AdmissionSchedule {
      Steady(GenerationRef),
      /// Stage 6: batches with t_a >= at, or stamped after this point's epoch advanced, get `new`.
      Cut { old: GenerationRef, new: GenerationRef, at: AdmissionTime },
  }
  impl AdmissionSchedule {
      pub fn generation_for(&self, t_a: AdmissionTime, epoch: u32, epoch_at_publish: u32) -> &GenerationRef;
      pub fn into_handle(self) -> ScheduleHandle;
  }
  pub type ScheduleHandle = Arc<ArcSwap<AdmissionSchedule>>;
  ```

  `generation_for` is the pure rule (`Cut`: `new` if `t_a >= at` or `epoch != epoch_at_publish`). Each point remembers
  the cut it last read by the `GenerationId` of its `new` generation (identifiers are never reused, so nothing is kept
  alive), together with its clock's epoch when it first read a cut to that generation and whether it has stamped a
  batch with it. Publishing a cut again, as a new value or with another `at`, keeps the record while `new` is the same
  generation, so a point that a reset moved to `new` stays there and the same stamp always gets the same generation;
  only a cut to another generation starts a new record. It reads the schedule before its clock, so a reset detected by
  that very stamp counts as after publication; heartbeats read it the same way, so a reset that a heartbeat finds moves
  later batches to `new`. One schedule handle exists per topology through stage 6. A commit is one store into it.
  Regions never assign generations; a batch without one is a wiring fault, counted and dropped as `invalid`.
- `AdmissionPoint::status() -> AdmissionPointStatus { last_admitted_at, epoch, admitted_to_new }` is what the stage 6
  installer reads to compute `T` and to detect a conflict: `admitted_to_new` is set once the point stamped a batch
  with the current cut's `new` generation, whether time reached the cut or a clock reset did (amendment 42).
- **Heartbeats** are a method on the shared admission-point state: `AdmissionPoint::heartbeat() -> HeartbeatOutcome
  { Sent, Closed }` sends a header-only `EventsBuffer::with_capacity(0)` stamped with `t_a`, `epoch`, and
  `admission_point` (never a generation or entry) through the output's `WeakSender`s, upgraded one at a time, under the
  same mutex, so their order relative to data is guaranteed. Before stamping, a heartbeat checks the owner token: once
  the output's dispatcher is dropped it reports `Closed` without stamping, even if sibling components keep the
  downstream channels open (every upstream of a fan-in holds a clone of the downstream component's one sender).
  `run_heartbeats()` sleeps until one period after the last stamp (on the Tokio timer, a deadline that saturates rather
  than overflowing), sends only if nothing was stamped for a full period, and returns once a heartbeat reaches no
  channel: the owner's dispatcher is gone, every receiver is gone, or the output has no channels. The topology runs one
  driver per point that has a period, as a temporary, non-significant worker in the component's own supervisor that
  also stops on its shutdown signal; the driver stops by itself too, within a period of its component stopping, so it
  never outlives its component for long and never keeps a channel open.
  Heartbeats are disabled by default and until stage 6; the period is `min(limit, smallest flush interval of state
  owners downstream)`.
- Stage 6 uses `status()` for what the cut protocol needs beyond the rule: a point's last `t_a` for computing `T`, and
  the conflict raised by a point that already admitted to `new` (outline item 4).

### Blueprint introspection and graph checks

- `TopologyBlueprint::snapshot() -> Option<BlueprintSnapshot>` (`None` once initialized) describes the blueprint's
  structure: `components()` sorted by ID (`ComponentSnapshot { id, component_type, input: Option<DataType>, outputs:
  [OutputSnapshot { id, data_type }] }`, plus `component(id)`), `edges()` sorted (`EdgeSnapshot { from:
  ComponentOutputId, to: ComponentId }`), and `admission_points()` (`AdmissionPointSnapshot { output, id, mode,
  heartbeat }`). Equal structures have equal snapshots whatever the assembly order. `DataType` is exported from
  `saluki_core::topology`. Lowering compares a lowered plan with this snapshot of the legacy blueprint.
- The graph rejects a repeated edge (`GraphError::DuplicateEdge`), which would deliver every buffer twice, and its
  cycle check runs a depth-first search from every component, so cycles on paths that end in encoders and forwarders
  (no destination) are found. A cycle is reported in edge order, starting and ending at its smallest component ID.

### Generations

```rust
pub struct GenerationId(pub u64);     // monotonically increasing, never reused, including for last-known-good
pub struct GenerationToken { id: GenerationId, on_retire: Option<RetireNotifier> }
pub type GenerationRef = Arc<GenerationToken>;
```

A generation is alive while any batch stamped with it exists, the schedule references it, the installer holds it, or a
stage 6 state lineage emits under it. Dropping the last reference runs `GenerationToken::drop`, which sends `retired(id)`
to the installer. Regions keep each generation's program behind a `Weak<GenerationToken>` and release the program when
the weak reference dies.

### Kernels and turns

```rust
pub trait Kernel: Send {
    /// Processes one batch. Single-port kernels transform `input` in place; multi-port kernels route through `turn`.
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut EventsBuffer) -> Progress;
    fn advance_time(&mut self, _turn: &mut Turn<'_>) -> Progress { Progress::Done }
    fn control(&mut self, _turn: &mut Turn<'_>, _control: &mut dyn Any) -> Progress { Progress::Done }
    fn finish(&mut self, _turn: &mut Turn<'_>, _policy: DrainPolicy) -> Progress { Progress::Done }
}
pub enum Progress { Done, Yielded, WaitUntil(Instant) }
```

- `Turn` exposes the header, host clocks, port outputs with a `route_all(port)` fast path (the whole input buffer moves
  to one port without touching events, matching native pass-through paths), a work `Budget` (events, scalar points,
  emitted bytes, state work), and a per-turn counter sink (`TurnCounters`, loss by class in events and points).
  `Turn::new(header, now, &mut Budget, &mut PortOutputs, &mut TurnCounters)` needs no topology. Time-driven calls
  (`advance_time`, `finish`) carry the state owner's watermark, set with `Turn::with_watermark` and read with
  `Turn::watermark() -> Option<AdmissionTime>`: a bucket `[s, e)` closes only when the watermark is at least `e`, and
  `None` closes nothing. `now` is for scheduling only and is never compared with bucket bounds. Port 0 is the primary
  port: what a kernel leaves in `input` goes there unless `route_all` chose another; after every non-`Yielded`
  `consume`, the host calls `PortOutputs::complete(input)`, which moves the leftover buffer whole. Individually pushed
  events go into buffers created with `with_capacity`. Writing to a missing port panics (a kernel bug, caught by
  supervision). `DrainPolicy` is `Shutdown` (native shutdown behavior; discards count as `shutdown` loss) or `Retire`
  (emit everything still held).
- Kernels never await, perform I/O, read configuration, or schedule their own work. Stateless kernels always return
  `Done`; at most 1,024 events per batch makes the batch the unit of stateless work. `Yielded` and `WaitUntil` exist
  for stage 6 drains. While any call is yielded, the host calls the same method again before any other: no batch
  reaches a kernel whose `advance_time`, `control`, or `finish` is yielded. `WaitForCapacity` is reserved until stage 7
  adds a dispatcher capacity API; until then the host simply awaits the send.
- Kernels are built inside a Tokio context (installer task, region task, or a current-thread runtime for the
  interpreter and tests). Native caches inside kernels keep their expiration tasks and native telemetry; expiry is
  unobservable. A host-driven expiration mode is a later refinement (see amendments).
- `control(&mut dyn Any)` carries kernel-specific control, such as the aggregation context-snapshot request in stage 6.

### Region host

```rust
pub trait RegionDriver: Send + 'static {
    type Control: Send + 'static;
    fn accepts_input(&self) -> bool;                       // false while a yielded drain is pending
    fn process(&mut self, turn: &mut Turn<'_>, batch: EventsBuffer, exits: &mut ExitSink) -> Progress;
    fn resume(&mut self, turn: &mut Turn<'_>, exits: &mut ExitSink) -> Progress;
    fn control(&mut self, message: Self::Control, exits: &mut ExitSink) -> Result<(), RegionFailure>;
    fn next_deadline(&self) -> Option<Instant>;
    fn advance_time(&mut self, turn: &mut Turn<'_>, exits: &mut ExitSink) -> Progress;
    fn finish(&mut self, turn: &mut Turn<'_>, exits: &mut ExitSink, policy: DrainPolicy) -> Progress;
    fn reset_after_panic(&mut self, failed: PanicSite) -> PanicOutcome;
    fn publish_telemetry(&mut self);
}
```

- `RegionHostBuilder<D>` implements `TransformBuilder`. Its outputs are the slot's exits from the boot
  [physical frame](#physical-frame-and-region-programs). It declares bounds for its kernels (from the operator registry)
  plus in-flight exit buffers. It keeps one `Health` handle across all generations.
- **Run loop.** `select!` over `health.live()`, input (only while `accepts_input()`), the control mailbox, and the
  driver's deadline. While exit buffers are being sent, the loop keeps answering `health.live()` and the control
  mailbox, because prepare and abort touch only the program table. Exits are dispatched in exit order, each send awaited:
  the native "outputs wait for downstream" delivery policy, head-of-line coupling included. At most `work_per_turn`
  batches run between control checks.
- **Control mailbox.** The installer never blocks on a region: each region has a small mailbox (a `watch` of the
  latest pending control plus a queue for acknowledged steps). Closed reply channels mean shutdown.
- **Supervision.** Every driver entry point that runs kernel code runs under `catch_unwind(AssertUnwindSafe(..))`; no
  build profile sets `panic = "abort"`. On a panic the host clears the `ExitSink` without dispatching (no partial
  delivery), counts the batch's events (recorded before the call) as `invalid` loss with reason `region_failure`, and
  calls `reset_after_panic`. The driver rebuilds every kernel of every live generation from its `KernelSpec` (factory
  plus parameters) and clears all derived per-generation state: memos, cursors, exit scratch, and per-turn counters.
  A panic is charged to the provenance of the panicking node. See [supervision](#supervision-and-fallback) for budgets.

## Physical frame and region programs

The topology is built once (`TopologyBlueprint` initializes once; transform outputs are fixed at `add_transform`), so
physical structure cannot follow each candidate plan. The physical layer is split in two:

1. **Frame** (boot, fixed until restart): computed from the boot default program. It contains the fixed components
   (source I/O hosts, opaque hosts, the stage 6 aggregation isolation slot, encoders) and the **slots** between them.
   A slot is a maximal connected set of synchronous nodes between fences. Slots may have several inputs. Each slot is
   one `RegionHost` component named by the lowering-supplied slot namer (never by an artifact node ID). Each slot's
   exits are every fence edge leaving it plus every boot output downstream of it, whether or not any program emits to
   it. Unused exits stay connected and idle.
2. **Program** (per generation): the planner places each logical node into a slot, honoring fences. A candidate is
   installable when every node can be placed and every edge between slots uses a frame edge. Otherwise the install is
   rejected with an error naming the missing capability (stage 7). R3 may prune nodes and emits inside programs, but
   never removes exits. Rewrites take the frame as input; a rewrite that would place work outside the frame is skipped
   and recorded in `explain`.

Entries: a slot's program maps each batch to a logical input. If the header's `entry` is set (by the upstream region,
which knows the consumer's program for the batch's generation), that is the entry; otherwise the slot maps the header's
`admission_point` (a source or opaque-host output) to an entry. Entry IDs are per generation, so a later generation can
send several logical branches over one physical edge (stage 6 per-branch widths) without new topology.

An `emit` or `drop` whose only input crosses a fence lowers to a direct blueprint edge (or nothing), never to an
emit-only region.

**Region programs** run batch-at-a-time steps: `map(kernel)` in place, `expand(kernel → ports)`, `decide(fn →
targets)`, `concat`, `state(kernel)` (stage 6), `exit`, and `drop`. A decision computes exit sets first, filters the
original buffer in place with the order-preserving `retain` and moves it to its last destination, and gives other exits
buffers created with exact capacity. Native whole-buffer fast paths are kept through `route_all`. Exit buffers are
never recycled (they leave by value); only region-internal scratch is.

**Decisions** (stage 4): a maximal connected group of `split` and `tee` within one slot program, with no intervening map
that writes a field its predicates read, compiles to one pure function to an exit set. The R4 memo is keyed by `Context`
(its 64-bit identity) plus kind, direct-mapped with a fixed capacity, and never allocates on eviction. Each decision
counts events per exit set per turn; at publish time, off the data path, per-node per-port counts are derived from the
decision's logical tree (exact, because a decision contains no merge) and published at INFO under the logical node ID.

**Watermark progress** (contract in stage 3, used in stage 6): for each exit the frame marks as leading to a state owner,
the driver tracks the last `t_a` forwarded per admission point. When a batch leaves no events on such an exit and the last
forwarded `t_a` is older than one heartbeat period, the driver emits a header-only progress batch on it. Exits to opaque
hosts, host taps, and encoders absorb progress. The validator rejects a stage 6 plan in which a state owner has more than
one physical path from the same admission point.

## The plan crate

### Semantic model

- `Kind { Counter, Rate, Gauge, Set, Histogram, Distribution }`, `KindSet`, `TimestampForm`, `PortType { kinds,
  timestamps }`. A rate's interval lives in values, not in kinds used by predicates and port types.
- `Field { Name, Host, Tags, OriginTags, Kind, Points, Metadata }` and `FieldSet`; derived field `IntakeName` reads
  `Name`, and a write to `Name` invalidates it. `EventView` computes derived fields lazily, once per event, with a stack
  `NameBuf`, only when a predicate needs them.

### IR

A plan is a DAG serialized as JSON (`deny_unknown_fields`). The schema is generated by `schemars` and checked into
`lib/saluki-metric-plan/schema/` with a test that it is current.

```json
{
  "ir_version": 1,
  "nodes": [
    {"id": "dsd_in", "op": "source", "params": {"binding": "dogstatsd", "timestamps": "mixed"},
     "provenance": {"native": "dsd_in"}},
    {"id": "dsd_prefix_filter_blocklist", "op": "split",
     "params": {"predicate": {"intake_name": {"matches": {
        "values": {"config": "dogstatsd.metric_filter.values"},
        "prefix": {"config": "dogstatsd.metric_filter.match_prefix"}}}}}}
  ],
  "edges": [{"from": "dsd_in.metrics", "to": "dsd_enrich.in"}],
  "outputs": [{"id": "dd_metrics_encode", "accepts": {"kinds": "all", "timestamps": "mixed"},
               "fingerprint": "sha256:…", "recipients": ["dd_out/primary"], "delivery": "native"}],
  "scopes": {"post_enrich": {"inputs": {"metrics": "metrics_enrich_host_tags.out"}}}
}
```

- **Node IDs** are typed: `NodeId { ns: Reserved | Policy, local }`, with `local` matching
  `^[a-z](?:[a-z0-9]|_(?=[a-z0-9])){0,62}$` (no trailing or doubled underscores), serialized as `local` or
  `policy/local`. Reserved IDs derive from native component IDs. Identity strings for telemetry use the serialized
  form; accounting groups exist only for reserved nodes, and all policy-namespace nodes of a slot share one `policy`
  group.
- **Ports** come from the operator and its parameters (`tee` has `ports: n`, `split` has `true`/`false`, `merge` has
  `inputs: n`). Every input and output port has exactly one edge; fan-in exists only through `merge`. Inline terminal
  IDs come from one function, `derived_terminal_id(parent, port)`, shared by lowering and the syntax.
- **Parameters** are typed by the operator's schema: booleans, integers (limited to ±(2^53−1)), decimals (encoded as
  strings), strings, durations, enumerations, lists, sets, string maps, predicates, output references, credential
  references (never values). A configuration-bindable parameter is an `Operand<T>`: a literal, or
  `{"config": "<binding>"}`. In a lowered or composed plan a bound operand also carries its resolved value, as
  `{"config": "<binding>", "resolved": <value>}`; exports and artifacts never do. Bindings occur only in node
  parameters, and within parameters the member names `config` and `blob` are reserved (a map or record parameter
  cannot have them), which lets hashing strip resolved values from each parameter without knowing its type. Nothing
  outside parameters is stripped, so outputs or scopes named `config` and `resolved` stay covered by hashes. Binding
  names are period-separated `[a-z0-9_]+` segments. Durations are strings of an integer and a unit (`h`, `m`, `s`,
  `ms`, `us`, `ns`), written only in canonical form, the largest unit that represents them exactly (`"1m"`, never
  `"60s"`): any other spelling is rejected with the canonical one in the message, so equal plans are written, and
  hashed, alike. `ParamType` (const-constructible), `ParamValue`, and `decode_params(&[ParamSpec], &Params)` interpret
  raw parameters against an operator's schema; the plan stores them as a raw JSON object. `ValueType` is the owned,
  serializable form of `ParamType` (`{"type": "list", "element": {"type": "string"}}`), for catalogs read away from
  the host; `ParamType::admits` and its `Display` delegate to it.
- **Blobs** are an artifact-document feature. Any parameter value of an artifact node, or any value nested in one, may
  be written as `{"blob": "<hash>"}`, referring to the artifact's `blobs` table (keyed by content hash). Reading an
  artifact checks that every blob is stored under its content's hash and that every reference names a stored blob.
  Composition replaces each reference with the blob's content (`Blobs::inline`), so plans have no blob table, no
  parameter type is a blob, and operators never see references.
- **Provenance** per node: `native(component_id)` or `artifact(revision, hash)`. Plans also carry plan-level provenance
  (configuration change number, artifact revisions).
- **Outputs** declare accepted port types, a binding **fingerprint** (a digest of encoder, recipient set, and forwarder),
  physical recipient identities, and a delivery policy (only `native` until stage 7).
- **Scopes** name the replaceable policy regions, their named inputs (output ports that feed the region), and their
  optional named exits (`exits: {name: input port}`, the ports outside the region that it feeds). The region is every
  node reachable from the inputs without entering an exit; a scope without exits extends to its `emit` and `drop`
  nodes. Validation (`scope_closed`) requires every scope to be closed: nothing outside the region feeds it except
  through its inputs, every exit is fed from inside it and leads out of it, and no two scopes share a node. Stage 5
  ships `post_enrich` with input `metrics` and no exits. A **policy artifact** is `{ir_version, scope, base:
  {program_hash, outputs: {id: fingerprint}}, nodes, edges, blobs}` and replaces exactly one scope; edges read
  `input.<name>` and `emit` nodes name declared outputs. From stage 6, edges to a scope's exits read `exit.<name>`.
- **Versioning.** `ir_version` bumps only for structural changes. New operators, parameters, and predicate leaves are
  additive and checked by the registry. A `migrate` table and an accepted-range constant exist from stage 1.
- **Canonical form and hash.** RFC 8785: object keys sorted by UTF-16 code units explicitly (never relying on map
  order), minimal string escaping, and ECMAScript number formatting restricted to the integer range above, with the RFC
  test vectors plus a cross-implementation fixture. Numbers must be written as JSON integers: anything a JSON parser
  reads as floating point (`1.0`, `1e3`, `-0`) is rejected, because parsers disagree on its precision, so within this
  profile ECMAScript formatting is the plain decimal integer. `PlanHash` is `sha256:<hex>`. Documents are read with a
  parser that rejects any object repeating a member name (RFC 8785 assumes I-JSON), so every reader of a document sees
  the same document.
- **What each hash covers.** Serializing a plan or artifact writes its **normalized form**: members at their defaults
  (empty `outputs`, `recipients`, `exits`, base `outputs`, `blobs`, and the `native` delivery policy) are left out, so a
  document this crate writes reads back and writes out byte for byte. `Plan::hash` is the RFC 8785 digest of the
  plan's normalized form without plan-level provenance, with resolved values stripped from node parameters under
  `ResolvedValues::Exclude`; the export's `base.program_hash` uses that form, so native parameter changes never change a
  base hash and structural changes do. **Approval binds to the received artifact document**: `ReceivedArtifact`
  (`{artifact, hash}`, from `ReceivedArtifact::from_json`) keeps the RFC 8785 digest of the document exactly as
  received, before migration, which is what an approval tool computes and what stays valid across ADP upgrades that
  migrate the document. A cross-implementation fixture checks both hashes against digests computed with Python.
- **Artifact shape.** Artifact nodes have `ArtifactNodeId`s (local identifiers other than `input` and `exit`) and no
  `provenance` member, so an artifact cannot claim native authority; edges use `input.<name>` or `<node>.<port>`. Port
  names are identifiers or decimal indices (`0`..`9999`). Reading an artifact rejects resolved values in any node's
  parameters (`IrError::ResolvedInArtifact`) and an inconsistent blob table (`IrError::BlobTable`). The checked-in
  schemas are `plan.schema.json`, `policy-artifact.schema.json`, and `predicate.schema.json` (parameters are open in
  the plan schema).

### Predicates

```text
const(bool) | flag(Operand<bool>) | is_empty(Operand<list>)
| name|intake_name|host : eq(s) | prefix(s) | matches { values: Operand<list>, prefix: Operand<bool> }
| host_unset
| has_tag(key) | tag(key): eq | prefix | matches
| has_origin_tag(key) | origin_tag(key): eq | prefix | matches
| kind_in(KindSet)
| all([..]) | any([..]) | not(..)
```

- `in(list)` and `prefix_in(list)` are sugar for `matches` with a literal mode. Every string match, including `eq` and
  `prefix`, compiles to the extracted native sorted-set matcher (`SortedSetMatcher`), so filterlists, allowlists, and
  predicates share one implementation. A missing `intake_name`
  makes every `intake_name` leaf false.
- `flag` and `is_empty` are constant given parameters. R3 folds them inside programs only; structure never depends on
  them. They express live toggles and "an empty allowlist forwards everything" without special-case nodes.
- Canonical form: flatten `all`/`any`, sort operands by canonical JSON, deduplicate, remove double negation. It also
  unwraps single-operand junctions, turns empty `all`/`any` into `const(true)`/`const(false)`, rewrites `in` and
  `prefix_in` as `matches`, and sorts and deduplicates literal and resolved value lists; it never folds constants
  (that is R3).
- JSON: every predicate is a one-member object, for example `{"host_unset": {}}`, `{"has_tag": "env"}`, and
  `{"tag": {"key": "team", "value": {"eq": "platform"}}}`. `Predicate::compile(&dyn BindingResolver)` resolves operands
  (literal, then the plan's resolved value, then the resolver) and builds each string match once; `CompiledPredicate::evaluate`
  takes an `EventView` and never allocates.
- The validator warns, without rejecting, when a predicate cannot match its input port type (for example `kind_in
  {counter}` after DogStatsD aggregation, which emits counters as rates).

### Operators and contracts

`OperatorRegistry::native()` is the closed set. Each `OperatorDef` provides its parameter schema (`&[ParamSpec]`),
`check_params` for rules between decoded values that types alone cannot express, `ports(params)`, its effect contract,
`output_types(params, ports, inputs)`, `transition(old, new)`, `specify_bounds(params, &mut MemoryBoundsBuilder)`, and
`kernel(params) -> Option<KernelSpec>`, which every synchronous native operator other than the routing operators has
from stage 2 (`dogstatsd_mapper`, `namespace_prefix`, `timestamp_split`, `tag_projection`, `host_enrichment`, and
`host_tags`); I/O and opaque hosts never have one, and hosts run `split`, `tee`, `merge`, `emit`, and `drop` themselves. `OperatorRegistry::configure(op, &Params)` decodes and checks the parameters,
reporting every problem, and returns a `ConfiguredOperator` with its ports and contract.
`OperatorRegistry::with_operators` (behind `test-util`) builds registries of test operators for tests in any crate.

- **Kernel factories.** `KernelSpec` wraps a factory closure, `Fn(&KernelBuildContext) -> Result<Box<dyn Kernel>,
  KernelBuildError>`, that has captured the node's parameters; hosts call `KernelSpec::build(&context)`. The
  `KernelBuildContext` carries the node ID and the `ComponentContext` the kernel reports telemetry and memory
  accounting under, and names its caches after (for a default-program node, its native component's identity, from
  `telemetry_profiles.rs`). `KernelBuildContext::detached(node)` names the component after the node in a topology
  named `plan`, for the interpreter and tests. Stage 6 adds the shared `StateBudget` to the context.
- **Transition classes come from contracts.** `ConfiguredOperator::transition_to` compares the old and new state
  contracts: two stateless ones `Retain`; keyed ones with a different reducer key or reducer `Reject`, with a different
  or unresolved time contract or a different admission policy `Cut`, and otherwise `Retain`; two opaque ones `Retain`
  before refinement; anything else `Reject`. The operator's `transition(old, new)` can only make the class stricter
  (the maximum is taken), which is how an opaque host refuses any change. The stage 6 lineage key hashes the same
  `KeyedState` fields.

- **Contract** (`EffectContract`): `reads` and `writes` as `PerKind<FieldSet>`; `cardinality` (`filter`, `map`,
  `expand { max }`, `sink`, and `source` for nodes without input; only opaque hosts may leave `max` undeclared); port
  relation (`copies` or `point_disjoint`); `identity` as `PerKind<Identity>` (`preserving`, `injective { image }`,
  `quotient`, `opaque`); `time { reads_admission_time }`; `state` (`none`, `keyed { key, reducer, time, admission }`, or
  `opaque` for host-owned state); `order` (`independent` or `dependent`); `loss` (`lossless` or a non-empty list of
  `(class, description)`, using the kernel `LossClass`); `signals`; and `boundary`. `EffectContract::violations` checks
  the registry invariants, which a test runs over every operator: `filter` implies `writes = ∅`, exactly the sources
  have no input and exactly the sinks no output, expansion maxima are at least one, and a lossy contract declares its
  losses. Cardinality counts records per port for `copies` and in total for `point_disjoint`.
- **Ports** (`Ports`): each input port declares the `PortType` it accepts; each output port declares a `PortMapping`,
  a `KindMap` (input kind to output kinds) and a `TimestampMap` (`preserve`, `select(form)`, or `set(form)`). The
  default output types map the union of the inputs. `split` narrows each port to the kinds for which its predicate can
  take that port's value, by `Predicate::truth_for_kind` (a three-valued abstraction where only `const` and `kind_in`
  decide, never `flag` or `is_empty`, so types never depend on live values); sources and opaque hosts declare their
  output types directly.
- **Parameters** use plan-owned types; those a kernel will consume live in `saluki-metric-kernels::params`
  (`MapperProfile`, `NamespacePrefixParams`, `TagFilterRule`, `TagValueRule` with `validate_tag_value_rules`,
  `HistogramParams`), with `encode_*`/`decode_*` helpers in the operator modules for lowering. Values that come from
  configuration are `Bindable`; structural values (port counts, the source binding, opaque-host port types) and host
  facts (host tags and their deadline) are not. Required parameters carry no plan-side defaults: lowering always writes
  the native value. `ParamType` gained `Record(&[ParamSpec])` and `PortType`, `decode_params` reports every problem, and
  `ParamType::admits` decides whether a binding's type can supply a parameter.

| Operator | Boundary | Parameters | Contract notes |
|---|---|---|---|
| `source` | `io_host` | `binding` (`dogstatsd`, `otlp`, `checks`, `liveness`, `trace_sampler`); `timestamps` for `dogstatsd` only (`mixed` with the no-aggregation pipeline, else `untimestamped`) | Output `metrics`. Kinds per binding: DogStatsD `{Counter, Gauge, Set, Histogram, Distribution}`; OTLP `{Counter, Rate, Gauge, Distribution}` mixed; checks `{Counter, Rate, Gauge, Histogram}` mixed; liveness `{Gauge}` timestamped; trace sampler `{Counter, Gauge}` untimestamped. Writes every field. |
| `dogstatsd_mapper` | sync | `profiles` (list of `{name, prefix, mappings: [{match, match_type, name, tags: [{name, value}]}]}`), `cache_size`, `string_interner_bytes` | `reads {name}`, `writes {name, tags}`, `quotient`, lossless. `check_params` validates known profiles as the kernel compiles them (`mapper::validate_profiles`: empty names, prefixes, and matches, the wildcard grammar, regex compilation), with native messages. Kernel: `MetricMapper`, mapping in place, its caches named after the component it is built for (`<component>/dsd_mapper/primary` and `.../result_cache`). Bounds: the interner and the result cache, as native. |
| `namespace_prefix` | sync | `namespace`, `exclusions` | Map. `reads {name}`, `writes {name}`, `quotient`. Kernel: `NamespacePrefix`, in place. |
| `timestamp_split` | sync | `window` (whole seconds, at least 1) | Ports `default` (`select(untimestamped)`) and `passthrough` (`select(timestamped)`, `Counter` to `Rate`), point-disjoint, `expand { max: 2 }`; writes `kind` for counters only. Kernel: `TimestampSplit`, sending a batch without timestamped points to `default` whole (`route_all`), counting non-metric events as `invalid`, and reporting the native passthrough metrics and one passthrough flush per batch under the component it is built for. |
| `tag_projection` | sync | `rules` (`{metric_name, action, tags}`), `value_rules` (`{metric_prefix, tag_name, values, on_miss, replacement}`, replacement exactly when `on_miss` is `replace`, validated as natively), `cache_capacity` | Reads `kind` always; reads `name`, tags, origin tags and writes `tags`, `origin_tags` for `{Counter, Distribution}` only; `quotient` for those kinds; `lossy(policy)`. Firm bounds: 64 bytes per cache entry, as native. Kernel: `TagProjection`, with the kind guard inside it, the native `tag_filterlist_*` telemetry, and a context memo named `tag_filterlist/context_cache` for a reserved node (the native, unscoped identifier) and `<component>/tag_filterlist/context_cache` for a policy node. |
| `aggregate` | `opaque_host` until stage 6 | `window`, `flush_interval`, `context_limit`, `counter_expiry` (`0s` never expires), `flush_open_windows`, `histogram_aggregates`, `histogram_percentiles` (whole percents, each statistic once), `copy_to_distribution`, `copy_to_distribution_prefix` | Output `{Rate, Gauge, Set, Distribution}`, timestamped. Writes `points` for every kind, plus `kind` for counters and `name`, `kind`, `metadata` for histograms; `opaque` identity for histograms; keyed state with the window as its time contract (the kernel's admission-time bucketing, from stage 6), so a window change is a `Cut`; lossy (policy: untimestamped gauges reduce to the latest and kind or rate-interval conflicts replace values; capacity: context limit; shutdown: open buckets). Declared precisely although an opaque host (amendment 32). Only its expansion maximum applies. |
| `host_enrichment` | sync | none | Passthrough. Kernel: `HostEnrichment`, which leaves batches as they are. |
| `host_tags` | sync | `tags`, `deadline` (an instant, as a duration since the Unix epoch) | Reads `t_a`; writes `tags`; `quotient`. Kernel: `HostTags`, adding the tags (`merge_shared`, as native) to batches with `t_a < deadline`; an unstamped batch passes unchanged. |
| `opaque_host` | `opaque_host` | `component`, `input` (accepted type), `outputs` (port name to type) | Reads and writes everything, copies, undeclared expansion, opaque state; `transition` rejects any change. |
| `split`, `tee`, `merge`, `emit`, `drop` | sync | `predicate`; `ports` (1 to 10,000); `inputs` (1 to 10,000); `output`; none | `split` is a point-disjoint map reading its predicate's fields; `tee` copies; `drop` is `lossy(policy)`. |

### Validation

`Validator::new(&registry, &dyn Catalog).validate(&plan)` returns a `ValidationReport` with every finding (`severity`,
`rule`, `location`, `message`) and the port type computed for every output port. Rules have stable names:

- **Structure**: `ir_version`, `unique_node_ids`, `known_operator`, `valid_parameters`, `edge_references`,
  `one_edge_per_port`, `paths_end_in_emit_or_drop` (an unconnected output port), `acyclic` (strongly connected
  components over every node, so unreachable cycles are found), `reachable`, `unique_sources` (a host source enters the
  plan once; two source nodes with one binding would each receive its records), and `provenance_namespace` (every
  `policy/` node has artifact provenance: only artifacts add them, and artifact limits apply to their provenance).
- **Types**: propagated in topological order; `port_types` (an operator cannot receive its input) and `output_accepts`
  are errors; `predicate_can_match` (a `split` that can never match) and `port_can_carry` (a port that can never carry a
  record) are warnings.
- **References**: `unique_output_ids`, `declared_output`, `catalog_output` (fingerprint, accepted type, and recipients
  must match the catalog), `unused_output` (warning), `known_binding`, `binding_type` (`ParamType::admits`, with
  predicate operands typed as `bool` and list of string), `consistent_binding` (every use of a binding resolves to the
  same value), `live_binding_on_host` (warning: a host-owned node applies live changes only at restart),
  `scope_inputs`, and `scope_closed` (see Scopes above). The `Catalog` trait declares outputs (`accepts`,
  `fingerprint`, `recipients`), bindings (`ValueType`, `Live | Startup` liveness, and optionally the current value), and
  scope inputs with port types. `MemoryCatalog` implements it and is also the catalog's serde document form,
  `{outputs: {id: {accepts, fingerprint, recipients}}, bindings: {name: {type, liveness, value}}, scopes: {scope:
  {input: port_type}}}`, which ADP exports and tools read back. `dyn Catalog` is a `BindingResolver` over the values it
  carries, so the interpreter previews unresolved (exported) plans against an exported catalog.
- **Duplicates**: outputs are grouped by shared recipients (union-find). For each group, distinct source-to-`emit`
  paths are enumerated, pruned to nodes that can reach the group, up to `work_per_input` (4,096) paths per group, beyond
  which `duplicate_analysis_limit` is reported. Pairs from the same source are partitioned by their first differing
  output port. Each distinct canonical predicate is numbered once, and a path's protected crossings are a bitset of
  (predicate, port), so comparing two paths costs a few word operations however long the predicates are. Paths with
  equal crossings are compared once; when every path on one side shares a crossing whose opposite every path on the
  other side shares, the pair of sides is accepted at once; otherwise pairs are compared one by one, charged against a
  budget of `work_per_input²` comparisons per plan, beyond which `duplicate_analysis_limit` is reported too. A
  divergence at copies is `duplicate_contribution` unless the complementary-`split` exception holds:
  equal canonical predicates (resolved values included), opposite ports, and no write to the predicate's reads from the
  divergence, the divergence node's own writes included (which only matters for opaque hosts), up to each `split`.
  Writes are taken over the kinds that can reach each node, so aggregation voids an exception on `kind` only when
  counters or histograms reach it. One finding per group, divergence node, and port pair, naming both paths.
- **Bounds** (artifact-provenance nodes only, grouped by artifact revision): `artifact_nodes`, `predicate_leaves`,
  `tee_fan_out` (any copying node), and `work_per_input`, computed in reverse topological order as visits to artifact
  nodes, summing over copies, taking the maximum over point-disjoint ports, and multiplying by declared expansion; it is
  reported at artifact entry nodes. Artifact and blob sizes apply to artifact documents, not to composed plans, so they
  are checked where artifacts are received (stage 5). `ValidationReport::bounds()` exposes every measure as a
  `BoundMeasure { limit, maximum, scope, value }`: first the whole plan's (scope `Plan`, counting every node and, for
  work per input, entering at the sources; never enforced), then each artifact revision's (enforced, and reported as a
  finding when exceeded). `explain` shows both.
- **Authority**: `loss_authority` (every lossy node, of any loss class, has provenance), `trusted_host` (sources and
  opaque hosts never have artifact provenance), and `provenance_consistency` (every artifact revision a node names is
  listed in the plan's `provenance.artifacts`). Whether a listed revision was approved is the catalog's to say once
  artifacts install (stage 5 adds an approved-revisions query and checks the list against it). Blob integrity is
  checked when an artifact is read; plans have no blobs.

### Reference interpreter and equivalence

`interpret::Interpreter::new(&registry, &dyn Catalog).with_opaque_host(node, factory).run(&Trace) -> Result<RunOutput,
InterpretError>` executes a plan literally: one trace event at a time, in trace order, visiting every node, with no
rewrites and no fusion, inside a current-thread Tokio runtime it creates and enters per run (spawned cache tasks are
never driven; their effects are unobservable). Each node runs one of four ways:

- **Routing operators** (`split`, `tee`, `merge`, `emit`, `drop`) are executed by the interpreter itself. `split`
  compiles its predicate with the catalog as its resolver, so operands come from literals and the plan's resolved
  values, then from the values an exported catalog carries; a binding with no value rejects the installation. A `tee`
  clones to every port but the last, which gets the original; a `drop` counts `policy` loss.
- **Kernels**: an operator whose `kernel()` returns a `KernelSpec` runs it with an unlimited `Budget`, a `Turn` per call,
  and `PortOutputs`; after `consume`, `complete(input)` moves the leftover to the remainder port, and each drained buffer
  becomes one downstream batch with the input's header. `Yielded` is resumed at once (at most 100,000 times in a row,
  then `KernelStalled`); `WaitUntil` is only a scheduling hint, since ticks come from the trace. Stateless kernels are
  rebuilt for every generation, which is unobservable. Every synchronous native operator other than the routing
  operators has a kernel from stage 2; the stateful paths are tested with test-only operators registered through
  `OperatorRegistry::with_operators`, and a synchronous test operator without a kernel is an internal error. Kernels are built with `KernelBuildContext::detached(node)`; time-driven turns
  carry the state owner's watermark. A kernel whose parameters are bound and unresolved fails to build, which rejects
  the installation.
- **Opaque hosts** (boundary `opaque_host`, including `aggregate` before stage 6) run through an `OpaqueHostAdapter`
  (`admit(HostBatch { admitted_at, arrived_at, events }, &mut HostOutput)`, `tick(HostTick { now, watermark }, ..)`,
  `finish(HostFinish { now, policy }, ..)`) built per node by a registered factory from its `ConfiguredOperator`. A
  host without a factory is an error; a factory that fails rejects the installation. Adapters report what they lose
  with `HostOutput::record_loss(class, events, points)` (aggregation's context-limit and shutdown loss), which the
  interpreter adds to the node's `RunOutput::loss`. `aggregate` nodes without a registered factory run through
  `interpret::adapters::AggregateAdapter`, over the kernels crate's `AggregateCore`, the same core the native transform
  runs: it admits each batch at `arrived_at` floored to seconds, because native aggregation buckets by the arrival
  time; flushes at each tick's `now`, doing nothing while empty; ignores watermarks until stage 6; and at `finish`
  runs the final flush and reports what is still held as shutdown loss (events are contexts, points their points).
  Context-limit drops are capacity loss. Registering a factory for the node replaces it.
- **Sources** are where `admit` events enter.

Time and admission:

- `admit(source, t_a, batch)` events are in arrival order. A source's `t_a` never decreases (an earlier one is a clock
  reset: `Unsupported`, stage 6), but it may be earlier than the last `advance`, which is how a trace expresses a
  batch that waited in a queue: it **arrives** at `max(last tick, t_a)`. `advance` clocks follow a monotonic timer and
  never decrease. Batches hold at most `batch_size` (1,024) events; an empty batch is a heartbeat. Every record
  admitted at a source, and every record an adapter emits on a host output, must be of the port type validation
  computed for that port (kind and timestamp form); anything else is an `IllTyped` error, since no host produces it
  and validation's typing (port types, `output_accepts`, `split` narrowing) assumes it never arrives.
- Admission points are source outputs and opaque-host outputs, keyed by `(node, port)` and kept across installations.
  An opaque host's output is re-admitted by its own latched admission clock: at the arrival time for output of `admit`,
  at the tick's clock for output of `tick` or `finish`, always under the active generation. At every tick, every output
  of the host either re-admits or heartbeats at the tick's clock.
- Every state owner (contract state other than `none`) has a watermark: the earliest latest `t_a` of the admission
  points from which a path reaches it without crossing another opaque host, or `None` while one of them has admitted
  nothing. Hosts receive it at every tick; kernels' time-driven output (ticks and finishes) is stamped with it as
  `t_a`. `RunOutput::watermark(node)` reports the final value.
- `advance` ticks state owners in topological order, delivering each one's output before the next ticks; `close`
  finishes every kernel and host under `DrainPolicy::Shutdown` in topological order, and any later event is an error.

Installations commit at once, since no batch is ever in flight. An invalid plan (validated against the interpreter's
catalog) is recorded as `InstallOutcome::Rejected` and the active plan stays. State owners correspond by node ID: an
unchanged one (`Retain` and equal parameters) keeps its kernel or adapter instance, so its later time-driven output
flows through the new plan; `Reject` rejects the installation; `Cut`, `Retain` with changed parameters, and adding or
removing a stateful kernel are `Unsupported` (stage 6), and adding or removing a host component is `Unsupported`
(stage 7). The old plan's other kernels finish under `DrainPolicy::Retire`, their output flowing through the old
generation. `capacity` is `Unsupported` until the delivery-policy slot (stage 3), `cancel` until a `cut` can leave an
installation pending (stage 6), and a plan whose operators read signals until stage 10; `signal` events are recorded
(`RunOutput::signal`) and change nothing.

`RunOutput` lists every declared output's `EmittedRecord { generation, install, admitted_at, metric }` (`install` is
the trace position of the generation's `install` event), the install outcomes, per-node loss by class (telemetry, not
equivalence), watermarks, and signals.

`equivalence` flattens each delivered record into `Point { context, kind, metadata, timestamp, value }`: the context by
its parts (name, host, deduplicated sorted tags and origin tags), the kind with a rate's interval, scalar values compared
bitwise in the IEEE total order (`-0.0` differs from `0.0`; NaNs equal only by bits), set members, histogram samples with
weights as a sorted multiset, and sketches by count, stored min/max/sum/avg, and bins. `OutputPoints` holds a multiset
per output (an output that received nothing equals an absent one); `compare` returns a `Mismatch` that renders as a diff
of missing and unexpected points per output. `compare_reordered(.., Tolerance::from_limits())` is the stage 9 oracle:
per (output, context, kind, metadata, timestamp) group, exact equality passes; groups with different numbers of points
differ, whatever their kind (reordering never changes a bucket's point count); otherwise counter and rate values are
sorted and compared pairwise within the relative tolerance (1e-9; a NaN or an infinity matches only identical bits), set
points by the union of their members, and gauges, histograms, and distributions are `Undecided` (the gauge candidate
check needs the admitted inputs, which stage 9 adds).
Any definite difference makes the verdict `NotEquivalent`, whatever is undecided.

### Explain

`explain::Explain::of(&plan, &registry, &report)` builds a versioned document (`explain_version: 1`), `to_json()`
(pretty, deterministic: members in declaration order, maps and parameter-value objects sorted, lists in plan order) and
`to_text()` (deterministic). Within a version, changes are additive and consumers ignore unknown members (`from_json`
does); the version bumps only on removals or renames. Stage 1 sections: `plan` (IR version, hash with and without
resolved values, provenance, validity); `graph` with `nodes` (ID, operator, boundary, state kind, declared loss classes,
provenance, parameters as written, every configuration binding with its parameter path and resolved value, and ports
with their accepted or carried types and the port each edge reaches), `edges` (each with its port type), `outputs`
(with the `emit` nodes that deliver to each), and `scopes`; `bounds` (each measure against its limit, with whether it
is enforced and within it); and `findings`. Golden files for the default-program fixture live in
`lib/saluki-metric-plan/fixtures/explain/` and are regenerated with `SALUKI_METRIC_PLAN_UPDATE_EXPLAIN=1`. They are
change detectors, generated and then reviewed; their semantic content is checked separately against hand-derived
values: every node's operator, boundary, state, and loss classes, port types at representative edges, bindings, and
bounds. Later sections
land with their stages: frame, slots, fences, exits, segments, and output bindings with recipients and delivery policies
(3); rewrites with justifications and telemetry effects (4); plan diff and acknowledgments (5); state owners and
transitions (6).

### Limits

`limits.rs` is a table of `LimitSpec { name, default, source, description }`, built from typed constants in
`limits::values` that enforcing code reads (`values::BATCH_SIZE: usize`, equal to `EventsBuffer`'s capacity, and so
on): artifact size 256 KiB, artifact nodes
256, predicate leaves 1,024 per artifact, tee fan-out 32, worst-case work per input 4,096 node visits, prepare timeout
10 s, drain deadline 30 s, overlap budget 4 live generations (active, last-known-good, and two draining), heartbeat period
`min(5 s, smallest downstream flush interval)`, clock-reset threshold 600 s, blob size 64 KiB, R4 memo 4,096 entries per
decision, batch size 1,024, work per turn 8 batches, floating-point tolerance 1e-9 relative. Behavior-changing limits get
decision-log entries. `source` is `Fixed` or `Parameter { operator, parameter }`: only the context limit comes from a
parameter (`aggregate.context_limit`, which the default program binds to native configuration in ADP's `bindings.rs`),
so the plan crate names no configuration path. The bucket-width restriction is described but not enforced until stage
6 (see the stage 6 outline).

### Extracted kernels

Native behavior moves into `saluki-metric-kernels` once, and the legacy components become thin hosts over it. Every
kernel whose logic was restructured keeps a frozen `#[cfg(test)]` copy of its pre-extraction code next to its tests
(imports and visibility adjusted only), and `property_test_*` tests compare the kernel with that copy over generated
inputs, so that rewiring the legacy component cannot make the comparison tautological.

- **Mapper** (`mapper::MetricMapper`): `new(identity, &[MapperProfile], cache_size, interner_bytes)` compiles the
  profiles (`validate_profiles` checks them alone, without a Tokio runtime), and `try_map(&Context)`, `map_buffer`, and
  its `Kernel` implementation map contexts. The resolver and result cache are named `<identity>/dsd_mapper/primary` and
  `<identity>/dsd_mapper/result_cache`, so the legacy `dsd_enrich` keeps its native telemetry IDs. The legacy
  `DogStatsDMapperConfiguration` keeps its configuration types and converts them: an empty match type is wildcard, and
  an unknown one is reported only after any problem the kernel would have found earlier, which keeps the native order of
  errors.
- **Aggregation** (`aggregate::AggregateCore`): `new(AggregateParams, AggregateTelemetry)`; `admit(now, metrics) ->
  AdmitOutcome` (accepted, dropped, dropped points, and whether the limit was first reached); `flush(now, last) ->
  Option<Flush>`, which is `None` while nothing is held, so an empty tick leaves the last flush time stale as natively;
  `Flush::emit_next(sink)` visits one context per call, `finish()` removes what the flush emptied or expired and reports
  a recovery from the context limit, and `abandon()` reproduces the native early return after a failed delivery;
  `flush_into(now, last, sink)` runs a whole flush; `snapshot_contexts()` and `held_points()`. The state is a
  `hashbrown::HashTable` with the native hasher, so a flush resumes by bucket position with no allocation and never
  holds more than one context's expansion: the legacy transform delivers each context's metrics before visiting the
  next, and stage 6's bounded drain yields between the same steps. A context's removal is queued only once its metrics
  were delivered, as natively. `HistogramConfiguration::from_params(&HistogramParams)` builds the statistics, and
  `HistogramParams::from_names` parses configured statistic names with the native rounding. Memory bounds come from
  `state_usage` and `snapshot_usage`. The legacy `Aggregate` keeps its timer, liveness, snapshot channel, and logging.
- **Namespace prefix** (`prefix::NamespacePrefix`): `new(&NamespacePrefixParams)` appends the `.`;
  `prefixed_name(&MetaString) -> Option<MetaString>` is `None` when disabled, the name itself when an exclusion (a raw
  `starts_with` of the name before prefixing) matches, and otherwise a new string, as natively; `rename` always
  replaces the context, as natively even for excluded names; `apply`, `apply_buffer`, and the `Kernel` map in place.
  The listener filterlist is not part of the kernel: the plan splits on the intake name after prefixing, and the
  legacy `dsd_prefix_filter` calls `prefixed_name`, matches the new name with `SortedSetMatcher`, and renames only
  metrics it keeps, so its allocations are the native ones. Its telemetry (`metric_filterlist_*`,
  `dogstatsd_listener_filtered_points_total`) stays in the component: in a plan it belongs to the `split`.
- **Timestamp split** (`timestamp_split`): `has_timestamped_metrics`, `split_timestamped`, `counter_values_to_rate`
  (moved), `to_passthrough`, and `split_metric(metric, window) -> (passthrough, default)`, which converts passthrough
  counters, so the legacy `PassthroughBatcher` no longer does; `TimestampSplit` is the two-port kernel (`DEFAULT_PORT` 0,
  `PASSTHROUGH_PORT` 1) with the `route_all` fast path. `TimestampSplitTelemetry` (moved, names unchanged) serves both
  the legacy batcher (`new`, every metric) and the kernel (`for_kernel`, only `aggregate_passthrough_metrics_total` and
  `aggregate_passthrough_flushes_total`: the buffer-full drop counter and the batch duration histogram describe the
  batcher, so a plan publishes no permanently zero series for them), which counts passthrough metrics and one flush per
  batch with any (amendment 12).
- **Tag projection** (`tag_projection`): `ValueRules::new(&[TagValueRule])` validates and compiles value rules once;
  `TagProjectionRules::new(&[TagFilterRule], ValueRules)` compiles tag rules with them (empty names ignored, same
  action unions, `exclude` wins); `filter_metric_tags` (moved); `is_projected` (the counter and distribution guard,
  moved into the kernel from the run loop); `ContextMemo::new(identifier, capacity)` (the native memo: capacity `0`
  holds one, 30-second idle expiry, misses not kept; needs a Tokio runtime); `TagProjection::new(rules, memo,
  telemetry)` records the rule count, `replace_rules` swaps rules and memo and counts an update, `apply` returns the
  outcome or `None` for other kinds. `TagProjectionTelemetry` moved with it, names unchanged; its `for_kernel`
  constructor, which the plan's factory uses, leaves out `tag_filterlist_updates_total`, which the installer records
  (see the telemetry profiles). The legacy
  `tag_filterlist` converts its configuration (`tag_filter_rules`, `tag_value_rules`, crate-visible for lowering) and
  keeps only its live-update loop.
- **One tag value rule validator.** `params::validate_tag_value_rule_keys` (over `(prefix, tag)` pairs) is the only
  implementation; `validate_tag_value_rules` and the plan's `tag_projection` check delegate to it, and
  `InvalidTagValueRules` is a structured enum (`EmptyMetricPrefix`, `EmptyTagName`, `TagNameContainsColon`,
  `DuplicateRule`, `OverlappingPrefixes`) so that `agent-data-plane-config-system` words the native messages from it.
  `agent-data-plane-config` no longer has a validator.
- **Host enrichment and host tags** (`host_enrichment`): `HostEnrichment` leaves metrics unchanged; `HostTags::new(tags,
  deadline)` with `applies_to(t_a)` (strictly before the deadline; unstamped never) and `apply`/`apply_buffer`, and
  `apply_tags(&SharedTagSet, &mut Metric)`, the native `merge_shared`, which the legacy `host_tags` calls while keeping
  its own `Instant` deadline and latch (amendment 9 applies only to the plan-driven topology).
- **Selection** (`selection`): `RawNameSet` (exact raw names: the multi-region failover and autoscaling failover
  allowlists), `IntakeNameAllowlist` (exact intake names plus compacted literal prefixes, normalizing each name at most
  once; `name_count` and `prefix_count` are what `metrics_endpoint_allowlist_size` reports), `is_scalar_series`, and
  `histogram_statistic_entries(values, &HistogramParams)`, the post-aggregation filter's derived entries, which lowering
  binds (`dogstatsd.post_aggregate_filter.values`) over `is_histogram_statistic_entry(entry, suffixes)`. The legacy
  filter keeps its native constructor contract: aggregate names are suffixes as written (it does not reject an unknown
  one, which aggregation's configuration rejects first in ADP), percentiles parse with the native messages, and it
  matches `is_histogram_statistic_entry` over those suffixes with `SortedSetMatcher`.
  `metric_filter`, the autoscaling gateway, and the post-aggregation filter call them; in plans they are predicates
  (stage 4). `matcher::compact_prefixes` is the one prefix compaction (`SortedSetMatcher` and endpoint routing groups
  use it), and `SortedSetMatcher` now keeps exact entries once and reports `len()`.
- **Memory bounds of the legacy components** follow their new structures; every change is a few bytes of the
  `minimum` amount, and firm bounds are unchanged. `Aggregate` no longer holds a telemetry clone beside its state or
  `flush_open_windows` (both live in `AggregateCore`), so its "component struct" shrinks by one `AggregateTelemetry`
  (16 metric handles, about 256 bytes). `TagFilterlist` holds a `TagProjection`, which embeds the
  `TagSetMutViewState` that used to be a run-loop local and an `Option` around the memo, so its "component struct"
  grows by those. `metric_filter`'s former hash set bucket term is "matcher entries" (`len + size_of::<MetaString>()`
  per entry), and `size_of::<Filter>()` changed with its variants. The autoscaling gateway's three former hash set
  terms are one "allowlist entries" term.
- **Errors keep their native wording.** `TagFilterlistConfiguration::new` reports an invalid value rule with
  `agent_data_plane_config_system::describe_invalid_tag_value_allowlist`, the message configuration loading gives, in
  terms of the configuration keys (unreachable from ADP, where loading rejects the rules first).
- **Frozen references and strategies.** `prefix`, `timestamp_split`, `tag_projection`, `host_enrichment`, and
  `selection` each have a `reference.rs` (the native code, with the configuration types it read reduced to their
  fields) and `property_test_*` tests against it; the inputs come from `saluki_metric_kernels::testing` (`test-util`),
  the metric strategies stage 1 wrote for the plan crate, moved so both crates draw from one set. Comparisons are by
  complete descriptions (name, host, tags and origin tags in order, values, metadata), as multisets where native
  filters reorder.
- **Compatibility fixtures.** Hand-written expected results pin every aggregation row of the specification's
  compatibility matrix and the recorded native quirks (amendment 11): as interpreter traces through the default
  aggregation adapter (`interpret/adapters/compatibility_tests.rs`), and directly against the core for what a trace
  cannot express, such as a wall clock moving back (`aggregate/compatibility_tests.rs` in the kernels crate). With all
  kernels, `interpret/default_program_tests.rs` runs the whole default-program fixture over a DogStatsD and OTLP trace
  against hand-derived outputs for all six outputs and the policy loss of each drop.

## Installation

### Lowering inputs and generations

Lowering is pure: `(startup config, current config, host facts, approved policy) -> logical plan`. Structure and
`Startup` bindings resolve from the boot snapshot, `Live` bindings from the current snapshot. A new generation is
created only when the composed, resolved plan differs from the active one.

### Plan diff and correspondence

`diff(active, candidate)` lists nodes as unchanged, changed, added, or removed; state owners with their transition
class; and outputs as reused. **Correspondence**: an artifact node whose local ID equals a reserved ID in its base
export's scope keeps that reserved ID. Its provenance switches to the artifact only if its operator or parameters
differ, so installing an unmodified export diffs as no change. New local IDs enter the `policy` namespace. The validator
rejects artifact local IDs equal to reserved IDs outside the scope.

Kernel reuse: stateless kernels are keyed by `(node, params_hash)`. State owners are keyed by `(node, lineage)`, where
the lineage key hashes only state-contract fields (reducer key, reducer, time contract, admission policy); a `Retain`
transition with other parameter changes is applied to the existing instance at commit.

### Prepare and commit

1. **Prepare.** Validate, compose, place into the frame, compute the diff, build changed kernels off the data path in the
   installer task while entering the target slot's resource-group token, and send `Prepare { generation, program,
   kernel_specs, new_kernels }` to **every** region (unchanged regions alias their existing program). Each region
   acknowledges.
2. **Barrier.** All acknowledgments must arrive within the prepare timeout. On timeout or any failure, `Abort` goes to
   every region. Artifacts report `rejected`. Default-program candidates stay `pending` and are retried with backoff,
   keeping only the latest candidate, so native eventual application of live configuration is preserved.
3. **Commit.** One store of `AdmissionSchedule::Steady(new)` (stage 6: `Cut`). It is irrevocable; the installer then
   reports `active`. Abort is legal only before the store. Later proposals are normal installs on top.
4. **Drain and retire.** Old batches finish under their header's generation. When the last reference drops, the token
   notifies the installer, which reports `retired`. Generations alive past the drain deadline are reported in status and
   telemetry.

**Overlap budget.** The installer counts live generations through their tokens. When a new candidate would exceed the
budget, it keeps only the latest candidate as `pending` until a generation retires. Data of stateless generations is
never dropped to make room.

### Supervision and fallback

- Default-program generations have a restart budget of 0: a panic fails the region component and ends the process,
  exactly as a native component failure does.
- Artifact generations: a panic in a node with artifact provenance restarts the region (kernels rebuilt, state lost).
  The generation fails when it exceeds 3 panics in 60 seconds, or 5 in total, or any panic in a node that owns state.
  Failure triggers rollback only if the failing generation is active or prepared; for a draining generation, its
  remaining batches are dropped and counted.
- Rollback chain: failed artifact → last-known-good; failed last-known-good artifact → default program. Last-known-good
  stays prepared in every region (the installer holds its token), so rollback is one schedule store. A failure does not
  change the revision high-water mark.

### Installer state

Installer state (active plan and hash, last-known-good, revision high-water mark, live generations, schedule handle)
lives in an `Arc` created in `run.rs`, outside the restartable worker, so a worker restart never forgets the high-water
mark. Candidate errors are reported, never fatal to the worker.

### Artifact handling

- Snapshots are complete; an empty snapshot revokes (the default program runs). More than one config in the policy
  product rejects all of them. A transport failure keeps the current feed.
- The payload envelope is `{revision, artifact_hash, approved_against: {active_plan_hash}, artifact}`. ADP recomputes
  `artifact_hash` and rejects a mismatch. A changed active plan since approval is reported as a stale review.
- Hash equal to the active or preparing one: acknowledge current state. Revision at or below the high-water mark: stale,
  rejected. A base `program_hash` mismatch: running but **stale**. An output fingerprint mismatch: treated as a removed
  output (invalid; the current generation keeps running; at boot the default program runs).
- Acknowledgments carry state (`rejected`, `pending`, `prepared`, `active`, `retired`, `failed`), artifact hash,
  revision, generation, cut time `T` when prepared, and error text. Mapping to Remote Config's three apply states is a
  stage 8 deliverable.
- Stateful operators in artifacts are rejected through stage 5 with an error naming stage 6.

## ADP integration

### Host facts, bindings, telemetry profiles

Stage 2 compiles `programmable_metrics` only for its tests (`#[cfg(test)]` in `main.rs`, the plan crate a
dev-dependency): nothing runs it until stage 3 builds the topology from the lowered program, so it would be dead code
in the binary. Stage 3 removes the gate and moves the dependency.

- `HostFacts { process_start: SystemTime, hostname, standalone, host_tags: Option<SharedTagSet>,
  cluster_agent_endpoint: Option<String> }` is captured once before lowering by `HostFacts::capture(process_start,
  &config, &env_provider, client_config)`, which `run.rs` calls in stage 3. The host-tags RPC runs once at boot, only
  when connected with a non-zero `expected_tags_duration`, with native failure behavior (it shares
  `components::host_tags::fetch_host_tags` with the native transform). The Cluster Agent endpoint is a host fact
  because Kubernetes service discovery reads the process environment; it is the endpoint that
  `ClusterAgentConfiguration::endpoint_and_token` resolves, without the token. `host_tags_deadline(expected)` is process
  start plus the duration, rounded up to a whole second and capped at 2^53 − 1 seconds, so a plan duration always
  represents it: a wall clock reads the process start to the nanosecond, and a nanosecond count since the epoch is
  beyond a plan's integers (amendment 9).
- `bindings.rs` is a closed table of `BindingSpec { name, key: Option<BindingKey>, ty: ParamType, liveness, resolve }`.
  Names are typed-model paths below the model's top-level section (`domains.dogstatsd.metric_filter.values` is
  `dogstatsd.metric_filter.values`, `shared.autoscaling_failover.metrics` is `autoscaling_failover.metrics`), never
  Datadog keys, so translation's precedence (`metric_filterlist` over the legacy `statsd_metric_blocklist`) is
  inherited.
  `resolve` is `Value(fn(&SalukiConfiguration))`, read from the startup snapshot for `Startup` bindings and the current
  one for `Live` bindings; `Derived(fn(live, startup))` for `dogstatsd.post_aggregate_filter.values` (the filterlist
  entries naming a histogram statistic's series: live filterlist, startup statistics); and `Keyed(fn(startup, key))` for
  `metrics_endpoint_routing.group.{key}.names` and `.prefixes`, keyed by the group's 16-hexadecimal-digit fingerprint
  (`routing_group_key`, a digest of its sorted endpoints, so it survives renumbering and list edits). Values are written
  as plan parameters (durations canonical, `None`/`0` expiry as `0s`, percentiles in whole percents, an empty mapper
  match type as `wildcard`, mapper profiles through the native `DogStatsDMapperConfiguration::profiles`), then
  normalized by decoding and encoding with the binding's type; a value outside the type (an integer beyond ±(2^53−1)) is
  kept as it is, so that validation reports it. `Live` entries are exactly the natively live inputs:
  `dogstatsd.metric_filter.{values,match_prefix}`, the derived post-aggregation values, `dogstatsd.tag_filterlist`, and
  `multi_region_failover.metric_mirroring.{enabled,allowlist}`. No binding reads a key, token, or secret (tested with
  distinctive credentials). The plan crate's hand-written fixture catalog uses the same names, types, and liveness,
  which an ADP test checks.
- `catalog.rs`: `AdpCatalog::new(&NativeOutputs, Snapshots)` implements the plan crate's `Catalog` over the outputs,
  every registry binding with its type, liveness, and current value (keyed bindings for every key of the startup
  configuration), and `post_enrich` with input `metrics` of any type; `to_document()` is the `MemoryCatalog` an export
  carries.
- `outputs.rs`: `NativeOutputs::from_configuration(&startup, &facts)` declares `dsd_client_telemetry` (with DogStatsD),
  `dd_metrics_encode`, `metrics_routing_encode_{i}` (one per `MetricsEndpointRoutingConfiguration` group),
  `mrf_metrics_encode` (when `MrfConfiguration::metrics_endpoint_override` resolves), and `af_metrics_encode` (when
  `AutoscalingFailoverConfiguration::is_branch_requested` and the Cluster Agent endpoint resolved; accepts `{Counter,
  Rate, Gauge, Set}`), each with its encoder or destination component and forwarder. Recipients come from the native
  forwarder configurations (`DatadogForwarderConfiguration::metrics_recipients(routing)` and
  `ClusterAgentForwarderConfiguration::metrics_recipients()`, added to `saluki-components` over the forwarder's own
  `build_routable_endpoints` and I/O-loop routing), so presence and recipients match native exactly. A recipient ID is
  `<forwarder>/<route>/<key index>/<configured endpoint>` (`route` is `primary`, `metrics_override`, or `additional`);
  an endpoint that is not plain printable ASCII, contains `@`, or is longer than 400 bytes is written `sha256:<16 hex>`
  (amendment 13). A fingerprint is the digest of `{"encoder": .., "forwarder": .., "recipients": [..]}`, where the
  encoder part names the encoder variant and its endpoint routing (`all_except`, `only`, the failover override, or
  `v2_series_only`); encoder tuning is not part of it.
- `telemetry_profiles.rs` maps (reserved node ID, operator) to `NativeTelemetry { component_id, component_type,
  profile }`, outside the IR; `{i}` patterns cover routing groups. Profiles list the native metrics a node keeps
  (`ListenerFilterlist`, `TimestampSplit`, `TagFilterlist`, `PostAggregateFilter`, `EndpointAllowlist`), or are `Host`
  (the component reports its own, for sources and opaque hosts), `Mapper` (caches named after the component), or
  `Silent`. Both gateway forms are listed: opaque hosts through stage 3, `split` with `_true`/`_false` terminals from
  stage 4. A reserved node keeps its native metric names while its operator matches the profile, whatever its
  provenance. Each kept metric has a `MetricSource`, which says who records it in the plan-driven topology: `Kernel`
  (the kernel's telemetry, registered by its factory for the node's identity), `PortEvents(port)` (events leaving a
  `split`'s port, which the region host counts: the listener filterlist's and post-aggregation filter's drops on `true`,
  an endpoint allowlist's on `false`), `BindingLength(binding)` (`metric_filterlist_size`, the configured list's length),
  `MatcherEntries { binding, prefixes }` (`metrics_endpoint_allowlist_size{match_type}`, the distinct names or
  compacted prefixes), and `BindingChanges(bindings)` (`metric_filterlist_updates_total`,
  `tag_filterlist_updates_total`: configuration changes that changed those live bindings). The installer records the
  binding-derived values at each installation; `KernelBuildContext` needs no replacement flag, since a rule change
  builds a new kernel that the installer counts. Tests check that every lowered node's profile names its provenance
  component, that the names equal the native components' constants, that kernels built with a profile's identity
  register exactly its `Kernel` metrics (no other metric under the node's labels), and that every other source names a
  port or bindings of the node it is kept on, in the lowered program and in the plan crate's stage 4 fixture, with
  `BindingChanges` naming only live bindings.

### Default program shape

With the no-aggregation pipeline (the default) and every optional branch wired:

```text
dsd_in.metrics -> dsd_enrich (dogstatsd_mapper) -> dsd_prefix_filter (namespace_prefix)
  -> dsd_prefix_filter_blocklist (split: intake_name matches filterlist)  .true -> drop
  .false -> dsd_no_agg_split (timestamp_split)
     .default     -> dsd_tag_filterlist (tag_projection) -> dsd_agg (aggregate) -> dsd_post_agg_filter_in.0
     .passthrough -----------------------------------------------------------> dsd_post_agg_filter_in.1
dsd_post_agg_filter_in (merge) -> dsd_post_agg_filter (split: all(kind_in{counter,rate,gauge,set},
                                   intake_name matches derived suffix filterlist))  .true -> drop
  .false -> dsd_post_agg_filter_fanout (tee)
     .0 -> emit dsd_client_telemetry         (internal output, no recipients)
     .1 -> metrics_enrich_in.0
otlp_in.metrics, checks_ipc_in.metrics, liveness_in.metrics, trace_sampler.metrics -> metrics_enrich_in.1..n
metrics_enrich_in (merge) -> metrics_enrich (host_enrichment) -> metrics_enrich_host_tags (host_tags, if wired)
  -> scope post_enrich, input "metrics":
     metrics_policy (tee)
       .0 -> emit dd_metrics_encode
       .i -> metrics_routing_filter_{i} (split: any(intake_name in names_i, intake_name prefix_in prefixes_i))
              .true -> emit metrics_routing_encode_{i}   .false -> drop
       .m -> mrf_metrics_gateway (split: all(flag(mrf.enabled)[startup], flag(mirroring.enabled)[live],
              any(is_empty(allowlist)[live], name in allowlist[live])))   .true -> emit mrf_metrics_encode  .false -> drop
       .a -> af_metrics_gateway (split: all(kind_in{counter,rate,gauge,set}, name in metrics[startup]))
              .true -> emit af_metrics_encode   .false -> drop
```

Frame for that program: slots `dsd_pre_aggregation` (mapper through tag filterlist) and `metrics_enrich` (post-aggregation
filter through the policy region); fixed components `dsd_in`, `dsd_agg` (opaque, readmit), the other sources, and the
encoders. The `metrics_enrich` slot keeps the native ID that upstream wiring uses. Before stage 4 the three gateways lower
to `opaque_host` nodes backed by today's components (`{component, input: any, outputs: {out: <the output's accepted
type>}}`), each with a direct edge from its `out` port to an `emit` named `<gateway>_out` by `derived_terminal_id`. A
branch exists only when native wiring builds it, decided from the startup configuration by the predicates `run.rs` uses
(`DataPlaneConfiguration`'s pipeline gates, `no_aggregation_pipeline`, connected mode for `liveness_in` and, with a
non-zero `expected_tags_duration`, host tags, and the output gates above). `metrics_enrich_in` and the tees are present
even with one input or port, so their IDs never depend on which other branches exist; `dsd_post_agg_filter_in` exists
only with the no-aggregation pipeline, whose passthrough is its second input. `dsd_stats_out` and `dsd_debug_log_out`
stay host edges on `dsd_in.metrics`, outside the plan.

`programmable_metrics::lowering::Lowering::new(startup: Arc<SalukiConfiguration>, facts: HostFacts) ->
Result<Option<Lowering>, GenericError>` runs once at boot (`None` when no metrics pipeline is required): it checks the
startup configuration as native construction of the metrics path does, in the native order, and resolves the
`NativeOutputs` once, since they depend only on the startup configuration and the facts (resolving them runs the
forwarders' endpoint resolution, which logs warnings for an unusable override URL or key). `Lowering::lower(&current) ->
Result<Lowered, GenericError>` produces this program on every configuration change, with `Lowered { plan, catalog,
notes }` and `Lowering::outputs()`. Structure and `Startup` bindings come from the startup configuration, `Live` bindings
from `current`, and every bound operand carries its resolved value; host facts and structural parameters are literals.
Preparation fails where native construction of the metrics path fails, with the native error and, with several
problems, the one native construction meets first: blueprint assembly checks endpoint routing, the Cluster Agent
forwarder's token, then the DogStatsD tag value rules and histogram statistics; component construction then checks
mapper profiles (transforms build before forwarders) and forwarder endpoints (between two forwarders native has no fixed
order). A test checks the order against `create_topology` for the assembly errors. A configuration that runs natively
lowers, except that a duration no plan duration represents, or host tags enrichment without host tags in the facts,
fails `lower` (amendment 38); if its plan does not validate, the default program runs degraded (amendment 14). A
`LoweringNote` records where lowering departs from the configuration as written: only repeated histogram statistics,
which lowering lists once, an approximation of native aggregation, which emits them once per listing (amendment 26).

The shape is checked in as hand-written plans, `lib/saluki-metric-plan/fixtures/default-program.json` (every optional
branch wired, the no-aggregation pipeline on, stage 4 gateways) and `default-program-no-aggregation-pipeline-off.json`,
which validate with no findings against a hand-written catalog. Inline terminals are named by `derived_terminal_id`, and
a test checks that the IR represents every row of the specification's default lowering coverage table. In ADP, a
hand-written `lowering/fixtures/every-branch.json` pins the stage 2 lowering of one every-branch configuration exactly,
with fingerprints checked against hand-written binding documents, and the same lowered program runs through the
reference interpreter (every native kernel, the default aggregation adapter, and gateway adapters applying their native
components' selections) over the plan crate's default-program trace, against hand-derived outputs and policy loss. A
configuration matrix (24 configurations: the no-aggregation pipeline on and off, every source alone and together,
including checks without DogStatsD and the trace sampler without the OTLP source (proxy mode with traces not proxied),
connected and standalone, host tags, zero, one, and two routing groups
including a shared group, an endpoint with empty lists, and a policy on the primary endpoint, both failover branches
wired or not, mapper profiles, namespace prefix, filterlists including the legacy blocklist, a metrics override intake,
the primary endpoint repeated as an additional endpoint, repeated histogram statistics, and aggregation settings) lowers
to plans that validate with zero findings, and each is compared structurally with the blueprint `run.rs`'s
`create_topology` builds for it (in standalone mode, or with standalone mode off and a fixed host provider for the
connected entries): every lowered node maps by a documented table onto a legacy component, a fan-in, or a fan-out
(emits through the output they name, opaque hosts through their `component` parameter), and the legacy edges leaving
the mapped outputs and entering the mapped components are exactly the plan's, plus the host taps on `dsd_in.metrics`.
What runs inside a chained component (the mapper in `dsd_enrich`, host tags in `metrics_enrich`) is outside the
comparison; the presence table pins whether host tags are lowered. Re-lowering tests check that a change to a `Live` input changes only the parameters bound to
it, and that a change to a `Startup` input changes nothing.

### Topology construction, installer, stub, control surface

- `topology.rs` builds the metrics path from the frame: fixed components as today (sources marked `Admit`, opaque hosts
  `Readmit`), one `RegionHost` per slot, the encoders and forwarders, and blueprint edges for every frame edge.
  Everything outside the plan keeps its current construction.
- The installer worker runs on its own task, watches `config_system.live(|c| c)` as a wake signal plus the artifact feed,
  and installs as above. It publishes `PlanStatus` and plan telemetry at INFO: `plan_active_generation`,
  `plan_active_revision`, `plan_installs_total{outcome}`, `plan_live_generations`, `plan_rewrites_applied`,
  `plan_loss_total{component_id, class}` (per point), and derived `plan_node_events_total{component_id, port}`.
- Saluki-only keys `experimental.programmable_metrics.artifact_channel.stub_url` and `.poll_interval` (default 1 s),
  per the config-system skill. Unset means no channel. The stub accepts only loopback addresses, warns at startup, and
  is documented as development-only. It polls `POST {stub_url}/v1/artifacts/poll` with rich acknowledgments and a status
  summary (`default_program_hash`, `revision_high_water`, active generation) and receives a complete snapshot.
- Privileged routes: `GET /programmable-metrics/status`, `/explain`, `/plan` (active composed plan with resolved
  values), and `/export?scope=post_enrich`. The export is a versioned bundle: `{artifact, ir_versions, catalog,
  labels}`, where `artifact` carries its base, `catalog` is the `MemoryCatalog` document (outputs, bindings with their
  types, liveness, and current values, and the scope's inputs), and `labels` maps output IDs to display labels. The
  payload envelope's `artifact_hash` is checked against `ReceivedArtifact::hash` of its `artifact` document. The
  `artifact` lists its nodes and edges in the surface syntax's canonical order (see Demo), the order lowering writes
  plans in, so printing an export and parsing the text gives back the same document and hash; stage 5 tests
  `parse(print(export)) == export` for every scope in the lowering test matrix (the syntax crate may be a
  dev-dependency there, never a runtime one).

## Demo

- **`saluki-metric-plan-syntax`** (built; depends only on `saluki-metric-plan` and `serde_json`, the type of IR
  parameter values; nothing in the runtime depends on it): `parse(&str) -> Result<Parsed { artifact, spans },
  Diagnostics>` and `print(&PolicyArtifact, &PrintOptions) -> String`. One generic form for every operator, `node <id>
  = <op>(<param>: <value>, ...) { <port> -> <target> ... }`, with no reserved words: the registry's parameter schemas
  say only where a predicate is expected, so a new operator or parameter needs no grammar change. Sugar: an operator
  whose only parameter is a predicate takes it positionally (`split(<predicate>)`); `tee` without parentheses numbers
  its ports by its edge lines; a node with one output port omits the port (`-> <target>`); targets are `<node>`,
  `<node>.<port>` (required for a `merge`), or inline `emit <output>` and `drop`, which create nodes named by
  `derived_terminal_id(parent, port)` (parent `input` for a scope input). Values are strings, numbers, durations kept
  as written (`10s`), `true`, `false`, `null`, lists, records, `config("<binding>")`, and `blob("<hash>")`; predicates
  combine the leaves of the predicate table with `and`, `or`, `not`, parentheses, `all(...)`, and `any(...)`, and a
  predicate value the leaf syntax cannot write exactly (a blob reference inside a leaf, kinds out of declared order) is
  written raw as a record. Optional `ir_version <n>` and `blobs { "<hash>" = <value> }`. Parsing and printing share
  one **canonical order** of nodes and edges, a depth-first traversal from the scope inputs in which a node follows the
  edge that completes its inputs and a node's edges follow its operator's port order; it is the order lowering writes
  plans in, so `parse(print(a)) == a` for exports, `print(parse(s))` is a fixed point, and an artifact's hash depends
  only on its graph, never on how its text is arranged. `SpanMap` maps node IDs, parameter paths (the `ParamError` path
  form, falling back to the enclosing value), and edge indices to source spans. Bounds: sources up to eight times the
  artifact size limit, 262,144 tokens (one per byte of the limit; printed text has fewer tokens than its JSON has
  bytes), nesting 128 deep, the built artifact's JSON within the artifact size limit, and time proportional to the
  source apart from sorting each node's edges. Printing writes every value on one line when breaking lines would make
  the text longer than that (deeply indented lists of short values), so the text of every artifact within the size
  limit parses back. Reference problems (undefined node, unknown port, missing port on a multi-port node, an inline
  terminal whose derived ID is taken) are all reported, a syntax error alone, after a note naming the operator when it
  is in the arguments of an operator that is not registered. `->` chains are not implemented.

  ```text
  policy post_enrich {
    base "sha256:…"
    outputs {
      dd_metrics_encode = "sha256:…",
      metrics_routing_encode_0 = "sha256:…",
    }

    input.metrics -> metrics_policy

    node metrics_policy = tee {
      -> emit dd_metrics_encode
      -> metrics_routing_filter_0
    }

    node metrics_routing_filter_0 = split(intake_name prefix_in ["demo.checkout."]) {
      true  -> emit metrics_routing_encode_0
      false -> drop
    }
  }
  ```

- **`programmable-metrics-demo`** (built; `bin/programmable-metrics-demo`, one Tokio process with axum, nothing
  depends on it): `programmable-metrics-demo run --adp-bin <path> [--run-dir <dir>] [--no-launch]`, every port a flag.
  Each component is a module with a small interface. The **stub** (`stub/`) serves `POST /v1/artifacts/poll`: a complete
  `policy` snapshot of at most one config (`demo`, version = revision), whose `payload` is the envelope as a JSON
  string with its SHA-256; the store is durable in the run directory (`store.json`, written to a temporary file,
  flushed, and renamed over the old one, so a failed write changes nothing; `history.jsonl`, a best-effort log whose
  highest revision also seeds the counter), the next revision is `max(local, ADP's high-water) + 1` (high-water from
  the latest poll and the privileged status route, for the preview in the UI as for the publish), revoke empties the
  snapshot (revoking an empty one changes nothing), and every poll is recorded, consecutive identical polls folding into
  one acknowledgment timeline entry. Only `protocol_version` decides whether a poll is answered: every other field is
  read on its own, and one ADP gets wrong is noted in the timeline rather than refusing the poll, since a refusal keeps
  ADP on its current feed; unreadable bodies are recorded too. `stub/protocol.rs` documents the messages; ADP's
  `channel_stub.rs` is the other side. The **control surface client** (`adp_api.rs`, `control.rs`) uses
  `build_ipc_client_ipc_tls_config` over the `saluki-io` HTTP client, polls `/programmable-metrics/status` every
  second (read field by field like a poll, and kept with its receive time, so the UI shows a status that stopped
  arriving as last known), and fetches the export when the default program hash the status reports changes or every
  30 s; a `404`, which is what ADP answers until stage 5, is reported as ADP not exposing the programmable-metrics
  control surface yet. The export reader ignores catalog members the plan crate's `MemoryCatalog` does not know, and
  takes output labels from `labels` or an output's `label`. **Compile** parses with the syntax crate, reads the
  document back with `ReceivedArtifact` (the hash approval binds to), and, with an export in hand, runs structural
  checks against it (`checks.rs`): scope, accepted IR versions, stale base, output fingerprints, emits to undeclared or
  unlisted outputs (the message gives the line to add), operator parameters through `OperatorRegistry::configure`,
  stateful operators (rejected through stage 5), bindings, and scope inputs, each placed in the source through
  `SpanMap`. Findings never block publishing, so a guardrail can be sent past the UI to show ADP's rejection.
  Composition, validation of the composed plan, and the plan diff preview are the stage 5 integration point marked in
  `checks.rs`. The editor shows programs printed 64 columns wide without output fingerprints; a source that lists none
  gets the export's fingerprint of each output it emits to when it is compiled. **Publish** takes the hash the user
  approved and refuses anything but the last compiled artifact's, and uses the approval up, so a repeated or concurrent
  approval publishes one revision; `approved_against` is ADP's active plan hash at compile time. **Recording intakes**
  (one listener per recipient) decode v1, v2, sketch, and v3 payloads with `stele` after `tower-http` decompression
  (on a blocking thread, so a decoder panic is a refused request), record name, tags, kind, points, API key (header,
  or the `api_key` query parameter of the forwarder's validation), path, and receive time in bounded rings, and
  summarize metric families per second as events (counted values, sketch samples; points for gauges), in bounded
  maps; `GET /api/v1/validate` → 200, anything else → 202, and every refusal (a body that cannot be decoded, an
  unsupported encoding, a body too large) is recorded with its reason. The **traffic generator** sends one datagram
  per tick with the whole metric set and counts the canaries it sends per second. The **canary check** compares those
  with the canaries the primary intake received by point timestamp over a recent 10-second range that every flush has
  had time to deliver, and waits while ADP is not up for the whole range, so it follows the present rather than
  accumulating since the demo started.
  The **telemetry scraper** reads `/metrics` every second, turns counters into rates averaged over 5 scrapes (`plan_*`,
  `component_events_*`, `component_errors_total`), and drops series 10 s after they were last seen, also while ADP
  does not answer.
- **UI** (built; one page, embedded with `include_str!`, no build step or CDN): a sticky header with the badges (ADP,
  control surface, active program, greyed as last known when ADP's status stops arriving), and on wide screens two
  panes of equal width that scroll on their own, so the editor and the intake columns stay on screen together. The
  editor (line numbers, diagnostics listed and highlighted by line, a click selects the span; Load default program,
  Compile with Ctrl+Enter, Format, Publish showing the hash and asking for approval with the revision it will get and
  warnings such as ADP never having polled, Revoke while something is published), the published revision's outcome
  from ADP's acknowledgments under the Publish button, the compile status worded as what ran ("structural checks
  passed; ADP validates the composed plan"), and the configuration bindings with their current values. Drafts are
  kept per default program hash and only for what the user types; the editor loads the default program as soon as ADP
  serves its export. The viewer: one column per intake, headed by the outputs that feed it and by any unexpected API
  key or refused request, listing metric families seen in the last 5 s, fading after 3 s, with sparklines of events
  per second over 30 s (numbered names fold into one family such as `demo.debug.*`; payloads arrive per flush, so
  sparklines average over 4 s, and rates over the seconds a family has been arriving, from 2 to 12), the
  acknowledgment timeline, per-node rates from `plan_node_events_total` (native per-component rates until the
  plan-driven topology, labelled as delivered per consumer), greyed while ADP does not answer, plan telemetry, traffic
  controls, and the canary check. Light and dark themes follow `prefers-color-scheme`; nothing scrolls sideways at
  1280 pixels. Not built yet: the plan diff and the plan graph, which need stage 5's routes.
- **Launcher** `make demo-programmable-metrics` (built): a run directory (default
  `target/programmable-metrics-demo/run-<unix seconds>`; reuse one with `--run-dir` to keep the published artifact,
  which is S7), an IPC certificate generated with `rcgen` (written readable only by its owner and renamed into place,
  and generated anew if it does not parse), and an ADP config with `api_key: demo-primary`, `hostname: adp-demo`,
  `dd_url: http://127.0.0.1:17101`, `additional_endpoints` `{17102: [demo-team-b], 17103: [demo-team-c]}`,
  `metric_prefix_allowlist` `{17102: [demo.checkout.], 17103: [demo.catalog.]}`, MRF `{enabled, api_key: demo-mrf,
  dd_url: http://127.0.0.1:17104}` (mirroring off), stub URL `http://127.0.0.1:17100` with `poll_interval: 1s` (keys
  ADP reads from stage 5), API ports 17110/17111, `dogstatsd_port: 17125` with both Unix sockets disabled, a run-local
  log file, `aggregate_flush_interval: 1s`, `flush_timeout_secs: 1`, and two keys not in the original list, both so
  that the intake columns follow what is sent within seconds: `aggregate_window_duration_seconds: 1` (with the native
  10-second window each context reaches the intakes once per 10 s, too slow for 5-second columns) and
  `dogstatsd_expiry_seconds: 0` (natively an idle counter keeps sending zeros for 300 s, so a paused or rerouted
  counter would stay listed for five minutes). Before starting ADP the launcher checks that ADP's API and DogStatsD
  ports are free, naming the one that is not (often an ADP left over from an earlier run). ADP runs in its own
  process group with inherited `DD_*` variables removed, so the file is the whole configuration; the launcher waits
  for `/ready`, and Ctrl-C, `SIGTERM`, or `SIGHUP` at any point, the ready wait included, sends `SIGTERM` to the group
  and `SIGKILL` after 10 s or on a second Ctrl-C. A `SIGKILL` or abort of the demo itself cannot stop ADP. The UI is on
  `http://127.0.0.1:17080`. `--no-launch` serves the demo for an ADP started separately, with the command line to
  start it.
- **Scenarios**: S0 baseline export; S1 drop `demo.debug.*`; S2 widen the checkout route with `or tag("team") ==
  "platform"` after its prefix match; S3 exclusive tag routing; S4 mirroring via `const(true)` and a literal allowlist;
  S5 guardrails (duplicate branch, undeclared output, stateful node) shown in the UI and once sent past the UI so ADP's
  rejection appears; S6 revoke with an empty snapshot; S7 optional restart and redelivery.
- **`make demo-programmable-metrics-check`** (not built yet: it needs stage 5) runs S0–S6 headless and asserts intake
  contents and canary conservation. It runs at every stage gate from the demo through stage 6. The demo server's API
  (`/api/compile`, `/api/publish`, `/api/revoke`, `/api/state`, `/api/intakes/{name}/recent`) is what it drives.

## Stage plan

Every stage gate runs `cargo check --workspace --tests`, focused nextest runs, `make fmt`, `make check-all`, and
`make test-all`, then a multi-lens review, fixes, and a local commit. Gates the local environment cannot measure (SMP
throughput, multiple platforms) are recorded as open, with the closest local evidence (criterion benches).

| Stage | Main deliverables | Local exit evidence |
|---|---|---|
| 1 | `saluki-core` execution types and kernel trait; plan crate model, IR + schema + versioning, canonical hash, predicates, registry with contracts for every coverage-table component, validator with duplicate analysis and catalog, interpreter (opaque seam), equivalence, explain, limits, extension-slot types. Proposed spec amendments recorded. | Hand-written fixtures: kinds, predicates, `intake_name`, canonical form (RFC vectors), validator and duplicate cases; IR round trip; the default-program shape valid. |
| 2 | Header, admission points, schedule, heartbeat capability, generation tokens; `saluki-metric-kernels` with frozen pre-extraction reference modules and `property_test_*` kernel-vs-reference before rewiring legacy; aggregation core moved unchanged with a sync driver; compatibility-matrix fixtures pinned against it; host facts, bindings, telemetry profiles, lowering; blueprint introspection. | Kernel-vs-reference proptests; matrix fixtures; lowered structure matches legacy blueprints over a config matrix; legacy topology still runs unchanged; native configs that fail validation recorded. |
| 3 | Region host and driver, frame and placement, R1, plan-driven topology, installer with lowering on every config change, two-phase install, retirement, overlap budget, supervision, delivery-policy slot (native). | In-process legacy vs plan-driven topology tests (multiset-equal per encoder input); live config change tests; fault tests (prepare under blocked exit, lost ack); health and accounting stable over repeated installs; telemetry remap tests; criterion bench. |
| 4 | Gateways lowered to routing operators; decisions with derived per-node counts; R2–R4, R5a, R6, R7. | Rewrite property tests vs the interpreter, including capacity variants; routing and gateway differential tests vs legacy components; live MRF toggle both ways; selection bench vs a hand-written baseline in `benches/`. |
| 5 | Channel, stub, export bundle, composition with correspondence, lifecycle, last-known-good, recovery, rich acknowledgments, stale base and fingerprints, provenance; stateful artifacts rejected. | Continuous-input test with non-aggregated sequence-numbered points across installs; recovery, stale, idempotency, multi-config, and fault suites. |
| Demo | Syntax crate, demo server and UI, launcher, headless check. | `make demo-programmable-metrics-check` passes; manual run with screenshots. |
| 6 | See [stage 6 outline](#stage-6-outline). | Same matrix fixtures rerun unchanged against the kernel; transition traces; R5b capacity tests; bucket assignment under a deterministic clock; demo check passes. |

## Stage 6 outline

1. Aggregation becomes a stateful kernel in the `dsd_agg` slot behind a declared isolation fence, keeping component ID
   `dsd_agg`, its telemetry, and the context-snapshot control (through `Kernel::control`). Buckets by `t_a`; resumable,
   bounded flush drain with `Yielded`, yielding between the `Flush::emit_next` steps stage 2 introduced; the region
   stops reading input while a drain is pending, as native does.
2. Watermarks per (admission point, epoch), heartbeats enabled with the derived period, progress batches on exits to
   state owners, clock resets closing buckets on a new epoch.
3. `aggregate_window_duration_seconds` restricted to allowed widths at configuration deserialization, with an
   actionable error. Validation gains the matching rules, which stage 1 only describes (`limits::BUCKET_WIDTHS`): an
   `aggregate` or `timestamp_split` window must be an allowed width, a plan uses at most one width above 60 seconds,
   and a `timestamp_split`'s window equals its aggregation's window.
4. State lineages keyed by contract; `transition()` classification; `retain` shares the instance; `cut` computes `T` as
   the first common boundary at or after `max(last t_a over affected points) + margin` and publishes
   `AdmissionSchedule::Cut`; a point whose `AdmissionPoint::status()` shows `admitted_to_new` before `T` (it stamped
   `t_a >= T`, or a clock reset moved it to `new`) raises a conflict, and the installer cancels and recomputes. A
   reset-switched point raises it like one that reached `T` by time (amendment 42).
5. Time-driven output is stamped at the state owner's output with its own admission point ID, its watermark as `t_a`,
   and the generation from the schedule.
6. Credits: one `StateBudget` per state owner node, shared by all its lineages, so both generations count against one
   budget during a cut. Hosts pass it to the kernel factory in `KernelBuildContext`.
7. Stateful artifacts: a `dogstatsd_pre_aggregation` scope from `dsd_in.metrics` with exits into the post-aggregation
   merge, closed by `scope_closed`, with `ArtifactPortRef::Exit` (`exit.<name>`) for artifact edges; state owners placed
   in the aggregation slot (per-branch widths travel as separate entries over the existing edge). One artifact replaces
   one scope, so the policy product accepts one config per scope from this stage (through stage 5, more than one
   config rejects all). New state owners elsewhere are rejected with a stage 7 capability error.
8. R5b with partial push-down, using aggregation's kind-conditional writes (counters write `kind`; histograms write
   `name`, `kind`, `metadata`).

## Proposed spec amendments

These are proposals for the specification's compatibility matrix and decision log. The code is authoritative for
native behavior. Nothing in the specification is edited without the author.

1. **Native components never restart** (topology and root supervisors allow zero restarts). Region restarts apply only
   to artifact generations; default-program failures end the process, as today.
2. **Native filters reorder within a batch** (`swap_remove_back`). Regions preserve order; tests compare multisets.
3. **Liveness follows native**: bindings carry liveness; startup-only keys stay startup-only.
4. **IDs**: the mapper's node is `dsd_enrich`; the prefix filter lowers to `dsd_prefix_filter` (map) plus
   `dsd_prefix_filter_blocklist` (split), both reporting under `component_id: dsd_prefix_filter`; merges are
   `dsd_post_agg_filter_in` and `metrics_enrich_in`; slot components are named by slot. Interconnect series for fused
   components disappear (release note); kernel metrics keep native names and `component_id` values.
5. **Physical frame**: structure is fixed at boot; candidates place into it; unused exits stay idle.
6. **Correspondence**: artifact nodes that keep a reserved ID correspond to it; provenance changes only with the node.
7. **Admission**: points are per component output; opaque hosts readmit; the generation is stamped at the point from a
   shared schedule.
8. **Progress**: starved branches carry header-only progress to state owners, not only idle sources (stage 6).
9. **Host-tags deadline** is process start plus `expected_tags_duration`, compared with `t_a` (native: transform build
   time, monotonic, at processing). Intentional change, in the plan-driven topology only. Lowering rounds it up to a
   whole second (and caps it at 2^53 − 1 seconds), so that the deadline is always a plan duration; rounding up keeps
   host tags at least as long as configured, as the native deadline, which starts after the process does.
10. **`dsd_client_telemetry_out` taps the post-aggregation filter's output**: `tee` plus `emit` to an internal output.
11. **Aggregation**: counters leave as `Rate(width)` (so `kind` is written for counters); `flush_open_windows` has no
    observable effect; new counters receive back-filled zeros from the previous flush; a first insert can emit
    duplicate timestamps; `dogstatsd_no_aggregation_pipeline` defaults to true. Fixtures pin these; quirks are tracked
    separately. Stage 2's fixtures also pin these refinements of the matrix rows: the previous flush is the last one
    that ran, since a tick while nothing is held is not a flush, so zeros reach back across idle periods; the first
    flush emits no zeros, which leaves a bucket without one when the first flush closes two; the last zero is at the
    last bucket starting before `last seen + expiry`; with no expiry, a counter still gains zeros before its last update
    and holds its context forever; a late point in a mixed metric's first insertion waits until the collapsed bucket
    closes; a backward wall-clock jump makes a later flush emit zeros for buckets already flushed; drops at the context
    limit count metrics, not points, and a limit of zero drops everything with the breach flag never clearing; the
    distribution copy keeps the unit and truncates sample weights while `.count` rounds them.
12. **Passthrough batching** is not kernel behavior. Regions emit passthrough per input batch, which removes the native
    shutdown loss of the last idle window; `aggregate_passthrough_flushes_total` counts passthrough exit buffers.
13. **Recipient identity in the default program** is native (forwarder, raw configured endpoint, key index). A
    configuration repeating the primary as an additional endpoint stays valid and keeps native duplicate delivery.
    Stage 2 writes it `<forwarder>/<route>/<key index>/<configured endpoint>`, with `route` one of `primary`,
    `metrics_override` (the Observability Pipelines Worker or Vector intake, which takes the primary's place for
    metrics and keeps its policy identity), and `additional`, and the endpoint replaced by `sha256:<16 hex>` when it is
    not plain printable ASCII, contains `@`, or is longer than 400 bytes. Recorded behavior for the compatibility
    matrix's default-program validation row: `dd_url: X` with `additional_endpoints: {X: [k]}`, or with another spelling
    of X, is two recipients of `dd_out` (natively two I/O loops, each receiving every payload), the default program
    validates, and a routing policy on `X` selects both.
14. **A default program that fails validation still runs** and is reported degraded; artifacts derived from it are
    rejected. Limits apply to artifact-provenance nodes.
    Stage 2 records, for the compatibility matrix's default-program validation row, the configurations that run
    natively but whose default program does not validate: integers beyond ±(2^53−1) (such as
    `aggregate_context_limit` or `dogstatsd_mapper_cache_size`) are written as they are and fail `valid_parameters`.
    `aggregate_flush_interval: 0s` fails `valid_parameters` too, and natively stops the process when aggregation
    starts (Tokio's interval timer panics on a zero period). Configurations the native topology fails to build for
    fail lowering with the native error instead, so they fail the boot as today. A duration no plan duration
    represents (more than 2^53−1 of the largest unit dividing it) fails lowering, a gap left open; the host-tags
    deadline is rounded to whole seconds so it is never such a duration (amendment 9).
15. **Gateways**: MRF and autoscaling branches exist only when native wiring builds them; the MRF predicate keeps the
    static `multi_region_failover.enabled` as a startup-bound conjunct; the autoscaling output accepts only
    `{Counter, Rate, Gauge, Set}`.
16. **Kernels may own native cache expiration tasks** (unobservable) and are built inside a Tokio context; a host-driven
    expiration mode is a later refinement.
17. **No configuration version exists**; the installer numbers observed configuration changes per process.
18. **Predicates** add `const`, `flag`, `is_empty`, and `matches` (with `in`/`prefix_in` sugar).
19. **Accounting** for fused kernels nests under the slot (`topology.primary.transforms.<slot>.<node>`); policy nodes
    share one group per slot.
20. **Behavior-changing limits** need decision-log entries, as the specification's limits table requires: the drain
    deadline (30 s) and the overlap budget (4 live generations) are set in stage 1's limits table.
21. **Canonical numbers**: plans contain only integers written as JSON integers within ±(2^53−1); integral values
    written as `1.0`, `1e3`, or `-0` are rejected rather than formatted.
22. **Contract vocabulary**: cardinality adds `source` for nodes without input, and `expand` may leave its maximum
    undeclared only for opaque hosts; for stateful operators only `expand`'s maximum applies (a reducer merges,
    drops at capacity, and synthesizes idle zeros); `state` adds `opaque` for host-owned state; `loss` is a list of
    `(class, description)`, because aggregation loses by policy (gauge reduction, replacement on a kind or rate-interval
    conflict), by capacity (context limit), and at shutdown (open buckets).
23. **Authority covers every loss class**: every lossy node, including capacity loss such as aggregation's, needs
    provenance; host-owned operators (sources, opaque hosts) never come from artifacts.
24. **A host source enters the plan once**: validation rejects two `source` nodes with one binding, which the
    "different sources are disjoint" verdict of the duplicate analysis relies on.
25. **The exception's intervening writers include the divergence node** (only opaque hosts write while copying), and
    writes are judged over the kinds that can reach each operator.
26. **Repeated histogram statistics are rejected**: native configuration accepts `histogram_aggregates` or
    `histogram_percentiles` with repeats (including percentiles that round to the same whole percent), which emits the
    same series twice per flush. Stage 2 decides that lowering lists each statistic once, keeping the first, and records
    a lowering note naming the repeats; the default program validates. While aggregation runs as its native component
    (through stage 5) the repeats are still emitted natively; from stage 6, when aggregation is built from the plan's
    parameters, each statistic is emitted once per flush, an intentional change (the repeated points are identical).
27. **Split port typing**: the ports of a `split` carry the input narrowed by the kinds for which the predicate can
    take each value, so an `emit` to an output that accepts only some kinds is valid behind a `kind_in` split, as the
    autoscaling gateway requires.
28. **Equivalence compares contexts by their parts**: the oracle's context is the name, host, and deduplicated sorted
    tags and origin tags, not the 64-bit identity hash. The two agree except on hash collisions, which the oracle then
    reports as a difference instead of hiding it.
29. **Trace time in the interpreter**: `admit` events are in arrival order; a batch stamped earlier than the last
    `advance` (a queued batch, as in the "backlog at the cut" trace) arrives at the later of the two. An opaque host's
    output admission clock latches, reads the arrival time when admitting and the tick's clock when ticking, and
    heartbeats at every tick, so a state owner behind an opaque host has a watermark from its own segment.
30. **Trace events before their stages**: `capacity` needs the delivery-policy slot (stage 3), and `cancel` and a
    source whose `t_a` moves back (a clock reset) need `cut` and clock resets (stage 6); the interpreter reports each
    as an explicit error rather than approximating it. State owners added, removed, reconfigured, or cut across an
    installation are likewise errors naming stage 6 (stage 7 for host components).
31. **The stage 9 sum check is pairwise per bucket**: "sums within the tolerance" compares, per output, context, kind,
    metadata, and timestamp, the sorted counter or rate values pairwise within the relative tolerance; "set unions
    equal" compares per bucket. Regrouping points (two points versus their sum, or two set points versus their union)
    is never equivalent: a bucket's point count must match exactly for every kind, and only then are gauges,
    histograms, and distributions left undecided. Non-finite values are within the tolerance only of identical bits.
32. **Aggregation declares a precise contract while it is an opaque host**: its kind-conditional reads, writes,
    identity, and keyed state, instead of the read-and-write-everything contract opaque hosts otherwise have. The
    duplicate analysis relies on its per-kind writes. Its time contract (`StateTime::Buckets`) is the kernel's,
    bucketing by `t_a`; the host buckets by arrival until stage 6, as the compatibility matrix records.
33. **Provenance is consistent**: every node in the policy namespace has artifact provenance (lossless ones included,
    so artifact limits always apply to them), and every artifact revision a node names is listed in the plan's
    provenance. Approval of the listed revisions is checked against the host from stage 5.
34. **Approval binds to the document as received**: the RFC 8785 digest of the artifact document before migration,
    not of its parsed and re-serialized form. Documents repeating a member name are rejected (RFC 8785 assumes
    I-JSON), artifacts carrying resolved values are rejected, and durations have one spelling.
35. **Scopes are closed and can end at exits**: a scope declares inputs and optional exits, its region is what lies
    between them, nothing outside feeds it except through its inputs, and scopes never overlap. One artifact replaces
    one scope; stage 6 accepts one policy config per scope.
36. **Traces are well typed**: the reference interpreter rejects a trace that admits a record outside its source's
    port type, or an adapter that emits one outside an opaque host's declared output types, rather than executing a
    trace no host can produce.
37. **Binding names are typed-model paths**, below the model's top-level section, never configuration keys:
    `dogstatsd.metric_filter.values`, `autoscaling_failover.metrics`. Values that combine settings have derived bindings
    (`dogstatsd.post_aggregate_filter.values`, live because the filterlist is), and endpoint routing groups have keyed
    ones, `metrics_endpoint_routing.group.<key>.names` and `.prefixes`, keyed by a digest of the group's endpoints so
    that a key survives renumbering. Credentials are never bindable.
38. **Lowering fails where native construction fails**, with the native error and in the native order (blueprint
    assembly before component construction), so a configuration the native topology rejects fails the boot as today
    with the error it reports today. A configuration that runs natively lowers, and validation reports what the plan
    cannot express (amendment 14), with two known exceptions where lowering fails instead: a duration no plan duration
    represents, and host tags enrichment without host tags in the host facts. Lowering approximates in one recorded
    place: it lists a repeated histogram statistic once (amendment 26), while native aggregation, which runs as an
    opaque host until stage 6, still emits it once per listing.
39. **Host facts include the Cluster Agent endpoint**, which Kubernetes service discovery reads from the process
    environment at boot, so that lowering stays a pure function of configuration and host facts.
40. **Before stage 4, gateway emits are named `<gateway>_out`**, by `derived_terminal_id` from the opaque host's
    output port `out`; from stage 4 the gateway is a `split` with `<gateway>_true` and `<gateway>_false` terminals. The
    stage 4 default program therefore changes those nodes, which correspondence reports.
41. **Output fingerprints cover where an output delivers**: the encoder variant and its endpoint routing, the
    forwarder, and the recipient identities. Encoder tuning (compression, payload limits) and API keys are not part of
    them, so neither a tuning change nor a key rotation makes an artifact's output invalid.

42. **A clock reset admits to the cut's new generation as reaching `T` does.** Transition protocol item 5 says that
    before `T`, cancellation discards the new generation because nothing was admitted to it. A point whose clock
    resets after it first saw the cut stamps later batches with `new` (item 2), possibly well before `T`, so something
    was admitted to `new`. The installer treats such a point like one that stamped `t_a >= T`: a conflict
    (`AdmissionPoint::status().admitted_to_new`), so the cut is completed or recomputed rather than cancelled. A point
    keeps the epoch it first saw a cut to `new` with for as long as the schedule's cut has that `new` generation, so
    republishing the cut, even with another `T`, never moves a reset-switched point back to `old`.

## Risks

- Output fingerprints catch routing-group renumbering after restarts, but an artifact then becomes invalid rather than
  following the endpoints. That is deliberate: the approver never chose the new endpoints.
- The per-admission-point mutex serializes the DogStatsD decoder pool's dispatches. Dispatch rate is low (full or
  100 ms buffers), but the stage 3 bench must check it.
- Per-kernel resource-group entry costs about 1.7 µs per kernel per batch on Linux, as `ChainedConfiguration` pays
  today. Measure; if needed attribute per slot with an `explain` note.
- Fused exits feed encoder channels directly, halving the queue slack before head-of-line blocking on some branches.
  Measure p99 under slow sinks at the stage 3 gate.
- `component_events_sent_total` double counts for `BufferedDispatcher` users natively; regions do not use it. Tracked
  separately.
