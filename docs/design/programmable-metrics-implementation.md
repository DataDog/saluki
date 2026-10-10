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
lib/saluki-core/benches/region_host.rs  a region host's per-buffer work vs a plain transform    [3]

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
  compile/  frame.rs (physical frame, R1 and its cost model), placement.rs, program.rs        [3]
            decision.rs: decisions, their functions, R2, R3's bound constants, the R4 memo     [4]
            rewrite/: the logical rewrites' pipeline (mod.rs), r3.rs, r5a.rs, r6.rs, r7.rs,
            filters.rs (what R5a and R6 share), and the property tests' random plans
            (harness.rs, random_plans.rs, cfg(test))                                           [4]
            R5b                                                                                [6]
  runtime/  MetricRegionDriver implementing saluki-core RegionDriver, RegionBounds, telemetry  [3]
  benches/  region_driver.rs: the default program's regions vs hand-written baselines         [3]
            selection.rs: endpoint routing selection vs hand-written selections, at 1, 3,
            and 7 policies: a one-region fan-out's decision and region, the stage 4
            default program's policy region in its own frame, and memo hits and misses        [4]
  install/  diff.rs (plan diff, kernel reuse), lifecycle.rs, core.rs (InstallerCore, no I/O)   [3]
            core/artifacts.rs (snapshots, the policy in effect, acknowledgments, status
            summary), correspondence.rs, artifact_tests.rs (the lifecycle table and property)  [5]
  channel/  snapshot.rs (products, configs, snapshots), envelope.rs, state.rs (acknowledgments
            and status summary, the stub protocol's wire form), feed.rs (the watch-based
            channel between a transport and the installer)                                     [5]
  compose.rs  receipt checks (receive_artifact) and composition (compose, Composer);
              compose/tests.rs (accept and reject tables, property tests)                      [5]
  export.rs   the export bundle (export) and an artifact's base (base); export/tests.rs        [5]
  fixtures/export/default-program-post-enrich.json  the hand-written export of the fixture     [5]
  testing/  `test-util`: metric strategies, fixtures, harnesses                               [1+]

lib/saluki-metric-plan-syntax/        surface syntax: parser, printer, spans                    [demo]
                                      for tools only: no crate that runs plans may depend on it

bin/agent-data-plane/src/programmable_metrics/
  host_facts.rs bindings.rs catalog.rs outputs.rs telemetry_profiles.rs    [2, 4]
  lowering/        Lowering::{new, lower}, and its tests: the configuration matrix, a hand-written
                   every-branch fixture, the structural comparison with the legacy blueprint, and a
                   run of the lowered program through the reference interpreter                         [2, 4]
  lowering/export_tests.rs  every matrix configuration exports, prints and parses back, and
                   composes back unchanged (the syntax crate is a dev-dependency)                         [5]
  topology.rs      PlanTopology: the metrics path of the blueprint, built from the frame, with each
                   region's input capacity (region_input_capacity); burst tests against the legacy
                   topology in topology/burst_tests.rs                                                 [3, 5]
  installer.rs     ProgrammableMetrics::boot, the supervised InstallerWorker, PlanStatus                 [3, 5]
  telemetry.rs     RegionTelemetry (RegisterTelemetry, kernel identities), InstallerTelemetry            [3]
  test_topology.rs test-only: the primary topology, legacy or plan-driven, with its I/O replaced by test
                   doubles in place (scripted sources, clocked, gated, or scripted aggregation,
                   collecting encoders, counted sends), which the differential, live-change, fault,
                   lifecycle, telemetry, burst, and measurement tests run                              [3, 5]
  channel_stub.rs  the local stub of the artifact channel (StubChannelWorker); tests against an
                   in-process fake stub server in channel_stub/tests.rs                                   [5]
  api.rs           the control surface on the privileged API (ControlSurfaceWorker); handler and
                   mutual TLS tests in api/tests.rs                                                       [5]
  installer/artifact_tests.rs  artifacts in a running topology, with a double of the core's
                   artifact paths and with the core's own; installer bookkeeping without a
                   topology                                                                               [5]
  installer/lifecycle_tests.rs  the stage 5 exit-gate suites through the core's own paths: continuous
                   input across a series of real artifacts, restarts, stale proposals, refusals, the
                   stub worker against an in-process stub, panicking artifact kernels, and 100 installs   [5]
bin/agent-data-plane/src/cli/run/legacy.rs   the legacy metrics path, a test-only differential reference [3]

