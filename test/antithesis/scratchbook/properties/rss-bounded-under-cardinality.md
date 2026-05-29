# rss-bounded-under-cardinality

**Family:** Resource Boundaries — memory
**Status:** Verified against code at commit 042f41db3b. Property is expected to **FAIL by design** under default configuration.

## What led to the property

The ADP headline guarantee (Confluence, "What Comes After DogStatsD") is **"ADP will not crash
under load, losing customer data,"** and the product is marketed on **deterministic resource
usage** (`docs/agent-data-plane/index.md:1-6`). The most direct reading of that guarantee is:
under a high-cardinality tag flood (or a single metric with many distinct timestamped values),
the process RSS stays within the operator's configured memory grant. This property tests that
runtime claim directly — not the static startup bound.

## Why it is expected to fail (the design gaps)

The runtime memory story is built from several independently-weak mechanisms; under default
config most are off or advisory:

1. **Memory limiting is DISABLED by default.** `MemoryMode::default() == Disabled`
   (`lib/saluki-app/src/accounting.rs:37-40`). In `initialize_memory_bounds`
   (`accounting.rs:149-181`), `Disabled` => `limiter_grant = None` => `MemoryLimiter::noop()`
   (`accounting.rs:174-178`). A noop limiter's `wait_for_capacity` returns immediately
   (`lib/resource-accounting/src/limiter.rs:73-77, 83-88`). So out of the box there is **no
   runtime backpressure at all**. A limit only auto-appears via cgroups when `DOCKER_DD_AGENT`
   is set (`accounting.rs:108-121`).

2. **Even when enabled, backpressure is advisory and tiny.** `MemoryLimiter::new`
   (`limiter.rs:42-68`) starts backoff at **95% of limit** (`backoff_threshold = 0.95`,
   line 47) and caps sleep at **25ms** (`backoff_max = Duration::from_millis(25)`, line 49).
   The checker thread samples RSS only every **250ms** (`limiter.rs:120`). Under a burst, RSS
   can blow well past the limit between samples while the worst penalty any cooperating task
   pays is a 25ms sleep.

3. **Only the source cooperates; the allocating hot path does not.** Backpressure is
   cooperative — it only throttles tasks that call `wait_for_capacity`. The DSD source calls it
   once per read loop iteration (`lib/saluki-components/src/sources/dogstatsd/mod.rs:1186`). But
   the **aggregate transform never references the memory limiter at all** (grep of
   `transforms/aggregate/mod.rs` for `wait_for_capacity`/`memory_limiter` => no matches). The
   aggregation `HashMap<Context, AggregatedMetric>` and the string interner both grow under
   pressure regardless of backoff; throttling the socket read does not stop the map from
   growing once packets are in flight.

4. **The interner spills to the heap by default => "effectively unlimited."** Context
   resolution interns into a fixed-size buffer, but on a full interner it falls back to a heap
   allocation when `allow_heap_allocations` is true (`lib/saluki-context/src/resolver.rs:339-353`).
   The builder defaults this to `true` (`resolver.rs:258` `unwrap_or(true)`, doc `:186-190`),
   and the DSD config default `dogstatsd_allow_context_heap_allocs` is also `true`
   (`sources/dogstatsd/mod.rs:149-151, 402-406`), wired through
   `sources/dogstatsd/resolver.rs:38,56,64`. The doc string is explicit: heap fallback means
   the interner memory is **"effectively unlimited"** (`resolver.rs:182-184`).

5. **The declared firm bound is known-incomplete.** `AggregateConfiguration::specify_bounds`
   carries a TODO (`transforms/aggregate/mod.rs:247-272`) admitting that a single metric with
   **many distinct timestamped values** is not accounted for — only `aggregate_context_limit`
   entries of `sizeof(Context)+sizeof(AggregatedMetric)` are summed. A many-distinct-timestamp
   flood inflates each `MetricValues` `SmallVec` beyond the modeled size. So even the static
   bound diverges from real RSS under exactly the workload this property injects.

Net: the static `BoundsVerifier` is a **startup assertion, not a runtime invariant**, and the
runtime mechanism is off-by-default / advisory / non-cooperative on the path that actually
allocates. RSS staying within grant is not guaranteed under high-cardinality load.

## Failure scenario (Antithesis)

Deploy ADP with `memory_mode: strict` (or `permissive`) and an explicit `memory_limit`, plus a
load generator (millstone-style) that emits a high-cardinality tag flood and/or a single metric
name with many distinct timestamped values into DogStatsD. Sample real process RSS. Expect RSS
to exceed the grant (interner heap spill + unaccounted timestamped values + aggregate map
growth that the 25ms/250ms advisory backoff cannot arrest). Antithesis timing/scheduling
exploration makes the 250ms-sample / burst race observable in a way the deterministic
correctness harness (fixed clock, healthy intake) cannot.

## Suggested assertion (NET-NEW — see existing-assertions.md: NO SDK assertions exist anywhere)

- A workload-side or SUT-side check reading `Querier::resident_set_size()` (the same source the
  limiter uses, `limiter.rs:44,100-102`): `Always(actual_rss <= grant.effective_limit_bytes() *
  tolerance)`. Honest framing: this assertion is **expected to fail** under default config and
  under high-cardinality load even with the limiter on — that failure is the finding.
- **SUT-side instrumentation beats a workload-only checker here.** A `Sometimes(backoff_applied
  && rss_still_climbing)` anchored in the limiter, plus an assertion that the aggregate insert
  path observes capacity, would localize *why* RSS escapes (advisory-only, wrong path
  cooperating) rather than just that it does. A pure workload RSS probe sees the symptom, not
  the mechanism.

## Configuration dependencies

