---
sut_path: /home/ssm-user/src/saluki
commit: fc4bb29728814ddf9321572b954ec28f58faeb53
updated: 2026-05-30
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: ADP Confluence space — headline guarantees, Phase 1 bug bash, gap analyses, weekly summaries.
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/pages/6497671050/What+Comes+After+DogStatsD
    why: Source of the "ADP will not crash under load, losing customer data" guarantee.
  - path: https://datadoghq.atlassian.net/browse/DADP
    why: ADP Jira project for tracked gaps/incidents (e.g. DADP-108 macOS gaps, DADP-45 telemetry compat).
---

# SUT Analysis: Agent Data Plane (ADP)

> Synthesis of a 5-agent discovery ensemble (12 attention focuses) over `bin/agent-data-plane`
> and the `saluki-*` libraries, grounded against the Datadog ADP Confluence/Jira spaces.
> Each major finding notes the focus(es) that surfaced it.

## 1. What ADP is (product context)

**Agent Data Plane (ADP)** is a Rust reimplementation of the Datadog Agent's telemetry data
paths — primarily the **DogStatsD pipeline** — built on the **Saluki** toolkit. It runs
*alongside* the Datadog Core Agent (not standalone in production; `standalone` mode is a
vestige, `AGENTS.md:31`). It is being delivered to first customers for the the design partner design
partner, targeting Agent **7.80.0**; `data_plane.enabled: true` routes DogStatsD through ADP.

Stated priorities (`AGENTS.md:13`): **correctness first, then performance.** Marketed benefit:
**deterministic resource usage** (`docs/agent-data-plane/index.md:1-6`).

**Headline guarantee (Confluence, "What Comes After DogStatsD"): "ADP will not crash under
load, losing customer data."** This single sentence decomposes into the two property families
that dominate this analysis: *no crash / bounded memory* and *no silent data loss*.

User-visible failure modes: lost metrics (silent drops), wrong aggregates (counter→rate, sketch
error, bucket misalignment), process crash (panics on hot paths), memory blowup (OOM).

## 2. Architecture & data flow (Focus 1, 9)

### Component topology model

ADP is a **streaming topology** of typed components wired in a blueprint, built on `saluki-core`:

- **Component kinds:** `source` → `transform`/`relay`/`decoder` → `encoder` → `forwarder`/`destination`.
- **Two data-model channel types** on edges (`topology/interconnect`, `built.rs:507-647`):
  `EventsBuffer` (source/decoder/transform → transform/encoder/destination) and
  `PayloadsBuffer` (encoder/relay → forwarder/decoder).
- **Build → spawn lifecycle** (`run.rs:238-239`): `TopologyBlueprint` is assembled, then
  `build()` then `spawn()`. The topology starts accepting data only after `health_registry.all_ready()`.

### The DogStatsD pipeline (production-critical path, `run.rs:593-698`)

Source `dsd_in` has three named outputs — `metrics`, `events`, `service_checks`:

```
dsd_in.metrics  → dsd_enrich (chained: dogstatsd_mapper) → dsd_prefix_filter
                → dsd_tag_filterlist → dsd_agg (aggregate) → dsd_post_agg_filter
                → metrics_enrich (host_enrichment + host_tags) → dd_metrics_encode → dd_out
dsd_in.metrics  → dsd_stats_out  (statistics tap)   [+ dsd_debug_log_out if enabled]
dsd_in.events   → events_enrich → dd_events_encode → dd_out
dsd_in.service_checks → service_checks_enrich → dd_service_checks_encode → dd_out
```

`dd_out` is the **Datadog forwarder** to the Datadog intake platform. Tag filtering happens
both before aggregation (`dsd_tag_filterlist`) and after (`dsd_post_agg_filter`). OTLP metrics
deliberately **skip aggregation** to avoid turning counters into rates (`run.rs:751-753`).

### Listeners & intake (Focus 1, 9)

DogStatsD source (`sources/dogstatsd/mod.rs`) listens on UDP (8125), TCP, **UDS datagram**, and
**UDS stream**. `SO_REUSEPORT` UDP autoscaling on Linux (`mod.rs:667-686`). DNS resolved via
`tokio::net::lookup_host` (skipped when `non_local_traffic=true`). For connectionless sockets,
framing/I/O errors **clear the buffer and continue** — a malformed packet never kills the socket
(`mod.rs:1283-1318`); per-frame parse errors are logged and skipped. Origin detection uses UDS
peer credentials; credential errors are counted but **do not drop the packet**.