bin/programmable-metrics-demo/        demo server, embedded UI, recording intakes, load, preview   [demo]
                                      (compose, validate, diff as ADP), headless check (scenario/)
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
- `saluki-metric-plan` adds `serde`, `serde_json`, `schemars` (with `derive`), `tokio` (`sync`, for the artifact
  channel's `watch` channels), `saluki-tls` (for FIPS-consistent SHA-256), and, for the region driver (stage 3),
  `saluki-error`, `saluki-metrics`, and `tracing`. It does not depend on `saluki-components` or
  `agent-data-plane-config`. Operators never read configuration; ADP converts typed configuration into the kernels
  crate's parameter types.
- ADP adds `rustls` (an empty root certificate store for the stub's plain-HTTP client) and, for tests only, `axum`
  (the fake stub server) and `saluki-metric-plan-syntax` (export round trips); all were already workspace
  dependencies.
- Legacy components stay where they are as thin adapters over the kernels until the stage 3 plan-driven topology
  replaces them. They then become test-only differential references (`#[cfg(test)]`), after the non-metric parts of the
  legacy builder functions are split out (construction rule 4). Stage 3 did this in ADP: `dogstatsd_prefix_filter`,
  `dogstatsd_no_agg_split`, `dogstatsd_post_aggregate_filter`, and the transforms of `tag_filterlist` and `host_tags`
  compile only for tests (the configuration conversions and `fetch_host_tags` stay), and the legacy metric builder
  functions moved to `cli/run/legacy.rs`. The `saluki-components` transforms (mapper, host enrichment, chained) stay:
  they are library components, and ADP still uses host enrichment and chaining for events and service checks.

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
  structure: `components()` sorted by ID (`ComponentSnapshot { id, component_type, input: Option<DataType>,
  input_capacity: Option<NonZeroUsize>, outputs: [OutputSnapshot { id, data_type }] }`, plus `component(id)`; the
  accessors are `input()` and `input_capacity()`, both `None` for sources and relays), `edges()` sorted (`EdgeSnapshot
  { from: ComponentOutputId, to: ComponentId }`), and `admission_points()` (`AdmissionPointSnapshot { output, id, mode,
  heartbeat }`). Equal structures have equal snapshots whatever the assembly order. `DataType` is exported from
  `saluki_core::topology`. Lowering compares a lowered plan with this snapshot of the legacy blueprint.
- The graph rejects a repeated edge (`GraphError::DuplicateEdge`), which would deliver every buffer twice, and its
  cycle check runs a depth-first search from every component, so cycles on paths that end in encoders and forwarders
  (no destination) are found. A cycle is reported in edge order, starting and ending at its smallest component ID.
- **Input capacity per component** (stage 5). `TopologyBlueprint::with_component_input_capacity(component, capacity:
  NonZeroUsize) -> Result<&mut Self, GenericError>` gives one transform or destination an input interconnect of its
  own capacity in place of the interconnect capacity of the topology (`with_interconnect_capacity`, default 128,
  unchanged), which `interconnect_capacity()` reads. A component's own capacity survives later
  `with_interconnect_capacity` calls and `replace_*`, and the last one set wins. The blueprint's interconnect bounds
  count each transform's and destination's interconnect at its capacity: the minimum is one `EventsBuffer` struct per
  slot, and the firm limit one full buffer per slot plus one in-flight buffer per source, decoder, and transform, as
  before. Other component types are refused with an actionable error (an unknown or invalid ID too), since the bounds
  count only the interconnects of transforms and destinations; encoders' event interconnects are not counted at all, a
  gap that predates this work.

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

`saluki_core::execution` (module `region/`, stage 3):

```rust
pub trait RegionDriver: Send + 'static {
    type Control: Send + 'static;
    fn accepts_input(&self) -> bool { true }               // false while a yielded drain is pending
    fn process(&mut self, turn: &mut RegionTurn<'_>, batch: EventsBuffer, exits: &mut ExitSink) -> Progress;
    fn resume(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress { Progress::Done }
    fn control(&mut self, turn: &mut RegionTurn<'_>, message: Self::Control, exits: &mut ExitSink)
        -> Result<(), RegionFailure>;
    fn next_deadline(&self) -> Option<Instant> { None }    // std::time::Instant, on RegionTurn::now's clock
    fn advance_time(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink) -> Progress { Progress::Done }
    fn finish(&mut self, turn: &mut RegionTurn<'_>, exits: &mut ExitSink, policy: DrainPolicy) -> Progress { .. }
    fn reset_after_panic(&mut self, panic: &RegionPanic) -> PanicOutcome;
    fn publish_telemetry(&mut self) {}
}
pub struct RegionTurn<'a> { now: Instant, budget: &'a mut Budget }   // new(now, &mut budget), now(), budget(_mut)()
pub struct ExitId(u16);                                              // exit i = the i-th named output
pub struct ExitSink { .. }        // new(exit_count), emit(ExitId, EventsBuffer), len, event_count, drain, clear
pub enum PanicSite { Process, Resume, Control, AdvanceTime, Finish, PublishTelemetry }
pub struct RegionPanic { site, message, lost_events }
pub enum PanicOutcome { Continue, Fail(GenericError) }
pub struct RegionFailure { error: GenericError }                      // a control message the driver cannot apply
```

- **Refinements from the code** (stage 3A). Driver calls take a `RegionTurn` (the host's clock reading and a budget),
  not a kernel `Turn`: a `Turn` borrows one kernel's ports and counters, which the driver owns per kernel, so the
  driver builds kernel turns from it (`Turn::new(&header, turn.now(), turn.budget_mut(), ..)`) and every kernel of a
  call charges one budget. Each call starts with a fresh copy of the builder's turn budget (unlimited by default).
  `now` is `tokio::time::Instant::now().into_std()`, so paused-time tests drive deadlines. `control` gets a turn too,
  because stage 6 kernel controls (the context snapshot) need one. `reset_after_panic` receives a `RegionPanic` (site,
  panic message, events the host counted as lost) rather than only the site, so a driver can charge the loss and the
  panic to the panicking node and word its `Fail` error. Only `process`, `control`, and `reset_after_panic` are
  required; the defaults suit stateless drivers. Nothing in a turn or a sink needs a topology, so tests and harnesses
  that wire regions together synchronously drive a `RegionDriver` directly.
- `RegionHostBuilder<D>` implements `TransformBuilder`: input `EventType::Metric`; outputs the slot's exits from the
  boot [physical frame](#physical-frame-and-region-programs), as named metric outputs in order (exit `i` is
  `ExitId(i)`), validated as output names, distinct, and at most 65,536. `RegionHostBuilder::new(exits,
  work_per_turn: NonZeroUsize, mailbox: ControlMailbox<D::Control>, factory: Fn(&BuildContext) -> Result<D,
  GenericError>)` builds the driver in `build`, with the region's build context; `from_driver(exits, work_per_turn,
  mailbox, driver)` hands over one instance. `.with_turn_budget(Budget)` and `.with_bounds(impl MemoryBounds)` (the
  driver's kernels, from the operator registry) are optional. `work_per_turn` is a required argument because the plan
  crate's `limits::values::WORK_PER_TURN` owns its value (ADP passes it); saluki-core keeps no copy. The mailbox is
  taken once in `build`, like `AggregateConfiguration::context_snapshot_receiver`, so a builder builds one transform.
  Bounds: the component struct (which holds the driver) as minimum; one full events buffer per exit (`(ExitId,
  EventsBuffer)` plus 1,024 events) as firm, for output held while a send waits; plus the driver's bounds. The
  interconnect bounds of the topology already count one in-flight buffer per transform, so this is slightly
  conservative.
  One `Health` handle serves all generations.
- **Run loop.** A biased `select!`, in priority order: `health.live()`; the control mailbox; the telemetry timer;
  resuming a yielded call (through `tokio::task::coop::consume_budget`, so a long drain that writes nothing still lets
  other tasks run); the driver's deadline (one Tokio `Sleep`, reset only when `next_deadline` changes); and input,
  only while no call is yielded and `accepts_input()`. After one input batch, up to `work_per_turn - 1` batches already
  waiting are processed (`now_or_never`) before control and timers are checked again, stopping early when a call
  yields or the driver stops accepting input. While a call is yielded, the host calls only `resume`, `control`, and
  `publish_telemetry`. `Progress::WaitUntil` means `Done` to the host, which reads `next_deadline` after every call.
- **Exits.** After every call, the host commits the `ExitSink` to an outbox and sends it buffer by buffer in the order
  the driver wrote them (so each exit receives its buffers in write order, and one channel fed by several exits sees
  them interleaved as written), each send awaited: the native "outputs wait for downstream" delivery policy,
  head-of-line coupling included. During each send it still answers `health.live()` and applies control messages,
  because prepare and abort touch only the program table; a control message's output queues behind what is waiting.
  Buffers go out as the driver wrote them: the driver stamps headers, and header-only buffers (progress batches) are
  sent like any other. An exit with nothing connected discards and counts, as any output does. A failed send (a
  downstream that stopped) is logged and processing continues, as every native metrics transform does.
- **Shutdown.** When the input closes, the host completes any yielded call, calls `finish(DrainPolicy::Shutdown)`,
  resumes it until it returns anything but `Yielded`, sends its output, publishes telemetry, and returns `Ok`.
- **Telemetry pacing.** `publish_telemetry` runs at most once per second while the driver works (a timer at the last
  publish plus one second, armed only after work), immediately after every control message, and once at stop.
- **Control mailbox.** The installer never blocks on a region. `control_mailbox() -> (ControlSender<C>,
  ControlMailbox<C>)`: `ControlSender::send(message) -> ControlAck` queues on an unbounded channel and never waits;
  `ControlAck` is a future of `Result<(), ControlError>`, with `ControlError::{Rejected { failure: RegionFailure },
  Panicked { message }, Stopped}`. A reply channel closed without a reply (the region stopped or failed, or never ran)
  resolves to `Stopped`: closed reply channels mean shutdown. `ControlSender::is_closed()` tells whether the region is
  gone. `ControlSender::running()` resolves to `true` once the region's host runs (it marks its mailbox running when its
  health handle reports ready) and to `false` if the mailbox is dropped first; `is_running()` reads the same without
  waiting, and `ControlMailbox::mark_running` (`test-util`) marks a mailbox whose driver a test calls directly. The
  installer waits for every region to run before it sends anything (stage 3 review): the topology starts only after the
  environment is ready, which in connected mode can take longer than any prepare timeout. This replaces the earlier
  "`watch` of the latest control plus a queue": every installer step (prepare, abort) needs its own acknowledgment, and
  the installer already coalesces candidates (it keeps only the latest pending one) and waits for each barrier before
  the next step, so the queue is bounded by the protocol, not by the channel.
- **Supervision.** `process`, `resume`, `control`, `advance_time`, `finish`, and `publish_telemetry` run under
  `catch_unwind(AssertUnwindSafe(..))`; no build profile sets `panic = "abort"`. `accepts_input` and `next_deadline`
  are plain reads of driver state that must not run kernel code; a panic there, or in `reset_after_panic`, fails the
  component. On a panic the host clears the `ExitSink` without dispatching (no partial delivery; output that earlier
  calls committed is still sent), ends any yielded call, counts the lost events, and calls `reset_after_panic`. Lost
  events are the batch's events (recorded before the call) for `process` and for `resume` of a yielded `process` (an
  upper bound there), and otherwise the events the call wrote plus the batch of a yielded `process` that the panic
  ends. They are counted per component as `component_events_dropped_total{intentional:false,
  drop_reason:region_failure, loss_class:invalid}`, with `component_errors_total{error_type:panic}` per panic. The
  host counts events, not points: counting points would cost a pass over every batch before every call, and the batch
  is gone after the panic; a driver that wants plan-level point loss counts points as it admits batches. The driver
  rebuilds every kernel of every live generation from its `KernelSpec` (factory plus parameters) and clears all
  derived per-generation state: memos, cursors, exit scratch, and per-turn counters. A panic is charged to the
  provenance of the panicking node. `PanicOutcome::Continue` keeps the region running (after a panic in `finish`, it
  stops normally); `PanicOutcome::Fail(error)` makes `Transform::run` return `error` with the context "Region panicked
  in <site>: <message>", which fails the component as any native component failure does (the default program's
  budget of 0). A control message whose call panicked is acknowledged `Panicked`. See
  [supervision](#supervision-and-fallback) for budgets.
- **Panic hook.** Catching a panic does not skip the process panic hook, which runs before unwinding. ADP's
  `antithesis` build installs a hook (`initialize_antithesis` in `bin/agent-data-plane/src/main.rs`) that reports every
  panic as `unreachable!("agent-data-plane panicked")`, so a region panic that the driver recovers from (an artifact
  generation within its restart budget, from stage 5) is still reported as a failed property there, and the default
  hook still prints it. Stage 3 does not change ADP's hook; stage 5, which introduces recoverable region panics, must
  decide whether that hook should distinguish them.

## Physical frame and region programs

The topology is built once (`TopologyBlueprint` initializes once; transform outputs are fixed at `add_transform`), so
physical structure cannot follow each candidate plan. The physical layer is split in two, in the plan crate's
`compile` module (no I/O, no topology):

1. **Frame** (boot, fixed until restart): `Frame::compute(&boot_plan, &registry, &namer)`, from the boot default
   program. It contains the **fixed components** (each fence: source I/O hosts and opaque hosts, from stage 6 also the
   aggregation isolation slot), the declared **outputs** (encoders), and the **slots** between them. Each slot is one
   `RegionHost` component named by the host-supplied `SlotNamer` (`Fn(&[NodeId]) -> Result<NativeComponentId,
   String>`, given the slot's boot nodes in plan order, never an artifact node ID). Slots may have several inputs.
2. **Program** (per generation): `compile::place(&candidate, &report, &registry, &frame)` assigns every logical node to
   a fixed component, a slot, or a direct edge, and `compile::compile` also builds every slot's `RegionProgram`. A
   candidate is installable when every node can be placed and every edge uses the frame's physical structure;
   otherwise placement is rejected with one problem per node or edge, each naming the missing capability and its
   stage. R3 may prune nodes and emits inside programs, but never removes exits. Rewrites take the frame as input; a
   rewrite that would place work outside the frame is skipped and recorded in `explain`.

**Slots and R1.** Fusion (R1) puts synchronous neighbors in one slot unless a path leaves one of them through a fence
and comes back to the other, which would make the slot and the fence a cycle. Concretely: in each connected set `C` of
synchronous nodes, a node's **fence depth** is the largest number of fences on a path from any node of `C` to it (zero
when no such path crosses a fence), and neighbors of equal depth share a slot. Every path between two slots then crosses
a fence or a depth step, so the frame is acyclic. In the default program the timestamp split's passthrough edge is such
a step: `dsd_post_agg_filter_in` is reached from the pre-aggregation chain through `dsd_agg` too, so the frame has the
slots `dsd_pre_aggregation` and `metrics_enrich` joined by an exit. R1 also cuts a set where its cost model says one
region would be slower than the native chain (below). The frame records one `FusionDecision` per boot edge (`fused`,
`fence`, `separated` with the fence that separates them, `pipelined` with the cost budget, or `direct`) for `explain`.

**Pipelining (R1's cost model, stage 3 review).** A region runs its program in one task, so one stream of batches gets
at most one core, where the native chain ran each component in a task of its own and moved consecutive batches through
it in parallel, at the throughput of its costliest component. Fusing the whole pre-aggregation chain cost 0.68 to 0.72
of the legacy throughput on one saturated stream (amendment 47). R1 therefore fuses only as far as that throughput is
kept:

- Every operator estimates its work in nanoseconds per event (`OperatorDef::cost`, 0 by default): the mapper 65 with
  profiles and 10 without, the namespace prefix 90 with a namespace and 0 without, a `split` 5 plus its predicate (an
  `intake_name` leaf 5, plus 10 to normalize the name, once per predicate, as `EventView` caches it; tag predicates
  10, name and host 5, constants, flags, emptiness and kind tests 1, operands summed), a `tee` 15 per port beyond the
  first, the timestamp split 10, tag projection 15, host tags 30. The estimates are coarse: they only need to rank the
  components of a chain, so that the costly ones run apart.
- A **component** is the nodes with one native provenance, or a node of its own otherwise. The **budget** is the
  estimate of the costliest component of the boot plan as it runs natively (each `split` alone), or
  `REGION_COST_FLOOR` (64 ns, over 15 million events per second) if that is less: a region within the floor keeps up
  with one stream whatever it fuses.
- In a region, the `split`s and `tee`s joined by edges are one **decision**, run as one function (stage 4), so R1
  never cuts one, and prices its `split`s together as the function runs them (`operators::decision_cost`): routing
  (5) and their predicates with derived fields computed once, or, when regions memoize the decision (R4: every
  predicate reads only context fields and the kind, and the `split`s cost at least `compile::MEMO_COST_FLOOR`, 25 ns,
  alone, the floor decision compilation uses), one memo lookup (10) in place of the predicates: the estimate is a
  hit's. A `tee` keeps its copies' estimate. The **units** R1 never cuts are the components, each joined with every
  other that one of its decisions reaches into: in the default program, `metrics_enrich` with every gateway, and the
  post-aggregation filter with the client telemetry tap.
- Walking each fused set (connected set and fence depth) in plan order, a unit starts on the latest **stage** of its
  inputs in the set and joins the regions of its inputs on that stage, unless the joined region would cost more than
  the budget; then it moves to the next stage. Regions are tracked as they form, so parallel branches, which run in
  regions of their own, never add up unless a node joins them (a `merge` of two full branches starts the next stage,
  with its emit), and a unit that adds no work to the one region it joins never moves (no region holds only a
  terminal, even behind a unit that alone costs more than the budget). Slots are the nodes of one set, depth, and
  stage that edges join, so a stage only grows along edges and the frame stays acyclic. No region does less than a
  native component did, and none costs more than the budget unless one unit alone does (`metrics_enrich` with every
  gateway and DogStatsD defaults: 75 of 64). `Slot::cost` and `Frame::cost_budget()` record the estimates, `explain`
  prints each slot's, and an edge between stages is `pipelined`. Tests: `compile::tests::{
  the_cost_model_sums_parallel_branches_only_where_a_node_joins_them,
  a_decision_is_never_cut_and_costs_what_its_function_does,
  with_gateways_as_routing_operators_enrichment_runs_the_whole_policy_region_as_one_decision}`,
  `operators::tests::a_split_costs_its_predicate_and_a_decision_at_most_a_memo_lookup`, and
  `runtime::frame_tests::property_test_the_cost_model_keeps_decisions_whole_and_regions_within_the_budget` (random
  plans: no decision cut, no terminal-only region, and a region over the budget only with a decision that copies).

**Fixed components and admission points.** Every output port of every fixed component is an admission point,
numbered `0..` in plan order and port order: sources `Admit`, opaque hosts `Readmit`. Each point's `route` is where its
one physical edge leads: a slot, another fixed component, an output (a direct edge), or nothing. An `emit` or `drop`
whose only input crosses a fence is a **direct edge** (or nothing), never an emit-only region. Through stage 3 the
default program's gateways were opaque hosts, whose emits were such direct edges; from stage 4 they are `split`s of the
policy region, so no fixed component stands between a region and an encoder. `FixedComponent` keeps the
boot operator and parameters, and what feeds its input (an admission point or a slot exit).

**Exits.** A slot's exits are, in order: one per target of the boot edges leaving it (a fixed component or another
slot), and of its `emit`s (an output), in plan edge order; then one per output the boot plan delivers to downstream of
the slot, through any node, in output order, whether or not the slot emits to it. Exits are named after their target
(the fixed node's local ID, the slot's name, or the output ID), which are valid component output names; two exits of
one slot with one name fail the frame. Unused exits stay connected and idle. `Frame::edges()` is the physical edge
list the topology builder turns into blueprint edges: each admission point's edge, then each slot's exits.

For the stage 3 default program (with the no-aggregation pipeline) and a default configuration, every estimate is small
(the mapper without profiles 10, the prefix filter without a namespace 20, the timestamp split 10, the tag filterlist
15, the post-aggregation filter 36), so the floor is the budget and nothing is cut: `dsd_pre_aggregation` holds the
mapper through the tag filterlist and has exits `dsd_agg`, `metrics_enrich` (passthrough), and an idle one to each
output; `metrics_enrich` holds the post-aggregation merge through `metrics_policy_0` and has exits to the outputs.
Without the pipeline there is no passthrough exit. A mapper profile (65) or a namespace (the prefix filter then costs
110, natively and in a region, where its filterlist, one intake-name leaf, is evaluated) raises the budget to that
component and pipelines the chain. With every branch wired, both, host tags, and five outputs, the frame has five
slots: `dsd_pre_aggregation` (the mapper, 65), `dsd_prefix_filter_region` (the prefix and the listener filterlist,
110), `dsd_no_agg_split_region` (the timestamp split and the tag filterlist, 25, with exits `dsd_agg` and the
passthrough), `dsd_post_agg_filter_region` (the post-aggregation merge, filter, and tap, one decision, 36, with an exit
to the client telemetry tap), and `metrics_enrich` (host enrichment and tags, and the whole policy region: the fan-out
and the four gateways, one memoized decision, 105 within the 110 budget), whose exits lead to every encoder directly.
Each slot also has an idle exit to every output downstream of it. With DogStatsD defaults (the floor is the budget),
Multi-Region Failover alone (64 ns with the post-aggregation filter's 36) and autoscaling failover alone (62) run in
`metrics_enrich` beside the post-aggregation filter; one routing group (the fan-out's copy and the memoized filter, 15
ns each: 66) or two (45 ns of policy region) leave the post-aggregation filter in `dsd_post_agg_filter_region`. The
gateways always run in `metrics_enrich`: the fan-out has `metrics_enrich`'s provenance, and a decision is never cut.

**Placement.** Fixed nodes run in the frame's fixed component of the same ID and must keep their boot operator and
parameters: a host component keeps the configuration it was built with (aggregation needs stage 6, other hosts stage
7). A fixed component the candidate leaves out is accepted only when it would be idle: a slot exit is all that feeds
it and none of its admission points feeds a slot (removing a gateway branch leaves its exit and host idle); a source
is never left out. A synchronous node's slot follows from its inputs: an admission point's route is a hard
constraint, two admission points routed to different slots are a problem at the joining node, a node of the boot plan
runs in its boot slot when every slot feeding it has an exit to that slot (so a candidate keeps the boot pipeline: a
filterlist change leaves every node where it was), and any other node fed only from other slots runs in the one every
other of them has an exit to. Every slot-to-slot, slot-to-fixed, and
slot-to-output edge must use an exit, every edge leaving a fixed component must match its route, and every declared
output must be a boot output with the boot binding ("adding an output requires stage 7 resource generations").
Synchronous state owners need stage 6 and signal readers stage 10.

**Entries.** A slot's program maps each batch to a logical input. If the header's `entry` is set (by the upstream
region, which knows the consumer's program for the batch's generation), that is the entry; otherwise the slot maps the
header's `admission_point` to an entry (`RegionProgram::entry_for(&BatchHeader)`). Entries are `EntryKey::Admission(id)`
or `EntryKey::Region(EntryId)`; region entries are numbered per slot and per generation in plan edge order, so a later
generation can send several logical branches over one physical edge (stage 6 per-branch widths) without new topology.

**Region programs** are plain data plus `KernelSpec`s, so the driver can build, rebuild, and share kernels. Each has a
kernel table (`ProgramKernel { key, operator, ports, spec }`), a decision table (one `Decision` per group of `split`
and `tee` nodes, see **Decisions** below, with its predicate table and its nodes' port names for per-port telemetry),
and one `EntryProgram` per entry: a list of steps, in topological order, over registers that hold sequences of buffers
carrying the batch's header (register 0 holds the batch):

- `Map { kernel, register }`: a single-port kernel in place;
- `Expand { kernel, input, ports }`: a multi-port kernel (the timestamp split), each port routed;
- `Decide { decision, input, targets }`: run a decision: compute each event's exit set, then move each event to the
  register of every exit in it, the original into one and copies into the others, in batch order everywhere (see
  **Decisions**); `targets` has one route per exit of the decision, in exit order;
- `Concat { inputs, output }`: a `merge`, only when the entry reaches more than one of its inputs (otherwise the
  merge is no step);
- `Exit { register, exit, entry }`: hand the buffers to an exit, setting the consumer's entry when the exit leads to
  another slot and clearing it otherwise; exit buffers leave by value and are never recycled;
- `Drop { register, node }`: policy loss of the `drop` node, per event and per point.

A route (`Route::Register` or `Route::Drop(node)`) folds a `drop` that a decision or kernel port leads to straight
into counting. `state(kernel)` arrives in stage 6. `KernelKey { node, params_hash }` (the hash of `{op, params}` with
resolved values) identifies a kernel instance across generations, and `RegionProgram::fingerprint()` (a hash of its
entries, steps, kernel keys, and predicates with resolved values, in canonical form and as written, which R7's order
changes, and whether each decision is memoized) tells a region that a new generation's program equals the one it
runs. A test-only executor runs the compiled programs of both default-program fixtures over
the default-program trace and delivers exactly what the reference interpreter delivers, policy loss included.

**Decisions** (stage 4, `compile::decision`). `program.rs` groups the `split` and `tee` nodes of a slot into
decisions: a group starts at each `split` or `tee` whose input does not come from another `split` or `tee` of the
slot, and takes in, depth first, every `split` and `tee` an edge from the group leads to. Every other node ends a
group: a `merge`, because a decision is a tree (every event reaches each node at most once, by one path), and every
map, including one that writes no field a predicate below it reads (amendment 51). Nothing in a group writes a field,
so the specification's condition on intervening maps holds trivially. The group's exits are its nodes' ports that lead
out of it, numbered node by node in depth-first order and port by port; a group whose exits would exceed
`EXIT_SET_CAPACITY` (256, the artifact node limit) is cut at node boundaries, a node that would take it over the
capacity starting a decision of its own, and a `tee` with more than 256 ports fails compilation
(`CompileError::TooLarge`, "output ports on one `tee`"). One `Decide` step runs the whole group; its other nodes are no
steps. Types: `Decision { nodes: Vec<DecisionNode>, exits: Vec<ExitTarget>, predicates: PredicateTable, memoized,
rewrites }`, each `DecisionNode` a `split` (`DecisionKind::Split { predicate: PredicateIndex }`) or `tee` with a
`Branch` per port (another node, by position, or a `DecisionExit`); `ExitSet` is a 256-bit set; `Decision::evaluate(&
Event, &mut DecisionScratch) -> ExitSet` is the reference walk. `PredicateTable::intern(&NodeId, written: &Predicate,
Arc<CompiledPredicate>)` takes the predicate as the `split` writes it and computes the canonical form itself;
`SharedPredicate { predicate (canonical), written, compiled, reads, folded, nodes }`, and with R2 sharing, `written`
stays the first `split`'s, which is what `compiled` evaluates.

At decision compilation three rewrites apply, each recorded on the decision (`Decision::rewrites`, then
`CompiledPlan::rewrites`, after the logical records, and `explain`), with no telemetry effect and no capacity rule:

- **R2.** `PredicateTable::intern` keeps each predicate once, by canonical form with resolved values, and lists every
  `split` using it (`SharedPredicate::nodes`). Equality is structural: `prefix` and `matches` by prefix are unified,
  but two bindings with equal values are not. A record lists the sharing `split`s.
- **R3 (bound constants).** `fold_bound_constants` evaluates a predicate's `const`, `flag`, and `is_empty` leaves by
  three-valued logic over their literals and resolved values (`all` is false as soon as one operand is; an unresolved
  binding decides nothing); a constant predicate is `SharedPredicate::folded`, and its `split` takes its constant
  branch in the compiled function. The plan keeps the node and the decision keeps its exits (the other one idles);
  structure never depends on a binding's value. In the stage 4 default program the Multi-Region Failover gateway folds
  to `true` with its fixture's values (enabled, mirroring, an empty allowlist), and to `false` whenever mirroring is
  off.
- **R4.** A decision is `memoizable` when its predicates read only context fields and the kind (every predicate form
  today does), and `memoized` when, in addition, the `split`s it evaluates (those R3 did not fold, a shared predicate
  once) cost at least `MEMO_COST_FLOOR`, 25 ns per event by R1's cost model (`Decision::evaluation_cost` over
  `OperatorDef::cost`, which normalizes the intake name once per predicate): an endpoint allowlist, two intake-name
  leaves (25), is memoized; one intake-name leaf (20, the listener filterlist; 21 with a kind test, the
  post-aggregation filter), a name or kind test alone (the autoscaling gateway's, 11; Multi-Region Failover's, 13),
  and a decision with nothing left to evaluate (a constant function) are not. R1 prices decisions by the same floor
  (see Pipelining), so the frame is cut by what regions run, and the record states the decision's estimated cost.
  Measured (`benches/selection.rs`, `memo_misses`): a lookup that hits costs about 10 ns, and one that misses adds
  about 4 ns to the evaluation, whatever the decision, so memoizing a decision that takes `t` ns pays at a hit rate `p`
  when `p (t - 10) > (1 - p) 4`. An endpoint allowlist (about 25 ns measured, as estimated) pays from about one hit in
  five, and costs at most 15% more (7% to 8% of its region) when every lookup misses; one intake-name leaf (about 18
  ns) would need one hit in three and costs 16% to 19% more when every lookup misses, as for a pre-aggregation filter
  over more contexts than the memo holds. This is a compile-time rule, not a runtime selection.

`DecisionFunction::new(&Decision)` compiles a decision to the function regions run (it validates the decision is a tree
reaching each exit once, `MalformedDecision` otherwise). Every exit set that does not depend on the event is
precomputed, R3's folded `split`s take their branch at compilation, and the function has one of four forms: a constant
(only `tee`s and folded `split`s); a single test; a list of independent tests (a `tee` to `split`s whose ports all leave
the decision, the shape endpoint routing lowers to); or straight-line code with forward jumps only, in which an event
runs each instruction at most once. A predicate several instructions test (R2) is remembered per event in a cache slot.
Events other than metrics take every `split`'s `false` port (a precomputed set). Evaluating never allocates.

Two changes outside the decision module serve selection cost: `EventView` reads the raw name once when it is created,
and `Predicate::compile` fuses an `any` whose operands all match the intake name (the shape endpoint allowlists lower
to) into one node that reads the intake name once and tests each matcher in turn.

The **R4 memo** (`DecisionMemo`) is keyed by `MemoKey { context: Context::key().as_u64(), kind }` (`saluki-core`'s
`Context::key()` and `ContextKey::as_u64()`, added for this), direct-mapped with `DECISION_MEMO_ENTRIES` (4,096)
entries, one probe per lookup (`get_or_insert_with` probes once for a lookup and the insertion after a miss), an
insertion overwriting its entry; an entry stores the key and only the exit-set words the decision needs, so a decision
of up to 64 exits takes 24 bytes per entry (96 KiB per memo). Colliding contexts share an entry, as native aggregation
treats them as one context. The region driver allocates one memo per memoized decision per generation when it prepares
the generation and drops it with the generation.

**Running a decision** (driver): for each buffer, the driver computes every event's exit set (from the memo when the
decision is memoized), counts runs of equal sets into the decision's `ExitSetCounts` (a hash table of
`COUNTED_EXIT_SETS`, 64, distinct sets, with events and their points), and counts events per register exit for exact
buffer capacities. Then: a buffer whose events all take one exit set moves whole to the last register its exits lead
to, with a copy to every other; when one register receives every event that goes to any (the later of two such), the
original buffer keeps those events in place (`retain`, order-preserving) and every other register gets a buffer of
copies of exactly its size; otherwise every register gets a buffer of exactly its size, each event moving to the last
register its set leads to and copied to the others. Per-node, per-port counts (`plan_node_events_total`, and the native
`PortEvents` counters ADP returns from `RegisterTelemetry::port_events`) and each decision `drop`'s policy loss (events
and points) are derived from the counts per exit set when the region publishes or releases the generation, off the data
path, exactly, because a decision has no `merge`; a decision whose events take more than 64 distinct sets between
publications derives early and counts on. `RegionConfig::with_memo_entries` (`test-util` only) changes the memo size
for tests and benches; production has no selection flag.

**Rewrites** (stage 4, `compile::rewrite`). `compile` runs `optimize(&plan, &RewriteContext::new(&registry, &frame,
&report))` before placement, on the one path from a candidate to its programs that the installer core takes for the
boot generation and every later one; `compile_unvalidated` (a degraded default program) runs no logical rewrite.
`optimize` applies `PIPELINE` (each a `Rewrite { id, apply(&RewriteContext, &Plan) -> Option<Rewritten> }`) to a fixed
point, at most `MAX_PASSES` (8) passes and `MAX_APPLICATIONS` (1,024) applications, in the order R3, R5a, R6, R7: R3
first, so that no later rewrite sees a constant `split` or an unreachable node; R5a before R6, since R5a brings filters
up to a `tee`'s direct targets, the only form R6 hoists; R7 last, so that it also orders the predicates R5a's partial
push-downs restrict and R6's hoisted copies. A site one rewrite opens for an earlier one is taken in the next pass.
Every application is checked: the rewritten plan must validate without the catalog (`validate::revalidate`: rewrites
change no outputs, bindings, or scopes, which are all the catalog checks; it also checks that every scope input names
an output port of the plan, as scope closure checks exits, since rewrites remove nodes), place into the boot frame, and
leave every kernel in the slot the plan before it placed the kernel in (`rewrite::moved_kernel`: a kernel is built in
the resource group `<region>.<node>`, so moving it would change its memory accounting and R1's cuts unrecorded; this
happens when a filter would move above a kernel across an R1 cut). One that fails is skipped, the plan unchanged, and
recorded, and a rewrite never proposes a skipped site again (`RewriteContext::is_rejected`); a plan that does not place
is not rewritten. Each application leaves a `RewriteRecord { id, outcome: Applied | Skipped { reason }, nodes,
justification, telemetry: Vec<TelemetryEffect>, capacity_rule }`. `CompiledPlan` carries the rewritten `plan`, the
logical records followed by every decision's records (`rewrites`, split by `logical_rewrites()` and
`decision_rewrites()`), and `converged`; the installer core keeps them per installed generation
(`InstallerCore::active_rewrites`, which ADP counts in `plan_rewrites_applied`), and `Explain::with_rewrites` renders
them as the `rewrites` section (left out of documents of plans not compiled). Tests run one rewrite alone through the
pipeline's own loop and checks (private `run_pipeline(plan, ctx, rewrites)`).

**R3 and R7 in structure** (stage 4). R3 (`compile::rewrite::r3`) folds a `split` when its validated port types make
it constant: one port carries no kind while its input carries some. That covers literal constants after canonical
form (`const`, a junction that `const` operands decide, `not` of either) and predicates that `kind_in` leaves decide
for every kind the input can carry (validation's `predicate_can_match` warning); `flag` and `is_empty` stay opaque
whatever their operands, as in port types, and decision compilation folds them. The `split` becomes an edge from its
input to its live port's target (whose type is the input's, so no downstream type changes); every node all of whose
inputs the dead port or a removed node feeds is removed; a `merge` the dead subgraph feeds that keeps another input
keeps its live inputs, renumbered in order (`inputs` decreases). A site is not proposed when the dead subgraph would
remove a source, a fence, a stateful or dynamic operator, or a node a scope's inputs or exits name, when a node other
than a `merge` would lose an input, when a scope exit names the live target's port, or when the `split`'s input
itself carries nothing (it sits below another operator's dead port, which needs its edge). Folding to `true` relies on
regions receiving only metrics: a compiled decision sends any other event down every `split`'s `false` port, and a
region host's input type is `EventType::Metric`
(`r3::tests::regions_receive_only_metrics_which_folding_a_split_to_its_true_port_relies_on` pins both). One
application folds the first such `split` in plan order with all it makes unreachable. Outputs stay declared; a removed
`emit` leaves its exit idle. Every port R3 keeps carries what it carried, in the same order, so the rewritten plan
delivers every output's records in the same order and, under any capacity bound, refuses the same ones
(`capacity_rule: false`). The record lists the `split`, removed nodes, and changed merges in plan order; telemetry
effects: the `split`'s per-port counts, and the (zero) series of removed `split`s, `tee`s, kernels, and `drop`s. A fold
the frame cannot run (for example one that would connect an opaque host straight to an output, where the frame routes
the host into a slot) is skipped by the pipeline and recorded; the decision that runs the `split` still folds its
constant predicate (a decision-time R3 record). R7 (`compile::rewrite::r7`) reorders the operands of every pure
`all`/`any` of a node's predicate parameters (every leaf is pure; an exhaustive `is_pure` makes a future signal leaf
opt out), cheapest to decide first: by static cost (`const` 1, `kind_in` 2, `flag`/`is_empty` 3, `host_unset` 6, exact
`name`/`host` 8, exact `intake_name` 12, prefix 16 and 20, `has_tag` 32, tag values 40 exact and 48 prefix; a junction
the sum of its operands; a `matches` whose mode is bound counts as prefix), ties by canonical JSON without resolved
values, so the order never depends on a binding's value. The ranks follow a measurement of each leaf over a
DogStatsD-like metric (27-byte name, ten tags, release build): constants, kinds, and configuration leaves about 1.5 ns,
`host_unset` 2, a name or host match 7, an intake-name match 17 to 19 (normalization check first), `has_tag` 11 on a
hit at the first tag and 90 on a miss, a tag value 58 to 63; R7 assumes a tag scan misses. R1's `split` estimates only
rank a `split` against its chain's kernels, and no default-program `split` reads tags. With an explicit
`OperandProfile` (`RewriteContext::with_profile`; observations of `(evaluated, matched)` keyed by canonical form without
resolved values), operands are ordered by expected cost per decision, `cost × evaluated / decided` (decided = false
evaluations for `all`, true ones for `any`; unobserved operands 1/2; never-deciding operands last), then by static
order; nothing compiles with a profile yet. R7 writes the order into the plan's predicate parameter, native nodes'
included (amendment 52); canonical forms, and so R2 and the duplicate analysis, do not change. A decision's predicate
table keeps each predicate as written as well, and `RegionProgram::fingerprint` covers it, so a program whose only
change is R7's order (after a degraded boot generation, which compiles without rewrites, or with another profile) is a
new program that regions install, and the installed generation's rewrite records describe what runs. Both
default-program fixtures and ADP's lowering write no constant `split` and every junction in the static order, so no
logical rewrite applies to the default program (`agent-data-plane`
`lowering::tests::no_configuration_of_the_matrix_gives_r3_or_r7_anything_to_rewrite` checks every configuration of the
lowering matrix: the compiled plan is the lowered one, and no logical R3 or R7 record exists).

**R5a** (`rewrite::r5a`) pushes a filter (a `split` one of whose ports leads straight to a `drop`, or an
order-independent operator of `filter` cardinality, shared as `rewrite::filters::Filter`) up across the operator `O`
directly above it, when `O` is synchronous, stateless, reads no signal, is order-independent, is a map (one record for
each record received: above an expansion, the filter and its `drop` would count one record where they counted each
record it made) with one output port (which leads to the filter, so the filter applies to every consumer), carries its
points in the form they arrived in, and is not itself a filter (so two filters never trade places). A map with several
inputs must be a `merge`, a per-record union: for another one, filtering each input need not filter its output. The
kinds that commute are those reaching `O` (the port types feeding it) for which `O` writes no `kind`, maps the kind to
itself, and writes no field the filter reads for that kind. Three forms, all at the first site in plan order: when
every reaching kind commutes and `O` has one input, the filter moves **whole** (its ID, its `drop`, and its counts
unchanged); when `O` is a `merge`, a copy goes on **each input whose kinds the filter can remove something of**
(`kinds_where(true, input kinds)` of its drop condition is not empty; the others keep their edge) and the filter goes;
when only some kinds commute, a `split` filter gets a **restricted copy** above `O` (`all(kind_in(K), p)` for a filter
dropping its `true` port, `any(kind_in(not K), p)` for one dropping its `false` port, written around the original
predicate's JSON so bindings stay as they are) and stays below unchanged as the residual. A filter operator is never
restricted (a `split` and `merge` around it would reorder a batch). A restricted copy is not inserted when it could
drop nothing (`kinds_where(true, K)` is empty) or when one that covers it already applies to every record reaching `O`:
`filters::applied_above` walks up through nodes that change nothing the copy reads and through merges on every input,
to a `split` whose **restriction** (`filters::restriction`: the drop condition of `p` and the kinds `K` of a restricted
copy, or a filter's own drop condition and every kind) has the same drop condition and kinds that include `K`.
Comparing restrictions rather than the copies' drop conditions is what makes partial push-down reach its fixed point in
one copy. **Native filters move only whole** (amendment 50): ADP reports native series under native node IDs (the
listener filterlist's and post-aggregation filter's `true` port, the endpoint allowlists' `false` port), which a copy
would not carry, so a filter any of whose nodes has native provenance is never copied (across a merge or partially).
The default program is therefore unchanged by R5a and R6: its only reachable site, the post-aggregation filter below
`dsd_post_agg_filter_in`, is native. A filter never crosses a scope boundary: the edge from `O` to the filter is no
scope input or exit, no input of `O` is an exit (an input of `O` may be a scope input; the filter is then first in that
scope), and no port of the filter's own nodes is a boundary (`ScopeBoundaries::bound_at`). New nodes are
`<base>_r5a<n>` in the base node's namespace and provenance (`rewrite::filters::Edit::fresh_id`, shortened to 63
characters). Records list `O`, the filter, and its `drop` in plan order, then the inserted nodes; telemetry effects:
`O`'s counts `changed` when `O` reports per-port counts (a kernel, or a one-port `tee`, a decision), the copies
`added`, the replaced filter and `drop` `removed` (merge form) or `changed` (residual), and, for a whole move across an
`O` that declares capacity loss, the filter and its `drop` `changed` (they now count what `O` sheds). `capacity_rule`
is set when `O` declares capacity loss (no native stateless operator does; the harness's `test_shed` does).

**R6** (`rewrite::r6`) hoists the filters on every port of a `tee` above it when each port leads directly to a filter,
none native or dynamic, and they remove the same records: `split` filters with equal canonical drop conditions (the
predicate, or its negation when the `false` port drops; operands compared with their bindings and resolved values, so
a binding never equals a literal of its value and structure never depends on a value), or one filter operator with
equal parameters. One copy of the first (`<first>_r6h<n>`, with its `drop`) goes between the `tee` and its input, and
the per-port filters, which would pass everything below it, are removed. No scope boundary is crossed (the `tee`'s
input is no exit; its ports and the filters' inputs are no scope input or exit) and no port of a filter or `drop` it
removes is a scope input or exit. Records list the `tee`, every filter and `drop`, then the hoisted pair; telemetry:
the `tee` `changed`, the per-port filters and drops `removed`, the hoisted pair `added`. Neither rewrite relies on the
capacity rule except across an operator that declares capacity loss: under the interpreter's delivery order (see
Reference interpreter), every output receives the same records in the same order.

**Watermark progress** (contract in stage 3, used in stage 6): for each exit leading to a state owner, the driver tracks
the last `t_a` forwarded per admission point. When a batch leaves no events on such an exit and the last forwarded `t_a`
is older than one heartbeat period, the driver emits a header-only progress batch on it. Exits to opaque hosts, host
taps, and encoders absorb progress, so through stage 5 no exit needs it (every state owner is a fixed host); stage 6
marks the exits into the aggregation isolation slot. The validator rejects a stage 6 plan in which a state owner has
more than one physical path from the same admission point.

**Region driver** (`runtime/`, stage 3). `MetricRegionDriver` implements `RegionDriver` for one slot, with
`type Control = RegionControl::{Prepare(Box<PrepareRegion>), Abort { generation }, Release}`:

- **Creation.** `MetricRegionDriver::new(RegionConfig, boot: PrepareRegion)`, where `RegionConfig::for_slot(&Slot, &dyn
  RegisterTelemetry)` takes the slot's name and exit count from the frame and registers the region's invalid-loss
  counter, and `.with_failure_reporter(GenerationFailureReporter)` (an `Arc<dyn Fn(GenerationId, &str)>`) is told of
  every generation the region fails, with the reason. The creator builds the boot program's kernels off the data path
  with `build_kernels(&program, keys, contexts: Fn(&NodeId) -> KernelBuildContext)` (each `PreparedKernel` keeps its
  spec and context for rebuilds), and `PrepareRegion::new(generation, program, kernels, PanicBudget, &dyn
  RegisterTelemetry)` registers the program's counters; the driver never registers telemetry. `RegionBounds::new(&plan,
  &registry, &program)` is the driver's `MemoryBounds` for `RegionHostBuilder::with_bounds`: each kernel's operator
  `specify_bounds` in a subcomponent named `kernel_accounting_name(node)` (the node's local ID, or `policy` for every
  policy node); each predicate of its decisions under the subcomponent of the first `split` using it (stage 4: the
  `SharedPredicate`, its values, `Predicate::value_bytes`, and its matchers' entries,
  `CompiledPredicate::matcher_bytes`); one full events buffer per register of the boot program; and what its decisions
  keep (all firm): the memo of each memoized decision (`decision memos`), the counts per exit set of each decision
  (`decision counts`, `ExitSetCounts::footprint(COUNTED_EXIT_SETS)`, 7 KiB each), and once the scratch of its decision
  steps (`decision scratch`: an exit set per event of a full batch, 32 KiB, and a count and an output buffer per exit of
  the widest decision). Kernels, predicates, memos, and counts of later generations that the boot program lacks are not
  accounted for until stage 7 scopes registrations.
- **Generations.** One program per live generation, sorted by ID, each behind a `Weak<GenerationToken>`. Generations
  whose token is gone are released off the data path, publishing their counts: on every control message, including
  `Release`, which the installer sends to every region when a generation retires, and when telemetry is published.
  `process` never releases: a batch holds its own generation, so the program it needs cannot go under it, and an idle
  region still releases a retired generation's kernels as soon as the installer says (stage 3 review). A preparation is
  checked whole before anything changes: the program's slot, a duplicate generation, every step's kernel, decision,
  register, and exit index (`MalformedProgram` names the entry), and every kernel either held or brought
  (`MissingKernel`); a rejected preparation leaves the region as it was. `Abort` removes a generation, and is a no-op
  for one the region lacks. A batch's generation and entry come from its header only; a batch without a generation, of a
  generation the region lacks, of a failed generation, or for an entry its program lacks is dropped and its points
  counted as `plan_loss_total{component_id:<region>, class:invalid}`, with one warning per kind of fault. A header-only
  batch (a heartbeat) is ignored.
- **Kernels.** A slab keyed by `KernelKey`, each instance counting the live generations that use it, with its own
  reused `PortOutputs`. A preparation reuses every instance the region holds for a key, whatever the installer
  computed, and drops brought kernels it does not need.
- **Execution.** Programs are translated once, at preparation, into ops with resolved indices (a `drop` node becomes
  an index into the program's counts; a decision holds its compiled `DecisionFunction`). A batch runs to completion in
  one `process` call over reused registers. A decision runs as **Decisions** above describes: a buffer whose events all
  take the same exits moves whole (a tee's copies, or a filter that keeps everything), a filter keeps its events in
  place with `retain`, and events split between registers move to their last register and are copied to the others, in
  buffers of exactly their size; events other than metrics take every `split`'s `false` port. Exits receive a copy of
  the header with the step's entry, and empty buffers are not sent. A kernel that yields on a batch is called again, as
  the interpreter does, and one that yields 1,024 times panics (a broken kernel, supervised): stateless kernels complete
  every batch.
- **Telemetry.** Per generation, in plain integers: events per output port of every kernel, events and points per exit
  set of every decision (from which each decision node's per-port counts and its `drop`s' policy loss are derived when
  the region publishes), policy loss per other `drop` node, and each kernel's reported loss by class; per region,
  invalid loss. `publish_telemetry` adds them to `plan_node_events_total{component_id:<node>, port}` and
  `plan_loss_total{component_id, class}` (per point), and to any further counters `RegisterTelemetry::port_events`
  returned for a node and port: ADP returns the native `PortEvents` metrics of its telemetry profiles there (the
  listener filterlist's and post-aggregation filter's `true` port, and from stage 4 each endpoint allowlist's `false`
  port). `PlanTelemetry` registers the plan-level counters with a `MetricsBuilder`; `NoTelemetry` registers none.
- **Supervision.** The driver records which generation and node each call is running. `reset_after_panic` clears the
  registers, port outputs, and per-call counters, then decides by the generation's `PanicBudget`: `Zero` (the default
  program's) returns `Fail` naming the node and generation; `Limited { per_window, window, total }` (`ARTIFACT` is 3 per
  60 seconds and 5 in all, each limit exceeded only by a panic beyond it) records the panic at the last turn's `now`,
  and once a limit is exceeded marks the generation failed (its batches are then dropped as invalid loss) and reports it
  to the failure reporter, which ADP's installer turns into `generation_failed` (so the installer rolls back). It then
  rebuilds the kernels of the panicking generation only (an instance another generation shares is rebuilt for both),
  each inside its resource group and under `catch_unwind`, and returns `Continue`. A kernel that cannot be rebuilt,
  because its factory fails or panics, would leave the instance that panicked in place: every generation using it fails
  and is reported, and the region fails if one of them has a zero budget. A panic outside any program's steps returns
  `Fail`. Through stage 4 every node of a generation shares its origin's budget (`PanicBudget::for_origin`); stage 5
  charges a panic to the panicking node's provenance. Counts recorded before a panic in the same call are kept: they
  describe work the nodes did. The host counts the lost batch in events; the driver does not count its points.
- **Accounting** (stage 3 review). A kernel's `KernelBuildContext` can carry a resource group
  (`with_resource_group`), and the driver enters it around every build, rebuild, and call of that kernel
  (`KernelBuildContext::accounted`), so each kernel's allocations and CPU time are accounted in a group of its own,
  `<region>.<kernel_accounting_name(node)>`, which is also where `RegionBounds` declares its bounds, as a native
  component's chained transforms are accounted. Decisions (`split`, `tee`), registers, and exits are the region's own.
  Entering a group costs two thread CPU-time reads on Linux (about 1.7 µs in all), once per kernel per buffer, as
  `ChainedConfiguration` pays natively.

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
  physical recipient identities, and a delivery policy (only `native` until stage 7). The delivery-policy slot's native
  default is typed: `DeliveryPolicy::overflow()` is `Overflow::Wait` (the region host awaits each exit's send in exit
  order) and `closure()` is `Closure::Native` (native shutdown; a retiring generation's batches finish in the shared
  encoder streams, which do not close).
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
  document this crate writes reads back and writes out byte for byte. `Plan::hash` is the RFC 8785 digest of the plan's
  normalized form without plan-level provenance, with resolved values stripped from node parameters under
  `ResolvedValues::Exclude`. An artifact's `base.program_hash` is the hash of the **export** it was derived from, not of
  the whole plan: the RFC 8785 digest of the exported artifact document without its `base.program_hash` member
  (`ir_version`, `scope`, `nodes`, `edges`, and `base.outputs`, the fingerprint of every output the scope emits to; an
  export has no blobs, no resolved values, and no node provenance). So native parameter changes never change a base
  hash; a structural change inside the scope, or another binding of an output the scope emits to, always does; and a
  change outside the scope (another DogStatsD configuration, a source enabled, an output the scope does not emit to)
  never does. The base program hash changes exactly when the export's artifact does, which is what the stub protocol's
  `default_program_hash` is for: a client that keeps an export fetches it again when the hash it last saw changes
  (catalog binding values change without it). A stale base always means that rebasing on the current export changes
  something. A tool can tell an unmodified export by comparing the hash of its document without `base.program_hash` with
  `base.program_hash`. `export::base(plan, scope)` is the one definition; ADP reports its `program_hash` as
  `default_program_hash` (amendment 61). **Approval binds to the received artifact document**: `ReceivedArtifact`
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
  are checked where artifacts are received (stage 5): the document, each blob, and the document's inlined size
  (written without blobs, each reference as a copy of its blob) within the artifact size; that last bound is what
  limits the artifact's part of the composed plan. `ValidationReport::bounds()` exposes every measure as a
  `BoundMeasure { limit, maximum, scope, value }`: first the whole plan's (scope `Plan`, counting every node and, for
  work per input, entering at the sources; never enforced), then each artifact revision's (enforced, and reported as a
  finding when exceeded). `explain` shows both. Since the stage 5 review, `artifact_memory` too: the memory an
  artifact's nodes declare in their memory bounds (`OperatorDef::specify_bounds`, what their kernels allocate up front
  and at most together, such as a mapper's string interner and caches) is within 64 MiB, reported at the node that
  declares the most (a finding only, not a measure). A kernel is built before the artifact runs, so a size the host
  cannot allocate would abort the process (allocation failure is not a panic), past any panic budget or rollback, and
  any size beyond the host's memory bounds would escape them, since they cover the boot program (stage 7).
- **Authority**: `loss_authority` (every lossy node, of any loss class, has provenance), `trusted_host` (sources and
  opaque hosts never have artifact provenance), and `provenance_consistency` (every artifact revision a node names is
  listed in the plan's `provenance.artifacts`). Whether a listed revision was approved is not validation's to say
  through stage 5, as built: artifact provenance is written only by composition (`compose::compose`), which the
  installer core runs, through the host's `Composer`, on the artifacts it accepted from the channel, under the accepted
  revision, so the list is the accepted revision by construction. An approved-revisions query in the catalog would
  check that path against itself; it is deferred until artifact provenance can come from elsewhere (another composer,
  or a host that proposes artifact candidates itself), and `check_provenance_consistency` says so under `# Missing`.
  Blob integrity is checked when an artifact is read; plans have no blobs.

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
- **Delivery order** (stage 4). Every trace event that moves records (`admit`, `advance`, `close`, and the retirement
  of an installation's old plan) is one **pass**: nodes run in topological order (ties by plan order), with the `emit`s
  last, in plan order; at its turn a node runs every batch waiting at it, input port by input port and in arrival
  order within a port, then ticks (`advance`, state owners) or finishes (`close` under `DrainPolicy::Shutdown`, a
  retirement under `Retire`, except carried state owners). A `merge` therefore passes its inputs on in port order, as
  a region's `concat` does, and the order in which an output receives records follows from the dataflow alone (what
  each node does to its input's order, `merge` port order, `emit` plan order), never from path lengths, from how a
  port's records are cut into batches, or from which of two unrelated nodes runs first. Through stage 3 the interpreter
  delivered breadth first, so a `merge` received a shorter path's records first, and a rewrite that only shortened or
  lengthened a path (an R3 fold, a partial push-down) changed which records a capacity bound inside one batch cut; a
  depth-first order (tried in stage 4) depends on how a kernel cuts its output into buffers instead. Any event after
  `close` is an error. Tests:
  `interpret::tests::{a_merge_passes_its_inputs_on_in_port_order_whatever_the_lengths_of_their_paths,
  records_from_several_emits_reach_one_output_in_the_plan_order_of_the_emits,
  time_driven_output_of_several_state_owners_reaches_a_merge_in_port_order}`, and
  `r5a::tests::a_partial_push_down_keeps_the_order_in_which_an_output_receives_records`.

Installations commit at once, since no batch is ever in flight. An invalid plan (validated against the interpreter's
catalog) is recorded as `InstallOutcome::Rejected` and the active plan stays. State owners correspond by node ID: an
unchanged one (`Retain` and equal parameters) keeps its kernel or adapter instance, so its later time-driven output
flows through the new plan; `Reject` rejects the installation; `Cut`, `Retain` with changed parameters, and adding or
removing a stateful kernel are `Unsupported` (stage 6), and adding or removing a host component is `Unsupported`
(stage 7). The old plan's other kernels finish under `DrainPolicy::Retire`, their output flowing through the old
generation. `cancel` is `Unsupported` until a `cut` can leave an installation pending (stage 6), and so is a plan whose
operators read signals until stage 10; `signal` events are recorded (`RunOutput::signal`) and change nothing.
`capacity(output, n)` (stage 4, `interpret::capacity::OutputCapacity`) bounds the output to `n` more records until the
next `capacity` event for it replaces the bound, across installations, and may name an output before a plan declares
it; an unbounded output accepts everything, and every `emit` to one output shares its bound. Records beyond the bound,
in delivery order, are counted (events and points) as `capacity` loss of the delivering `emit` and kept in
`RunOutput::refused(output)`, with their generation and installation
(`interpret::tests::{capacity_bounds_an_output_across_installations_and_counts_what_it_refuses,
capacity_bounds_what_an_output_accepts_until_the_next_bound_and_across_installations}` pin each of these with
hand-written expectations).

`RunOutput` lists every declared output's `EmittedRecord { generation, install, admitted_at, metric }` (`install` is
the trace position of the generation's `install` event), the records each output refused to capacity, the install
outcomes, per-node loss by class (telemetry, not equivalence), watermarks, and signals.

Under capacity pressure, `equivalence::compare_under_capacity(&policy, &original, &rewritten)` checks the
specification's rule, where `policy` is the original plan's delivery on `Trace::without_capacity()` and each side is a
`CapacityOutcome { delivered, refused }` (`CapacityOutcome::from_run`): no output receives a point more often than the
policy delivers it (no excluded contribution), no point is lost (policy count minus delivered count) more often than
in the original execution (no more capacity loss for any policy-included contribution), and the outputs refuse no more
points in all. Without capacity pressure it holds exactly when both deliver what the policy delivers.

The per-point check is meaningful because both executions use the interpreter's one delivery order (amendment 49): a
rewrite that changed the interleaving at a `merge` would move loss between contributions without changing how much is
lost. The rewrite property tests check what the rule needs directly: a rewrite that keeps every remaining port's
records in order delivers every output's records in the same order (`compile::rewrite::random_plans::check_same_order`,
which compares the ordered `EmittedRecord`s), and the capacity variants check delivered and refused points exactly,
plus `compare_under_capacity`, under arbitrary bounds (0 to 3 records, cutting inside batches) and under batch-aligned
bounds (whole batches' counts from an unbounded run); where a rewrite crosses an operator that loses to capacity, the
rewrite harness checks the rule itself and that no node loses more to capacity in the rewritten plan
(`harness::Case::check_capacity_loss`), at an operator as at an output.

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
bounds. Stage 3 adds the physical sections, additively within version 1: `Explain::with_physical(&plan, &frame,
&placement)` sets `physical` (left out of documents without a frame, so the stage 1 golden files are unchanged) with the
fixed components and their admission points (mode and route) and inputs, the slots with their nodes, entries, and
exits (idle ones marked), the physical edges (`admission`, `direct`, or `exit`), R1's fusion decision with its reason
for every boot edge, the segments (per admission point: the nodes and slots its batches reach and where they are
re-admitted or delivered), and each output's binding (recipients, delivery policy with its overflow and closure
behavior, the physical edges feeding its encoder, and its idle exits). Golden files:
`fixtures/explain/default-program-stage3.{json,txt}` and `default-program-stage3-no-aggregation-pipeline-off.txt`, for
the fixtures with opaque gateways, checked separately against hand-derived exits, idle flags, edge counts, segments, and
bindings. Stage 4 adds the `rewrites` section, before `physical`, also additively: `Explain::with_rewrites(&records)`
lists every record of `CompiledPlan::rewrites` in order (the logical rewrites as the pipeline considered them, then the
decision-time ones by slot and decision), each `RewriteEntry { id, outcome, reason, nodes, justification,
capacity_rule, telemetry: [{ node, change, detail }] }`, rendered in text as `rewrites (n)`, one `<id> applied|skipped:
<nodes>` line per record (`[capacity rule]` when it relies on the rule), then its reason, justification, and each
telemetry effect. A document built with `with_rewrites` describes the candidate in its graph and the rewritten plan in
its physical sections. The stage 3 golden files list none (`rewrites (0)`: in the opaque-gateway form nothing is memoized or
folded). Two more golden files show records: `default-program-stage4.{json,txt}`, the stage 4 default program (gateways as
`split`s) in the frame ADP builds from it, whose only records are decision-time (Multi-Region Failover folded by R3, the
policy decision memoized by R4); and `artifact-like-policy.txt`, the same program with its policy region edited as an
author edits the export (a `debug` filter on each of the fan-out's five ports, a tag projection and a name filter on the
primary path, a canary switch left on in front of Multi-Region Failover), compiled into the default program's boot
frame, which gets every logical rewrite (R3 folds the canary, R5a moves the name filter above the projection, R6 hoists
the five filters above the fan-out, R7 orders the moved filter's predicate) and both decision-time ones on the
enlarged policy decision. Hand-written checks
(`explain::tests::physical::{the_stage_4_default_program_gets_only_decision_time_rewrites,
an_artifact_like_policy_region_explains_every_rewrite_it_gets}`) pin the records, the rewritten plan's edges, and the
reordered predicate. Later sections land with their stages: plan diff and acknowledgments (5); state owners and
transitions (6).

### Limits

`limits.rs` is a table of `LimitSpec { name, default, source, description }`, built from typed constants in
`limits::values` that enforcing code reads (`values::BATCH_SIZE: usize`, equal to `EventsBuffer`'s capacity, and so
on): artifact size 256 KiB, artifact nodes
256, predicate leaves 1,024 per artifact, tee fan-out 32, worst-case work per input 4,096 node visits, prepare timeout
10 s, drain deadline 30 s, overlap budget 4 live generations (active, last-known-good, and two draining), heartbeat period
`min(5 s, smallest downstream flush interval)`, clock-reset threshold 600 s, blob size 64 KiB (each reference to a blob
counts as a copy of it toward the artifact size, as composition inlines it), R4 memo 4,096 entries per
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

`install::diff(active, candidate, &registry) -> PlanDiff` lists every node as unchanged or changed (operator, parameters
including resolved values, provenance, or the edges at its ports, each flagged) in candidate order, then the removed
ones; the edges added and removed; every node that keeps state in both plans with its transition class
(`ConfiguredOperator::transition_to`); and every output as reused, rebound, added, or removed. **Correspondence**
(composition, `compose::compose`). An artifact node whose local ID equals a reserved ID inside the scope of the current
default program keeps that reserved ID. It keeps the default program's node exactly, with its native provenance and
resolved values (`Unmodified`), if its operator and parameters are the default program's (parameters compared without
resolved values, first as written and then as the operator decodes them, so a set written in another order is the
same set) and, for a node that can lose records (`contract.loss` is `Lossy`) or is a fence (an opaque host), it is
also **fed what the default program feeds it**. Otherwise it gets the artifact's provenance (`Modified`). Every other
local ID enters the `policy` namespace with the artifact's provenance (`Added`).

What feeds a node is computed over the scope's region in both plans (`compose::Feeds`): for each input port, the
streams entering it, counted. A stream is named by where it comes from: a port outside the region (the scope input),
or the output port of a region node that is itself unchanged, that is, has the default's operator and parameters and is
fed as in the default program (recursively, back to the scope input). Any other node produces a changed stream, except
`tee` and `merge`, which pass on what enters them: a stream through them keeps its name, so resizing a `tee` or moving
a branch to another of its ports changes nothing downstream. Counting makes a node fed one stream twice different from
one fed it once, and keeps the computation linear however copies nest; a cycle is a changed stream. Lossless nodes
(`emit`, `split`, `tee`) keep native provenance whatever feeds them, so edits off the lossy paths keep it, and an
unmodified export still composes to the default program exactly.

Why: native configuration authorizes the policy loss it already expresses, and that loss is a node's operation **on
the records it receives**. A kept `drop` fed the whole primary stream (or a split's other side) would otherwise keep
native provenance, so `plan_loss_total`, explain, and per-provenance supervision would attribute loss to native
configuration that it never expressed; a kept opaque host fed another stream would escape `trusted_host`. So an
artifact that filters in front of the policy tree, or changes a gateway's predicate, gives the drops behind it the
artifact's provenance (they diff as `changed provenance`). Since stage 4 the gateways are `split`s, `emit`s, and
`drop`s, whose artifact provenance is valid, so such an artifact installs; with stage 3's opaque gateways it was
rejected by `trusted_host` (resizing the tee or removing a branch was not), which the plan crate's tests still pin
over `testing::with_opaque_gateways`.

Composition, not the validator, rejects a local ID equal to a reserved ID outside the scope
(`ComposeError::ReservedOutsideScope`): once composed, the plan no longer says which IDs were an artifact's local IDs.
The node feeding the scope's input, outside the scope, diffs as `changed edges` when the artifact's first node changes.
The composed plan lists the default program's nodes and edges in order, the artifact's taking the places of the
scope's in order (any left over follow the last), so installing an unmodified export composes to the default program
itself (same nodes, edges, and hash; only plan-level provenance, which is not hashed, lists the artifact revision):
`propose` answers `Unchanged`, and `install::correspondence::diff_with_correspondence(active, candidate,
&Correspondence, registry) -> CorrespondingDiff { diff, artifact_nodes }` lists no changed node, with every scope node
mapped back to its artifact node. That diff is the ordinary diff by ID, since composition already gave every artifact
node its corresponding ID, plus the way back from each plan node to its artifact node, so a preview can place the diff
in the artifact's source (`Correspondence::{get, artifact_node, iter, removed}`; `removed` lists, in default order,
the scope's reserved nodes the artifact did not keep).

Kernel reuse: stateless kernels are keyed by `KernelKey { node, params_hash }`. `install::kernel_reuse(active,
candidate)` gives, per slot, whether the program is unchanged (equal fingerprints), the keys to reuse (the active
program in that slot has them), to build, and to release. Reuse is against the active generation only, which the
installer holds, so a reused instance cannot be released concurrently by a retirement. From stage 6, state owners are
keyed by `(node, lineage)`, where the lineage key hashes only state-contract fields (reducer key, reducer, time
contract, admission policy); a `Retain` transition with other parameter changes is applied to the existing instance at
commit.

### Prepare and commit

`install::InstallerCore` holds the installer's state and makes every decision, with no I/O: the caller (the ADP
installer worker) performs the `Action`s it returns, in order, and reports what happens. `InstallerCore::new(frame,
registry, config, notifier, boot)` makes the boot candidate active at once and returns `Boot { generation, programs }`:
the topology starts with those programs and the schedule `Steady(generation)`, so nothing is prepared at boot.
`notifier` creates each token's `RetireNotifier`, which must lead to `retired(id)`. Events are `propose(Candidate {
plan, report, origin })`, `prepare_acknowledged(generation, slot, Result<(), String>)` (a kernel that fails to build is
that slot's failed acknowledgment), `prepare_timed_out`, `retry`, `retired`, and `generation_failed`; actions are
`Prepare(Prepare { generation, slots: [SlotPrepare { slot, program, unchanged, reuse, build }], timeout })`, `Abort {
generation, slots }`, `Commit { generation }`, `RetryAfter(delay)`, `InstallDefaultProgram`, and `Report(Report {
candidate, origin, generation, outcome })`. A candidate equal (by `Plan::hash` with resolved values) to what is being
prepared, or to the active plan when nothing is, is `Unchanged` and creates no generation. An artifact whose validation
report has errors is rejected; a default-program candidate is installed anyway, as long as it is structurally sound
and fits the frame (`compile::compile_unvalidated`, over `place_unvalidated`), because it is what native configuration
says and nothing else could run in its place (amendment 14). The caller reports it degraded.

1. **Prepare.** Validate, compose, place into the frame, compute the diff, build changed kernels off the data path in the
   installer task, each inside its own resource group under its slot's, and send `Prepare { generation, program,
   kernel_specs, new_kernels }` to **every** region (unchanged regions alias their existing program). Each region
   acknowledges.
2. **Barrier.** All acknowledgments must arrive within the prepare timeout. On timeout or any failure, `Abort` goes to
   every region. Artifacts report `rejected`. Default-program candidates stay `pending` and are retried with backoff
   (1 s doubling to 30 s), keeping only the latest candidate, so native eventual application of live configuration is
   preserved. One preparation runs at a time. A newer default-program candidate waits behind it (replacing any waiting
   one) rather than cancelling it, so that a stream of configuration changes always commits, and so does the
   re-composition of the artifact in effect after a configuration change (since the stage 5 review: it cancelled
   before, so changes arriving faster than regions acknowledged never committed); a newly accepted artifact cancels an
   uncommitted preparation, as the specification's ordering rule says, and starts at once, without waiting out the
   retry delay of a candidate whose preparation failed and which it replaces (`InstallerCore::submit`'s `Arrival`).
3. **Commit.** One store of `AdmissionSchedule::Steady(new)` (stage 6: `Cut`), which the caller performs on
   `Action::Commit` before anything else; the core counts the generation active from that action. It is irrevocable.
   Abort is legal only before the store. Later proposals are normal installs on top. Before the store, a state owner
   whose transition class is not `retain` rejects the candidate (`cut` names stage 6).
4. **Drain and retire.** Old batches finish under their header's generation. When the last reference drops, the token
   notifies the installer, which reports `retired`. Generations alive past the drain deadline are reported in status and
   telemetry.

**Overlap budget.** The installer counts live generations through their tokens: from preparation (or boot) until
`retired`. A preparation starts only while fewer generations than the budget (4) are alive; otherwise the latest
candidate stays `pending` until a generation retires. Data of stateless generations is never dropped to make room.
A `Prepare` message holds its generation until the region takes it from its mailbox, so a region that stops reading
control messages keeps every preparation sent to it alive, aborted or not: the retries of a default-program candidate
stop once the budget is full, which also bounds what that region's unbounded mailbox holds, and resume when the
region drains its mailbox or goes away (stage 3E's fault test pins this).

**Lifecycle.** `install::GenerationState` is `pending` (preparing), `prepared`, `active`, `draining` (superseded, its
batches finishing), `retired`, `rejected` (preparation failed or cancelled), or `failed`; `GenerationState::apply(
LifecycleEvent)` is the transition table, and an impossible transition is an error. A generation kept as
last-known-good goes back from `active` to `prepared`. A rejected or failed generation also retires, which leaves its
state. The core keeps records of live generations plus the 16 most recent others.

### Supervision and fallback

- Default-program generations have a restart budget of 0: a panic fails the region component and ends the process,
  exactly as a native component failure does.
- Artifact generations: a panic in a node with artifact provenance restarts the region (kernels rebuilt, state lost).
  The generation fails when it exceeds 3 panics in 60 seconds, or 5 in total, or any panic in a node that owns state.
  Failure triggers rollback only if the failing generation is active or prepared; for a draining generation, its
  remaining batches are dropped and counted.
- Rollback chain: failed artifact → last-known-good; failed last-known-good artifact → default program. Last-known-good
  stays prepared in every region (the installer holds its token), so rollback is one schedule store. A failure does not
  change the revision high-water mark. The core keeps last-known-good only when an artifact generation commits over a
  generation that serves another artifact document (compared by artifact hash, not by revision) or the default
  program, and never over one that serves a revoked artifact; a default-program generation cannot roll back, so
  committing one releases any kept generation. A composition of the same document, whether a re-composition after a
  configuration change or an approval under a newer revision, replaces the earlier composition without keeping it, so
  the generation kept before stays kept and a rollback never reinstalls the document that just failed.
  `generation_failed` on the active artifact generation returns `Commit` of last-known-good (only while it is still
  `prepared`); on a default-program generation it only records the failure, and a failure already recorded changes
  nothing.
- A failed active artifact generation takes its artifact document out of effect first: every composition of it in
  flight, under any revision that approved the same document, is dropped (`rejected`, "failed under supervision")
  rather than prepared again against the generation rolled back to, and no other generation serves it any more. A
  newer, different artifact in flight is kept and, after a rollback, prepared again against the generation rolled back
  to. A delivery of the failed document after the failure (a newer revision) is accepted anew.
- **Asking for the default program** (`Action::InstallDefaultProgram`, which the host performs by lowering the current
  configuration and calling `on_default_program`, never `propose`). Every public event of the core ends in one settling
  step (`artifacts.rs`, `settle`). After a failed or timed-out preparation, or a supervision failure, it asks when
  nothing in flight will replace the active generation and either the active generation is an artifact's that failed,
  or the event rolled back or ended the policy's installation and the active generation predates the latest
  configuration change (its candidate is older than the one `on_default_program` last created). It asks at most once
  per event, and never after an event that changed nothing, so it cannot loop. Revocation asks too (see Artifact
  handling). With the policy in effect, `on_default_program` composes the policy again, so a rollback to an artifact
  catches up with configuration changes made while a newer artifact ran.
- A re-composition of an artifact that was running when the configuration changed is retried after a failed
  preparation, as a default-program candidate is (1 s doubling to 30 s), so configuration changes are applied
  eventually; an artifact's first installation is rejected on a failed preparation, as before. A re-composition that
  cannot be composed, or whose plan is invalid (such as a removed output), leaves the artifact as it was composed, and
  the status summary's `degraded` names it. The failure is tied to that artifact: it is reported while the artifact is
  the policy in effect or the active generation serves it, and forgotten when a later re-composition of it succeeds or
  the artifact neither is in effect nor runs any more (a newer artifact committed, or a revocation's default program
  replaced it). A newer artifact that is rejected resolves nothing, so the degraded state stays.
- **Panic budgets as built in stage 5.** The runtime's budgets are per generation and region: `PanicBudget::ARTIFACT` (3
  in 60 s, 5 in all) and `PanicBudget::DEFAULT_PROGRAM` (zero). ADP gives each region of an artifact generation
  (re-compositions included) `ARTIFACT` only if the composed plan places a node with artifact provenance in that region,
  and zero otherwise (`installer::slot_budgets`, from `InstallerCore::preparing_plan` and `place_unvalidated` into the
  frame; a plan that does not place again falls back to `ARTIFACT` everywhere, with a warning). A native kernel's panic
  in a region that runs native nodes only therefore fails the region as the native component does, and is never charged
  to the artifact, nor survived silently; an artifact whose nodes are all native (an unmodified export composed again
  after a configuration change) has a zero budget everywhere. Default-program generations keep zero everywhere. "Any
  panic in a state-owning node fails at once" is vacuous through stage 5, since every state owner (aggregation) is a
  fixed component outside the regions; stage 6 adds it to the driver with aggregation as a kernel. **Per node since the
  stage 5 review:** a region that runs both kinds of node, such as `metrics_enrich` (host enrichment beside the policy
  region's decision), charged a native node's panic to the artifact. The budget now covers the artifact's nodes only:
  ADP passes the composed plan's artifact-provenance nodes with each preparation (`PrepareRegion::with_artifact_nodes`,
  from `installer::artifact_nodes`), and `reset_after_panic` treats a panic charged to any other node as budget zero:
  the region fails at once, as the native component does, whatever the generation's budget. A decision runs its group as
  one step, so a panic while it runs is the artifact's if any node of the group is (a group can hold both kinds, such as
  an artifact's `split` in front of the native policy `tee`); a panic in the region's own steps between nodes stays
  charged to the generation's budget, which is zero in a region with no artifact node. Tested in the driver
  (`a_panic_in_a_node_the_artifact_budget_does_not_cover_fails_the_region`) and end to end (the lifecycle suite's
  `a_native_node_that_panics_beside_an_artifact_fails_the_topology_and_is_not_charged_to_the_artifact`: a native
  `test_panic` node beside the artifact's own in `metrics_enrich`; the artifact's panic is survived, the native one
  fails the topology, the artifact stays active, not failed or rolled back).
- A failed generation stays failed (stage 3 review). A commit that replaces it does not supersede it and never keeps it
  as last-known-good: it keeps the last-known-good it had (for an artifact) or releases it (for the default program).
  A candidate equal to a failed active generation is not `Unchanged`: it is prepared as a new generation. A
  preparation in flight when the core rolls back was diffed against the failed generation and reuses its kernels, so
  the core aborts it, reports it `rejected` ("the generation it was prepared against failed, so it is prepared
  again"), and queues the candidate again, to be prepared against the generation rolled back to.

### Installer state

Installer state (active plan and hash, last-known-good, revision high-water mark, live generations, schedule handle)
lives in an `Arc` created in `run.rs`, outside the restartable worker, so a worker restart never forgets the high-water
mark: the `InstallerCore` (all but the schedule handle, which the caller stores into) is that state. Candidate errors
are reported, never fatal to the worker. `InstallerCore::status()` is the status snapshot (active, last-known-good,
preparing, pending, live generations, high-water mark, and generation records).

In ADP (stage 3), `programmable_metrics::installer::InstallerState` is that `Arc`: one `tokio::sync::Mutex` over the
core, `Lowering`, the schedule handle, every region's `ControlSender`, the retirement receiver, the configuration view
the worker wakes on (`Live<SalukiConfiguration>`), the last lowered plan, the acknowledgments still awaited, the
prepare and retry deadlines, the actions not yet performed, and the draining generations with when they started
draining; plus the `Arc<ArcSwap<PlanStatus>>` it publishes. The worker holds the mutex while it runs, so a restarted
worker (after a panic) continues exactly where the previous one stopped, with the same pending acknowledgments and
deadlines; actions are queued and performed one at a time, so a panic loses at most the action being performed. The
worker is a child of the internal supervisor (one restart per five seconds), on the main runtime, so kernels it builds
keep their background tasks there.

**As built in stage 5.** `ProgrammableMetrics` holds the `Installer` itself until `into_installer`, which creates the
`InstallerState` `Arc` (one mutex over the installer, plus the published status). Before then, `run.rs` can still
change the installer without locking: `connect_artifact_channel` gives it the installer's end of the artifact channel.
Everything above about the state outliving worker restarts is unchanged, and the artifact channel's installer end is
part of that state, with the last snapshot whose handling by the core returned. A restarted worker hands the core the
latest snapshot again only if it is not that one (`ArtifactFeed::mark_unseen`), that is, only if a panic interrupted
its handling. Handling a snapshot again is not idempotent in general: after its artifact failed under supervision and
rolled back, its revision is at the high-water mark, so handling it again would reject it as stale and replace its
`failed` acknowledgment. (The core also ignores a snapshot equal to the last one it handled; the worker does not rely
on that.) The actions a handled snapshot queued are in the installer's state, so a restarted worker performs them. The
core's artifact state (`ArtifactState`: the policy in effect, acknowledgments, revocations, and the delivery that set
the high-water mark) is inside the `InstallerCore`, so it survives a worker restart with the high-water mark and is
lost with the process; after a restart of ADP the channel's next delivery installs the artifact anew.

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
- Stateful operators in artifacts are rejected through stage 5 (they need stage 6's state transitions), with an error
  that says artifacts cannot contain them yet.

**As built in stage 5: the shared shapes.**

- `channel` (plan crate). `ArtifactProduct::Policy` (`"policy"`), `ArtifactConfigId` (a string), `ArtifactConfig { id,
  version, sha256, payload }` with `transport_hash(payload)` and `verify_transport_hash()`, and `ArtifactSnapshot {
  product, configs }` (`revokes()` when empty). `Envelope { revision, artifact_hash, approved_against:
  Option<ApprovedAgainst { active_plan_hash }>, artifact: Value }`; `Envelope::parse(payload)` reads it with the IR
  document parser, so a repeated member name anywhere is refused. `ApplyState` (`pending`, `prepared`, `active`,
  `retired`, `rejected`, `failed`), `ConfigAcknowledgment { product, id, version, revision, artifact_hash, state,
  generation, cut_at, error }` (`cut_at` in whole milliseconds since the Unix epoch, `None` through stage 5),
  `StatusSummary { default_program_hash, revision_high_water, active_generation, active_plan_hash, active_artifact:
  Option<ActiveArtifact { id, revision, artifact_hash }>, stale_base, stale_review, degraded, live_generations }`, and
  `ApplyStates { status (flattened), config_states }`, which is both what the installer reports and the body of the
  status route. Every member is written, `null` when it has no value. `artifact_channel() -> (TransportEnd { publisher,
  apply_states }, InstallerEnd { feed, reporter })`: `ArtifactPublisher::publish(snapshot)` (an equal snapshot wakes
  nobody), `ArtifactFeed::{latest, changed, mark_unseen}` (`latest` is `None` until the first delivery; `changed` is
  `None` once the publisher is gone, and is cancel-safe, so the worker can wait on it in `select!`; a restarted worker
  calls `mark_unseen`), `ApplyStateReporter::report(states)`, and `ApplyStateReceiver::{latest, changed}`. A transport
  failure publishes nothing.
- `compose` (plan crate). `receive_artifact(Value) -> Result<ReceivedArtifact, ReceiptError>` and `compose(default_plan,
  &ReceivedArtifact, &Approval { revision, approved_against, active_plan_hash }, &dyn Catalog, &OperatorRegistry) ->
  Result<Composed { plan, report, correspondence, stale_base, stale_review }, ComposeError>`; the report is returned,
  not judged, so the core rejects an artifact with errors as it does any candidate. The host implements the `Composer`
  trait (`compose(&ReceivedArtifact, &Approval)`) over `compose` with the default program it last lowered and its
  catalog; the core composes only through it.
- `export` (plan crate). `export(plan, scope, &MemoryCatalog, &labels) -> Result<ExportBundle { export_version,
  scope, artifact, ir_versions: IrVersions { current, accepted }, catalog, labels }, ExportError>` and `base(plan,
  scope) -> Result<ArtifactBase, ExportError>`. `base` is the one definition of an artifact's base: the export fills
  its artifact's base with it, composition compares an artifact's base with it (program hash: stale base; a missing or
  different output fingerprint: removed output), and the status's `default_program_hash` is its `program_hash`.
- `install` (plan crate). `InstallerCore::on_snapshot(&ArtifactSnapshot, &dyn Composer) -> Vec<Action>`,
  `on_default_program(Candidate, &dyn Composer) -> (CandidateId, Vec<Action>)` (`propose` while no artifact is in
  effect; otherwise the artifact is composed again against the new default program and proposed under its own
  revision, so bindings stay live), `policy_in_effect()` (test-only since the stage 5 review: nothing outside the
  core's tests reads it), `config_states()`, `status_summary(default_program_hash,
  degraded)`, `apply_states(..)`, and `preparing_plan()` (the plan of the generation being prepared, from which ADP
  derives each region's panic budget). The failure entry point is the existing `generation_failed`, which also
  acknowledges a failed artifact `failed`. `Rejection::Refused { message }` reports an artifact refused before it became
  a plan (envelope, receipt, or composition), so it is counted like any rejected candidate. These live in
  `install/core/artifacts.rs`, a child module of `core.rs`, with the core's artifact state (`ArtifactState`).

**As built in stage 5: receipt, composition, and export.**

- **Receipt** (`compose::receive_artifact`), in order: the canonical form's length within `ARTIFACT_SIZE` (a document
  without one is refused by the read, which names the number); the read (`ReceivedArtifact::from_value`: accepted IR
  version and migration, structure, no resolved values in node parameters, consistent blob table); the node count
  within `ARTIFACT_NODES`; each blob's canonical length within `BLOB_SIZE`, and no resolved value in any blob's content
  (`ResolvedInBlob`); the **inlined size**, the size the artifact would have written without blobs (the document's
  canonical length, less its `blobs` member, less each reference's canonical length, plus the referenced blob's),
  within `ARTIFACT_SIZE` (`TooLargeInlined`), computed from the sizes without copying anything; and the predicate
  leaves of every node, with blob references inlined and parameters decoded by the native registry, within
  `PREDICATE_LEAVES` (a node that does not decode counts none; validation rejects it). Composition copies a blob into
  every place that refers to it, so without the inlined bound a 136 KB document referring to one 63 KiB blob 500 times
  composes into a 32 MB plan; with it, blobs never let an artifact cost the host more than an artifact written without
  them may. Receipt bounds what the host does with an untrusted document before it knows what it does, so it counts
  every node of the document, kept from the default program or not; validation of the composed plan applies the node
  and leaf limits again to artifact-provenance nodes only (`bounds`), unchanged.
- **Composition** (`compose::compose`): the scope's region is every node reachable from the scope's inputs in the
  current default program (`export::ScopeRegion`, shared with the export); the artifact's nodes replace it and its edges
  replace the edges entering it; outputs and scopes are the default program's. Node IDs and provenance follow the
  correspondence (Plan diff and correspondence). Blob references are inlined. Modified and added nodes resolve their
  bindings from the catalog (`resolved` is set to the catalog's value, which ADP's catalog computes with the function
  lowering uses); a binding the catalog does not declare stays unresolved for validation (`known_binding`), and one
  declared without a value is `ComposeError::UnresolvedBinding`. Unmodified nodes are the default program's own, so a
  live binding change reaches them by recomposing against the re-lowered default program. Every `emit` of the artifact
  (kept or not) must name an output its base records (`UnrecordedOutput` otherwise, for an output the catalog declares;
  one it does not declare either is `UndeclaredOutput`, which lists the outputs the default program declares, since no
  export could record it) that the catalog declares with that fingerprint (`RemovedOutput { declared: false | true }`
  otherwise); base entries for outputs the artifact no longer emits to are ignored. An artifact-provenance node whose
  operator keeps keyed state is `ComposeError::Stateful` (artifacts cannot contain stateful operators yet): by its
  contract when its parameters configure, and by its operator (`aggregate`) when they do not, so an author is never sent
  to complete parameters for a node that cannot be installed. An opaque host (opaque state, a host component) is left to
  validation, whose `trusted_host` rule rejects host components in artifacts. `stale_base` is a base program hash other
  than the current export's; `stale_review` is an `approved_against` other than the active plan hash (or with no active
  plan). The composed plan's provenance is the default program's `config_change` and the one artifact revision
  `{revision, hash: received.hash}`.
- **Errors**: `ComposeError::{UnknownScope, DefaultProgram (the default program's scope cannot be written as an
  artifact), ReservedOutsideScope, Stateful, UnresolvedBinding, UndeclaredOutput, UnrecordedOutput, RemovedOutput,
  Malformed (two nodes with one ID, an edge from an undeclared input or into one, an edge to a missing node, a blob that
  cannot be inlined)}`; `ReceiptError::{TooLarge, TooLargeInlined, TooManyNodes, TooManyPredicateLeaves, BlobTooLarge,
  ResolvedInBlob, Document}`. Both are `Clone + Eq`; the core reports them as `Rejection::Refused { message }`.
- **Export** (`export::export(plan, scope, &MemoryCatalog, &labels)`): the region's nodes in plan order with local IDs,
  their parameters without resolved values, and no provenance; every edge entering the region in plan order, an edge
  from a scope input's port written `input.<name>`; the base as in the IR section. `ExportError::NotExportable` for a
  scope with exits (stage 6), two inputs naming one port, a region node fed from outside other than through an input, a
  missing node, a local ID artifacts cannot have (`input`, `exit`) or two region nodes with one local ID, and an `emit`
  to an output the plan does not declare. With the stage 4 default program the export holds the policy tree: the
  `metrics_policy` tee, the main output's `emit`, and each gateway's `split` with its `_true` `emit` and `_false`
  `drop`, its predicate written with its bindings (`fixtures/export/default-program-post-enrich.json`).
- **Reading a bundle** (`ExportBundle`'s `Deserialize`) ignores members it does not know, in the bundle and in its
  catalog's top level, outputs, and bindings (additive evolution, as the stub protocol requires), reads the artifact
  as `PolicyArtifact::from_value` does, and takes an output's display label from `labels` or, when `labels` has none
  for it, from a `label` member of the catalog's output. The catalog document alone (`MemoryCatalog`) stays strict.

**As built in stage 5: the lifecycle** (`install/core/artifacts.rs`).

- **The policy in effect** is the latest artifact the core accepted, until a newer one replaces it, the channel revokes
  it, or its installation ends without it running (rejected, or failed under supervision). Each candidate composed from
  an artifact records the artifact it serves; when the policy's installation ends, the policy in effect becomes the
  artifact of what the schedule will hold (the waiting candidate, else the one being prepared, else the active
  generation unless it failed), unless that artifact is revoked: an older artifact still running, the artifact of the
  generation rolled back to, or none. `InstallerCore::policy_in_effect()` (test-only) returns it.
- **Snapshot handling** (`on_snapshot`). A snapshot equal to the latest one handled changes nothing. More than one
  policy config: every config is acknowledged `rejected` and no generation changes, the policy in effect included; the
  message names at most 10 config IDs ("and N more"), so the acknowledgments stay linear in the number of configs. One
  config is checked in order: transport hash, envelope, receipt (`compose::receive_artifact`), and `artifact_hash`
  against `ReceivedArtifact::hash`; a failure is `rejected` and does not move the high-water mark. Then: an artifact
  whose hash equals the policy in effect acknowledges its current state if it is a redelivery, that is, its revision is
  the high-water mark and the mark was set by a delivery of the same hash at that revision, or its revision is above the
  mark (an approval of the same document under a newer revision, which moves the mark). Since the stage 5 review such a
  delivery becomes the policy in effect without composing anything (`reapprove`): the candidates serving the policy
  serve it instead, as an attribution to a composition of the same document does, so the status summary's
  `active_artifact` names its config and revision (it named the first acceptance's before), its review is checked as a
  new acceptance's is (`approved_against` against the active plan hash, which also becomes the hash re-compositions
  compare with), every acknowledgment that followed the policy follows it, and later re-compositions compose under its
  revision (so the first configuration change after it creates a generation whose artifact nodes name the new revision).
  Any other revision at or below the mark is `rejected` as stale; otherwise the mark moves to the revision, even if
  composition or validation then rejects the artifact (amendments 53 and 54). Composition goes through the host's
  `Composer`, with `Approval { revision, approved_against, active_plan_hash }`, the active plan hash when the artifact
  is accepted; re-compositions reuse that hash, so composing again after a configuration change does not make the review
  stale. The accepted artifact becomes the policy in effect and is proposed as `Origin::Artifact { revision, hash }`
  without a second stale check, and so cancels an uncommitted preparation and starts without waiting out a replaced
  candidate's retry delay. A refusal after the envelope is read is also reported as
  `Report { outcome: Rejected(Refused | Stale) }` under the envelope's revision and hash, so the host counts it; a
  refusal before it has no origin and is acknowledged only.
- **An artifact equal to what the schedule will hold** (an unmodified export composes to the default program) creates
  no generation: the generation holding that plan serves it (an attribution), keeping its own origin and panic budget
  (amendment 57). An artifact cancels the preparation in flight, so "what the schedule will hold" is then the active
  plan: an artifact equal to the active plan cancels the preparation and is `Unchanged` (no duplicate generation, no
  artifact budget on a plan of native nodes). When the generation holding the plan was composed from the same document
  (an approval under a newer revision, or a delivery of a revoked document again), it serves the new acceptance in
  place of the old one. A later re-composition replaces that generation without keeping it. A default program equal to
  what the schedule holds, with no policy in effect (after a revocation), takes the generation back from any artifact
  it served.
- **Revocation** (empty snapshot) revokes every artifact accepted before it, through a revocation count each accepted
  artifact compares with the count when it was accepted. The policy leaves effect, attributions are dropped, an
  artifact waiting or being prepared is dropped (`rejected`, "revoked"), a kept last-known-good generation that serves
  an artifact is released (a kept default program stays kept), and the core asks for the default program if a policy
  was in effect, an artifact was in flight, or an artifact generation is active, unless a default-program candidate is
  already in flight. A revoked artifact never becomes the policy in effect again (so `on_default_program` never
  composes it), is never kept as last-known-good, and runs only in the generation that was active at the revocation,
  until that generation is replaced, unless the channel delivers it anew under a newer revision (amendment 58). An empty
  snapshot with nothing to revoke changes nothing.
- **Configuration changes** (`on_default_program`): without a policy, `propose`; with one, the policy is composed again
  and proposed under its own revision (amendment 56; the host's `Composer` must already compose against the default
  program just lowered; the candidate passed in is then unused). The returned candidate ID is the re-composition's.
  Like a default program, a re-composition waits for a preparation in flight (replacing a waiting candidate, latest
  wins) instead of cancelling it, so a stream of configuration changes always commits; it is still `Unchanged` when it
  equals what the schedule will hold.
- **Acknowledgments** are kept per config ID (at most one), for the configs of the latest snapshot, those still in
  progress, and the 16 most recent others. A config refused on receipt is `rejected` with the reason. Any other config
  follows the installation of the artifact it was accepted as (or the policy in effect it equalled): `pending` (with the
  preparing generation, if any) while a candidate serving it waits or is prepared, `active` while a generation serving
  it is active (unless failed), `prepared` while one is kept as last-known-good, and then how its installation ended,
  from the core's reports: `rejected` (with the rejection, or "superseded" or "revoked"), `failed` (with the
  supervision reason; a failure is never overwritten by the rejection reported with it), or `retired`. When a
  generation stops serving the artifact without ending its installation (superseded and draining, released from
  last-known-good, or taken over by the default program after a revocation), the acknowledgment stays as it was
  (`active`, or `prepared` for a released last-known-good) until the generation retires, and then reads `retired`: it
  never reads `retired` while its generation is alive, nor `active` again after `prepared` (amendment 55). An ended
  acknowledgment keeps naming the last generation it had. `cut_at` is `None` through stage 5.
- **Earlier deliveries.** A new delivery of a config does not hide what becomes of the artifact an earlier delivery of
  that config installed: when the new delivery is refused, the earlier installation stays followed while it runs or is
  in flight; when it is accepted, while the earlier one runs (active or kept), until the new delivery's artifact is
  active. The acknowledgment reported is the latest change (the new delivery's state, then any change of the earlier
  installation, such as its failure, a rollback to it, or its retirement, under the earlier delivery's version and
  revision; amendment 60).
- **Status summary**: `active_artifact` and the stale flags are those of the artifact the active generation serves
  (none when it failed; a revoked artifact while its generation is still active), from the composition that generation
  runs; `revision_high_water`; `live_generations`; and `degraded`, the host's reason joined with the re-composition
  failure of the artifact in effect or running, if any.

## ADP integration

### Host facts, bindings, telemetry profiles

Stage 2 compiled `programmable_metrics` only for its tests (`#[cfg(test)]` in `main.rs`, the plan crate a
dev-dependency). Stage 3 removed the gate and made the plan crate a normal dependency of ADP; the few stage 2 items only
later stages use (`AdpCatalog::to_document`, `NativeOutputs::routing_groups`) stay test-only until then.

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
  (the component reports its own, for sources and aggregation), `Mapper` (caches named after the component), or
  `Silent`. The gateways are listed in their stage 4 form only, a `split` with a `_true` emit (the encoder's identity)
  and a `_false` drop (the gateway's): the routing filters keep `EndpointAllowlist`, the failover gateways are
  `Silent`, as natively; an `opaque_host` with a gateway's ID has no native identity. A reserved node keeps its native
  metric names while its operator matches the profile, whatever its provenance. Each kept metric has a `MetricSource`,
  which says who records it in the plan-driven topology: `Kernel` (the kernel's telemetry, registered by its factory for
  the node's identity), `PortEvents(port)` (events leaving a `split`'s port, which the region host counts: the listener
  filterlist's and post-aggregation filter's drops on `true`, an endpoint allowlist's on `false`),
  `BindingLength(binding)` (`metric_filterlist_size`, the configured list's length), `MatcherEntries { binding, prefixes
  }` (`metrics_endpoint_allowlist_size{match_type}`, the distinct names or compacted prefixes), and
  `BindingChanges(bindings)` (`metric_filterlist_updates_total`, `tag_filterlist_updates_total`: configuration changes
  that changed those live bindings). The installer records the binding-derived values at each installation;
  `KernelBuildContext` needs no replacement flag, since a rule change builds a new kernel that the installer counts.
  Tests check that every lowered node's profile names its provenance component, that the names equal the native
  components' constants, that kernels built with a profile's identity register exactly its `Kernel` metrics (no other
  metric under the node's labels), and that every other source names a port or bindings of the node it is kept on, in
  the lowered program and in the plan crate's stage 4 fixture, with `BindingChanges` naming only live bindings.

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

Frame for that program: with default estimates, slots `dsd_pre_aggregation` (mapper through tag filterlist) and
`metrics_enrich` (enrichment and the whole policy region, one decision), the post-aggregation filter beside the policy
region when it fits the budget and in `dsd_post_agg_filter_region` when the fan-out is wider (a routing group or more,
see Physical frame); a mapper profile or a namespace pipelines the pre-aggregation chain; fixed components `dsd_in`,
`dsd_agg` (opaque, readmit), the other sources, and the encoders. The `metrics_enrich` slot keeps the native ID that
upstream wiring uses. From stage 4 the gateways lower as drawn (through stage 3 they were `opaque_host` nodes backed by
the native components, with `<gateway>_out` emits), and the policy region is one decision of `metrics_enrich` (see
Pipelining). A routing group's filter is bound to its group by key,
`metrics_endpoint_routing.group.{key}.{names,prefixes}`, the lists in the group's canonical form (sorted, deduplicated,
prefixes compacted, names a prefix covers removed), matched as intake names; Multi-Region Failover reads
`multi_region_failover.enabled` (startup; always true when the branch exists) and
`multi_region_failover.metric_mirroring.{enabled,allowlist}` (live), matched as raw names; autoscaling failover reads
`autoscaling_failover.metrics` (startup), raw names of scalar series. `flag` and `is_empty` stay in the program as
written (only compiled programs fold them), so a mirroring change changes only `mrf_metrics_gateway.predicate`, and
installs a generation. Non-metric events take `false`, which drops them, as the native gateways do. The legacy
`MetricFilter` component (`components/metric_filter`) is `#[cfg(test)]`, a reference for the legacy topology only;
`saluki-components`' autoscaling gateway stays public. A branch exists only when native wiring builds it, decided from
the startup configuration by the predicates `run.rs` uses (`DataPlaneConfiguration`'s pipeline gates,
`no_aggregation_pipeline`, connected mode for `liveness_in` and, with a non-zero `expected_tags_duration`, host tags,
and the output gates above). `metrics_enrich_in` and the tees are present even with one input or port, so their IDs
never depend on which other branches exist; `dsd_post_agg_filter_in` exists only with the no-aggregation pipeline, whose
passthrough is its second input. `dsd_stats_out` and `dsd_debug_log_out` stay host edges on `dsd_in.metrics`, outside
the plan.

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
hand-written `lowering/fixtures/every-branch.json` pins the lowering of one every-branch configuration exactly (routing
group keys hand-computed with `shasum -a 256`), with fingerprints checked against hand-written binding documents, and
the same lowered program runs through the reference interpreter (every native kernel, the default aggregation adapter,
and the gateways as `split`s; Multi-Region Failover also with an allowlist and with mirroring off) over the plan crate's
default-program trace, against hand-derived outputs and policy loss. A configuration matrix (25 configurations: the
no-aggregation pipeline on and off, every source alone and together, including checks without DogStatsD and the trace
sampler without the OTLP source (proxy mode with traces not proxied), connected and standalone, host tags, zero, one,
and two routing groups including a shared group, an endpoint with empty lists, and a policy on the primary endpoint,
both failover branches wired or not, metric mirroring with an allowlist, mapper profiles, namespace prefix, filterlists
including the legacy blocklist, a metrics override intake, the primary endpoint repeated as an additional endpoint,
repeated histogram statistics, and aggregation settings) lowers to plans that validate with zero findings, and each is
compared structurally with the blueprint `run.rs`'s `create_topology` builds for it (in standalone mode, or with
standalone mode off and a fixed host provider for the connected entries): every lowered node maps by a documented table
onto a legacy component, a fan-in, or a fan-out (emits through the output they name; a gateway's `split` and its `false`
drop onto the legacy gateway transform), and the legacy edges leaving the mapped outputs and entering the mapped
components are exactly the plan's, plus the host taps on `dsd_in.metrics`. What runs inside a chained component (the
mapper in `dsd_enrich`, host tags in `metrics_enrich`) is outside the comparison; the presence table pins whether host
tags are lowered. Re-lowering tests check that a change to a `Live` input changes only the parameters bound to it, and
that a change to a `Startup` input changes nothing.

### Topology construction, installer, stub, control surface

- `topology.rs` builds the metrics path from the frame: fixed components as today (sources marked `Admit`, opaque hosts
  `Readmit`), one `RegionHost` per slot, the encoders and forwarders, and blueprint edges for every frame edge.
  Everything outside the plan keeps its current construction.
- The installer worker runs on its own task, watches `config_system.live(|c| c)` as a wake signal plus the artifact feed,
  and installs as above. It publishes `PlanStatus` and plan telemetry at INFO: `plan_active_generation`,
  `plan_active_revision`, `plan_installs_total{outcome}`, `plan_live_generations`, `plan_rewrites_applied`,
  `plan_loss_total{component_id, class}` (per point), and derived `plan_node_events_total{component_id, port}`.

**As built in stage 3.**

- **Boot** (`installer::ProgrammableMetrics::boot`, called by `handle_run_command` once the environment provider
  exists). `main` captures the wall-clock process start beside the monotonic one. Boot returns `None` when no metrics
  pipeline is required; otherwise it creates the wake view `config_system.live(|c| c)` first, captures `HostFacts` (host
  tags fetched once, in connected mode with a non-zero `expected_tags_duration`, failing the boot as natively), prepares
  `Lowering::new` from that boot configuration, lowers and validates the boot default program (lowering notes are
  logged; validation errors are logged as degraded and kept in `PlanStatus::degraded`), computes the frame with
  `topology::slot_name` (the slot holding `dsd_enrich` is `dsd_pre_aggregation`, the one holding `metrics_enrich` is
  `metrics_enrich`, and a slot the cost model cuts from them is named after its first node, without the `_in` of a
  merge, plus `_region`, such as `dsd_prefix_filter_region`), and creates the `InstallerCore` (G0 active at once), the
  schedule `Steady(G0)`, and one control mailbox per slot. The host tags kernel takes its tags and deadline from the
  facts, as literals of the program, so the RPC is not repeated. The binding-derived profile gauges are set from the
  boot program.
- **Topology** (`run.rs` `create_topology(config_system, env_provider, registry, Option<&mut ProgrammableMetrics>)`).
  Every pipeline is built by the same code in both modes, through four hooks of a `MetricsWiring` (`None`, `Plan`, and
  the test-only `Legacy`): `add_baseline` (plan: the frame's outputs, with their encoders and forwarders, built by the
  helpers the legacy path uses: `main_metrics_encoder`, `routing_group_encoder`, `mrf_metrics_branch`,
  `autoscaling_failover_branch`, which return `MetricsBranch { encoder, forwarder }`; the Cluster Agent endpoint is the
  host fact; the legacy path adds the gateway components itself), `metrics_source`
  for each source's `metrics` output (plan: recorded; legacy: connected to `metrics_enrich`), `add_dogstatsd` once
  `dsd_in` exists (plan: `dsd_agg` from `dogstatsd_aggregate_configuration`, with its context snapshot channel feeding
  the context dump handler as before), and `finish` (plan: checks that the frame's fixed components (sources and
  aggregation; no opaque host since stage 4) and sources are exactly what the native pipelines built, then a
  `RegionHostBuilder<MetricRegionDriver>` per slot, a blueprint edge per frame edge, and an admission point per
  fixed-component output with `CLOCK_RESET_THRESHOLD` and no heartbeat). A source's port is its named output
  (`dsd_in.metrics`, `trace_sampler.metrics`); aggregation's one port is the default output of `dsd_agg`; a slot exit is
  `<slot>.<exit>`; an output target is its `NativeOutput::component`. A region's factory builds the boot program's
  kernels when the topology builds the region (each inside its own resource group), with `PanicBudget::DEFAULT_PROGRAM`,
  and reports generation failures to the installer; its bounds are `RegionBounds`. `create_legacy_topology` (test-only)
  builds the legacy metrics path through the same hooks, so the two blueprints differ only where the plan replaces
  native wiring.