- `memory_mode` (default `disabled` => no limiter), `memory_limit` (default unset),
  `enable_global_limiter` (default true, but moot when mode is disabled),
  `memory_slop_factor` (default 0.25).
- `dogstatsd_allow_context_heap_allocs` (default true => unbounded interner spill).
- `dogstatsd_string_interner_size_bytes` / `dogstatsd_string_interner_size` (interner capacity;
  default 2 MiB).
- `aggregate_context_limit` (default 1,000,000) bounds map entries but not per-entry value count.

## Open questions

- What RSS tolerance band over `effective_limit_bytes` is "acceptable"? The 95%-threshold +
  25ms-backoff + 250ms-sample design implies meaningful overshoot is expected even in the happy
  case; the assertion threshold determines whether this reports as a real violation or noise.
  Matters because too-tight a bound makes the property flap; too-loose hides the design gap.
- With `allow_context_heap_allocs=false`, does RSS actually become bounded, or do the aggregate
  map and per-context value `SmallVec`s still escape the grant under the many-timestamp flood?
  This decides whether the property is "fails always" or "fails only with heap spill enabled."
- Is there any RSS ceiling enforced by the container/cgroup that would OOM-kill ADP before the
  assertion fires? If so the observable outcome is a process crash (a different, arguably worse,
  violation of the same guarantee) rather than an RSS-over-grant reading.

## Investigation Log

#### What enables a memory limit by default; does cgroup auto-detection require `DOCKER_DD_AGENT`?
- **Examined**: `lib/saluki-app/src/accounting.rs`: `MemoryBoundsConfiguration` (55-94),
  `try_from_config` (101-130, the cgroup auto-detect block at 107-121), `get_initial_grant`
  (133-138), `initialize_memory_bounds` (149-181); `MemoryMode` default
  (`memory_mode` serde `default` => `MemoryMode::Disabled`, fields/doc near 90-94);
  call site `bin/agent-data-plane/src/cli/run.rs:205-206,239`; the `DOCKER_DD_AGENT` reference in
  `components/apm_onboarding/install_info.rs:90`.
- **Found — cgroup auto-detect is gated on `DOCKER_DD_AGENT`**: In `try_from_config`, cgroup
  detection runs **only if** `config.memory_limit.is_none()` AND
  `env::var("DOCKER_DD_AGENT")` is `Ok` AND its value is non-empty (107-110). Only then does
  `CgroupMemoryParser.parse()` run and, on success, set `config.memory_limit` (111-118). So with
  no explicit `memory_limit` config and no non-empty `DOCKER_DD_AGENT`, `memory_limit` stays
  `None`.
- **Found — two independent gates make the limiter a no-op by default**:
  (1) `memory_mode` defaults to `MemoryMode::Disabled`; in `initialize_memory_bounds`, Disabled
  logs "Memory limiting disabled." and yields `None` grant (158-161). (2) Even in
  Permissive/Strict, a `None` `memory_limit` logs "No memory limit set ... Skipping memory bounds
  verification." and yields `None` (167-170). A `None` grant => `MemoryLimiter::noop()` (174-178).
- **Not found**: Any default config shipping `memory_mode != disabled` or a default
  `memory_limit`; any auto-detect path that doesn't require `DOCKER_DD_AGENT`.
- **Conclusion**: RESOLVED. By default there is **no enforced memory limit**: `memory_mode` is
  `Disabled` and `memory_limit` is unset. A limit becomes active only if the operator sets
  `memory_limit` (or `memory_mode` Permissive/Strict + a limit) explicitly, OR a non-empty
  `DOCKER_DD_AGENT` env var is present AND cgroup parsing succeeds (which only populates
  `memory_limit`, still requiring a non-Disabled `memory_mode` to actually limit). For the
  Antithesis run this means: unless the harness sets `DOCKER_DD_AGENT` (non-empty) with a cgroup
  memory limit, OR sets `memory_mode`+`memory_limit`, the limiter is a noop and RSS is unbounded by
  ADP itself — confirming this property FAILs by design under default config. The harness should
  verify whether its container sets `DOCKER_DD_AGENT`, since that silently changes the baseline.

#### With `allow_context_heap_allocs=false`, does RSS actually become bounded, or do per-value `SmallVec`s still escape? `(partial)`

- **Examined**: `transforms/aggregate/mod.rs` `AggregationState` insert/cap (`:566-571`) and
  `specify_bounds` (`:247-273`, incl. the self-documented gap comment `:249-256`); the
  `aggregate-context-limit-enforced` finding that the live context count is bounded to exactly
  `context_limit`; `MetricValues` storage for multi-value / many-timestamp metrics.
- **Found**: the *context count* is firmly bounded to `context_limit` (new contexts over the cap are
  dropped) — confirmed. But the declared firm bound is `context_limit * (sizeof(Context) +
  sizeof(AggregatedMetric))`, which `specify_bounds` itself admits does **not** account for a single
  metric carrying many distinct timestamped values (per-value `SmallVec`/sketch-sample growth). So
  bounding the context count does **not** bound per-value memory.
- **Not found**: a measured figure for how much per-value memory a single many-timestamp context can
  consume under `allow_context_heap_allocs=false` — i.e. whether interner-bounded mode actually caps
  total RSS or merely caps the number of contexts while per-context value vectors grow on the heap.
  This needs a runtime measurement (a workload feeding one context thousands of distinct timestamps
  and reading RSS), not a static read.
- **Conclusion**: `(partial)` — context count is bounded exactly; per-value memory is unaccounted and
  remains an empirical question for the workload to settle. Does not change the property statement
  (RSS-within-grant), only sharpens *why* it may still fail even with heap allocs disabled.