### Egress: the Datadog forwarder (Focus 8, 9 — high subtlety)

`TransactionForwarder` (`common/datadog/io.rs`): one main I/O loop fans transactions to **one task
per resolved endpoint** (own bounded `mpsc(8)` channel + own retry queue). Per-endpoint Tower
stack: set URI/API-key → version headers → `concurrency_limit` → **`RetryCircuitBreakerLayer`** →
HTTP client.

- **Retry model is a circuit breaker, not inline retry.** When the policy says "retry," the
  breaker returns `Error::Open`, arms a shared **exponential-with-jitter** backoff, and the request
  is re-enqueued to a **low-priority** queue (`io.rs:468-474`). All calls are rejected until backoff
  elapses.
- **Retry classification** (`classifier/http.rs`): transport errors + 5xx + most 4xx retry;
  **400/401/403/413 are NOT retried** — treated as permanent failure and **dropped** (data loss).
- **Two-tier `PendingTransactions`:** high-priority in-memory `VecDeque` for fresh data, low-priority
  `RetryQueue` for retries + overflow; **oldest dropped on overflow** (bias to freshest data),
  counted as `track_queue_drops` telemetry — silent loss.
- **Disk persistence** (`retry/queue/persisted.rs`): if `forwarder_retry_queue_storage_max_size > 0`,
  the retry queue persists to disk; **init failure silently falls back to in-memory only** (durability
  downgrade). On shutdown, pending txns flush to disk; **without disk persistence they are dropped**.
  Retry-queue IDs are built to survive API-key rotation.

### Control plane / config (Focus 1, 9)

ADP registers as a Core Agent "remote agent" (`internal/remote_agent.rs`) and receives an
**authoritative config over a gRPC config stream** (snapshot + partial updates). Startup **blocks**
on `dynamic_config.ready().await` for the first config (`run.rs:119-121`, *no timeout shown*). Stream
end → reconnect after fixed **5s**. ADP exposes Status/Flare/Telemetry gRPC services back to the
Agent. **Note:** the grounding mentions a "METRIC_CONTROL relay," but no `METRIC_CONTROL` identifier
exists in the tree — config control flows through the generic snapshot/partial mechanism (open
question, see §9).

### Supervision (Focus 1, 3)

Erlang/OTP-style `Supervisor` (`runtime/supervisor.rs`) with `OneForOne`/`OneForAll` restart
strategies bounded by intensity/period. **Crucial split:** the **internal supervisor** (control
plane, internal telemetry, env/workload) *is* supervised/restartable, but **the primary data
topology is NOT** — `RunningTopology` spawns each data component into a `JoinSet` with **no restart**.
Any data component finishing → `wait_for_unexpected_finish` → **whole-process shutdown** (`run.rs:280-283`,
`running.rs:40-51`). Data-plane components are **fail-stop**; recovery is full-process restart (the s6
supervisor in the container restarts ADP on exit). Init failures never restart; only runtime failures do.

## 3. State management & persistence (Focus 2)

- **Aggregation state** (`transforms/aggregate/mod.rs`): a single `HashMap<Context, AggregatedMetric>`
  owned exclusively by the transform's task — **no locks, single-task ownership**, all mutation `&mut self`.
  Hard `context_limit` (default **1,000,000**) enforced at insert: a *new* context over the cap is
  **dropped** (existing contexts always merge). This is the central memory-determinism lever.
- **Zero-value counter keep-alive:** flushed counters emit zeros until `counter_expiry_seconds`
  (default 300s); kept-alive contexts still count against the limit.
- **Shutdown drop-by-design:** open (current-window) buckets are flushed on shutdown **only if
  `flush_open_windows`** (default **false**) — by default in-flight open-bucket data is dropped to
  avoid double counting on restart. `PassthroughBatcher` (pre-timestamped metrics) buffers up to
  `passthrough_idle_flush_timeout` (default 1s); drops events if its buffer stays full.