- **Telemetry** (`telemetry.rs`). `RegionTelemetry` implements `RegisterTelemetry` over `PlanTelemetry` and adds, for a
  node whose profile keeps a `PortEvents` metric on the port, the native counter under the node's native identity; it
  also gives each kernel its `KernelBuildContext`: the native component of its profile (or a transform named after the
  node) as the identity its telemetry reports under, and, with `with_accounting(registry, region identities)`, the
  resource group `<region>.<kernel_accounting_name(node)>` its memory and CPU time are accounted in. The native identity
  is for telemetry only: what a kernel holds is accounted under its region. `InstallerTelemetry` keeps handles to
  `plan_active_generation`, `plan_live_generations`, `plan_installs_total{outcome}` (`committed`, `unchanged`,
  `superseded`, `retried`, `rejected`), `plan_rewrites_applied{rewrite}` (one gauge per rewrite of `RewriteId::ALL`,
  `R2` to `R7`, registered by the installer's first report, at boot, and set at boot and at every commit from
  `InstallerCore::active_rewrites()`: applied records only, skipped ones are not counted; with Multi-Region Failover's
  mirroring off, `R3` is 1, the gateway's folded predicate,
  `installer::tests::the_installer_reports_the_rewrites_of_the_boot_generation_and_of_every_commit`), and to every
  binding-derived native metric it records: `BindingLength`, `MatcherEntries`, and `TagProjectionRules` gauges set at
  boot and at every commit from the committed plan's resolved binding values (`tag_filterlist_size`, the number of tag
  filterlist rules, is `TagProjectionRules` since the stage 3 review: a kernel set it when it was built, so a
  candidate's kernel changed it before the commit, and an aborted one left it wrong), and `BindingChanges` counters
  registered at boot, at zero, as the native components register theirs when built (`register_change_counters`, added in
  stage 3E: before, the series appeared only with the first change), and incremented once per observed change whose
  lowering changed a watched binding's resolved value. Handles are kept because the internal telemetry evicts a metric
  that stays idle with no live handle. A change is compared by resolved (normalized) values, so reordering a set-typed
  list is not an update, unlike the native live view, which counts any change of the list as written.
