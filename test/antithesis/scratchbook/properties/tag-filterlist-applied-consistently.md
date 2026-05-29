# tag-filterlist-applied-consistently

## Origin

Coverage-gap analysis of the DogStatsD transform chain. `tag_filterlist` removes/retains tags
per-metric and claims Datadog-Agent equivalence, but has two correctness subtleties no property
covers: (1) it filters **only Counter and sketch** metrics — gauges/rates/sets are deliberately
**not** filtered (`mod.rs:235-237`); and (2) it serves results from a **context cache** keyed by the
original context, which must always agree with a fresh filter computation and must be invalidated on
config reload. A wrong metric-type predicate, or a stale cache entry, silently retains tags the
operator intended to drop (cardinality/PII leak) or drops tags that should remain.

## Code paths

- `bin/agent-data-plane/src/components/tag_filterlist/mod.rs`
  - **Type gate:** `if metric.values().is_sketch() || matches!(metric.values(),
    MetricValues::Counter(_))` (`mod.rs:235-237`). Only sketches + counters are considered; every
    other metric type bypasses filtering entirely.
  - **Filter logic:** `filter_metric_tags` (`mod.rs:299-318`) — looks up `filters.get(name)`; on a
    rule, `retain_tags` + `retain_origin_tags` with `should_keep_tag` (`mod.rs:289-291`,
    `is_exclude != names.contains(tag.name())`). Filters **both instrumented and origin tags**.
  - **Context cache:** `context_cache: Cache<Context, Option<(Context, usize)>>`, capacity 100_000
    (now operator-tunable via the `aggregator_tag_filter_cache_capacity` config key, PR #1771; eviction
    just forces recompute, not incorrectness), TTI 30s (`mod.rs:40-42,204-214`). Hit path replaces
    context with the cached filtered context
    (`mod.rs:240-247`); miss path computes, then caches `None` (NoChange) or `Some((filtered, n))`
    (`mod.rs:248-263`).
  - **Compile/merge rules:** `compile_filters` (`mod.rs:111-140`) — same name+action unions tag sets;
    conflicting actions → **exclude wins**; empty `metric_name` entries are dropped.
  - **Reload:** on `watch_for_updates("metric_tag_filterlist")` (`mod.rs:222,274-277`) it rebuilds
    `self.filters` **and** `self.context_cache = build_context_cache()` (cache invalidated on reload —
    good, but see `filter-config-reload-correct` for the lag/partial/clear hazards that gate whether
    the *new* filters are even applied).
  - Agent reference comments cite `pkg/aggregator/time_sampler.go` equivalence for the sibling
    post-aggregate filter; the tag filter targets per-metric tag stripping.

## Failure scenario

- **Cache divergence:** the cached filtered context for name X disagrees with a fresh
  `filter_metric_tags(X)` — e.g. a metric whose tagset differs but collides on the cache key, or a
  cache entry that survives a config reload window. The cached (stale/wrong) tagset is applied,
  silently producing a different tag set than the current rules dictate.
- **Type-gate divergence:** a metric type the Agent *would* filter (or would *not*) is treated
  differently by ADP's Counter+sketch-only gate, so a tag the operator listed is retained on (say) a
  gauge that ADP never filters — silent cardinality/PII leak; or dropped on a type the Agent leaves
  alone.
- **Merge divergence:** include/exclude conflict for the same name resolves differently than the
  Agent ("exclude wins" + first-exclude-wins, `mod.rs:127-135`), changing which tags survive.

All silent — only `tag_filterlist_*` telemetry counters move; no error, no drop.

## Property

- **Type:** Safety.
- **Invariant:**
  - `Always(cache-hit filtered tags == fresh filter_metric_tags result)` — SUT-side: on a cache hit,
    recompute and assert the cached context's tags equal the freshly-filtered tags. Catches stale/
    colliding cache entries.
  - `Always(only Counter and sketch metrics ever have tags removed by this transform)` /
    `AlwaysOrUnreachable(a gauge/rate/set leaves tag_filterlist with tags identical to input)` —
    pins the deliberate type gate so a future refactor that widens/narrows it is caught.
  - Differential (optional, ride Add-on 2): `Always(post-filter (name, tags) within ratio of Agent
    time-sampler tag filtering)` for the same `metric_tag_filterlist` config and a corpus spanning all
    metric types — the strongest equivalence check.
  - `Sometimes(a tag was removed)` + `Sometimes(cache hit served a filtered result)` for non-vacuity.
- **Antithesis angle:** sustained mixed-type metric load (counters, sketches, gauges, rates, sets)
  with overlapping names that stress the context cache (eviction at 100_000 / TTI 30s), interleaved
  with config reloads (compose with `filter-config-reload-correct`) and node-throttling to widen the
  reload-vs-apply window. Timing exploration surfaces cache entries that outlive the reload that
  should have invalidated them.
- **Priority:** Medium (High if the Counter+sketch type gate is found to diverge from the Agent).

## Config dependencies

- `metric_tag_filterlist` (array of `{metric_name, action: include|exclude, tags:[...]}`); set
  identically on the Agent baseline for the differential facet.
- Dynamic config enabled (remote-agent mode) only for the reload-interaction facet; the cache-vs-fresh
  and type-gate invariants can run with a static config in the primary topology.
- Corpus must include **all** metric value types to exercise the type gate.

## Open Questions

- Does the Datadog Agent restrict tag filtering to the same Counter+sketch subset, or does it filter
  other metric types too? Pivotal for the type-gate invariant and the differential facet.
  `(needs Agent-source confirmation)`
- The context cache is keyed by the **full original `Context`** (`mod.rs:204`); can two metrics with
  the same name+tags but different *origin tags* collide and get the wrong filtered result, given
  filtering also touches origin tags (`mod.rs:308`)? Needs a cache-key audit.
- Cache TTI is 30s; under a config reload the cache is rebuilt (`mod.rs:276`), but only **when the
  reload event actually fires** — if the reload is Lagged-dropped (`filter-config-reload-correct`
  Hazard 1) the cache is *not* rebuilt and stale filtered contexts persist up to TTI. Confirm this
  compound failure.
- Does "exclude wins on conflict, first-exclude-wins" (`mod.rs:127-135`) match the Agent's merge
  semantics for duplicate metric-name entries?

### Investigation Log

- Examined: full `tag_filterlist/mod.rs` incl. the type gate (`235-237`), cache hit/miss/insert
  (`240-263`), `filter_metric_tags`/`should_keep_tag` (`289-318`), `compile_filters` merge rules
  (`111-140`), and the reload arm rebuilding both `filters` and `context_cache` (`274-277`).
- Found: a Counter+sketch-only type gate, a 100k-entry / 30s-TTI context cache on the hot path, and
  exclude-wins merge — all claiming Agent equivalence with only self-consistent unit tests. The
  cache-vs-fresh and type-gate facets are SUT-side invariants; the equivalence facet rides the
  differential harness. Distinct from `filter-config-reload-correct` (which owns the *reload
  mechanism* hazards) — this property owns the *filter application* correctness.