- **Restart loses all state:** supervisor restart re-runs `initialize()` from the spec template; a
  restarted component starts empty. (Mostly moot for data components, which aren't restarted.)
- **Two disk-backed subsystems only:** the forwarder retry queue (above) and **DogStatsD capture/replay**
  (`sources/dogstatsd/replay/`) — capture files written by a dedicated OS thread (protobuf
  `UnixDogstatsdMsg` records + `TaggerState` trailer), bounded `sync_channel`, lock-free `ArcSwapOption`
  for replay tagger state. Everything else is in-memory.
- **Object pools** (`pooling/`): `FixedSizeObjectPool` (Mutex+Semaphore, async-blocks when empty),
  `ElasticObjectPool` (min/max + background EWMA shrinker task — *if the shrinker future isn't driven,
  the pool never shrinks*), `OnDemandObjectPool` (allocates every time).
- **String interner** (`stringtheory/interning/fixed_size.rs`): fixed byte buffer, sharded
  `[Arc<Mutex<…>>; SHARD_FACTOR]`, manual reclamation/tombstoning; `try_intern` returns `None` when full.
  Interner determinism backs the context memory bound — *but* the resolver's default
  `allow_heap_allocations=true` lets full-interner strings **spill to the heap (effectively unbounded)**.

## 4. Concurrency model (Focus 3)

- **Backpressure is real and is the load-safety mechanism:** all inter-component edges are bounded
  `tokio::mpsc`; `Dispatcher::send` **awaits** on a full channel (`interconnect/dispatcher.rs:86-122`),
  so a slow downstream blocks upstream all the way back to the socket read loop. The DSD source calls
  `memory_limiter.wait_for_capacity().await` once per read (`mod.rs:1186`).
- **Fan-out hazards:** an output with N senders clones to N-1 and moves into the last, awaiting each
  **sequentially** — one slow consumer stalls delivery to the others. A disconnected output (zero
  senders) **silently discards** events (`events_discarded_total`). Send is **not atomic** across
  senders — partial delivery possible if a later sender errors.
- **Memory limiter** (`resource-accounting/limiter.rs`): a dedicated **std::thread** polls RSS every
  **250ms** and stores a backoff in an `AtomicU64`. Backpressure is **advisory/cooperative** (only
  tasks that call `wait_for_capacity` are throttled) and **capped at 25ms** of sleep, starting at 95%
  of limit. The checker `.expect()`s on the RSS read — **panics the limiter thread if `/proc` reads
  fail mid-run**, silently disabling all memory backpressure.
- **Interner reclamation** is loom-tested; the documented hazard (intern vs drop-last-ref race) is
  resolved by a mutex + refcount re-check; worst case is a duplicate entry, never corruption. This is
  the most concurrency-unsafe component in the bounded-memory story (raw pointers as `'static &str`
  keys into a buffer that gets overwritten on reclaim).
- **Health registry** (`health/mod.rs`): single `Arc<Mutex<…>>`; a single liveness `Runner` task;
  per-component probe over `mpsc(1)`, 5s probe timeout. A deadlocked/blocked component fails to answer
  → marked not-live, but **is not killed or restarted** — "blocked but alive" is an observable degraded
  state with no auto-recovery.
- **Config reload:** no in-place hot-reload of aggregate state was found; `live_config` is read once at
  endpoint-task init. Config change appears to be reload-by-restart (open question, §9).

## 5. Safety & liveness guarantees (Focus 4, 5) — candidate properties

Claimed/observed guarantees, each a property seed (full treatment in `property-catalog.md`):

**Safety (a bad thing never happens):**
1. Backpressure, never silent inter-component drop (the await-on-full design; tested in DSD UDP path).
2. Bounded memory — static startup bounds verification (`BoundsVerifier::verify`) rejects over-budget
   topologies in strict mode. *(But not enforced at runtime — see §7.)*
3. Aggregation output matches the Datadog Agent (counter→rate using bucket width as interval, half-open
   `[start, start+width)` buckets — explicitly "to match the Datadog Agent").
4. DDSketch relative-error guarantee: eps=1/128 (~0.78%), bin_limit=4096, bin count **must never exceed**
   4096; merge associative/commutative.
5. Config incompatibility is fatal at startup (high-severity incompatible non-default key → refuse to run).
6. Graceful shutdown completes within 30s without forceful kill (in-flight data drained).

**Liveness (a good thing eventually happens):**
1. Aggregate always flushes on its interval (default 15s) regardless of input; final flush on stream close.
2. Every passthrough/pre-timestamped metric is eventually forwarded.
3. Topology starts accepting data only after all components report ready.
4. Intake outage doesn't grow memory unbounded (retry queue caps) — but cap implies eventual drop on
   prolonged outage (tension to investigate).
5. After a transient intake outage clears, queued data is eventually delivered.

## 6. Existing test strategy & coverage gaps (Focus 7) — where Antithesis adds value

- **Unit tests:** dense (saluki-components ~618, saluki-core ~146, saluki-io ~102, ddsketch ~82).
  Forwarder I/O and circuit breaker have good targeted tests but all use the `tokio::time` **virtual
  clock** — no real interleaving/scheduling exploration.
- **Correctness suite** (`make test-correctness`, `bin/correctness/`): **diff-testing**. `panoramic`
  drives an **identical deterministic workload** (`millstone` load generator) into both the baseline
  (Datadog Agent) and ADP, captures both via `datadog-intake` (mock intake), normalizes to a shared
  `stele` representation, and diffs. Comparison is approximate (`RATIO_ERROR = 1e-8`); internal
  telemetry filtered out; fixed `FLUSH_WAIT = 32s` after load. 21 cases, all DSD/OTLP happy-path.
- **Integration suite** (`make test-integration`): real ADP in Docker, process-level assertions only
  (`log_contains`, `port_listening`, `http_check`, `process_exits_with`, etc.). 27 cases (startup,
  port binding, config-check exit codes, memory-mode exit behavior). Note: container s6 supervisor
  **restarts ADP on exit**, so the container never actually exits.

**Gaps (Antithesis's value):**
1. **No fault injection of any kind** — grep for fault/chaos/partition/kill/crash/inject/toxiproxy/netem
   found nothing. The intake is always healthy and reachable.
2. **Intake down / slow / 5xx-storm never tested system-level** — retry queue, circuit breaker, disk
   persistence, backpressure only unit-tested against in-process mocks.
3. **No process-crash/restart mid-flow** — disk-persisted retry queue recovery never tested across a
   real kill+restart.
4. **No network partition / connection reset / TLS handshake failure** under steady state.
5. **No timing/interleaving exploration** — diff testing is deterministic by design; concurrency bugs
   in multi-endpoint fan-out, shared circuit-breaker state, JoinSet handling are invisible to it.
6. **DogStatsD replay has zero suite coverage** despite being the newest, largest, untrusted-input feature.
7. **Memory-pressure behavior under real load is untested** beyond boolean exit-code cases.
8. **Config-stream drop/flap** not tested (one happy-path case `adp-config-stream`).

## 7. Failure & degradation modes + unproven assumptions (Focus 8, 11) — attack surfaces

The highest-value Antithesis targets. Several directly tension the headline guarantee.

1. **Memory limiting is DISABLED by default** (`MemoryMode::default() == Disabled`,
   `saluki-app/accounting.rs:37-40`) — no bounds validation *and* no runtime limiter unless the operator
   sets `memory_mode` + `memory_limit`. cgroup auto-detect only triggers when `DOCKER_DD_AGENT` is set.
2. **The bounded-memory guarantee is a startup assertion, not a runtime invariant** (Wildcard). Static
   `BoundsVerifier` sums *declared* firm limits; nothing measures actual allocation. The only runtime
   mechanism (`MemoryLimiter`) is advisory, cooperative, ≤25ms backoff, 250ms sampling, and the aggregate
   insert hot loop does **not** call `wait_for_capacity` — so the aggregation map + interner grow under
   pressure regardless; only the source is throttled.
3. **Firm bound is known-incomplete** (`aggregate/mod.rs:249-256`): a single metric with many distinct
   timestamped values isn't accounted for. Combined with default heap-fallback in the interner, declared
   bound and real RSS diverge arbitrarily under high-cardinality / many-timestamp load.
4. **Limiter thread panics if RSS becomes unreadable mid-run** (`.expect`, `limiter.rs:101-102`), silently
   removing all memory protection.
5. **≤25ms backoff + 250ms sampling may not prevent OOM** under bursty load — directly tests "won't crash
   under load."
6. **Source dispatch errors are logged and swallowed** (`dogstatsd/mod.rs:1667-1716`): a mid-buffer
   dispatch failure can mis-route remaining events (eventd/service-check events leaking into the metrics
   path), and the TODO admits it will "continue to fail to dispatch … until the process is restarted."
7. **Silent drops, no warning:** aggregate context-limit (one warn/episode), non-finite metric values
   (`non_finite_metric_values_are_silently_dropped`), interner-full contexts (config-dependent).
8. **Sub-second aggregate window → panic — FIXED UPSTREAM (PR #1772):** historically
   `bucket_width_secs = window.as_secs()` with no validation, so a value < 1s yielded `% 0`
   divide-by-zero and `step_by(0)` panics. The key is now `aggregate_window_duration_seconds`, typed
   `NonZeroU64`, and `bucket_width_secs` is `NonZeroU64` end-to-end (`aggregate/mod.rs:95-98,822-823`),
   so zero/sub-second values fail config parsing rather than reaching the divisor. Retained here as a
   closed wildcard; see `aggregate-no-panic-any-window` (now a regression tripwire).
9. **Two-clock hazard** (Wildcard): bucketing uses **wall clock** (`get_unix_timestamp`), flush cadence
   uses **monotonic** `tokio::interval`. A backward wall-clock jump makes the zero-value range empty
   (silent counter gap); a forward jump floods zero-value points and allocates a large `SmallVec`. No
   monotonicity guard. Also means a replayed capture buckets differently than when captured (the
   aggregator ignores per-record timestamps for non-timestamped metrics).
10. **NaN poisons a DDSketch** (`agent/sketch.rs:188-206`): `sum`/`avg` go NaN permanently; finiteness is
    guarded per-source (DSD codec), not at the sketch boundary — fragile if a new producer is added.
11. **All-non-finite packet → ghost metric** with a valid context but zero data points (interner/cache
    pressure, flows downstream) rather than a dropped packet.
12. **Replay reader treats corruption as clean EOF** (`replay/reader.rs:84-104`): a corrupt/oversized
    length prefix silently truncates the remaining record stream — false replay-fidelity confidence.
    ~25 unwrap/expect sites parsing untrusted capture files.
13. **Core Agent reachability assumed at startup:** ADP blocks indefinitely on `dynamic_config.ready()`
    with no visible timeout — if the Agent never sends config, ADP never starts the pipeline.
14. **Hot-path panics:** numerous `.expect("… should always exist")` on default outputs, events/
    service-check outputs, framing, sketch gamma/offset; `unreachable!("semaphore should never be
    closed")` in pools; metrics-recorder `panic!`. Each is a crash if its invariant is violated.
15. **UDP statsd-forward target:** on connect failure, packet forwarding is permanently disabled (no
    retry); send errors debug-logged and dropped.

## 8. Bug history & churn (Focus 6)

Churn hotspots (last ~300 commits): `cli/run.rs` (wiring), `sources/dogstatsd/mod.rs` (the ~2500-line
DSD source — biggest, most-changed file), `internal/control_plane.rs`, config-registry alignment files,
`common/datadog/io.rs` (forwarder), metrics encoder. Notable correctness fixes (good property seeds):
drop non-finite floats in codec; compensated summation for histograms; unitless histogram counts;
match agent timestamped-count sampling; **moved DSD prefix/filter in front of enrich** (pipeline
ordering bug); stabilize additional-endpoint retry-queue IDs (now load-bearing for API-key rotation).
**Most regression-prone area: DogStatsD replay** (`e88d04b10a`, +1765 LOC, draft, validated only with
`cargo check`, zero suite coverage, parses untrusted files). 123 TODO/FIXME/HACK markers, clustered in
saluki-components and saluki-io; safety-relevant ones around dispatch partial-failure, disk-limit
"bailing out," and undecided malformed-input error policy in the codecs.

## 9. Assumptions & open questions

- **METRIC_CONTROL relay naming:** grounding references a Remote Config METRIC_CONTROL relay; no such
  identifier exists in the source. Config control appears to use the generic snapshot/partial config
  stream. Confirm naming/mechanism against Confluence ("Tag Filter RC Relay Stress Test").
- **`interconnect_capacity` default** not yet read — needed to model backpressure precisely.
- **Config hot-reload semantics:** confirmed no in-place aggregate-state reload; is *any* runtime config
  change applied without restart? Affects whether config-reload-mid-flight is a real attack surface.
- **Saturated forwarder retry queue under prolonged outage:** drop vs block tension — confirm exact
  bound and whether memory stays bounded while data is eventually shed.
- **`persisted.rs` disk-full / partial-write / corrupt-file across crash** not deeply read (~47
  unwrap/expect sites) — relevant to the durability/data-loss property family.
- Discovery was read-only; no builds/tests executed. Test *counts* are annotation greps.

## 10. Implications for property selection & topology

Antithesis is strongest exactly where the existing suite is blind: **degraded/down/slow intake under
sustained load**, **process kill+restart with disk-persisted retry-queue durability**, **memory overload
to test the soft-backpressure-only design**, **malformed/corrupt replay capture parsing**, **clock skew
into aggregation**, and **timing/interleaving** in the forwarder and interner. The natural deployment
mirrors the correctness harness — ADP + a controllable mock intake + a deterministic load generator —
but adds Antithesis fault injection (network, process, clock) that the harness lacks. See
`property-catalog.md` and `deployment-topology.md`.