- **Series the plan-driven topology no longer reports** (measured in stage 3E against the legacy topology, for the
  release note; the fused components are `dsd_pre_aggregation` and `metrics_enrich`, plus the regions the cost model
  cuts from them). For each fused transform (`dsd_enrich`, `dsd_prefix_filter`, `dsd_no_agg_split`,
  `dsd_tag_filterlist`, `dsd_post_agg_filter`): the interconnect series `component_events_received_total`,
  `component_events_received_size`, and, per output (`_default`, and `passthrough` for the split),
  `component_events_sent_total`, `component_send_latency_seconds`, and `component_events_discarded_total`; the health
  series `health_component_ready`, `health_component_live`, and `health_component_liveness_latency_seconds`
  (`component_id:topology.primary.transforms.<id>`); and the task series `runtime_task_poll_count` and
  `runtime_task_poll_duration_seconds` (`task_name:topology.primary.transforms.<id>` and `.<id>.transform`).
  `metrics_enrich` keeps its ID but reports `component_events_sent_total`, `component_send_latency_seconds`, and
  `component_events_discarded_total` per exit instead of for `output:_default`, and its
  `component_events_received_total` counts what reaches the post-aggregation filter, which it now runs. The passthrough
  batcher's `aggregate_passthrough_batch_duration_secs` and `component_events_dropped_total{component_id:
  dsd_no_agg_split, drop_reason:buffer_full}` are gone (amendment 12), and `aggregate_passthrough_flushes_total` counts
  one flush per input batch with timestamped metrics. Every other native series of the fused components keeps its
  name, labels, and value; none of the removed series is matched by `get_datadog_agent_remappings()` or
  `get_compat_remappings()`, and the remapped output is identical but for `no_aggregation.flush`. From stage 4 the
  gateways (`metrics_routing_filter_{i}`, `mrf_metrics_gateway`, `af_metrics_gateway`) lose the same component series
  (received, sent per `output:_default`, latency, discarded, health, task); the routing filters keep
  `metrics_endpoint_allowlist_metrics_filtered_total` (counted on the `false` port, which leads to the drop) and
  `metrics_endpoint_allowlist_size{match_type}` (the installer's `MatcherEntries` gauges) with their values, and the
  remapped output is unchanged but for `no_aggregation.flush`
  (`telemetry::tests::the_gateways_keep_their_native_metrics_and_lose_only_their_component_series`, which also pins
  the regions: the gateways run in `metrics_enrich`, none in a region of its own). Release note
  `plan-driven-metrics-gateways-8f5f8bebdc56b2e3.yaml`.
- **Resource groups** (stage 3 review). Each kernel of a region is accounted in its own group under the region's,
  `topology.primary.transforms.<region>.<node>`, in `/memory/status` and in the `group_*{group_id}` series
  (`group_allocated_bytes_total`, `group_allocated_bytes_live`, `group_allocated_objects_total`,
  `group_allocated_objects_live`, `group_deallocated_bytes_total`, `group_deallocated_objects_total`,
  `group_cpu_time_nanos_total`), where natively each fused transform had its own group,
  `topology.primary.transforms.<transform>` (`dsd_enrich.0_dogstatsd_mapper` and `metrics_enrich.0_host_enrichment` for
  the chained ones). So `dsd_tag_filterlist`'s memory is reported as
  `topology.primary.transforms.dsd_pre_aggregation.dsd_tag_filterlist`, for example. The groups of
  `dsd_post_agg_filter` and the listener filterlist, which run as decisions, are gone: their work is accounted to their
  region's group. Each region's bounds are its kernels' native bounds, without the struct of each native component, its
  decisions' predicates, memos, counts, and scratch (from stage 4), plus a full buffer per exit and per register; every
  other component's bounds are the legacy ones, and the interconnects hold as many buffers, queued and in flight, as the
  legacy ones (see Buffering below; a topology test pins all of it against `create_legacy_topology`). The
  gateways run as decisions, so their groups (`topology.primary.transforms.<gateway>`) are gone; their CPU time is their
  region's. `RegionBounds` declares each predicate of the boot program's decisions under the subcomponent of the first
  `split` using it (`<region>.<split>`, `policy` for an artifact's): the predicate itself, its values
  (`Predicate::value_bytes`: each literal and resolved string as a `String` and its bytes), and its matchers' entries
  (`CompiledPredicate::matcher_bytes`, `SortedSetMatcher::entry_bytes`: each a `MetaString` and its bytes, the fused
  intake-name `any` included). A routing filter's region therefore declares exactly what the native `MetricFilter`
  declared for its lists (one copy of the strings and the matcher's entries); Multi-Region Failover's declares one more
  copy of its allowlist (its emptiness test's), and autoscaling failover's the names its matcher keeps besides them
  (`topology::tests::gateways_run_in_regions_and_declare_their_lists_as_the_legacy_gateways_do`, which also pins each
  region's memoized decisions). Each gateway removes one interconnect and one transform's in-flight buffers. The
  gateways are parallel, and the policy region's input holds one gateway's buffers on each path, so with more than one
  gateway the plan-driven topology holds 129 buffers fewer per gateway beyond the first, and never fewer on a path to an
  output.
- **Buffering** (stage 3 review; as built in stage 5). Fusion removes the interconnects between fused components. With
  one 128-buffer input per region, DogStatsD metrics crossed two queues between `dsd_in` and aggregation where the
  legacy chain had five, and a burst into a stalled aggregation backed up to the source after 257 batches where legacy
  took 644. Each region host therefore gets the input capacity of the transforms it replaces
  (`topology::region_input_capacity`, set with `TopologyBlueprint::with_component_input_capacity`). Natively, each of
  them had an input of `c` buffers (the blueprint's interconnect capacity, 128) and held one more while it waited to
  send; the region holds one while it waits. A region whose boot nodes run through `n` of them on one path gets an input
  of `n * (c + 1) - 1` buffers, for the path that runs through the most, so that no path holds fewer buffers than
  natively and a region that runs one chain holds exactly as many. A path runs through a native component where it
  enters the component's nodes (by `Provenance::Native`) from outside the region or from another component's nodes; the
  components the topology still builds are not counted: the frame's fixed components, and the outputs' encoders and
  destinations, whose provenance the `emit`s and the client telemetry tap's fan-out carry. With a default configuration,
  `dsd_pre_aggregation` gets 515 (the mapper, the prefix filter with the listener filterlist, the no-aggregation split,
  and the tag filterlist) and `metrics_enrich` 257 (the post-aggregation filter and enrichment); with Multi-Region
  Failover, `metrics_enrich` gets 386 (and the gateway); the regions of a pipelined chain share the count (1, 1, and 2
  for the mapper-profile configuration's three pre-aggregation regions). The interconnects' firm bounds (queued and
  in-flight buffers) then equal those of the legacy topology, less 129 per parallel gateway beyond the first; the
  minimum grows by `n - 1` buffer structs (80 bytes each) per region. A path that leaves a region before its longest
  path ends holds more than natively: the timestamped metrics the no-aggregation split sends past aggregation leave
  `dsd_pre_aggregation` after three of its four transforms, and their path holds 129 buffers more. The capacity follows
  the boot program; later generations, artifacts included, run in the same regions. The region's own exits hold a buffer
  each while a send waits, as before.
- **Installer worker** (`InstallerWorker`, a child of the internal supervisor). Before anything else it waits for every
  region to run (`ControlSender::running`, or shutdown), so that a configuration change made while the topology waits
  for its environment is prepared once the regions can answer, rather than timing out and filling the overlap budget
  (stage 3 review). It then waits, in priority order, for a retirement, a generation failure a region reported, an
  acknowledgment, the prepare deadline, the retry delay, a drain deadline, or a configuration change; on a retirement it
  sends `Release` to every region, and a failure goes to `generation_failed`; lowers and validates on every change (a
  lowering error is logged, counted `rejected`, and kept in `PlanStatus::last_error`); proposes; and performs the core's
  actions: kernels a slot lacks are built off the data path, each in its own resource group under its region's (a build
  failure is that slot's failed acknowledgment, and the rest are not sent), each `Prepare` is sent without waiting and
  its `ControlAck` awaited in a `FuturesUnordered`, `Abort` is sent to every listed region without waiting, `Commit` is
  one schedule store followed by the binding gauges, and reports update telemetry, logs, and status. A preparation's
  panic budget follows its generation's origin. Generations that are still draining after `DRAIN_DEADLINE` are logged
  once and listed in `PlanStatus::overdue`.
- The stub protocol and control surface contract is `docs/design/programmable-metrics-stub-protocol.md` (normative for
  ADP's `channel_stub.rs` and the demo server). Saluki-only keys `experimental.programmable_metrics.artifact_channel.
  stub_url` and `.poll_interval` (default 1 s), per the config-system skill. Unset means no channel. The stub accepts
  only `http://` URLs of loopback addresses, warns at startup, and is documented as development-only. It polls `POST
  {stub_url}/v1/artifacts/poll` with rich acknowledgments and a status summary and receives a complete snapshot of the
  `policy` product.
- Privileged routes: `GET /programmable-metrics/status` (`ApplyStates`), `/explain` (`?format=text`), `/plan` (active
  composed plan with resolved values), `/default-program` (the default program last lowered, which artifacts are
  composed into; added for the demo's preview), and `/export?scope=post_enrich`. Every body is read from the published
  `PlanStatus`, so a request never waits on an installation. The export is a versioned bundle `{export_version, scope,
  artifact, ir_versions, catalog, labels}`, where `artifact` carries its base, `catalog` is exactly the `MemoryCatalog`
  document (outputs, bindings with their types, liveness, and current values, and the scope's inputs), and output
  display labels are in the top-level `labels` member, left out when empty (the demo reads both forms). The payload
  envelope's `artifact_hash` is checked against `ReceivedArtifact::hash` of its `artifact` document. The `artifact`
  lists its nodes and edges in the surface syntax's canonical order (see Demo), the order lowering writes plans in, so
  printing an export and parsing the text gives back the same document and hash. `lowering/export_tests.rs` tests
  `parse(print(export)) == export`, that the base program hash is the digest of the exported document without it and
  records the catalog's fingerprint of every output the export emits to, and that composing the export back gives the
  default program, for every configuration of the lowering test matrix (the syntax crate is an ADP dev-dependency,
  never a runtime one); the syntax crate's own fixture tests do the same for the plan crate's default-program fixtures,
  with their gateways as routing operators and as opaque hosts.
- The status summary adds `stale_review` to the protocol's `status` object, as an additive member: the protocol says a
  stale review is flagged in status but named no member for it. The repository copy of the protocol documents it.

**As built in stage 5 (ADP channel).**

- **Configuration.** `control.programmable_metrics.artifact_channel` (`agent_data_plane_config::control::{
  ProgrammableMetrics, ArtifactChannel { stub_url: Option<String>, poll_interval: Duration }}`), seeded from the
  Saluki-only source `experimental.programmable_metrics.artifact_channel.{stub_url, poll_interval}` (`SalukiOnly::
  experimental.programmable_metrics.artifact_channel`, a `DurationString` interval). The default interval is defined
  once, `defaults::DEFAULT_ARTIFACT_CHANNEL_POLL_INTERVAL` (1 s), which the model's `Default` and `seed` reference. An
  empty `stub_url` is the same as unset (the Agent-style convention, so
  `DD_EXPERIMENTAL_PROGRAMMABLE_METRICS_ARTIFACT_CHANNEL_STUB_URL=""` turns the stub off). A zero `poll_interval` is
  rejected by the source's deserializer, so it fails the configuration load, as a zero
  `aggregate_window_duration_seconds` does. Both keys are in `SALUKI_KEYS` (`programmable_metrics.rs`, metric
  pipelines' affinity) with a prose section in the generated configuration reference. A live configuration update
  cannot reconfigure the stub: the keys are read once.
- **Where the stub URL is checked (an explicit exception to the config-system skill's "input validation at the
  boundary").** The model keeps the URL as a `String`, as it keeps every other URL (`dd_url`, the Cluster Agent's
  `url`), and the stub checks it: `StubChannelConfig::new` parses it and requires an `http://` URL of a loopback
  address (`127.0.0.1`, `[::1]`, or `localhost`) without a query. These rules are the stub's own security policy (it
  has no authentication, so it may only be reached on the loopback interface over plain HTTP), kept in one place with
  one actionable error naming the key, rather than split between a syntax check in the source and a host check in
  ADP. `run.rs` calls `StubChannelConfig::from_configuration(&ArtifactChannel)` right after the configuration is
  loaded and checked for compatibility, before the environment provider is set up and the metrics path booted, so a
  bad URL fails startup at once. `StubChannelConfig::new` also refuses a zero interval, as a precondition of the
  constructor (Tokio's interval timer panics on a zero period), which the configuration already guarantees.
- **Stub worker** (`StubChannelWorker`, on the internal supervisor, beside the installer; `ClientInfo::for_process()`
  creates the random client ID once per process). On every start it logs, at `warn`, that the stub is for development
  only. It polls `POST <stub_url>/v1/artifacts/poll`: a path in the stub URL is kept and the poll path appended to it
  (less a trailing `/`). It polls at once, then every `poll_interval` (`tokio::time::interval` with
  `MissedTickBehavior::Delay`, so a late poll delays the next rather than bunching them up), sending
  `PollRequest::new(&client, &apply_states.latest())` as `application/json`. The HTTP client is saluki-io's
  `HttpClient`, HTTP/1.1 only, with an empty root certificate store (the stub is plain HTTP; building any `HttpClient`
  still loads the platform's root certificates for its proxy connector, `hyper-http-proxy`'s `ProxyConnector::new`,
  which is why the stub's tests can fail to build a client in a sandbox where the platform's trust settings cannot be
  read). Each poll has a 5 s deadline (`channel_stub::POLL_TIMEOUT`) over the whole exchange, from connecting to reading
  the whole answer: the client's own request timeout ends when the headers arrive, so a stub that sends its headers and
  then stalls its body would otherwise stop polling for good. An answer is read up to 32 times the artifact size limit
  (8 MiB). Transport failures, which publish nothing, are: a connection error, a poll that does not complete within the
  deadline, a status other than `200 OK`, a body that is not a poll answer (including one without a `products` member,
  such as an error object), an answer that states another `protocol_version`, an answer larger than the limit, and a
  `policy` member that is not a list of configs (including `null`). Products ADP does not know are skipped unread,
  whatever their shape (`PollResponse::products` is read with a lenient deserializer). An empty `products` object, or a
  missing or empty `policy` list, revokes. A failure is logged when it starts or changes, and once more when polls
  succeed again. Shutdown stops the worker at once, also during a poll.
- **Control surface** (`ControlSurfaceWorker`, also on the internal supervisor: it asserts its `DynamicRoute` on the
  privileged endpoint into the shared dataspace, as `ConfigRuntimeWorker` does). `GET /programmable-metrics/status`
  answers `PlanStatus::apply_states` (the summary flattened, plus `config_states`); `/export?scope=<scope>` the
  published `ExportBundle` of the scope (`400` without `scope`, `404` naming the declared scopes for an unknown one,
  `500` with the reason when the scope could not be exported); `/plan` the active generation's plan as JSON, resolved
  values included; `/default-program` the default program last lowered (`PlanStatus::default_program`, the installer's
  `last_lowered`), resolved values included, which a client composes artifacts into as the installer does; `/explain`
  its explain document (`Explain::to_json`, or `to_text` with `?format=text`; `400` for another format). `/plan` and
  `/explain` answer `503` while no generation is active. Errors are plain text. With a metrics pipeline, `run.rs` always
  runs the control surface and runs the stub worker when it is configured (after `connect_artifact_channel`); without
  one, it warns that the stub is not polled.
- **Installer wiring.** `ProgrammableMetrics::{connect_artifact_channel() -> TransportEnd, status() ->
  Arc<ArcSwap<PlanStatus>>}`. The worker's `next_event` also waits for `ArtifactFeed::changed` (after the deadlines,
  before configuration changes); a closed feed (the transport is gone) stops that branch and keeps the last snapshot in
  effect. A snapshot goes to `InstallerCore::on_snapshot`, and every lowered default program to
  `InstallerCore::on_default_program` (so a configuration change recomposes an artifact in effect), both with a
  `DefaultProgramComposer` over the last lowered default program and its `AdpCatalog`, which implements `Composer` over
  `compose::compose`; the composer is updated to the just-lowered program before `on_default_program` is called, and
  `Action::InstallDefaultProgram` lowers the current configuration and calls `on_default_program` too. These calls, and
  `InstallerCore::apply_states`, go through a small seam, `installer::ArtifactPaths` (production: `CoreArtifactPaths`,
  which delegates; tests: a double), so that the worker's handling of what the core decides is tested independently of
  the core's artifact rules, and with them
  (`an_exported_artifact_installs_follows_configuration_and_rolls_back_through_the_real_core`). Each lowering that
  changes the default program or its catalog recomputes a `DefaultProgramView`: `export::export` of every scope the plan
  declares, with the catalog document (`AdpCatalog::to_document`) and the output labels, and `export::base(plan,
  post_enrich).program_hash`, reported as `default_program_hash`. Output labels name what an output is, never its
  endpoints (`Main metrics output`, `Endpoint routing group <i>`, `Multi-region failover`, `Autoscaling failover`,
  `DogStatsD client telemetry`). After every event (and once at `connect_artifact_channel`, so the first poll carries
  the boot status), the worker computes the apply states (`core.apply_states(default_program_hash, degraded)`), reports
  them through the channel, and keeps them in `PlanStatus::apply_states`.
