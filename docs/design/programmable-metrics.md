# Programmable metric processing: specification

**Status:** draft; decisions are recorded in the [decision log](#decision-log). **Evidence:** the archived spike
(`experiment/programmable-metrics-spike`, commits `426f6f624d..9d4bc08924`), including its
`test/program-region/RETROSPECTIVE.md`. This specification replaces that code. Nothing from it is carried forward except
extracted kernels, re-derived under the contracts below.

This specification is normative unless a passage says otherwise. "Native" means ADP's behavior at upstream commit `b940bf1f`; source references are
approximate and relative to that commit.

## Contents

1. [Goal](#goal) and [construction rules](#construction-rules)
2. [Lessons from the spike](#lessons-from-the-spike)
3. [Semantic model](#semantic-model) and [native compatibility matrix](#native-compatibility-matrix)
4. [Extension points](#extension-points)
5. [Logical IR](#logical-ir)
6. [Compiler](#compiler)
7. [Execution](#execution)
8. [State](#state)
9. [Installation](#installation)
10. [Concurrency](#concurrency)
11. [Control plane and authority](#control-plane-and-authority)
12. [Telemetry](#telemetry)
13. [Default lowering coverage](#default-lowering-coverage)
14. [Limits](#limits)
15. [Verification](#verification)
16. [Capability stages](#capability-stages)
17. [Placement in the repository](#placement-in-the-repository)
18. [Decision log](#decision-log)

## Goal

Separate **what** happens to metrics from **how** ADP executes it.

A validated logical plan describes filtering, transformation, routing, aggregation, and delivery. A compiler applies a
closed set of proven rewrites, then places the work into fused synchronous regions, state owners, and existing
asynchronous I/O hosts. Approved plan revisions install live, with explicit ownership of state and buffered work.

Correctness comes first: metric kinds, identity, time, and delivery have one written definition, and every optimization
must preserve it. Performance comes second: fewer execution boundaries, shared work, and filtering before expensive state.

### Non-goals

- A second async runtime, aggregation implementation, or configuration/defaults system.
- General-purpose code, loops, regexes, or JIT compilation in plans.
- Untrusted plans that declare their own operator semantics, endpoints, or credentials.
- A universal cost-based optimizer. The rewrite set is closed and grows only with proofs.
- Exactly-once network delivery.

## Construction rules

These rules exist to prevent the spike's sprawl.

1. **One model.** One logical IR is authoritative. Surface syntaxes, studios, and JSON are views or serializations of it.
2. **Strict layering.** A stage builds only on completed lower stages. No feature ships with a workaround for a missing
   lower capability; build the capability or defer the feature.
3. **No second implementation of metric semantics.** Extract native behavior into kernels; never reimplement it. New
   behavior is a new, explicitly named operator or parameter.
4. **Baselines live in benchmarks.** Hand-written comparison paths never appear in production types, flags, or hosts.
5. **One entry point.** Native typed configuration lowers into the IR. An approved artifact optionally replaces the
   [policy region](#policy-region-and-artifact-composition). There are no mode flags selecting alternative engines.
6. **Observability is off the data path.** Counters aggregate per turn; serialization and reporting happen elsewhere.
7. **Every stage has an exit gate** measured against the legacy path and, where relevant, a hand-written baseline.
8. **Extension points before features.** The core reserves a slot with a defined contract and a default that reproduces
   native behavior. Richer designs for a slot come later and do not change the core.
9. **Start fenced, then open up.** The first stages keep many native behaviors behind fences and opaque hosts, such as
   aggregation. Later stages remove them one at a time. The long-term target is for as much of ADP's metric processing,
   routing, filtering, and aggregation as possible to be expressed, programmed, and optimized in the plan.

## Lessons from the spike

| Observation | Consequence in this specification |
|---|---|
| Routing in one region cost 2.9% (3 policies) and 8.4% (7 policies) less CPU than the task/channel graph; no memory win. | Fusion is worth doing but is not the main prize. Filtering before state and shared work matter more. |
| The graph-walking selector took 55 ns/input against 11 ns for fused code (3 policies). | Routing compiles to [decision functions](#decision-functions), memoized per context (R4). |
| Predicates could read only names. Tag policies became special-case nodes. | [Predicates](#predicates) cover names, host, tags, and kind. Special-case nodes are not allowed. |
| Routing ran after aggregation and enrichment. Dropped metrics still paid for both. | Filter push-down across operators is a rewrite with written proof obligations (R5). |
| Three documents (program, pipeline, deployment) shared no IR. The "planner" enforced a fixed order. | One IR with effect contracts. Placement follows from contracts. |
| A downstream 30-second reducer reimplemented aggregation, used a different timestamp convention, and on DogStatsD only received rates. | Port types carry metric kinds. Aggregation is the native state machine, extracted and parameterized. |
| Old and new generations could emit partial results for the same window and context. | A [transition protocol](#transition-protocol) with a defined cut. |
| A yielded batch could, in principle, resume under newly installed instructions. | Every batch carries its generation in a [batch header](#batches-and-segments). |
| The encoder's final flush could reach the forwarder queue and then be discarded at shutdown. | Closure behavior is part of every output's and recipient's [delivery policy](#extension-points). |
| `Live<T>` views refresh independently in each host. | Lowering resolves configuration into plan parameters. Any change re-lowers into a new generation. |
| Nested topologies needed private health registries to avoid permanent global identities. | Scoped registration and release for health, accounting, metrics, and diagnostics. |
| Native forwarding isolates recipients with bounded drop-oldest or spill queues, but that behavior is hard-coded. | Backpressure and load shedding are a programmable delivery-policy slot, defaulting to native behavior. |
| Encoder and forwarder settings were partly inherited and partly overridden, never resolved together. | [Output bindings](#output-bindings) are resolved and validated jointly. |
| The spike's 1,024-context cap would become 8,192 if copied to eight workers. | Configured limits are global budgets that hand credits to partitions. |
| The compile → preview → approve → acknowledge discipline was sound. | Kept, with an untrusted proposal author and an approval bound to the artifact hash. |

## Semantic model

### Fields

| Field | Native source |
|---|---|
| `name`, `host`, `tags`, `origin_tags` | `Context` |
| `kind` | `MetricValues` variant; `Rate` carries an `interval` |
| `points` | Values and optional timestamps |
| `metadata` | Unit, origin, and other `MetricMetadata` fields |

A **record** is one `Metric`: a context with one or more points. Contribution is counted per point.

**Derived fields** are read-only views computed from other fields. Reading a derived field counts as reading its sources,
so a write to a source invalidates it. Derived fields are computed lazily, at most once per event, and are memoizable per
context when their sources are context fields.

The first derived field is `intake_name`: `name` normalized by the intake's rules
(`bin/agent-data-plane/src/components/metric_name.rs`). It is the same for every output. A name the intake rejects
(empty, longer than 350 bytes, or without an ASCII letter) has no `intake_name`. `name` always means the raw name, which
is what is forwarded. The IR has no default view: every predicate names its field.

### Identity

Three identities are distinct.

1. **Context identity:** `(name, host, tags, origin_tags)`, as `Context` defines it. This inherits two native properties:
   identity is a 64-bit hash, so distinct contexts can collide, and duplicate tags are deduplicated. An unset host
   differs from an empty host. Tag order does not matter.
2. **Reducer key:** what a stateful operator groups by. Native aggregation's key is context identity alone; kind and
   metadata are not part of it.
3. **Backend series identity:** defined by the intake. The spec relies on one assumption: two points with the same
   context identity, kind, and timestamp may overwrite each other at the intake.

A plan whose structure could deliver one contribution twice to the same backend series is a correctness bug. Distinct
inputs that collide at the intake, such as two sources emitting the same series, are native behavior and outside this
invariant.

### Kinds

| Kind | Stored meaning | Native merge in one bucket |
|---|---|---|
| `Counter` | Sum of deltas. | Sum. |
| `Rate(interval)` | Total change over `interval`. Encoding emits `value / interval` per second; a zero interval means already per-second. | Sum if intervals are equal; otherwise the incoming value replaces the existing one. |
| `Gauge` | Latest value. | Untimestamped samples collapse to the latest at the bucket start. Points at distinct timestamps are kept. Equal timestamps take the incoming value, including a timestamped point exactly at the bucket start. |
| `Set` | Distinct members. | Union. |
| `Histogram` | Samples. | Native merge. Flush expands into derived series. |
| `Distribution` | Sketch. | Sketch merge. |

A **port type** is a set of kinds plus a timestamp form: `timestamped`, `untimestamped`, or `mixed`. Operators declare
accepted kinds and their kind mapping. The validator rejects an operator placed where it cannot receive its input.

### Time

- **Admission time** `t_a` is stamped once per batch where it enters the plan (see
  [Batches and segments](#batches-and-segments)). Every host-time decision in the plan uses `t_a`, never the time a
  kernel runs, so fused and unfused execution make the same decisions.
- Event time is the point timestamp, when present.
- **Buckets** are aligned to Unix time and half-open: `[k·w, (k+1)·w)`. Each state owner aligns with its own width.
  Untimestamped values collapse to the bucket start.
- **Bucket widths** are a divisor of 60 seconds (1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30, 60) or one of 120, 180, 300.
  One plan uses at most one width above 60 seconds, so the least common multiple of a plan's widths is
  max(60 s, largest width).

### Clocks

- Each admission point has an **admission clock**: `t_a = max(previous t_a, wall clock)`. Small backward wall-clock
  jumps are absorbed.
- A backward jump larger than the **clock-reset threshold** (see [Limits](#limits)) is a **clock reset**. Every state
  owner fed by that admission point first closes all of its open buckets, then the admission clock restarts from the
  wall clock. Without this, `t_a` would freeze for the length of the jump and put all of that time's input into one
  bucket.
- Flush ticks and other scheduling use a monotonic host timer. They decide *when* work runs, never which bucket data
  belongs to.

### Watermarks

A state owner may close a bucket only when no input with an earlier `t_a` can still arrive. Each state owner therefore
tracks a **watermark**: the minimum, over every admission point upstream of it, of the latest `t_a` it has received.

- Batches travel through FIFO queues, so once a state owner has received `t_a = x` from an admission point, nothing
  earlier from that point remains in flight.
- An idle admission point emits an in-band **heartbeat**, a batch header with no events, at least once per heartbeat
  period. Idle sources therefore cannot hold watermarks back.
- A bucket `[s, e)` closes at the first flush tick at which the watermark is at least `e`. Native aggregation flushes
  every `primary_flush_interval` (default 15 seconds, independent of width), and a flush emits every point in a closed
  bucket. A tick with empty state emits nothing.
- A batch therefore never arrives for a bucket that has already closed. Queueing delays closure instead; it never makes
  input late.

### Ordering

Ordering guarantees are only strong enough to compare output with native behavior and to leave room for concurrent
execution. Native ADP already reorders across datagrams (decoder pool) and across requests (forwarder concurrency).
Aggregated output has one point per context and bucket, so it does not depend on delivery order.

- **Through stage 8:** order is preserved within a batch and along each path, which is a single FIFO.
- **Stage 9:** order is preserved per admission context. Inputs with the same pre-mapping context go to the same worker;
  others may reorder, even if mapping later merges them into one key.
- **Sinks** declare no ordering requirement.

Floating-point sums are not assumed to be order-invariant.

### Delivery

The delivery guarantee concerns **contribution**, not records or network requests.

- Each admitted point contributes **at most once** to each pair of physical recipient and backend series, per
  generation. The validator enforces a conservative approximation (see [Duplicate analysis](#duplicate-analysis)).
- Legitimate expansion is declared, not inferred: `expand` operators (timestamp split, histogram flush) and time-driven
  synthesis (idle counter zeros) produce new records that are not duplicates.
- Each `emit` receives its own value. Operations on other paths never alter it.
- There is no implicit default destination. Every path ends in `emit` or `drop`.
- What happens when an output or recipient cannot keep up is its delivery policy. Data it sheds is capacity loss.
- Network delivery is at-least-once under retries and out of scope for this guarantee.

### Loss and authority

Every way a contribution can fail to reach an output is classified and counted in telemetry:

| Class | Examples | Authority |
|---|---|---|
| Policy | Filters, tag removal, value restriction, gauge reduction. | Native configuration, or an approved artifact. |
| Capacity | Context limit; a delivery policy shedding under overload. | Operational. A delivery policy chooses how overload is shed. |
| Invalid | An event a kernel in the plan cannot process. | Operational. |
| Shutdown | Open buckets at process stop (native default discards them). | The native shutdown policy. |
| Host | Drops inside I/O hosts outside the plan: source decode filters, encoder-side drops. | Native configuration. Reported by host telemetry. |

Native configuration authorizes the policy loss it already expresses. An approved artifact must authorize any policy loss
it adds or changes. Lowering never adds policy loss. The compiler never changes the result of a lossy operator on any
contribution it delivers, and never changes which contributions policy loss applies to.

### Equivalence

Equivalence is defined over a **trace**: a sequence of `admit(source, t_a, batch)`, `heartbeat(source, t_a)`,
`advance(clock)`, `install(...)`, `capacity(output, n)`, `signal(name, value)`, `close`, and `cancel` events.

Executions are compared by **points**, not records. Each emitted record is flattened into tuples of
`(context, kind, metadata, point)`:

| Kind | One point |
|---|---|
| `Counter`, `Rate`, `Gauge` | A timestamp and a scalar value. |
| `Set` | A timestamp and its set of members. |
| `Histogram` | A timestamp and its samples, with sample rates. |
| `Distribution` | A timestamp and its sketch. |

Two executions of a trace are **equivalent** when, for every output, the multisets of these tuples are equal. How points
are grouped into records or batches is not observable. Neither are order, payload boundaries, emission timing within a
turn or flush, loss counts, or internal telemetry. Equal outputs already imply that no policy-included contribution was
lost. Where execution may reorder inputs (stage 9), the oracle uses per-kind checks that hold under any reordering
across admission contexts (see [Verification](#verification)).

**Capacity:** equivalence is required on every trace where neither execution hits a capacity limit. Under capacity
pressure, a rewrite may only reduce capacity loss. It must never deliver a contribution the policy excludes, and never
increase capacity loss for any policy-included contribution. Capacity behavior is reported, not promised. Rewrites that
rely on this rule are on by default; `explain` flags them.

## Native compatibility matrix

Default lowering reproduces the **Native** column. The last column lists behavior available only by explicit opt-in.
Rows marked **intentional change** or **divergence** require release notes and fixtures. Where documented native behavior
differs from what the code does, the code is authoritative and the difference is tracked as a separate issue.

| Behavior | Native (default lowering) | New operator or parameter |
|---|---|---|
| Aggregation key | Context identity: the 64-bit hash alone. | None planned. |
| Kind conflict in one context | The incoming value set replaces the whole existing value set, in every bucket. | None planned; reported as a counter. |
| Metadata | First insertion wins, for the life of the context entry: until a flush leaves it empty (non-counters) or it expires (counters). **Divergence at a cut:** the new generation records fresh metadata. | None planned. |
| Rate interval mismatch | Incoming value replaces existing. | Explicit interval conversion operator. |
| Bucket assignment | By the aggregator's clock when a batch arrives. **Intentional change (stage 6):** by `t_a`, with buckets closed by [watermarks](#watermarks). A point's bucket differs from native only if it was admitted within its queueing delay of a bucket boundary. | Per-branch bucket width (stage 6). |
| Bucket width values | Any positive integer. **Intentional change (ships in stage 6):** `aggregate_window_duration_seconds` must be an allowed width; startup rejects others with an actionable error. | Same rule for artifact parameters. |
| Wall-clock jumps | Bucketing follows the wall clock, including backward. **Intentional change (stage 6):** small backward jumps are absorbed; large ones trigger a [clock reset](#clocks). | — |
| Flush ticks | Every `primary_flush_interval` (default 15 seconds). A tick with empty state does nothing. | — |
| Timestamped DogStatsD points | Parsed only when the no-aggregation pipeline is enabled; they then bypass aggregation, and counters become rates whose interval is the aggregation width. With it disabled, timestamps are discarded at parse time. They never reach aggregation in the default program. The effective typed configuration value is authoritative. | Event-time windows with lateness (stage 10 candidate). |
| Late timestamped points in aggregation | Emitted at the next flush. Not reachable in the default program. | Lateness drop, inside the event-time operator only. |
| Histogram flush | Expands into `<name>.<statistic>` series. The count statistic is a rate with its unit cleared. With `copy_to_distribution`, it also emits a distribution, optionally under a prefixed name. Aggregation therefore writes `name`, `kind`, and `metadata` for histograms only. | None planned. |
| Counter expiry and idle zeros | Idle counters emit zeros until the expiry passes. The default program uses the effective typed value (300 seconds, translated from the Agent's `dogstatsd_expiry_seconds`). Unset or zero at the aggregation layer never expires. **Divergence at a cut:** zero emission stops until the counter is seen again. | None planned. |
| Context limit | Default 1,000,000 entries, including idle counters. New contexts are dropped and counted when full. The breach flag clears only after a flush brings the count below the limit. During a cut, both generations' entries count against the same budget. | Global budgets with partition credits (stage 9). |
| Open buckets at shutdown | Discarded. `flush_open_windows` affects only idle-counter zeros, not other open data. | None planned. |
| Default-program validation | Behavior of native configurations that would fail validation (for example, the primary endpoint repeated as an additional endpoint) is investigated in stage 2 and recorded here. | — |

## Extension points

Each slot is part of the core from the stage listed. The core guarantees the slot's contract and ships a default that
reproduces native behavior. Richer designs plug into the slot later without changing the core.

| Slot | Core contract | Native default | Reserved for later |
|---|---|---|---|
| **Operators** (stage 1) | Trusted implementation with an effect contract; bounded, typed parameters. | Extracted native kernels. | New operator families. |
| **Delivery policy** (stages 3 and 7) | Attached to each output and recipient. Decides what happens when it is full (wait, shed, or spill) and when it closes (attempt, persist, or discard, within a deadline). Bounded by host budgets. Requires policy authority. | Outputs wait for downstream. Recipients use the native retry queue: byte bound, oldest entries evicted or spilled to disk, fresh requests before retries. Native shutdown. | Other overflow rules, priorities, per-stream budgets. |
| **Admission policy** (stage 6) | Part of a stateful operator's contract. Decides whether new keys enter state. Each policy carries its own R5b proof. | First-come context limit. | Eviction, partition credit schemes. |
| **Signals** (stage 1 contract, stage 10 use) | Named, typed, host-published state, such as queue occupancy. Operators declare signal reads; signal changes are trace events. Reading a signal makes an operator dynamic and excludes it from rewrites. | No signals published. | Reactive operators, such as shedding low-value metrics when a queue passes a watermark. |
| **Transition strategy** (stage 6) | Decides how state moves between generations. | `retain`, `cut`, `reject`. | `split`: versioned windows with merge rules. |
| **Artifact channel** (stage 5) | Delivers complete snapshots of versioned, hashed artifacts; receives per-config apply states. | No artifacts: the default program runs. A local stub for development and demos. | Remote Config. |
| **Authority scope** (stage 5) | Per-node provenance checked by the validator. | Whole-artifact approval. | Finer approval scopes; required nodes. |
| **Modules** (later) | Versioned, reusable subgraphs shipped with ADP, instantiated with parameters, like a standard library. They expand into ordinary operators before validation. | None: the default program contains expanded operators. | `mrf_mirror(...)`, `autoscaling_failover(...)`, and a default program composed of modules. |

## Logical IR

### Plan structure

A plan is a directed acyclic graph:

| Element | Definition |
|---|---|
| **Plan** | Version, provenance, sources, outputs, nodes, and edges. |
| **Node** | A stable ID, a registered operator name, and typed parameters. Its ports follow from the operator and parameters. |
| **Port** | A name, a direction, and a port type. |
| **Edge** | From one node's output port to another node's input port. Every output port has exactly one edge. |
| **Source** | A node with no inputs, bound to a registered host source (`dogstatsd`, `otlp`, `checks`, `liveness`, `trace_sampler`). Its output port type comes from the binding. |
| **Output** | A declared ID bound to a resolved [output binding](#output-bindings). `emit(output)` references it by ID. |

**Parameter values** are booleans, integers, floats, strings, durations, enumerations, lists, sets, string maps,
predicate expressions, references to outputs or credentials, and content-addressed blobs for large values such as mapper
profiles and allowlists. Blobs travel inside the artifact and count toward its size limit. A parameter may also be a **configuration binding**, `config(key)`, which lowering resolves from
native typed configuration. A change to a bound key re-lowers into a new generation, so bound parameters stay live.

Routing uses ordinary operators:

| Operator | Ports | Semantics |
|---|---|---|
| `split(p)` | 1 in; `true`, `false` out | Each event goes to exactly one output, by predicate `p`. |
| `tee` | 1 in; n out | Each event goes to every output. |
| `merge` | n in; 1 out | Fan-in. Interleaving is unspecified (see [Ordering](#ordering)). |
| `emit(output)` | 1 in | Terminal. Delivers to a declared output. |
| `drop` | 1 in | Terminal. Discards as policy loss. |

Every path ends in `emit` or `drop`. Edges carry values; implementations may share data, such as reference-counted
contexts, only where sharing is unobservable. Surface sugar such as "emit, then continue" lowers to `tee`.

This section fixes the abstract structure. The concrete JSON serialization and its generated JSON Schema are stage 1
deliverables.

### Node IDs and namespaces

- Lowering assigns the same ID to the same node every time. Default-program IDs derive from native component IDs
  (`dsd_agg`, `dsd_tag_filterlist`, …). They are reserved.
- Artifact nodes live in their own namespace, so they cannot collide with reserved IDs.
- Old and new nodes correspond by ID across generations. Correspondence does not imply compatibility; contracts decide.
- Telemetry identity follows node IDs. The artifact node cap (see [Limits](#limits)) bounds the telemetry cardinality an
  artifact author controls.

### Policy region and artifact composition

The **policy region** is the part of the plan an artifact may replace. Initially it is everything after
`metrics_enrich`: endpoint routing, the multi-region and autoscaling failover branches, and every output. Its single
input is the output of `metrics_enrich`.

- **The default program implements these features as ordinary operators.** Failover is not a hard-coded native branch;
  it is part of the default program, whose parameters are configuration bindings. An artifact that keeps those nodes
  keeps their live behavior, such as the multi-region failover toggle. An author may replace a binding with a literal.
- **The default program is the starting point.** `explain` exports it as an artifact, so authors edit it rather than
  start from nothing.
- **Removals are visible.** The plan diff shows removed nodes, such as a dropped failover branch, and approval binds to
  that diff. Stronger guardrails, such as required nodes, are a later extension.
- An artifact references its input by the reserved port and its outputs by reserved native IDs or by IDs declared in a
  resources artifact.
- Stateful operators inside the policy region are allowed from stage 6.

**An artifact is a full replacement with a recorded base.** It records the hash of the default program it was derived
from and the IR version it uses. While it is active:

- **Parameter changes** in native configuration reach the artifact through its configuration bindings, as usual.
- **Structural changes** to the default program, such as a newly configured additional endpoint or failover branch,
  are not merged into the artifact. ADP reports the artifact as **stale**: its base no longer matches the current
  default program. The artifact keeps running; the author rebases it on the new export.
- **A removed output** that the artifact still references makes the re-lowered plan invalid. The current generation
  keeps running, and ADP reports a degraded state until a new artifact or configuration resolves it.
- **ADP upgrades** accept a declared range of IR versions and migrate older artifacts deterministically. An artifact
  that no longer validates, for example because it uses a removed operator, is rejected, and the default program runs.

Until stage 7, structural changes that need new or removed hosts, such as enabling the multi-region failover endpoint,
take effect at restart, as they do natively today.

The region's upstream boundary is a fence like any other, removed over time. Through stage 5, everything upstream of
`metrics_enrich` is defined by native configuration. From stage 6, with aggregation as a kernel, the region can extend
upstream to pre-aggregation processing and aggregation, including per-branch bucket widths. Later steps can bring in
source decode-path filters. Each step moves the boundary; the model does not change.

### Operators and effect contracts

Only trusted implementations supply contracts. A plan references operators by name and passes bounded, typed parameters.

| Field | Meaning |
|---|---|
| `reads` / `writes` | Field sets from [Fields](#fields). May be conditional on input kind. Operators never read configuration; it reaches them only as resolved parameters. |
| `cardinality` | `filter` (0..1), `map` (1), `expand` (1..n, with declared maximum), or `sink`. Several output ports are declared as **copies** or as **point-disjoint** (each point goes to exactly one port). |
| `identity` | `preserving`, `injective` (one-to-one, optionally with a declared image), `quotient` (many-to-one), or `opaque`. May be conditional on input kind. Undeclared means `opaque`. |
| `kinds` | Accepted input kinds and the output kind mapping, per output port. |
| `time` | Whether the operator reads `t_a`, for example to compare against a deadline parameter. |
| `state` | `none`, or `keyed(key, reducer, time contract, admission policy)`. |
| `order` | Whether the result depends on admission order. |
| `loss` | `lossless` or `lossy(class, description)`. |
| `signals` | Host signals read. Any signal read makes the operator dynamic. |
| `boundary` | `synchronous`, `io_host`, or `opaque_host`. |

Kind-conditional contracts matter in practice. Native aggregation preserves `name` for every kind except histograms,
whose flush writes `name`, `kind`, and `metadata` (see the [compatibility matrix](#native-compatibility-matrix)). The timestamp split's outputs are point-disjoint, and its passthrough port maps
`Counter` to `Rate` with the window interval.

Every operator implementation guarantees:

- **Determinism:** output depends only on input, parameters, declared `t_a` and signal reads, and its own state.
- **Purity, where declared:** a pure operator is a function of input and parameters. Unobservable internal caches are
  allowed.
- **Bad input:** an event it cannot process is counted as `invalid` loss and never fails the region.
- **Panics:** a panic is a bug and fails the region through supervision (see [Failure](#failure-and-supervision)).
- **Bounded expansion:** `expand` operators declare maximum fan-out as a function of their parameters.
- **In-place mutation:** `map` kernels may mutate their buffer. The `partition` step clones before paths diverge, so no
  path observes another's mutation.

Native components that have not been extracted appear as **opaque hosts**. Their contract reads and writes everything
and acts as a fence. A component stays opaque only while it is stateful or asynchronous and has no extracted kernel.
Pure filters and routers are lowered into operators by stage 4. Filters inside a source's decode path (DogStatsD
payload-type filtering, origin enrichment, tag cardinality, sample-rate scaling) belong to the source `io_host` through
stage 8.

Initial operator families:

- **Pure:** name mapping, namespace prefixing and listener filtering, tag projection, timestamp split, post-aggregation
  filtering, host enrichment, and host tags (time-dependent: adds expected tags until a deadline parameter).
- **Routing:** `split`, `tee`, `merge`, `emit`, `drop`.
- **Stateful:** native aggregation, parameterized by bucket width.
- **Later:** cardinality observation and correction, sampling, event-time windows. Each needs its own contract and stage.

### Predicates

| Predicate | Semantics |
|---|---|
| `name`, `intake_name`, `host`: exact, prefix, set membership | String comparison. A predicate on a missing `intake_name` is false. `host is unset` matches an unset host. |
| `has_tag(key)` | True if any tag has this key, with or without a value. Bare tags are keys without values. |
| `tag(key)`: exact, prefix, set membership on the value | True if **any** tag with this key has a matching value. Never matches a bare tag. |
| `origin_tag(...)` | The same forms over `origin_tags`, which are separate from `tags`. |
| `kind in {…}` | Matches the `MetricValues` variant. Does not inspect a rate's interval. |
| `all`, `any`, `not` | Boolean composition. |

The **canonical form** flattens nested `all`/`any`, sorts operands in a deterministic total order, deduplicates them,
and removes double negation. R2 and
the duplicate analysis compare predicates in canonical form.

### Validation

Validation checks:

- references, acyclicity, reachability, and that every path ends in `emit` or `drop`;
- port types against each operator's declared kinds;
- duplicate contribution (see below);
- resource bounds from [Limits](#limits), including worst-case work per input;
- authority: every lossy node's provenance is native configuration or an approved artifact;
- capability bindings: outputs reference resources declared natively or in an approved resources artifact.

Errors name the logical node and the violated rule. The default program is validated by the same rules.

### Duplicate analysis

Outputs that share a physical recipient count as one for this check. For each pair of distinct paths that reach the same
output, find the node where they diverge:

| Divergence | Verdict |
|---|---|
| Opposite ports of one `split` | Disjoint. |
| Different sources | Disjoint (see [Identity](#identity)). |
| Point-disjoint outputs of one operator, such as the timestamp split | Disjoint. |
| A `tee`, or outputs declared as copies | Rejected, unless the exception below applies. |

**Exception:** after the divergence, one path passes the `true` port of a `split` and the other passes the `false` port
of a `split` whose predicate is equal in canonical form, and no operator between the divergence and either `split`
writes a field the predicate reads.

Contribution is tracked through stateful operators: two copies aggregated into one output are rejected like any other. A
rejection names both paths and the divergence point. The rule is fixed; the algorithm is not.

This version treats every series reaching an output as potentially overlapping. It rejects some valid plans, such as a
`tee` into fine-grained and renamed wide aggregations of the same metric. A later analysis can accept them by proving
that `identity` images are disjoint, without changing contracts or plans.

### Reference interpreter

The reference interpreter executes a plan over a trace literally: one event at a time in trace order, every node
visited, no rewrites, no fusion, using the same kernels. It is the oracle for the compiler and planner, and it powers
preview. It never runs in production.

Trace events mean:

| Event | Effect in the interpreter |
|---|---|
| `admit`, `heartbeat` | Deliver the batch, or advance the admission point's latest `t_a`. There are no queues, so every state owner's watermark is the minimum latest `t_a` across its admission points. |
| `advance(clock)` | Run one flush tick for every state owner at that host time. |
| `install`, `cancel` | Apply or abandon an installation, including any cut. |
| `capacity(output, n)` | Bound how much an output accepts, for delivery-policy tests. |
| `signal` | Set a signal's value. |
| `close` | End of input: every kernel finishes under the shutdown policy. |

Opaque hosts have no kernel. Before stage 6, the interpreter runs native aggregation through a synchronous test adapter
over its existing state machine, with an in-memory output. Segments containing other opaque hosts are covered by
differential tests instead.

Because the interpreter shares kernels with production, it cannot catch a kernel's own semantic mistakes. Independent,
hand-written expected-result fixtures cover the semantic model and the compatibility matrix.

## Compiler

### Rewrites

The rewrite set is closed. Each rewrite has proof obligations stated in terms of effect contracts and a property test
against the reference interpreter. In R5 and R6, a **filter** is an operator with `filter` cardinality, or a `split`
whose one port leads directly to `drop`.

| ID | Rewrite | Proof obligations |
|---|---|---|
| R1 | Fuse adjacent operators into one region. | Both are `synchronous` and no fence separates them. Fences: repartition, `io_host`, `opaque_host`, declared isolation. A region may contain stateful kernels and owns their state. |
| R2 | Share predicate results. | Equal in canonical form; nothing writes their `reads` in between. |
| R3 | Fold constant `split`s and remove unreachable subgraphs. | The predicate is constant given its parameters. |
| R4 | Memoize a decision per interned context plus kind. | Every predicate in the decision reads only those fields. The memo is bounded and belongs to one generation. Keying by the context hash reproduces native collision behavior. |
| R5a | Push a filter upstream across a stateless operator `O`. | For the kinds where `filter.reads ∩ O.writes = ∅` and the filter reads no kind or values `O` changes. The filter applies to every consumer of `O`'s output. |
| R5b | Push a filter upstream across a stateful operator `O`. | R5a's obligations; `O`'s reducer key ⊇ `filter.reads`, so whole keys are removed; `O` treats keys independently except through its admission policy; removing keys can only reduce capacity loss. Each admission policy needs its own proof. For native first-come admission with per-key expiry, the optimized plan's occupied keys are always a subset of the original's. |
| R6 | Hoist a drop that every path below a `tee` applies. | The predicate is equivalent on all paths. |
| R7 | Reorder operands of pure `all` / `any`. | All operands are pure. Ordering may be profile-guided. |

**Partial push-down:** R5 applies per kind. When a filter commutes with `O` only for some kinds, the rewrite inserts a
copy restricted to those kinds above `O` and keeps the original filter below `O` as a residual. For native aggregation,
a name filter is pushed for every kind except histograms, whose expanded names the residual still checks.

Parameters are fixed per generation, so a filter evaluates identically above or below a stateful operator. Rewrites
never change routing, lossy operators, operator parameters, or delivery. A rewrite can change internal telemetry, such
as the aggregator's context count after R5b; `explain` lists each change. Dynamic operators (signal readers) are never
moved, merged, or memoized, and nothing moves across them.

Deferred, each needing its own proof: stream splitting, sharing encoder batches across outputs, push-down through
injective maps, and moving partitions between workers.

### Physical plan

The logical IR is what rewrites, validation, the reference interpreter, and artifacts use. The **physical plan** is what
runs. It is derived from the logical IR, never authored or serialized as an artifact, and shown by `explain`.

The planner assigns operators to regions and puts bounded queues at fences. Each output has exactly one encoder stream,
which handles all its payload types (series and sketches). Recipients of one output share its payloads. Outputs never
share an encoder stream.

A region runs **batch-at-a-time**: each step processes a whole buffer before the next step starts.

| Step | Effect |
|---|---|
| `map(kernel)` | Transforms a buffer in place. |
| `decide(fn)` | Computes each event's exit set. |
| `partition` | Moves events into exit buffers: the original into its last destination, clones into the others. |
| `concat` | Implements `merge` inside a region. |
| `state(kernel)` | Feeds a stateful kernel; its time-driven output enters the following steps. |
| `emit` / queue | Hands a buffer to an output binding or a fence queue. |

Exit buffers are bounded by batch size times fan-out and are recycled. A step that yields keeps a cursor into its buffer.

### Decision functions

A **decision** is a maximal connected group of `split` and `tee` operators within one region, with no intervening map
that writes a field one of its predicates reads. It compiles to one pure function from decision-relevant fields to an
exit set. There is no per-event graph walk.

The specification fixes the function's semantics, not its form. A predicate tree, decision table, decision diagram, or
multi-pattern matcher are all valid if they match the reference interpreter. Competing forms may be compared in
benchmarks; production uses one, with no runtime selection flag. With R4, a cache hit costs one lookup plus buffer
pushes, independent of the number of policies.

### Explain

`explain` is generated from the IR, physical plan, and plan diff, in a stable, versioned format with these sections:

- logical graph, port types, and node provenance;
- rewrites applied, each with its justification, including capacity-rule and telemetry effects;
- regions, segments, fences, and state owners;
- output bindings, recipients, and delivery policies;
- resource estimates against [limits](#limits);
- the [plan diff](#plan-diff), when an update is proposed.

## Execution

### Batches and segments

`EventsBuffer` gains a small **batch header**: `t_a` and the generation ID. Every place that constructs or consumes a
buffer must carry it, so this is a broad mechanical `saluki-core` change, delivered in stage 2.

- **Admission points** stamp the header: each source's entry into the plan, and the output of each opaque host. Each
  admission point has its own [admission clock](#clocks) and emits heartbeats when idle.
- Regions and interconnect queues carry headers unchanged and in order. Opaque hosts do not; their output is re-admitted.
- A **segment** is the subgraph between admission points. Within a segment, a batch is processed entirely by its
  header's generation. Segments on either side of an opaque host switch generations independently, as native live
  refresh does today.
- A region keeps the plans of every generation that still has batches in its segment, and releases them when drained.

### Kernels

This sketch is illustrative, not a final API.

```rust
trait Kernel {
    fn consume(&mut self, turn: &mut Turn<'_>, input: &mut Batch) -> Progress;
    fn advance_time(&mut self, turn: &mut Turn<'_>) -> Progress;
    fn apply_control(&mut self, turn: &mut Turn<'_>, control: Control) -> Progress;
    fn finish(&mut self, turn: &mut Turn<'_>, policy: DrainPolicy) -> Progress;
}

enum Progress { Done, Yielded, WaitUntil(Deadline), WaitForCapacity(OutputSet) }
```

`Turn` provides clocks, output writers with remaining capacity, and a work budget. A kernel that yields keeps its
cursor. Kernels never await, perform I/O, read configuration, or own timers. `SynchronousTransform` and
`ChainedConfiguration` remain valid adapters for kernels that need none of this. Most native filters are async
`Transform`s today; stage 2 extracts their processing into kernels.

### Turns

A region host runs a **turn** when input, a deadline, a control message, or output capacity becomes ready. A turn does
bounded work and returns. There is no global tick. Budgets limit events, scalar points, emitted bytes, and state work.
Timers and control messages are reserved scheduling opportunities so that heavy input cannot starve them.

### Hot-path rules

- Selection, routing, and predicate evaluation allocate nothing per event and use resolved numeric indices.
- Transforming operators allocate no more than their native kernel does to build a changed context.
- When fanning out, move the original into its last delivery and clone it only for the others.
- Compute derived fields lazily, once per event, only when an evaluated predicate needs them.
- Accumulate counters per turn and publish them off the data path.

### Failure and supervision

A panicked region restarts through supervision in the same generation and loses its state and in-flight batches, as a
native component restart does.

If supervision exhausts its restart budget for a generation that came from an artifact, the artifact is marked failed
and the last-known-good plan is reinstalled. This is the only automatic rollback. It does not change the revision
high-water mark: later proposals must still exceed the failed revision. If the default program itself fails this way,
the failure propagates as a native component failure would.

## State

- Native aggregation is extracted. Insertion is already synchronous; the flush becomes a resumable, bounded drain. Every
  row of the [compatibility matrix](#native-compatibility-matrix) is preserved, apart from the recorded changes.
- State is indexed by reducer key and bucket. Limits draw credits from a global budget.
- Each **state lineage** has one owning kernel instance. Generations are its clients.
- **Compatibility** is computed per state owner by contract: reducer key, reducer, time contract (including width),
  and admission policy. Changes upstream or downstream of a state owner do not affect its compatibility.

### Time-driven output

A stateful kernel emits on its own schedule: bucket flushes and idle-counter zeros. That output is not tied to an
admitted batch, so it is assigned to the lineage's **current generation**: the newest generation committed for that
state owner. It is stamped with that generation and with the state owner's watermark as its `t_a`, and flows through
that generation's downstream plan.

This matches native behavior, where a live change to post-aggregation filtering or endpoint routing applies to the next
flush, including data aggregated before the change.

### Transition classes

- `retain`: compatible. Old- and new-generation batches feed the same instance in arrival order. Time-driven output
  uses the newest generation's downstream plan from the moment it commits. Policy edits downstream of aggregation are
  therefore `retain`, and apply at the next flush.
- `cut`: incompatible, for example a changed width. Ownership changes at a bucket boundary using the
  [transition protocol](#transition-protocol).
- `reject`: refused, for example because the reducer key changed while state is retained.

`split` is deferred.

## Installation

### Lowering and generations

**Lowering** is a pure, deterministic function. Its inputs are native typed configuration, approved artifacts, and
**host facts** fixed for the life of the process, such as the process start time and the resolved hostname. Its output
is a logical plan with every parameter resolved, including output bindings. Credentials remain references; rotating a
key is not a plan change. Time-based parameters derive from host facts, so re-lowering never resets them. For example,
the host-tags deadline is process start plus `expected_tags_duration`.

A **generation** is an immutable plan plus its provenance: the configuration version and artifact revisions that
produced it. Any input change re-lowers. A new generation is created only if the re-lowered plan differs from the
active one, so changes to unrelated configuration keys cause no churn. A new physical plan from profile-guided R7 is
also a new generation.

- **Resources** (encoders, forwarders, queues) are leases. Generations reuse a resource only when its stream, batching,
  and lifecycle contracts permit it. Until stage 7, encoder and forwarder settings are not plan parameters; those hosts
  keep their native live views.
- Acknowledgments identify the artifact hash, revision, and generation.
- A change that the current stage cannot perform, such as a stateful edit before stage 6, is rejected with an explicit
  error naming the missing capability. It is never approximated.

### Plan diff

Preparation starts by diffing the active and candidate plans, like a `plan` before an `apply`. The diff lists each node
as unchanged, changed, added, or removed; each state owner's transition class; and each resource as reused or new.
`explain` shows it, so an operator can see what an update will do before approving it.

- A new generation reuses every kernel instance, state lineage, and resource whose ID, contract, and parameters are
  unchanged, including internal caches. A one-rule change rebuilds only the affected nodes and decisions.
- Preparation cost is proportional to the changed part of the plan.
- R4 memos belong to a generation and start cold, as native caches do after a configuration update.

### Lifecycle

| Phase | Requirement |
|---|---|
| Prepare | Validate the plan, authority, output bindings, state compatibility, and peak overlap resources. Build changed resources off the data path. Failure leaves the active generation unchanged. |
| Commit | Stateless changes and `retain` switch at the next batch boundary of each affected segment. `cut` follows the transition protocol. |
| Drain | Old batches finish in their generation. Old state closes by its transition class. Outputs and recipients apply their delivery policy's closure behavior within the drain deadline. |
| Retire | Release state, buffers, tasks, and scoped registrations (health, accounting, metrics, diagnostics, credentials). |

Default closure behavior:

- **Process shutdown:** native. In-flight requests finish within the shutdown timeout; queued work is persisted where
  configured, otherwise dropped.
- **Generation retirement:** queued work is attempted within the drain deadline, then handled as at process shutdown.
  A live update loses nothing beyond what a native shutdown would.

A drain promises that every output and recipient carried out its closure behavior or reached its deadline. It does not
promise delivery. Existing forwarder and encoder telemetry is the source of truth; `retired` summarizes it.

Acknowledgments report `rejected`, `pending`, `prepared`, `active`, or `retired`. A missing acknowledgment means unknown.
Overlapping generations draw from a declared, bounded budget.

### Transition protocol

A `cut` requires the state owner and everything upstream of it in its segment to share admission stamps, so it applies
from stage 6, when aggregation becomes a kernel.

1. After preparation, the installer computes the cut time `T`: the first instant at least 1 second from now that is a
   bucket boundary for every affected state owner. The wait is at most max(60 s, largest width) + 1 s. The
   acknowledgment reports `prepared` with `T`. A newer proposal before `T` cancels the prepared generation.
2. Each admission point stamps batches with `t_a < T` with the old generation and batches with `t_a ≥ T` with the new
   one. The choice depends on the stamp alone, so backlog cannot change it. A clock reset before `T` is treated as
   reaching `T`.
3. The old generation's buckets all end at or before `T`. They close normally: at the first flush tick at which the
   watermark reaches their end. Every old-generation batch has arrived by then. The old generation retires after its
   last bucket closes.
4. Where timestamped points can reach state, the old generation flushes any remaining future-timestamped buckets when it
   retires. Emitting them early changes neither values nor timestamps.
5. Before `T`, cancellation discards the new generation; nothing was admitted to it. After `T`, rollback is a new
   installation.
6. Plans whose affected state owners mix admission-time and event-time contracts are rejected for `cut`.

Known divergences at a cut, also listed in the compatibility matrix: the new generation records fresh metadata, and idle
counters stop emitting zeros until they are seen again.

### Transition traces

Each trace is normative and becomes a fixture. Widths are 10 seconds unless stated.

| Scenario | Trace | Required result |
|---|---|---|
| Backlog at the cut | `T = 100`. A batch stamped `t_a = 99` is still queued when the flush tick at 101 fires. | The watermark is still below 100, so `[90, 100)` stays open. The batch lands in it, and it closes at the first tick after the batch arrives. The new generation never sees it. |
| Late batch, no cut | A batch stamped `t_a = 99` arrives after a tick at 101. | Same rule: `[90, 100)` was still open, because the watermark had not reached 100. No point is ever late. |
| Changed bucket width | Old 10 s, new 30 s; now 95. | `cut` with `T = 120`. Old buckets through `[110, 120)` close from the old generation. The new generation starts at `[120, 150)`. |
| Changed destination | Output `A` replaced by `B` downstream of aggregation; reducer unchanged. | `retain`. Flushes after commit go to `B`, including data aggregated before the change, as native routing changes do. |
| Small clock jump back | Admission clock at 103; wall clock reads 97. | `t_a` stays at 103. |
| Large clock jump back | Admission clock at 7,203; wall clock jumps back one hour. | Clock reset: affected state owners close every open bucket, then `t_a` restarts from the wall clock. |
| Rollback | Revision 3 replaced 2 with `cut` at `T = 100`; revision 4, with the same plan as 2, is approved at 105. | A normal installation with its own cut at `T = 110`. Nothing delivered is reverted. |
| Late timestamped point (not reachable in the default program) | Timestamp 95, `t_a = 103`, `T = 100`. | New generation; emitted at its next flush, as native would. |
| Future timestamped point (not reachable in the default program) | Timestamp 130, `t_a = 98`, `T = 100`. | Old generation; flushed at its retirement. |

### Output bindings

An **output** is one encoder stream. Each output's binding is resolved jointly at prepare and validated as a whole:

- encoder: protocol and series API version, compression, payload and point limits;
- **recipient set:** which endpoints receive this output's payloads, such as "only these endpoints" or "every endpoint
  except these";
- recipients: canonical URL (lowercase scheme and host, default port removed, normalized path) and credential reference;
- the delivery policy of the output;
- the provenance of every inherited value.

Selecting which metrics reach an output is policy, done by operators in the policy region. A recipient set only selects
endpoints.

**Recipients are shared across outputs.** A recipient is one endpoint's I/O loop and retry queue inside a forwarder, and
every output whose recipient set includes it feeds the same queue, as native allowlist routing does today: several
encoders feed one forwarder, `dd_out`. A recipient's delivery policy and byte budget therefore belong to the recipient,
not to an output.

All recipients of one output must accept its protocol and limits. Two recipients are the same when their canonical URLs
match and their credential references resolve to the same key fingerprint, checked at prepare. Rotating the key behind
one reference does not change recipient identity. A rotation that makes two references resolve to the same key is
reported as a conflict.

When inherited native configuration changes, bindings are re-resolved. Unchanged bindings do nothing. Changed bindings
produce a new generation. An invalid result keeps the current generation and reports a degraded state.

### Recovery and concurrent proposals

- **Default program:** the plan lowered from native typed configuration. It runs whenever no approved artifact is
  active, including at first startup. There is no separate fallback setting.
- **Last-known-good:** the plan that was active before the current one. ADP reinstalls it only when a generation fails
  under supervision (see [Failure](#failure-and-supervision)). Deliberate rollback is the control plane approving an
  earlier plan under a new, higher revision.
- **Durability:** ADP persists nothing itself. Persistence and redelivery belong to the artifact channel. After a
  restart, the default program runs until the channel delivers the active artifact.
- **Idempotency:** installing an artifact whose hash matches the active or preparing one acknowledges the current state.
- **Ordering:** revisions strictly increase. A proposal at or below the highest revision seen is stale and rejected. A
  newer proposal cancels a preparation that has not committed.
- **Pending resources (stage 7):** a policy that references a resource not yet delivered is held as `pending` and
  re-evaluated whenever the resources snapshot changes.
- **Revocation:** revoking authority installs the default program.
- **Crash:** retained aggregation state and unflushed buckets are lost, as with native aggregation. Forwarder
  persistence follows its configuration. Startup follows the rules above.

## Concurrency

Ownership is decided first; scheduling follows from it.

- Stateless regions run in parallel over independent batches.
- Each stateful operator is partitioned by its reducer key. Injective identity transforms preserve grouping; quotient
  and opaque ones may merge keys across partitions, so the planner inserts a repartition edge after them.
- Partitions are not threads. A worker can own several partitions. Moving a partition is an explicit lease transfer.
- A hot key is split only when its reducer has a proven merge operation.
- Configured limits (context limits, state bytes, delivery queue bytes, install overlap) are global and mean the same
  thing at any partition count. Scheduling limits (work per turn, batch size) are local.
- Stateful operators and delivery queues take credits from a budget instead of reading limits directly, from stages 6
  and 7. Through stage 8 one owner holds every credit. Stage 9 chooses how credits are split, inside the admission-policy
  slot, keeping R5b's proof valid. With partitions, first-come admission becomes approximate; that is capacity behavior.
- Deliberate queues remain where parallelism or isolation pays: decoding, state ownership, encoding, slow sinks.
- Repartition edges carry watermarks: a partition's watermark is the minimum over the inputs that feed it.

**Boundary through stage 8:** regions replace only chains of synchronous work. Every existing task boundary stays:
source and decoder pools, the aggregator task, encoders, and forwarders. The aggregator boundary is a declared isolation
fence; removing it later needs no model change. Throughput and p99 latency must be no worse than legacy at every gate.

## Control plane and authority

- With no approved artifact, the default program runs and behavior is identical to today's.
- An artifact is a versioned JSON serialization of the IR, validated against the generated schema. Approval binds to a
  hash over its canonical form (RFC 8785).
- Policy and resources are separate artifacts with separate revisions and authority. A policy artifact replaces the
  [policy region](#policy-region-and-artifact-composition), typically starting from the exported default program. A resources artifact declares endpoints and credential references; resources artifacts are supported from stage 7,
when outputs gain a lifecycle. Until then, policies reference native outputs only.
- **Authority** is per-node provenance: native configuration, or an artifact revision. The validator checks that every
  lossy node has one. Approval covers a whole artifact; finer scopes are an extension point.
- Artifacts arrive through the **artifact channel**, whose contract follows Remote Config's model:
  - each update is a complete snapshot of a product's configs, never a delta; an empty snapshot revokes;
  - each config has an ID, a version, and a content hash checked on receipt;
  - delivery can repeat or arrive after a restart, so installation is idempotent by hash;
  - outcomes are reported per config as an apply state; finer states appear in ADP's status and telemetry;
  - no ordering is assumed between policy and resources artifacts.
- A **local stub** implements the channel for development, tests, and demos until stage 8. Mapping acknowledgments onto
  Remote Config apply states is a stage 8 deliverable. Remote Config plugs into the
  same seam then; integrity comes from its chain, not from an ADP signing scheme.
- Surface syntaxes and studios are tools that produce IR outside the runtime.
- Proposal authors, including LLMs, are untrusted. Preview runs the reference interpreter on fixtures. A human approves.

## Telemetry

Existing telemetry is preserved where it remains meaningful and cheap to record.

- **Identity survives fusion.** Each kernel reports telemetry and memory accounting under its node ID, which equals
  today's component ID in the default program.
- **Elided work is not measured.** Metrics for removed queues or skipped work disappear rather than being simulated, as
  does any existing metric that would be costly to keep. Release notes list removed metrics.
- **Values change only through rewrites.** `explain` lists each affected metric and the reason. With no rewrites and
  below capacity, values match legacy.
- **New plan-level telemetry is added:** active generation and revision, rewrites applied, cuts, and loss by class.
  Replacing component telemetry with plan-level telemetry is a decision for after rollout.

## Default lowering coverage

Through stage 8, default lowering covers metric paths as follows. Everything else is host-owned and unchanged.

| Component | Representation |
|---|---|
| DogStatsD source: decoding, payload-type filtering, origin enrichment, tag cardinality, sample-rate scaling | Source `io_host`. |
| DogStatsD mapping, prefix and listener filtering, tag filterlist | Kernels extracted from async transforms in stage 2. |
| Timestamp split (no-aggregation pipeline enabled) | Kernel in stage 2. Point-disjoint outputs. The passthrough port converts counters to rates with the window interval, skips the tag filterlist and aggregation, and rejoins at the post-aggregation filter. |
| DogStatsD aggregation, including histogram expansion | Opaque host until stage 6, then a native stateful kernel. |
| Post-aggregation filter | Kernel in stage 2. |
| OTLP metrics receiver and translation; checks IPC; `liveness_in`; `trace_sampler.metrics` | Source `io_host`s feeding `metrics_enrich`. |
| `metrics_enrich`: host enrichment, then host tags | Kernels in stage 2. Host enrichment leaves metrics unchanged. Host tags adds expected host tags until `expected_tags_duration` elapses, reading `t_a` against a deadline parameter. |
| Endpoint allowlists and additional endpoints | Policy region. Natively, each allowlist policy has its own encoder sending only to its selected endpoints; the main encoder sends to every endpoint except those; all share the forwarder `dd_out`. Default lowering keeps this shape: one output per encoder, with shared recipients. Opaque filters until stage 4, then policy operators. Allowlist entries are not normalized; names without an `intake_name` never match. |
| Multi-region failover gateway | Policy region. Present only when the failover endpoint is configured. Opaque filter until stage 4, then default-program operators bound to two flags, the static `multi_region_failover.enabled` and the live `metric_mirroring.enabled`, and a live `name` allowlist; an empty list forwards everything. |
| Autoscaling failover gateway | Policy region. Opaque filter until stage 4, then default-program operators bound to configuration: drops everything unless enabled with a non-empty list; otherwise counters, rates, gauges, and sets with an exact `name` match. |
| Encoders and forwarders | Output bindings. Encoder-side transformations and drops (for example non-finite values, `device` tag extraction, v3 per-endpoint filtering) are host loss. |
| Internal telemetry | Not a pipeline source (telemetry API). Outside the plan. |
| `dsd_stats_out`, `dsd_debug_log_out` (taps on the DogStatsD source's metrics), `dsd_client_telemetry_out` | Host-owned. |
| OTLP relay and proxy mode | Unsupported: payload path, not metric events. |
| Logs, events, service checks, traces | Outside the plan. |

## Limits

Stage 1 delivers a limits table with a default and a configuration source for each entry. Known values:

| Limit | Default |
|---|---|
| Aggregation context limit | 1,000,000 entries (native). |
| Bucket widths | The allowed set in [Time](#time). |
| Cut margin | 1 second. |
| Artifact size, node count, predicate count, fan-out, worst-case work per input | Stage 1. The node cap also bounds artifact-controlled telemetry cardinality. |
| Prepare timeout | Stage 1. |
| Drain deadline, overlap budget | Stage 1. These change behavior, not just tuning, so each gets a decision-log entry when set. |
| Heartbeat period | Stage 1. At most the flush interval, so idle sources never delay a flush tick. |
| Clock-reset threshold | Stage 1. At least the largest bucket width. |
| Artifact blob size | Stage 1. Counts toward the artifact size limit. |
| R4 memo size and eviction | Stage 1. Bounded per decision; eviction must not allocate on the hot path. |
| Batch size, work per turn | Stage 1. Native batch sizes are the starting point. |
| Floating-point tolerance for stage 9 oracle checks | Stage 1. |

## Verification

| Subject | Method |
|---|---|
| Kernels | Differential tests against the legacy component on the same input. |
| Semantic model | Independent fixtures for every kind, every compatibility-matrix row, every transition trace, and each gateway's empty-list behavior. |
| Rewrites and planner | Property tests (named `property_test_*`): random valid IR and traces; optimized execution is [equivalent](#equivalence) to the reference interpreter. Each rewrite is also tested under capacity pressure. |
| Execution and state | Deterministic clocks and controlled output capacity; yield and resume at every budget boundary; segment switching across opaque hosts; watermarks with backlog, idle sources, and clock resets. |
| Bucket assignment (stage 6) | Under a deterministic clock with no queueing, exact equality with native. Under load, record each point's `t_a` and its arrival time at the aggregator: points may differ from native only where those two fall on opposite sides of a bucket boundary. |
| Lifecycle | Fault tests: blocked sinks, retries, preparation failure, cancellation, stale and concurrent proposals, pending resources, repeated installs, clock jumps, crashes. |
| Stage 9 reordering | Per-kind checks that hold under reordering: sums within the tolerance, set unions equal, and each gauge value one of the candidates admitted to its bucket. No permutation search. |
| Performance | Equal delivered work against the legacy topology and a hand-written baseline in benches. Vary policies, cardinality, kinds, fan-out, batch size, core count, key skew, slow sinks, and update frequency. Measure CPU/input, throughput, p95/p99 latency, allocations, RSS, queue bytes, and state occupancy. |

## Capability stages

Each stage depends only on earlier stages. A narrow production release becomes possible after stage 8. Stages 9 and 10
need separate evidence.

| # | Capability | Deliverables | Exit gate | Not included |
|---|---|---|---|---|
| 1 | Semantic core | Semantic model; compatibility matrix; IR structure, serialization, and schema; effect contracts; predicates and canonical forms; extension-point contracts; validator; kernel trait; trace-based reference interpreter; `explain`; limits table. | Independent fixtures pass. The IR represents every component in the coverage table. | ADP integration. |
| 2 | Native kernels and lowering | Kernels extracted from native transforms; lowering of native configuration, with opaque hosts for the rest; batch header, admission clocks, and heartbeats; recorded behavior for native configurations that fail validation. | Differential tests match legacy. Lowered plans compare structurally to the blueprint the legacy topology builds; the legacy blueprint still runs. | Execution changes. |
| 3 | Fused regions and stateless generations | The physical plan drives topology construction: regions become transforms, I/O and opaque hosts remain existing components, and fences become edges in the `TopologyBlueprint`. Region host on Tokio with turns and budgets; R1; segments; generations for stateless changes, switching at batch boundaries; lowering on every configuration change, with plan diff and kernel reuse; delivery-policy slot with native defaults. | Behavior equal to legacy. CPU, throughput, and p99 no worse, including under frequent live configuration updates. Health, memory accounting, shutdown, and retained telemetry preserved. | New policy features. |
| 4 | Policy and stateless rewrites | Policy region; `split`, `tee`, `merge`, `emit`, `drop`; predicates; lowering of endpoint allowlists and both failover gateways into default-program operators with configuration bindings; R2–R4, R5a, R6, R7. | Rewrite property tests pass. Selection cost near the hand-written baseline. Routing and gateway behavior match legacy. | Artifacts; R5b. |
| 5 | Live policy installation | Artifact channel and local stub; default program exported as an artifact; artifact composition into the policy region; installation, last-known-good, recovery, acknowledgments; base tracking and stale reporting; per-node provenance. Artifacts reference native outputs only; artifacts with stateful operators are rejected with an explicit error. | Continuous-input tests show no loss or duplication across installs. Recovery, stale-proposal, and stale-base tests pass. | Stateful transitions; new outputs. |
| 6 | Stateful kernels | Native aggregation as a kernel bucketing by `t_a`, with watermarks and clock resets; the bucket-width restriction; shared state lineages; time-driven output; compatibility; `retain`, `cut`, `reject`; stateful operators in artifacts; policy region extended upstream to pre-aggregation processing and aggregation; R5b with partial push-down. | Compatibility-matrix parity, with bucket assignment checked as in [Verification](#verification). Transition traces pass. R5b capacity tests pass. | Repartitioning. |
| 7 | Resource generations | Prepare/commit/drain/retire for outputs; resolved output bindings as plan parameters; resources artifacts and pending resources; live structural changes to hosts; scoped registration; delivery-policy slot open to artifacts. | Fault tests pass. Resources reclaimed after repeated installs. | Upstream source changes. |
| 8 | Production control plane | Remote Config implementation of the artifact channel; separate resource authority; audit; preview tooling. | End-to-end rollout and rollback on representative workloads. | Partitioning. |
| 9 | Partitioned execution | Parallel stateless regions; keyed state partitions; repartition edges; credit splitting; per-context ordering. | Scales with cores at least as well as legacy, with unchanged semantics. | Hot-key splitting without proofs. |
| 10 | Measured extensions | Candidates: identity-aware duplicate analysis and push-down through injective maps; signals and reactive operators; richer delivery policies; event-time windows; cardinality operators; encoder CPU extraction; stream splitting; shared batches; partition movement; removing the aggregator isolation fence. | Each candidate shows an isolated and an end-to-end benefit, with semantics preserved. | Anything unmeasured. |

## Placement in the repository

- Generic execution contracts (kernel trait, batch header, turns, region host, lifecycle primitives) go in `saluki-core`.
  Their documentation does not reference Datadog.
- Metric operators, the IR, the validator, the compiler, and the reference interpreter go in a dedicated library crate.
- Native configuration lowering and Datadog protocol bindings stay in `bin/agent-data-plane`.

## Decision log

Decisions made while reviewing this specification. The sections above are authoritative; this log records why.

1. **Capacity and equivalence:** exact equivalence below capacity, monotone improvement above it. Capacity loss is
   operational, so a rewrite that delivers more policy-included data under overload is an improvement. R5b is on by
   default.
2. **Transition protocol:** a boundary cut computed after preparation with a 1-second margin. Bucket widths are
   restricted, in native configuration too, so the wait is at most max(60 s, largest width) + 1 s.
3. **Timestamped points at a cut:** assigned by `t_a`, as natively; future-timestamped buckets flush at retirement.
   Today's default program never routes timestamped points into aggregation.
4. **Recovery:** the default program is the only fallback. Last-known-good is held in memory; persistence belongs to
   the artifact channel.
5. **Ordering:** only enough to compare with native and allow concurrency. Equivalence compares multisets.
6. **Delivery policies:** a slot on outputs and recipients, defaulting to native behavior. Reactive policies are later.
7. **Drain promise:** native closure at shutdown; generation retirement attempts queued work first. Existing telemetry
   is the source of truth.
8. **Budgets:** configured limits are global and enforced through credits; scheduling limits are local.
9. **Artifact channel:** a seam following Remote Config's model, with a local stub until stage 8.
10. **Telemetry:** preserve where meaningful and cheap; never measure elided work; rewrite effects listed in `explain`.
11. **Name fields:** distinct raw `name` and derived `intake_name`; no default view; sugar and renames are easy later.
12. **Opacity:** only stateful or asynchronous components without an extracted kernel stay opaque. Source decode-path
    filters stay with the source through stage 8.
13. **Plan structure:** a dataflow DAG with routing operators, so pure, stateful, and routing operators compose
    uniformly.
14. **Execution form:** a logical IR and an internal physical plan. The "VM" is a set of decision functions with fixed
    semantics and free form; regions run batch-at-a-time.
15. **Parameters:** lowering resolves configuration into parameters; any change re-lowers. Plan diff and reuse keep
    updates precise and cheap.
16. **Operator guarantees:** determinism, declared purity, bad input as `invalid` loss, bounded expansion, and hidden
    in-place mutation.
17. **Duplicate analysis:** the invariant is per recipient and backend series. Contracts declare `identity` now; v1
    analysis is conservative.
18. **Regions:** may own state; fences are repartition, I/O, opaque hosts, and declared isolation. Timestamp form is a
    port type.
19. **Admission:** `t_a` and the generation travel in a batch header stamped at each admission point by its admission
    clock.
    Opaque hosts re-admit, creating segments that switch independently.
20. **Retain:** state lineages are shared between generations, not transferred. Stateless and `retain` changes switch
    at batch boundaries; `cut` applies from stage 6.
21. **Policy region:** initially everything after `metrics_enrich`, including both failover branches, which the
    default program implements as operators with configuration-bound parameters. The upstream boundary moves over
    time. Artifact IDs are namespaced and capped. Reusable modules are a later extension.
22. **IR definition:** the specification fixes the abstract structure; concrete serialization is a stage 1 deliverable.
23. **Loss and push-down:** loss counts are telemetry, not part of equivalence. Effect contracts may be kind-conditional,
    and R5 pushes down per kind with a residual filter.
24. **Cut divergences:** fresh metadata and paused idle-counter zeros at a cut are documented, not hidden.
25. **Failure:** a panicked region restarts in the same generation and loses its state; no automatic rollback.
26. **Pending resources:** a policy that references an undelivered resource waits and is re-evaluated, since artifact
    order is not guaranteed.
27. **Encoder streams:** each output has one encoder stream for all payload types. Sharing across outputs is deferred.
28. **Watermarks:** buckets close only when every upstream admission point has passed their end, with heartbeats from
    idle sources. Late batches cannot happen; queueing delays closure instead. Large backward clock jumps trigger a
    clock reset.
29. **Time-driven output:** belongs to the lineage's newest committed generation. Policy edits downstream of
    aggregation are `retain` and apply at the next flush, as native live routing changes do.
30. **Artifacts and native configuration:** an artifact is a full replacement with a recorded base. Parameter changes
    flow through bindings; structural changes mark it stale; upgrades migrate a declared range of IR versions.
31. **Layering:** resources artifacts move to stage 7. A change the current stage cannot perform is an explicit error.
32. **Recipients:** shared across outputs, with their own delivery policy and budget, matching native `dd_out`. Output
    bindings select endpoints; policy selects metrics.
33. **Equivalence by points:** records are flattened to points, so record and batch grouping is not observable.
34. **Automatic rollback:** only when supervision gives up on an artifact generation, without changing the revision
    high-water mark.