- **Test seams for the exit gate (stage 5 lifecycle tests).** `ProgrammableMetrics::from_lowering` takes the operator
  registry plans are validated, laid out, composed, and compiled with (`boot` passes `OperatorRegistry::native()`;
  the registry is `Copy`, so the frame, the core, the composer, and explain all use the one passed). Tests reach it
  through `test_support::programmable_metrics_with_operators` and `test_topology::Options::operators`, which default
  to the native registry; the lifecycle suite registers a test operator, `test_panic` (a passthrough kernel that
  panics on a metric of a given name), since no native kernel panics on its input. A default program whose kernel
  panics is stood in by an `ArtifactPaths` wrapper over the core's own paths that adds a native-provenance
  `test_panic` node to every default program lowered after boot. `TestTopology::failure` waits for a topology that
  stops on its own (a component failed) and returns the supervisor's error and what the outputs received; the
  supervisor's error does not carry the component's (the test supervisor restarts the topology, which then fails to
  initialize, `FailedToInitialize { child_name: "primary" }`; ADP's root supervisor restarts nothing), so the suite
  shows the cause by the timing (the topology stops right after the fatal batch, after surviving eight artifact
  panics) and by the installer's status (the default program's generation is still active, not failed).
- **Degraded and last error.** The validation errors of every default-program candidate are kept with its candidate
  for as long as the core holds it (pending, preparing, active, or kept as last-known-good; the boot candidate's from
  boot), and used whenever the candidate becomes active, so a rollback to a default program that does not validate
  reports it degraded again. `PlanStatus::last_error` is set by a rejection of a default-program candidate or of the
  candidate proposed for the latest configuration change (an artifact composed again), and cleared by a commit, or by
  an `Unchanged` report of the candidate proposed for the latest configuration change: the plan that change resolves
  to is what runs or is being prepared, so an earlier change that was not installed no longer describes the
  configuration in effect (for example, a configuration change reverted after the artifact in effect could not
  follow it). A lowering failure sets it and forgets the latest change's candidate. A rejected artifact from the
  channel is acknowledged through the channel instead. The reason ADP passes as `degraded` is `the active default
  program does not validate: <first error> (and <n> more)`, or `the last configuration change was not installed:
  <error>` for a default program or lowering that was not installed, or `None`. A rejected re-composition of the
  artifact in effect is left out of it (integration): the core joins that reason (`artifact revision <r> ... stays as
  it was composed before: <error>`) to the summary itself, so the summary says it once, while `PlanStatus::last_error`
  still records it.
- **Active plan and telemetry.** `PlanStatus` also carries `exports` and `active: Option<Arc<ActivePlan { generation,
  plan, explain }>>`, described once per active generation: the plan's validation report against the current catalog,
  the rewrites compiling the active generation considered (`InstallerCore::active_rewrites`, stage 4's rewrites
  section), and the physical sections of the plan as it runs, rewritten, from compiling the active plan into the frame
  again as the core did (`compile`, or `compile_unvalidated` for a default program that does not validate; the
  sections are omitted, with a warning, if it does not compile). Panic budgets are per region (Installation >
  Supervision and fallback); region failures reach `generation_failed`, whose `Commit` of last-known-good or
  `InstallDefaultProgram` the worker performs as before. The installer publishes `plan_active_revision`: the revision
  of the artifact in effect as the status summary reports it (`status.active_artifact`), `0` while none is, so that
  the metric and the status sent to the stub agree, also while an artifact is served by a generation of another origin
  (an unmodified export, which creates no generation). Generation failures are logged as failed under supervision,
  not as not installed.

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

- **`programmable-metrics-demo`** (built; `bin/programmable-metrics-demo`, one Tokio process with axum, nothing depends
  on it): `programmable-metrics-demo run --adp-bin <path> [--run-dir <dir>] [--no-launch]`, every port a flag. Each
  component is a module with a small interface. The **stub** (`stub/`) serves `POST /v1/artifacts/poll`: a complete
  `policy` snapshot of at most one config (`demo`, version = revision), whose `payload` is the envelope as a JSON string
  with its SHA-256; the store is durable in the run directory (`store.json`, written to a temporary file, flushed, and
  renamed over the old one, so a failed write changes nothing; `history.jsonl`, a best-effort log whose highest revision
  also seeds the counter), the next revision is `max(local, ADP's high-water) + 1` (high-water from the latest poll and
  the privileged status route, for the preview in the UI as for the publish), revoke empties the snapshot (revoking an
  empty one changes nothing), and every poll is recorded, consecutive identical polls folding into one acknowledgment
  timeline entry. Only `protocol_version` decides whether a poll is answered: every other field is read on its own, and
  one ADP gets wrong is noted in the timeline rather than refusing the poll, since a refusal keeps ADP on its current
  feed; unreadable bodies are recorded too. `stub/protocol.rs` documents the messages; ADP's `channel_stub.rs` is the
  other side. The **control surface client** (`adp_api.rs`, `control.rs`) uses `build_ipc_client_ipc_tls_config` over
  the `saluki-io` HTTP client, polls `/programmable-metrics/status` every second (read field by field like a poll, and
  kept with its receive time, so the UI shows a status that stopped arriving as last known), fetches the export, and the
  default program with it, when the default program hash the status reports changes or every 30 s, and the active plan
  when the active plan hash the status reports changes; a `404`, which is what ADP answers until stage 5, is reported as
  ADP not exposing the programmable-metrics control surface yet. The export reader ignores catalog members the plan
  crate's `MemoryCatalog` does not know, and takes output labels from `labels` or an output's `label`. **Compile**
  parses with the syntax crate and receives the document as ADP does (`compose::receive_artifact`: the artifact limits,
  then `ReceivedArtifact`, the hash approval binds to; a document refused on receipt is reported and can still be
  published). With ADP's export and default program in hand (`preview.rs`), it then does what ADP's installer does when
  the artifact arrives: `compose::compose` into the default program ADP serves (`GET
  /programmable-metrics/default-program`, kept only while its base is the export's) with the export's catalog, as
  revision `max(local, ADP's high-water) + 1` (the revision a publish gets) and approved against the active plan hash;
  validation of the composed plan (the report `compose` returns); and `install::diff_with_correspondence` against the
  active plan (`GET /programmable-metrics/plan`, fetched when the status reports another active plan hash, and again at
  compile time if it is stale), or against the default program when the active plan is not available. A composition
  error is one finding at the node, output, or scope it names (an emit to an output the catalog does not declare also
  lists the declared ones) and no diff; validation findings are placed through the correspondence (plan node to artifact
  node to `SpanMap::node`; `valid_parameters` at the parameter, `SpanMap::param`; an edge finding at the artifact edge
  entering the same port; an output finding at its `emit`), and findings outside the artifact are prefixed with their
  location; a stale base is a warning at the base line. The compile outcome carries the preview: what it was diffed
  against (kind, plan hash, generation, artifact revision), the composed plan's hash (the active plan hash ADP reports
  once it installs the artifact, which the headless check asserts), validity, `unchanged`, stale base, and every changed
  node (plan ID, artifact node, operator, added, removed, or changed with the aspects that differ, its provenance, its
  source range), edge, and output. With an export but no default program (an ADP before this route), the structural
  checks (`checks.rs`) run instead, and the outcome says so. Findings never block publishing, so a guardrail can be sent
  past the UI to show ADP's rejection. The editor shows programs printed 64 columns wide without output fingerprints; a
  source that lists none gets the export's fingerprint of each output it emits to when it is compiled. **Publish** takes
  the hash the user approved and refuses anything but the last compiled artifact's, and uses the approval up, so a
  repeated or concurrent approval publishes one revision; `approved_against` is ADP's active plan hash at compile time.
  **Recording intakes** (one listener per recipient) decode v1, v2, sketch, and v3 payloads with `stele` after
  `tower-http` decompression (on a blocking thread, so a decoder panic is a refused request), record name, tags, kind,
  points, API key (header, or the `api_key` query parameter of the forwarder's validation), path, and receive time in
  bounded rings, and summarize metric families per second as events (counted values, sketch samples; points for gauges),
  in bounded maps; `GET /api/v1/validate` → 200, anything else → 202, and every refusal (a body that cannot be decoded,
  an unsupported encoding, a body too large) is recorded with its reason. The **traffic generator** sends one datagram
  per tick with the whole metric set and counts the canaries it sends per second. The **canary check** compares those
  with the canaries the primary intake received by point timestamp over a recent 10-second range that every flush has
  had time to deliver, and waits while ADP is not up for the whole range, so it follows the present rather than
  accumulating since the demo started. The **telemetry scraper** reads `/metrics` every second, turns counters into
  rates averaged over 5 scrapes (`plan_*`, `component_events_*`, `component_errors_total`), and drops series 10 s after
  they were last seen, also while ADP does not answer.
- **UI** (built; one page, embedded with `include_str!`, no build step or CDN): a sticky header with the badges (ADP,
  control surface, active program, greyed as last known when ADP's status stops arriving, and marked "stale base" or
  "stale review" when the status says so), and on wide screens two panes that scroll on their own, so the editor and
  the intake columns stay on screen together (the editor's slightly wider, in a 12-pixel font, so that it shows the
  default program's longest lines, about 81 columns, whole at 1280 pixels). The
  editor (line numbers, diagnostics listed and highlighted by line, a click selects the span; Load default program,
  Compile with Ctrl+Enter, Format, Publish showing the hash and asking for approval with the revision it will get and
  warnings such as ADP never having polled, Revoke while something is published), the published revision's outcome
  from ADP's acknowledgments under the Publish button, the compile status worded as what ran ("composed into ADP's
  default program and validated as ADP will: no findings", or "1 error, so ADP will reject it"; "structural checks"
  when ADP does not serve its default program), each diagnostic tagged with its validation rule or the step that found
  it (receipt, composition, check) and kept to three lines until clicked, the **plan diff** directly above Publish
  (against the active plan, naming its generation and artifact revision: "✓ No change" for an unmodified export,
  otherwise counts and one line per added, removed, or changed node with what differs, clickable to its source line;
  nodes whose only change is provenance, the losses the artifact now authorizes, folded into one line; edges in a
  collapsed list; "✓ ADP runs it now (gen N)" once the status reports the composed plan active, and a note when the
  active plan changed since the compile; hidden, its verdict cleared, once the editor holds anything else), the approval
  panel saying what installing it changes (or that it composes to the active plan itself, acknowledged without a new
  generation), for an artifact with errors that ADP will reject it, with the first error, and, when ADP's active plan
  changed since the compile, that the diff is against the plan it ran then and that ADP will flag the review as stale,
  and the configuration bindings with their current values. Drafts are
  kept per default program hash and only for what the user types; the editor loads the default program as soon as ADP
  serves its export. The viewer: one column per intake, headed by the outputs that feed it and by any unexpected API
  key or refused request, listing metric families seen in the last 5 s, fading after 3 s, with sparklines of events
  per second over 30 s (numbered names fold into one family such as `demo.debug.*`; payloads arrive per flush, so
  sparklines average over 4 s, and rates over the seconds a family has been arriving, from 2 to 12), the
  acknowledgment timeline, per-node rates from `plan_node_events_total` (native per-component rates until the
  plan-driven topology, labelled as delivered per consumer), greyed while ADP does not answer, plan telemetry, traffic
  controls, and the canary check. Light and dark themes follow `prefers-color-scheme`; nothing scrolls sideways at
  1280 pixels. A family that begins arriving at an intake well after the page loaded is marked "new" for 8 s (green),
  next to the fading of one that stopped, so a routing change reads at a glance. ADP's long rejection messages (a
  duplicate contribution names both paths from the source) are kept to two lines in the publish outcome and three in
  the acknowledgment timeline until clicked; a run of polls' count goes under its time, and the timeline's notes say
  "stale base" and "stale review" when the status says so. Not built: the plan graph.
- **Launcher** `make demo-programmable-metrics` (built): a run directory (default
  `target/programmable-metrics-demo/run-<unix seconds>`; reuse one with `--run-dir` to keep the published artifact,
  which is S7), an IPC certificate generated with `rcgen` (written readable only by its owner and renamed into place,
  and generated anew if it does not parse), and an ADP config with `api_key: demo-primary`, `hostname: adp-demo`,
  `dd_url: http://127.0.0.1:17101`, `additional_endpoints` `{17102: [demo-team-b], 17103: [demo-team-c]}`,
  `metric_prefix_allowlist` `{17102: [demo.checkout.], 17103: [demo.catalog.]}`, MRF `{enabled, api_key: demo-mrf,
  dd_url: http://127.0.0.1:17104}` (mirroring off), stub URL `http://127.0.0.1:17100` with `poll_interval: 1s` (keys
  ADP reads since stage 5), API ports 17110/17111, `dogstatsd_port: 17125` with both Unix sockets disabled, a run-local
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
  start it. `programmable-metrics-demo check --adp-bin <path> [--run-dir <dir>] [--port-offset 10000]
  [--traffic-rate 20]` runs the same components headless (run directory
  `target/programmable-metrics-demo/check-<unix seconds>`), on the default ports moved up by the offset (UI 27080, stub
  27100, intakes 27101-27104, APIs 27110/27111, DogStatsD 27125), so it can run beside a demo on the default ports.
- **Scenarios**, each an edit of the default program as exported (`scenario/edits.rs`, which find the lines they change
  by what they say and fail naming what they looked for): S0 publish the unmodified export; S1 drop `demo.debug.*`
  (`split(name starts_with "demo.debug.")` between the scope input and the policy `tee`); S2 widen the route that feeds
  team B with `or tag("team") == "platform"` after its prefix match; S3 exclusive tag routing
  (`split(tag("team") == "checkout")` in front of the `tee`, its `true` branch emitting to team B's output only); S4
  mirroring, the failover gateway's predicate replaced by `true and name in ["demo.checkout.requests"]`; S5 guardrails
  (a second `tee` branch to the primary output, an emit to `metrics_routing_encode_99`, an `aggregate` node), each
  shown in the UI and sent past it so ADP's rejection appears; S6 revoke with an empty snapshot; S7 optional restart
  and redelivery (manual). Each scenario edits the default program, not the previous scenario's program, so every
  step's expected intake contents are the default routing with one change. **For a presenter** that means a step
  also undoes the step before it, since an artifact replaces the one in effect: at S2 the primary intake marks
  `demo.debug.*` new again (S1 undone) as team B gains the platform family, and the diff lists S1's nodes removed; at
  S3 the platform family leaves team B as the checkout families leave the primary intake; at S4 the checkout families
  come back to the primary intake beside the mirror. To narrate one change at a time, either revoke between steps (the
  default program comes back, which shows the previous step undone on its own), or keep editing the program in the
  editor instead of loading the default program again (S1 and S2 together diff as one change and five re-authorized
  nodes against S1). The **Load default program** button's hover text says so.
- **`make demo-programmable-metrics-check`** (built; `DEMO_CHECK_ARGS` for extra flags) builds ADP and the demo in
  `devel` and runs `programmable-metrics-demo check`: the demo's components and ADP, launched as `run` launches them,
  then S0-S6 driven through the demo server's API (`scenario/mod.rs`, over plain HTTP on loopback), then everything
  stopped; exit 1 on any failure, with the run directory named. The intake contents are the `demo.` families seen within
  3.5 s at each intake, and each step waits (30 s at most) for every intake to receive exactly what the scenario says,
  and for that to hold 3 s. Each step: compile (the editor must compose it: no findings and a non-empty diff, an empty
  one for S0, or for S5 an error naming the problem: `duplicate_contribution`, the undeclared output, "keeps state");
  publish; ADP's acknowledgment on the privileged status route and in its next poll of the stub (`active`, or `rejected`
  with an error naming the same problem); the active artifact revision; ADP's active plan hash equal to the plan hash
  the editor composed (so the preview is what ADP installed); a new generation, or for S0 the boot generation still
  running; for S5 the generation and artifact of S4 still running and the S4 routing still delivered; for S6 the boot
  plan hash active again, the stub's polls reporting it, and the default routing back. S0 also checks `/export` (base
  hash = the status's default program hash) and `/default-program` (hashes to the boot plan); after S1, `/plan` (hashes
  to the status's active plan hash, has `policy/drop_debug`) and `/explain` (JSON and text, describing it). Throughout,
  every sample of `/api/state` (about 4 per second) checks the rolling canary range (balanced, never missing or excess),
  that no other intake receives canaries, and that no intake refuses a request or sees an unexpected API key; at the
  end, once the rolling range covers the last change, the canaries of the whole run, from the first balanced range to
  the last, are checked through `GET /api/canary?from_s=&to_s=` (a new demo route). Both checks balance within the
  allowance for canaries counted next to a range's edges (6 at 20 datagrams per second): they catch an aggregated canary
  point lost or delivered twice (20 canaries with 1-second windows), not a few datagrams lost or doubled ahead of
  aggregation, which ADP's continuous-input tests check exactly. The report, with compile, publish-to-acknowledgment,
  and publish-to-visible timings per step, is written to `check-report.json` in the run directory and printed. A run
  takes about 70 s, 18 s of it waiting at the start for the canary check to have a whole range behind ADP's start, and
  about 10 s at the end for the rolling range to cover the last change. It runs at every stage gate from the demo
  through stage 6.

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

**Stage 3 exit evidence (stage 3E).** In ADP, over the primary topology as `run.rs` builds it (`test_topology.rs`),
with every source and the trace sampler replaced by scripted components, every metrics encoder and the client
telemetry tap by collectors, and every forwarder by one that drops, in place, through the test-only
`TopologyBlueprint::replace_{source,transform,destination,encoder,forwarder}` of `saluki-core`'s `test-util`:

- **Differential** (`topology/differential_tests.rs`): twelve configurations (the no-aggregation pipeline on and off, a
  mapper profile, a namespace prefix with both filterlists, a tag filterlist with a value allowlist, zero, one, and two
  routing groups, Multi-Region Failover, OTLP beside and without DogStatsD, and every branch with the pipeline on and
  off, including checks, the trace sampler, and autoscaling failover) deliver equal multisets of flattened points to
  every encoder input (`equivalence::compare`), and two of them match hand-written names, counts, and points.
  Aggregation runs the native core on a test clock (admitted at one fixed time, flushed once when its input ends), so
  timing cannot change what it emits; the inputs avoid two native timing effects (no gauge context repeats within a
  batch, since native filters reorder; the native passthrough batcher is set to flush when it stops).
- **Live configuration** (`installer/live_tests.rs`), on the connected configuration path (`LoadedConfiguration::run`,
  `ConfigUpdatesWorker`, `ConfigUpdate::Partial`): a filterlist change installs generation 2, every batch is processed
  whole by one generation (its buffers' headers and its filtering agree), batches switch once, and unrelated metrics
  arrive exactly once; a startup-only key changes nothing; thirty changes with batches in flight keep at most the
  overlap budget alive, wait at the budget, and end on the last configuration with nothing lost or duplicated.
- **Faults** (`installer/fault_tests.rs`): a preparation commits while a region waits on a full downstream channel
  (the old generation drains until the channel is read, and nothing is lost); a region that never reads its mailbox
  times out, is aborted everywhere, and its unread preparations stay within the overlap budget until the mailbox goes;
  a change made while the topology waits for its environment is installed once the regions run, without a timeout;
  a restarted installer worker continues from its shared state (generation IDs, configuration changes); twenty
  installs leave health registrations, readiness, resource groups, the memory bounds tree, and the metric series as
  they were, with `/ready` healthy.
- **Telemetry** (`telemetry/tests.rs`): against the legacy topology, the series removed are exactly the fused
  components' component series listed in the stage 3 ADP integration notes, every other native series keeps name,
  labels, and value (two hand-derived exceptions), and both remapping rule sets render the same but
  `no_aggregation.flush`.
- **Accounting** (`topology/tests.rs`, `telemetry/tests.rs`, added in the stage 3 review): each region's bounds are
  its kernels' native bounds plus a full buffer per exit and per register, every other component's bounds are the
  legacy ones, and the interconnects were four queues fewer, all exactly against `create_legacy_topology` (since
  stage 5 they hold as many buffers as the legacy ones; see Stage 5 exit evidence); each kernel is accounted in a
  resource group of its own under its region, where its memory shows.
- **Measurements** (`topology/measurement_tests.rs`, rerun after the stage 3 review): on one and on four workers,
  throughput, worker time per metric, allocated bytes per metric, and p99 latency are no worse than legacy within the
  run-to-run noise of legacy against itself, in every phase; see [Risks](#risks) for the numbers and what is not
  measured.

**Stage 4 exit evidence.** The plan crate's part:

- **Decisions** (`compile::decision::tests`, `runtime::frame_tests`, `runtime::tests`).
  `property_test_decision_functions_and_memos_compute_what_the_reference_walk_computes` (random decision trees over a
  small pool of random predicates and one on the kind, interned as written, so that R2 shares, R3 folds `flag` and
  `is_empty` over literals and over resolved bindings, two flag bindings with equal values stay apart, and memos meet
  each context with two kinds: the compiled function, a one-entry and a full memo, and the reference walk against a
  literal walk without R2 and R3, over metrics each followed by a sibling of the same context and another kind, every
  event twice so memos hit, and derived per-port counts against counted ports);
  `property_test_decisions_with_and_without_rewrites_and_memos_deliver_what_the_interpreter_delivers` (random plans,
  half of whose predicates are endpoint allowlists restricted to some kinds so that R4 memoizes decisions on the kind,
  through frames of region drivers with the default memo, a one-entry memo, no memo, and programs without decision-time
  rewrites: outputs and policy loss equal the interpreter's, per-port counts equal across configurations); and
  `property_test_decision_rewrites_only_reduce_capacity_loss_under_capacity_pressure` (outputs accepting at most `k`
  records: `compare_under_capacity` against the original, and exact equality). A memo keyed by the context alone fails
  all three. Deterministic tests check that the random plans exercise groups, R2, R3, and R4 (on the kind too); hand
  tests cover grouping boundaries, R2 over equal and distinct bindings, the default program's decisions and records,
  derived counts in the driver (more distinct exit sets than counted included), memo lookups, every function form,
  and the region bounds of decisions.
- **R3 and R7** (`compile::rewrite::{r3,r7}::tests`, over `compile::rewrite::random_plans`: `Rejoin` chains so that
  merges receive paths of different lengths, opaque hosts, kernels, timestamp splits, bound and literal
  `flag`/`is_empty`, random batches, and for R7 with and without a random `OperandProfile`): the rewritten plan
  delivers what the original does, every output receives the same records in the same order, every kept node loses
  what it lost, removed nodes lost nothing; under arbitrary and batch-aligned capacity bounds, delivered and refused
  points are equal and `compare_under_capacity` holds. Vacuity tests require R3 to apply to over a third of generated
  plans, in every form, bounds to refuse records in over a fifth and to cut inside a batch of a rewritten plan in over
  a twentieth, and profiles to change R7's order in over a tenth; hand-written cases cover each obligation branch.
  Mutation checks: an interpreter that ignores `merge` port order fails the R3 order and capacity properties; a
  fingerprint without the written predicate fails R7's fingerprint test.
- **R5a and R6** (`compile::rewrite::{r5a,r6}::tests`, over `compile::rewrite::harness`: random valid plans below a
  DogStatsD and an OTLP source from links (`split` filters, the `filter`-cardinality test operator `test_keep`, the
  five stateless kernels, tag projection followed by a tag filter, and `test_shed`, a map that loses metrics with more
  than `max_points` points to capacity), `tee`s, `tee`s whose ports start with equivalent filters, `split`s whose
  ports rejoin a `merge` through different chains, complementary `split`s, timestamp splits, opaque hosts, and scopes;
  traces of up to 40 metrics with up to four capacity bounds; `Case::{check_equivalent, check_capacity,
  check_structure}`). Property tests: `property_test_r5a_rewritten_plans_deliver_what_the_original_delivers`,
  `property_test_r5a_only_reduces_capacity_loss`, the same two for R6,
  `property_test_r5a_and_r6_together_deliver_what_the_original_delivers_under_capacity_too`,
  `property_test_capacity_bounds_split_each_outputs_delivery_into_accepted_and_refused`,
  `property_test_harness_plans_are_valid_compile_into_their_frame_and_run`, and
  `compile::execution_tests::property_test_compiled_programs_of_rewritten_plans_deliver_what_the_interpreter_delivers`
  (random plans compiled with every logical rewrite, their programs run by the minimal executor against the
  interpreter on the plan as written). Sampled counts check that each form applies. Mutation check: breadth-first
  delivery fails the R5a-only capacity property at its default case count.
- **The pipeline** (`compile::rewrite::tests::pipeline`):
  `property_test_the_pipeline_keeps_what_plans_around_filters_deliver_under_capacity_too` (`optimize` itself over
  `harness` plans: structure, equivalence, and the capacity rule, with R3's removals and R7's new operand orders allowed by
  `check_structure`) and
  `property_test_the_pipeline_keeps_what_plans_around_constant_and_reorderable_predicates_deliver` (`optimize` over
  `random_plans` plans: equivalence, the same order at every output, and exact delivery and refusal under arbitrary
  bounds); `random_plans_of_both_families_exercise_every_logical_rewrite_through_the_pipeline` requires every rewrite
  to apply through the pipeline (over 512 plans: R3 288, R5a 221, R6 9, R7 262 applications; 122 plans get several
  rewrites; 16 skips). The stage 3 frame property test compiles with `compile`, so it runs rewritten programs through
  region drivers too.
- **Explain** (see Explain): the stage 4 default program's and an artifact-like policy region's records, with
  hand-written checks.

ADP's part, on the stage 3 harness:

- **Gateway differential** (`topology/differential_tests.rs`, `GATEWAY_MATRIX`): zero, one, and two endpoint routing
  groups (two endpoints sharing a group with lists written differently, an endpoint with empty lists, a prefix only a
  rejected name would start with, a name a prefix covers), Multi-Region Failover mirroring everything, its allowlist,
  and nothing, autoscaling failover, and all together, with DogStatsD and OTLP input whose names the intake normalizes
  (`my-service.requests`) or rejects (`9lives.count`): every output receives the same multiset of points from both
  topologies, and, for all together, the hand-derived names and point counts.
- **Live Multi-Region Failover toggle** (`installer/live_tests.rs`), on the connected configuration path, legacy and
  plan-driven: mirroring off, on, off (`ConfigUpdate::Partial` of `multi_region_failover.failover_metrics`), OTLP
  batches throughout; on both paths every batch is routed whole by one setting, the phases are contiguous and found
  from what was routed, and the plan-driven path installs generations 2 and 3, each batch's buffers carrying the
  generation whose setting routed it. `installer/tests.rs` checks `plan_rewrites_applied` across the same toggle
  (`R3` 1, 0, 1).
- **Structure** (`topology/tests.rs`, `lowering/legacy_tests.rs`, `lowering/tests.rs`): no gateway component,
  `metrics_enrich` feeding every encoder, hand-written edge lists for Multi-Region Failover alone and every branch (five
  regions), and, for four gateway configurations, the regions, their memoized decisions, the gateways' declared bytes
  against the legacy gateways', the other bounds, and the interconnects against the legacy topology; no matrix
  configuration gets a logical rewrite.
- **Telemetry** (`telemetry/tests.rs`): the gateways keep their native metrics and lose only their component series.
- **Measurements** (`topology/measurement_tests.rs`): a gateway configuration (two routing groups and Multi-Region
  Failover mirroring an allowlist, beside the stage 3 workload) on one and four workers; numbers in Risks.
- **Selection** (`benches/selection.rs`): the gate's "selection cost near the hand-written baseline" rests on
  `selection_default_program`, the frame ADP runs; numbers in Risks.

**Stage 5 exit evidence** (as integrated onto stage 4). The plan crate's part:

- **Lifecycle** (`install::artifact_tests`): a table of 32 hand-derived lifecycle cases (each expected summary written
  by hand: acknowledgments, the policy in effect, the high-water mark, kept and active generations, degraded), covering
  idempotent redelivery and re-approval, stale revisions (below the mark, equal to a mark another artifact set),
  multi-config snapshots, damaged transport hashes and envelopes, rollback to last-known-good and then to the default
  program, revocation (of a running, a kept, and an in-flight artifact), re-composition after configuration changes
  (accepted, rejected, retried), unmodified exports served by the running generation, and acknowledgments that outlive
  a refused redelivery; and `property_test_the_high_water_mark_never_decreases_and_only_accepted_plans_run` (512 cases
  of up to 40 random steps against a model: deliveries at offsets from the mark, redeliveries of earlier configs,
  revocations, preparation outcomes, supervision failures by hash, retirements; the mark never decreases, only
  accepted plans run, a revoked or failed hash is never in effect, kept, or prepared, every non-final acknowledgment
  names a revision and hash the model accepted, and a `retired` one never names a live generation). Mutation checks of
  nine rules each fail the table, the property, or both.
- **Composition and export** (`compose::tests`, `export::tests`, `install::correspondence::tests`): the unmodified
  export of every default-program fixture (both configurations, gateways as splits and as opaque hosts) composes back
  to the default program exactly and diffs as no change; accept, reject, and composes-but-invalid tables with
  hand-written correspondence, provenance, removed lists, and diffs (removed branch, renamed node, new node, modified
  predicate, binding to literal, reordered set, blob reference, stale base, stale review, stateful node, unknown and
  unrecorded output, fingerprint mismatch, reserved ID outside the scope); the receipt table (oversized, inlined size,
  too many nodes, leaves, leaves inside a blob, oversized blob, resolved values in a blob, IR version 2, at-limit
  documents accepted); `property_test_valid_random_artifacts_compose_into_valid_plans` and
  `property_test_edited_artifacts_compose_or_are_refused_cleanly`; the export fixture and its program hash computed
  independently, base invariance and sensitivity, and bundles read ignoring unknown members.
- **Channel** (`channel::*::tests`): wire forms member for member against the protocol's JSON, transport hashes, the
  refusals of the envelope parser, and the feed's wake-ups, including a cancelled wait that loses no snapshot.

ADP's part, on the stage 3 harness:

- **Artifacts in a running topology** (`installer/artifact_tests.rs`, timestamped DogStatsD batches through the
  no-aggregation pipeline; every test ends by checking that every batch arrived exactly once and whole, processed by one
  generation): with the double of the core's artifact paths, install and revocation, rollback to last-known-good with
  the high-water mark unchanged and a later stale revision rejected, the chain artifact, last-known-good artifact,
  default program, a configuration change the artifact cannot follow (degraded in the status and through the
  channel, once) resolved by a revocation or by reverting the change, an unmodified export in effect without a
  generation of its own (and `plan_active_revision` 1), the status read by the channel before the worker runs, worker
  restarts that hand the core only an unfinished snapshot again, the transport going away, panic budgets per region,
  and `continuous_input_across_installs_a_rollback_and_a_revocation_arrives_exactly_once_in_whole_batches` (32 batches
  per millisecond without waiting; commits and the rollback land mid-stream; generations `[1, 2, 1, 3, 4]`). With the
  core's own paths, `an_exported_artifact_installs_follows_configuration_and_rolls_back_through_the_real_core`: the
  export served by the status, the unmodified export acknowledged `active` with generation 1, an edited export (a split
  in front of the main output) installed as generation 2, a `metric_filterlist` change composed again under the same
  revision as generation 3 with the new binding and nothing degraded, and a supervision failure rolled back to the
  generation kept, whose unmodified export is composed again as generation 4; every batch arrives exactly once, with
  what each generation's policy keeps.
- **Exit-gate suites through the core's own paths** (`installer/lifecycle_tests.rs`). One endpoint routing group, so the
  policy region delivers to the main output and the group's; every batch carries one timestamped counter per family
  (`kept`, `orders.total`, `app.requests`, `debug.noise`) tagged with its sequence number, a context of its own that the
  no-aggregation pipeline passes without aggregating; every artifact is an edit of the export the status serves, in a
  real envelope with real hashes; after stopping, each batch is checked at the main output, the routing group's, and the
  client telemetry tap against the policy of the generation that processed it (each included point exactly once, with
  that generation; no excluded point). Tests run on a four-worker runtime, so admission, preparation, and commits run in
  parallel.
  - `continuous_input_across_a_series_of_real_artifacts_delivers_every_point_the_policy_includes_exactly_once`: 32
    batches per millisecond without waiting, across the unmodified export (acknowledged `active` with generation 1,
    no generation), a route change (the routing filter's predicate edited: generation 2), a family drop on top of it
    (generation 3, the route change kept), a revocation (the default program as generation 4; the revoked config reads
    `retired` once generation 3 retires; the mark stays), the route change re-approved under a higher revision
    (delivered anew after the revocation: generation 5, same structure as generation 2 but for provenance, the
    default program kept), and the same document under a higher revision again (acknowledged under the new revision,
    no generation, mark moves). Generations `[1, 2, 3, 4, 5]` in order, about 100 to 200 batches each.
  - Recovery: `after_a_restart_the_default_program_runs_until_the_channel_delivers_again_and_then_the_artifact` (a
    second topology with new installer state reports nothing seen, runs the default program until the snapshot is
    delivered again, then installs the artifact under the same revision with the same plan hash as the first);
    `a_restarted_installer_worker_keeps_the_high_water_mark_and_the_acknowledgments` (after an aborted worker run,
    another artifact under the mark's revision and one below are stale, then a newer revision installs).
  - `stale_revisions_are_rejected_and_a_redelivery_or_a_reapproval_acknowledges_the_current_state`: another artifact at
    or below the mark is `rejected` with the stale message and the delivery's revision and hash; the artifact in
    effect at the revision that set the mark acknowledges `active` with its generation; under a newer revision it is
    acknowledged under that revision and moves the mark; another artifact at a mark it did not set is stale, and so is
    the artifact in effect below the mark; nothing is prepared, nothing degraded, and the transport sees the status.
  - `an_artifact_on_a_stale_base_or_review_runs_and_is_reported_stale`: a base program hash other than the export's
    installs with `stale_base`; an approval against the running plan is fresh; one against another plan installs with
    `stale_review`.
  - `deliveries_the_core_refuses_are_acknowledged_with_the_reason_and_leave_what_runs` (table, run while the default
    program runs, as at boot, and again while an artifact runs): an output fingerprint other than the host's
    (`RemovedOutput`, declared), an output the host does not declare (`RemovedOutput`), an output the base does not
    record (`UnrecordedOutput`), a stateful `aggregate` node (`Stateful`), a payload damaged in transit
    (transport hash), an envelope hash that is not the artifact's, and a snapshot of two configs (both rejected); each
    with its exact message, its revision only once the envelope is read, the mark moved only for authentic configs,
    and what runs unchanged.
  - `the_stub_worker_installs_what_the_stub_serves_and_keeps_it_through_transport_failures`: the real stub worker
    polling an in-process stub every 20 ms; unanswered polls leave the default program; a served artifact installs and
    the next polls carry its acknowledgment; a 500, a body that is not JSON, an answer without `products`, and another
    protocol version keep the artifact (feed kept, polling continues, acknowledgment still reported); the same snapshot
    changes nothing; an empty one revokes.
  - Faults: `an_artifact_that_panics_falls_back_to_last_known_good_and_then_to_the_default_program_which_fails_fast`
    (artifacts with a `test_panic` node in front of the main output, in `metrics_enrich`): three panics of the second
    artifact are survived (each loses its batch at the policy region's outputs, which the tap, fed by
    `dsd_post_agg_filter_region`, still receives); the fourth fails it ("it exceeded its panic budget, with 4 panics,
    the last in node `policy/faulty`", acknowledged `failed`) and rolls back to the first; the first fails the same way
    and, with nothing kept, the default program is lowered and installed; that default program's first panic fails
    the region and stops the topology.
  - `a_hundred_installs_leave_health_accounting_metric_series_and_acknowledgments_bounded`: after two warm-up installs,
    100 more, alternating two artifacts (each a family drop and a kernel of its own), each in a config of its own:
    health registrations, resource groups, the memory bounds tree, and metric series equal the warm-up's; at every
    install at most 18 acknowledgments (latest, kept, 16 retired) and 18 generation records (2 alive, 16 history), and
    exactly 18 of each at the end, with the states by config hand-derived; `plan_live_generations` 2.
- **Export** (`lowering/export_tests.rs`): every configuration of the lowering matrix exports, prints and parses back
  to the same document, composes back with ADP's catalog to the same plan with no findings and an empty diff, and has
  a base program hash equal to the digest of its document without it.
- **Burst slack** (`topology/burst_tests.rs`, `topology/tests.rs`, on a paused clock, so a test waits for a topology
  to settle). Five bursts of 1,000 one-metric batches through both topologies, with the component after the stretch
  reading nothing until the burst has settled: into a gated aggregation with defaults and through the mapper-profile
  configuration's three pipelined regions (644 batches each: four transforms of 129, then aggregation's 128), and from
  a scripted aggregation to the main output (386), to a routing group (515), and to Multi-Region Failover (515). Both
  topologies take exactly the hand-derived count before the feeding component waits, and deliver every batch exactly
  once when the stalled component reads; without the region capacities the plan-driven topology took 257 into
  aggregation. `each_region_takes_what_the_native_transforms_it_replaces_held_on_the_way_through_it` pins every
  region's capacity in nine configurations by hand (and 128 for every other component), and the two accounting tests
  pin the interconnect bounds against the legacy topology (equal; one gateway's fewer per extra gateway). In
  `saluki-core`, `topology::blueprint::tests` cover the API: the capacities in the snapshot and the bounds, kept across
  interconnect-capacity changes and replacement, refused for other component types, and a running topology whose
  source sends exactly what the inputs and the transform's in-flight buffer hold before it waits.
- **Stub and control surface** (`channel_stub/tests.rs`, `api/tests.rs`): against an in-process fake stub, the poll
  body, acknowledgments carried on the next poll, snapshots published once, revocations, eight transport-failure
  shapes and the poll timeout (feed kept, polling continues), the answer limit, poll spacing, a stub under a path, and
  shutdown during a hung poll; every route and error path over a booted installer's status (the published export
  included), and the routes over the real privileged API with mutual TLS.
- **Configuration** (`agent-data-plane-config-system`): the keys round-trip through the inventory, are off by default,
  an empty URL leaves the stub off, and an unreadable or zero interval fails the load.

End to end, against the built binaries (`make demo-programmable-metrics-check`, macOS, `devel` builds, DogStatsD over
UDP on loopback): S0 to S6 pass in five runs, three of them in a row (each about 69 s: ADP ready in 2 s, a balanced
canary range after 18 s). Publish to acknowledgment 0.8-2.1 s (one poll interval plus preparation); publish to the
endpoints showing an addition 1.1-2.3 s, a removal 3.4-4.7 s (a family counts as gone 3.5 s after its last point); S0
acknowledged `active` in the boot generation with an empty diff; S1-S4 each a new generation whose active plan hash is
the editor's composed plan hash; the three guardrails rejected by ADP with errors naming `duplicate_contribution`,
`metrics_routing_encode_99`, and `keeps state`, the S4 generation kept; S6 back to the boot plan in a new generation.
Canary conservation held in every one of about 187 rolling 10-second ranges per run and over the whole run (1,199/1,199,
1,219/1,220, 1,200/1,200, 1,219/1,218, 1,239/1,242 sent/received; the differences are canaries counted in the second
next to a range edge, within the allowance of 6, while a lost or doubled aggregated point would be 20). A mutant S2 edit
(`tag("team") == "nobody"`) fails the check at S2 naming `team-b is not receiving demo.platform.queue_depth`. With every
stage 5 part merged (lifecycle tests and burst slack included), it passed twice more in a row (67.8 s and 67.3 s;
publish to acknowledgment 0.8-2.1 s; canaries 1,199/1,199 and 1,200/1,199 over the whole run, 184 and 187 rolling ranges
balanced). After the stage 5 review fixes it passed three more times in a row (70.9 s, 73.0 s, and 70.2 s; publish to
acknowledgment 0.8-2.1 s; S5b rejected with `UndeclaredOutput`, which lists the outputs ADP declares, S5c with a message
that names no implementation stage; canaries 1,259/1,259, 1,299/1,299, and 1,239/1,239 over the whole run, 193, 203,
and 193 rolling ranges balanced), and the full nextest run of the twelve crates involved passed whole under default
parallelism (3,257 tests), without the platform-certificate failures of the runs before.

Not covered by a test: with the real core, a re-composition the core rejects (no live configuration change was found
that makes a stage 5 re-composition invalid without also changing the frame); the ADP double that stands in for it
reports the core's own refusal (`Rejection::Refused` with the composition error and the core's wording) since the stage
5 review, not a stale revision the core never reports on that path. A native kernel's panic beside artifact nodes is
covered since the review (Supervision and fallback).

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
6. **Correspondence**: artifact nodes that keep a reserved ID correspond to it; provenance changes only with the node,
   or, for a lossy node or a fence, with what feeds it (stage 5): native configuration authorizes only the loss it
   expresses, on the records it gives the node.
7. **Admission**: points are per component output; opaque hosts readmit; the generation is stamped at the point from a
   shared schedule.
8. **Progress**: starved branches carry header-only progress to state owners, not only idle sources (stage 6).
9. **Host-tags deadline** is process start plus `expected_tags_duration`, compared with `t_a` (native: transform build
   time, monotonic, at processing). Intentional change, in the plan-driven topology only. Lowering rounds it up to a
   whole second (and caps it at 2^53 − 1 seconds), so that the deadline is always a plan duration; rounding up keeps
   host tags at least as long as configured. The native deadline starts when the host tags transform is built, after
   connected startup has waited for the Datadog Agent to accept the registration and send the initial configuration;
   both waits are unbounded, and they now count against the duration, which they can use up (stage 3 review): with
   `expected_tags_duration: 1m` and an Agent that answers after 90 seconds, native adds host tags for a minute after
   the topology builds, and the plan-driven topology never adds them.
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
    Stage 3 runs a default program whose report has errors whenever it is structurally sound and fits the frame: the
    installer core compiles a default-program candidate without the validity check (`compile_unvalidated`), and ADP
    reports it degraded (logs, `PlanStatus::degraded`). The configurations recorded above are not structurally sound:
    an operator cannot be configured with an out-of-range parameter, so no frame can be computed, and the boot fails
    with the validation finding, naming the node and parameter. That is an intentional change for these values (beyond
    9×10^15 contexts or cache entries), which natively run; `aggregate_flush_interval: 0s` stops the process natively
    anyway. A configuration change whose program cannot be configured is rejected, and the active generation keeps
    running.
15. **Gateways**: MRF and autoscaling branches exist only when native wiring builds them; the MRF predicate keeps the
    static `multi_region_failover.enabled` as a startup-bound conjunct; the autoscaling output accepts only
    `{Counter, Rate, Gauge, Set}`.
16. **Kernels may own native cache expiration tasks** (unobservable) and are built inside a Tokio context; a host-driven
    expiration mode is a later refinement.
17. **No configuration version exists**; the installer numbers observed configuration changes per process.
18. **Predicates** add `const`, `flag`, `is_empty`, and `matches` (with `in`/`prefix_in` sugar).
19. **Accounting** for fused kernels nests under the slot (`topology.primary.transforms.<slot>.<node>`); policy nodes
    share one group per slot. Only kernels have groups: decisions (`split`, `tee`) run in the region's own, so the
    native groups of filters that lower to decisions (the listener filterlist and post-aggregation filter) are gone.
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
30. **Trace events before their stages**: `capacity` was such an event until stage 4 implemented it; `cancel` and a
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
    opaque host until stage 6, still emits it once per listing. From stage 3, `Lowering::new` runs before the topology
    is assembled (the frame needs the default program), so the metrics path's errors are reported before those of the
    pipelines outside it, which native assembly checked first when it met them first (a malformed checks IPC endpoint,
    a debug log path that is not UTF-8); and host tags are fetched before assembly rather than when the topology is
    built. With one error, the error is the native one.
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
43. **Fusion stops where it would form a cycle through a fence.** R1 fuses synchronous neighbors "no fence separates";
    a maximal connected set of synchronous nodes can still reach itself through a fence, as the default program's
    pre-aggregation chain reaches the post-aggregation merge both directly (the passthrough) and through aggregation.
    One region would then be a cycle with the aggregation host. A slot is therefore a connected set of synchronous
    nodes of equal fence depth (the most fences on a path from the set to the node), and `explain` reports the edge
    as `separated` through the fence.
44. **A superseded generation is `draining` until it retires.** The lifecycle reports `rejected`, `pending`,
    `prepared`, `active`, and `retired`; between being replaced and retiring, a generation still processes its stamped
    batches. Status reports that state as `draining` (and generations alive past the drain deadline), and a generation
    kept as last-known-good goes back to `prepared`. Mapping the states onto acknowledgments is a stage 5 deliverable.
45. **Configuration changes queue behind a preparation instead of cancelling it.** "A newer proposal cancels a
    preparation that has not committed" applies to artifact revisions. A newer default-program candidate waits until
    the preparation in flight commits or fails, replacing any candidate already waiting, so a steady stream of
    configuration changes cannot keep every preparation from committing; the latest configuration is always applied
    next. Since the stage 5 review this also holds for the artifact in effect composed again after a configuration
    change (amendment 56): it is a configuration change, not a newer approval, so it queues too; only a newly accepted
    revision cancels.
46. **Last-known-good is kept only below an artifact generation.** It is "the plan that was active before the current
    one", which matters only when the current one can fail and roll back: an artifact generation. A default-program
    generation's panic budget is zero, so committing one releases any kept generation instead of holding its programs
    and kernels in every region and a place in the overlap budget.
47. **R1 pipelines by cost** (from stage 3E's measurements; resolved in the stage 3 review). R1 fuses the synchronous
    neighbors that no fence separates, which made the five pre-aggregation transforms one region and removed the
    pipeline parallelism their five tasks had: one saturated DogStatsD stream ran at 0.68 to 0.72 of the legacy
    throughput on four workers, below the gate that throughput and p99 latency be no worse than legacy at every gate. R1
    now also cuts where one region would be slower than the native chain: by the cost model in [Physical frame and
    region programs](#physical-frame-and-region-programs), a chain runs as one region while its estimated cost stays
    within that of the costliest native component (or 64 ns per event), and pipelines over several regions otherwise,
    never cutting a native component. With that, the gate holds as written in the stage 3 measurements (see Risks), so
    neither restating it per unit of CPU nor bringing stage 9's partitioned parallel regions forward is needed. A
    region's slot is therefore a connected set of synchronous nodes of equal fence depth and equal pipeline stage, and
    `explain` reports a cut as `pipelined`, with the budget. Stage 4 refined the cost model for decisions: R1 never
    cuts a decision (its `split`s and `tee`s run as one function), prices a decision as its function runs (derived
    fields once, a memoized decision as one lookup, by decision compilation's own `MEMO_COST_FLOOR`), joins a component
    with those its decisions reach into, and compares a unit with the regions it joins rather than with its whole
    stage, so parallel branches are not summed. The specification's R4 cost ("a cache hit costs one lookup plus buffer
    pushes, independent of the number of policies") then holds for the default program's policy region, which runs in
    `metrics_enrich`. With DogStatsD defaults, one routing group or more leaves the post-aggregation filter in a
    region of its own (an extra hop and queue against the stage 3 frame, where the routing filter was an opaque host).
48. **Fusion removes queues** (stage 3 review). Each interconnect between two fused components is gone, so the
    plan-driven topology buffers less between DogStatsD and aggregation (two queues instead of five with a default
    configuration) and after it (two instead of three). The specification's capacity rule ("a rewrite may only reduce
    capacity loss") speaks of loss inside the plan; this moves loss outside it, to the source socket, sooner under a
    burst. The proposal is to accept it as R1's cost, to measure burst loss against legacy before stage 3's gate is
    final, and, if it is worse, to give a region's input interconnect the capacity of the queues it replaces.

    **Resolved in stage 5.** Measured with a stalled component (see Stage 5 exit evidence), fusion made a burst back
    up to the DogStatsD source after 257 batches where legacy took 644. The proposal's last step is built: each
    region's input gets the capacity of the queues it replaces and of their in-flight buffers, and both topologies
    now take exactly as many batches. No amendment is needed.
49. **The capacity rule is checked against one interleaving** (stage 4). "Never increase capacity loss for any
    policy-included contribution" depends on which records an output accepts first, and the specification leaves how
    a `merge` interleaves unspecified. The reference interpreter fixes one delivery order, in which a `merge` passes its
    inputs on in port order and an output's order follows from the dataflow alone (see Reference interpreter), so
    inserting, moving, or removing a node along a path keeps every output's order, and the rewrite property tests check
    the per-contribution rule against it. The proposal is to say so in the Equivalence section: "the rule compares the
    two executions under the reference interpreter's delivery order, in which a `merge` passes its inputs on in port
    order".
50. **Native filters are moved only whole** (stage 4). The specification lets rewrites change internal telemetry if
    `explain` lists it; native series that hosts report under a native node's ID would instead stop (copies carry
    other IDs). R5a and R6 therefore never copy, restrict, or hoist a filter with native provenance; R5a may still
    move one whole, which keeps its ID and counts. Stage 6's R5b partial push-down of the post-aggregation filter
    (the specification's own example) needs its native counter to sum the copy's and the residual's ports; until a
    telemetry profile can do that, this restriction stands.
51. **Decisions group only `split`s and `tee`s joined directly by edges** (stage 4). The specification's decision is "a
    maximal connected group of `split` and `tee` operators within one region, with no intervening map that writes a
    field one of its predicates reads", which allows a group to span a map that writes nothing its predicates read.
    Programs cut every group at every map: the events below a map are the map's output, which an exit set computed on
    its input cannot route when the map drops, adds, or changes events, and carrying exit sets through a map would need
    a correspondence between its input and output events that kernels do not expose. The function's semantics are
    unchanged; only its form is smaller where such a map sits between two `split`s. Proposed wording: "a maximal group
    of `split` and `tee` operators joined directly by edges within one region".
52. **R7 writes the order it chooses into the plan** (stage 4). "Rewrites never change routing, lossy operators,
    operator parameters, or delivery", but R7 reorders the operands of a predicate parameter, and does so for native
    nodes too (their telemetry is unaffected; lowering writes every junction in R7's static order, so it never applies
    to the default program, which a test over the lowering matrix checks). The canonical form, every result, and so R2
    and the duplicate analysis are unchanged; the candidate, its hash, and its diff are what was proposed, since
    rewrites run in compilation; and `explain` lists each reordering. The proposal is to say "operator parameters, other
    than the order of a pure `all` or `any`'s operands (R7)".
53. **Idempotency compares with the policy in effect, and only for redeliveries** (stage 5). "Installing an artifact
    whose hash matches the active or preparing one acknowledges the current state" is read as: matches the artifact the
    schedule will hold, which is the one being prepared when there is one, and the delivery is a redelivery (its
    revision is the high-water mark, set by the same artifact) or a newer approval (its revision is above the mark). A
    redelivery of the active artifact while a newer one is prepared, of a revoked one, or under a revision below the
    mark or equal to one seen for another artifact, is stale, since it is not what the installer is installing or not
    newer than what it has seen.
54. **A revision counts as seen once its artifact is authentic** (stage 5). The high-water mark moves for a config that
    passes its transport hash, envelope, receipt, and artifact hash checks, even if composition or validation then
    rejects it, and does not move for one that fails them: an unauthenticated revision could otherwise block every
    later proposal.
55. **An acknowledgment whose generation stops serving it stays as it was until the generation retires** (stage 5; see
    44): acknowledgments have no draining state, and the generation still processes its stamped batches. A superseded
    artifact stays `active`, a released last-known-good stays `prepared`, and an unmodified export whose generation the
    default program takes over after a revocation stays `active`; each is `retired` once its generation retires, or
    `failed` if supervision fails it while it is active or kept (a draining one that fails drops its remaining batches,
    counted; nothing rolls back).
56. **Configuration changes reach an artifact by re-composition under its revision** (stage 5). The re-composition
    replaces the artifact's earlier composition without keeping it as last-known-good (the plan before the artifact
    stays kept), is retried after a failed preparation as the default program is, and a rollback whose plan predates
    the latest configuration change is composed again, so live configuration is applied eventually whichever plan
    runs.
57. **An artifact equal to the running plan creates no generation** (stage 5). An unmodified export composes to the
    default program; the generation already holding that plan serves it, with its own panic budget (zero: every node in
    it is native). This holds also when the artifact cancels a preparation: it is compared with the active plan then.
58. **Revocation is final for every artifact accepted before it** (stage 5). A revoked artifact is never in effect,
    composed, or kept as last-known-good again; its generation runs until the default program (or a newer artifact)
    replaces it. Only a new delivery under a newer revision brings the document back.
59. **A supervision failure is the artifact document's** (stage 5). Last-known-good and failure handling compare
    artifacts by hash, not by revision: a newer approval of the document that failed, already in flight, is dropped
    with it, and a generation of the same document is never kept to roll back to beneath another of it.
60. **Per-config outcomes survive a refused redelivery** (stage 5). A config's acknowledgment follows the installation
    an earlier delivery of it started while that installation runs and the latest delivery's does not, so a failure,
    rollback, or retirement of the running artifact is acknowledged even after a stale, damaged, rejected, or
    multi-config delivery replaced the config's acknowledgment.
61. **A stale base is relative to the policy region and its outputs** (stage 5). The specification reports an artifact
    stale when its base "no longer matches the current default program." The base program hash covers the exported
    policy region and the fingerprints of the outputs it emits to, so a structural change elsewhere in the default
    program (upstream of `metrics_enrich`, which native configuration defines through stage 5) does not make an
    artifact stale: the artifact is composed into the new default program as it is, and rebasing it on the new export
    would change nothing. A structural change inside the region, or another binding of an output it emits to, does (an
    artifact that still emits to a rebound output is also invalid: a removed output). When the region extends upstream
    (stage 6), the base covers the extended region.

## Risks

- Output fingerprints catch routing-group renumbering after restarts, but an artifact then becomes invalid rather than
  following the endpoints. That is deliberate: the approver never chose the new endpoints.
- **R1's cost estimates are coarse** (stage 3 review, amendment 47). Whether a chain fuses or pipelines follows fixed
  per-operator estimates, not measurements of the configuration at hand: a chain costlier than its estimates fuses
  where pipelining would pay (one saturated stream then gets less than legacy), and one cheaper than them pipelines
  where one region would do (an extra hop and queue). Before the cost model, fusing the whole pre-aggregation chain ran
  one saturated stream at 0.68 to 0.72 of legacy on four workers. The measurements below cover two configurations (a
  mapper profile: two pre-aggregation regions; and that with two routing groups and Multi-Region Failover, whose policy
  region runs in `metrics_enrich`); SMP runs on Linux should check others before the estimates are trusted. Since stage
  4 the estimates also decide where the post-aggregation filter runs: with DogStatsD defaults and one routing group it
  no longer fits beside the policy region (66 of 64 ns) and gets a region, a hop, and a queue of its own, where stage
  3's frame (an opaque routing filter) kept it in `metrics_enrich`.
- **Passthrough per batch** (amendment 12) sends one buffer per input batch where the native batcher sent one per 1,024
  timestamped metrics. Since the stage 3 review the timestamp split moves a buffer whose metrics all go one way whole,
  prepares passthrough metrics in place, and moves (rather than copies) what a split separates, and the plan-driven
  topology uses less worker time per metric than legacy in every phase, passthrough-only batches included. The
  encoders are replaced by collectors, so the encoder-side cost of more, smaller buffers is not measured.
- The per-admission-point mutex serializes the DogStatsD decoder pool's dispatches. Dispatch rate is low (full or
  100 ms buffers). Stage 3E's measurement dispatches through the `dsd_in.metrics` admission point from one scripted
  source, not from the decoder pool, so contention between decoders is still unmeasured.
- Per-kernel resource groups (restored in the stage 3 review) cost two thread CPU-time reads per kernel per buffer on
  Linux, about 1.7 µs, as `ChainedConfiguration` pays natively. The measurements ran on macOS, with the tracking
  allocator and so with that entry, but at the cost on macOS; Linux numbers are open.
- **Buffering** (amendment 48, resolved in stage 5). Region inputs take what the transforms they replace held (see
  ADP integration, Buffering), so a burst backs up to the DogStatsD source after exactly as many batches as natively,
  and the interconnects' firm bounds do not grow. Still open: loss at a real UDP socket under a burst (the tests stall
  a component instead of measuring the socket), p99 under slow sinks, and the sizing for a region's longest path, by
  which paths that leave a region early, such as the passthrough path, hold up to one region's buffers more than
  natively. A region's capacity follows the boot program, so an artifact that adds work to the policy region runs
  within it.
- **Measurements** (after the stage 3 review, release mode, Apple Silicon, local). Criterion benches, each against a
  hand-written baseline that calls the same kernels with no program, registers, or telemetry:

  | Bench | Driver | Baseline |
  |---|---|---|
  | `region_driver` pre-aggregation, three regions, 1,024 metrics, 256 contexts | 224.9 µs | 222.1 µs |
  | the same, 4,096 contexts | 225.4 µs | 221.7 µs |
  | the same, 128 metrics | 28.4 µs | 27.7 µs |
  | `region_driver` post-aggregation, two regions, five-way fan-out | 173.2 µs | 184.8 µs |
  | `region_host` dispatch, 256 one-event buffers | 81.1 µs | 63.0 µs (plain transform) |
  | `region_host` dispatch, 256 full buffers | 3.89 ms | 3.73 ms (plain transform) |

  The host's own work is about 70 ns per buffer (its select, the supervised call, and dispatch by exit). The
  in-process comparison is `topology/measurement_tests.rs` (ignored; five alternating rounds, medians; the
  `region_driver` workload with a mapper profile, one timestamped sentinel per batch, native aggregation, the
  connected configuration path, and the production runtime layout, in which the topology and the installer share one
  runtime). Legacy against itself stays within 1% of throughput, 4% of worker time, and 1% of p99 (four workers),
  which the tolerances (7%, 7%, and 10%) cover:

  | Workers | Phase | Throughput | Worker time per metric | Allocated bytes per metric | p99, plan / legacy |
  |---|---|---|---|---|---|
  | 4 | saturated | 1.04 | 0.94 | 0.82 | (queueing) |
  | 4 | paced at half the legacy rate | 1.00 | 0.90 | 0.82 | 1.09 / 3.20 ms |
  | 4 | paced, a filterlist change every 50 ms | 1.00 | 0.94 | 0.83 | 1.21 / 3.21 ms |
  | 4 | paced, passthrough only | 1.00 | 0.80 | 0.83 | 0.73 / 2.22 ms |
  | 1 | saturated | 1.27 | 0.77 | 0.82 | (queueing) |
  | 1 | paced at half the legacy rate | 1.00 | 0.83 | 0.82 | 0.82 / 6.02 ms |
  | 1 | paced, a filterlist change every 50 ms | 1.00 | 0.89 | 0.84 | 1.45 / 6.03 ms |
  | 1 | paced, passthrough only | 1.00 | 0.80 | 0.83 | 0.70 / 2.75 ms |

  Ratios are plan over legacy. In the passthrough-only phase the native batcher fills a buffer per batch, as regions
  send one, so its p99 compares the paths themselves rather than the batching. Allocations per metric (reported, not
  checked) are 1.10 to 1.22 of legacy with live updates, from lowering and preparing every change, and 1.01 otherwise;
  live memory at the end of a phase is reported too. SMP throughput, CPU, memory, and RSS on Linux, queue bytes, slow
  sinks, key skew, and decoder-pool contention remain open, as the stage plan says.

  The gateway configuration (`measurement_tests::GATEWAYS`: the stage 3 workload with two routing groups and
  Multi-Region Failover mirroring an allowlist) has no idle-machine numbers yet; legacy against itself exceeded its own
  tolerances on every run. With the policy region in `metrics_enrich` (four regions), the last runs (load 4 to 30)
  gave saturated throughput 1.13 to 1.23 of legacy, worker time per metric 0.71 to 0.94 in every phase, allocated bytes
  0.83 to 0.89, and p99 0.27 to 0.76 of legacy except one loaded four-worker run's passthrough phase (1.49);
  allocations per metric with live updates are 1.16 to 1.37 of legacy (each change lowers, compiles, and prepares every
  region; reported, not gated). Those runs predate the integration's memo floor (25 ns), which leaves the
  post-aggregation filter unmemoized; rerun idle before the gate is called, and if live updates still cost more CPU and
  allocations than legacy, have regions reuse an unchanged program's translated code and telemetry
  (`SlotKernels::unchanged` already hands them the active program).
- **R1 prices a memoized decision as a memo hit** (stage 4). With more contexts than the memo holds, most lookups
  miss, and the decision costs its predicates plus a probe and a write: a region R1 sized for hits can then be slower
  than its estimate. And a decision is never cut, so a wide policy fan-out runs in one region however many copies it
  makes: with DogStatsD defaults and every gateway, `metrics_enrich` is estimated at 75 ns per event against a 64 ns
  budget (natively, enrichment made the same copies, 60 ns, and each gateway filtered in a task of its own). The
  decision copies an event only to the gateways that keep it, which the estimate (a copy per port) does not credit.
- **Memo memory.** Each memoized decision's memo is 96 KiB (4,096 entries of 24 bytes, for up to 64 exits) per
  generation per region, up to the overlap budget's four live generations; the stage 4 default program memoizes its
  policy decision in `metrics_enrich` once it has a routing group (96 KiB, or 384 KiB with four generations live).
  `RegionBounds` declares the boot program's memos, counts, decision scratch, and predicates; later generations' are
  not accounted, as their kernels are not, and a live list that grows later (Multi-Region Failover's allowlist) is not
  declared again, as the native component, which declared its startup snapshot, did not either.
- **Memo misses.** With more contexts than entries, nearly every lookup misses and overwrites; a miss adds about 4 ns
  to the evaluation (an endpoint allowlist's region: 7% to 8% more; the policy region with one routing group, as a
  function, 8%, and with enrichment, under 1%). There is no adaptive switch (a selection flag); `MEMO_COST_FLOOR` (25)
  keeps the memo off decisions too cheap to repay misses, which includes the pre-aggregation listener filterlist (one
  intake-name leaf, measured 16% to 19% slower with every lookup a miss) and the post-aggregation filter.
  Post-aggregation decisions see each context once per flush, so their memos hit only while the agent has fewer
  contexts than entries.
- **Selection cost** (stage 4 gate; `benches/selection.rs`, `selection_default_program`: the stage 4 default program
  with 1, 3, and 7 routing policies in its own frame, where the policy fan-out and every gateway are one memoized
  decision of `metrics_enrich`, beside the enrichment kernels; a batch of 1,024 events entering the region). Medians of
  61 alternating rounds of the bench's own setup (a temporary harness, not checked in; criterion runs on the shared
  machine were too noisy to compare), load 3.5 to 4, µs per batch:

  | Policies, contexts | Region, memoized | Region, evaluated | Hand-written selection | Native fan-out | Enrichment only |
  |---|---|---|---|---|---|
  | 1, 256 (hits) | 151.4 | 173.5 | 130.7 | 171.8 | 78.3 |
  | 1, 16,384 (misses) | 181.8 | 174.7 | 133.5 | 173.9 | 80.5 |
  | 3, 256 | 162.2 | 222.8 | 162.6 | 259.1 | 78.3 |
  | 3, 16,384 | 226.9 | 223.6 | 164.6 | 262.5 | 79.9 |
  | 7, 256 | 177.6 | 288.5 | 218.8 | 430.3 | 77.1 |
  | 7, 16,384 | 291.9 | 290.3 | 223.3 | 434.6 | 79.3 |

  Less the enrichment, the region's selection costs 1.40x, 1.00x, and 0.71x the hand-written selection's (enrichment,
  then every event's outputs at once and buffers of exact copies, with no telemetry or loss counts) with memo hits, and
  1.91x, 1.74x, and 1.48x with misses (unmemoized: 1.78x, 1.70x, 1.47x); against the native shape (a copy of the batch
  per gateway, each filtered in place) 0.78x, 0.46x, and 0.28x with hits and 1.08x, 0.81x, and 0.60x with misses. With
  hits the cost grows little with the number of policies (the specification's "one lookup plus buffer pushes"), so the
  gate's "near the hand-written baseline" holds from three policies and is within 21 ns per event at one; with misses
  the region pays the predicates, which `CompiledPredicate::evaluate` dominates, plus telemetry and loss accounting.
  Before the integration's R1 change (each routing filter a region of its own) the same frame cost 1.6x to 2.3x the
  hand-written selection with hits and 2.0x to 3.5x with misses. In one region (`selection_function`,
  `selection_region`, a tee to every policy in one slot, from the decisions part's harness), with hits the decision
  takes 10.1, 11.6, and 14.6 against the hand-written selection's 22.5, 48.8, and 92.0, and the region 21.1, 30.7,
  and 47.8 against the hand-written region's 26.2, 57.5, and 115.2; unmemoized, or with misses, the function is 1.1x to
  1.2x the hand-written selection and the region 1.2x to 1.4x the hand-written region.
- **Partial push-down and copies never apply to native filters** (amendment 50), so R5a and R6 change nothing in the
  default program and only optimize what artifacts add. R5b in stage 6 needs native telemetry that sums a filter's
  copies before it can push the post-aggregation filter above aggregation. R5a also declines to move a filter above a
  kernel when an R1 cut separates them (the kernel, fed from the filter's slot, would follow it there), so in pipelined
  plans a filter can stop below a cut; one in the same slot as the kernel still moves, and may then cross a cut into an
  earlier slot itself, since a decision has no resource group.
- **Two random-plan families for rewrite tests.** `compile::rewrite::harness` (filters, scopes, capacity-shedding
  operators) and `compile::rewrite::random_plans` (predicate strategies, constant and reorderable predicates) were
  written by different parts and kept both, with a pipeline property test over each; one generator with both
  families' strengths would be simpler to maintain. R6 applies to few generated plans through the whole pipeline (9 of
  512; its own tests sample more).
- `component_events_sent_total` double counts for `BufferedDispatcher` users natively; regions do not use it. Tracked
  separately.
- **Receipt limits count every node of an artifact document** (stage 5), including the nodes it keeps from the default
  program unmodified, while validation exempts native-provenance nodes. A native configuration whose policy region
  alone exceeds an artifact limit (256 nodes, 1,024 predicate leaves, 256 KiB; about 80 endpoint routing groups)
  exports, but its unmodified export is refused on receipt. Nothing in the lowering matrix comes close; if it matters,
  receipt can exempt nodes the default program's export has unchanged.
- **Panic budgets per region, not per node** (stage 5; resolved in the stage 5 review). A host-enrichment panic under
  an edited artifact was survived up to the artifact's budget, since `metrics_enrich` runs host enrichment beside the
  policy region's decision. The budget now covers the artifact's nodes only (Supervision and fallback). What remains
  charged to the artifact: a panic in a decision group that holds any artifact node, and one in the region's own steps
  between nodes in a region that runs artifact nodes.
- **Status details of re-approvals** (stage 5; the revision gap resolved in the stage 5 review). An approval of the
  artifact in effect under a newer revision, or a delivery of it in another config, now becomes the artifact in effect
  (`reapprove`): `status.active_artifact` names its config and revision, and `stale_review` its own review. What
  remains: the generation's `Origin`, and its plan's node provenance, keep the revision it was composed under until a
  re-composition (the next configuration change) composes under the new one, which makes that change a generation
  even when it changes nothing else; a config that followed the earlier acceptance and is no longer delivered keeps
  following the installation, so it stays acknowledged `active` beside the new config while the document runs. After
  a revocation, an unmodified export taken over by the default program stays acknowledged `active` until its
  generation retires, which may be the next configuration change (amendment 55).
- **A transition rejection after a cancellation** (stage 6). An artifact cancels the preparation in flight before
  `start()` checks state-owner transitions, so a re-composition of the old policy that a `Transition` rejection
  cancels leaves the reverted policy behind the configuration until the next event. Unreachable through stage 5 (no
  state owner is in a region); stage 6 should check transitions before cancelling.
- **Stub tests under load** (resolved in the stage 5 review). Tests that build an `HttpClient` (the `channel_stub`
  tests, the control surface's, the demo's, and the lifecycle suite's stub test) failed to load the platform's root
  certificates under full-workspace parallel runs. The certificates were loaded for the proxy connector's own TLS,
  which only a proxy reached over HTTPS uses: `saluki-io`'s `HttpClientBuilder::build` now builds that TLS only for a
  client with proxies, so the loopback stub and demo clients, which trust no certificate authority, need none of the
  platform's.
- **The editor's preview composes with the export's catalog** (stage 5 demo). ADP composes with `AdpCatalog`; the
  editor with the `MemoryCatalog` document the export carries (`AdpCatalog::to_document`), so the two agree only as
  long as that document carries everything composition and validation read. The headless check asserts it at every
  install (ADP's active plan hash equals the editor's composed plan hash). Compilation into the frame (capability
  errors, such as a new output) is not previewed: a plan the editor validates can still be refused by ADP's compile.
