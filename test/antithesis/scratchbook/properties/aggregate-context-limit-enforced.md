# aggregate-context-limit-enforced

**Family:** Resource Boundaries — bounded state / queues
**Status:** Verified against code at commit 042f41db3b. Property is expected to **HOLD** (this is
a real, enforced invariant) — it is the load-bearing memory-determinism lever for the aggregator.

## What led to the property

`sut-analysis.md` §3 and §7 identify the aggregation state map as "the central memory-determinism
lever." Unlike the interner (which spills to heap by default) and the advisory memory limiter
(off by default), the aggregate context limit is a **hard, always-on cap enforced at insert
time**, independent of any `memory_mode`. It is the one runtime memory bound that is not
advisory. Worth asserting precisely because it is the strongest claim ADP actually makes about
bounded aggregation state.

## The invariant and where it lives

Aggregation state is a single `HashMap<Context, AggregatedMetric>` (`hashbrown`) owned solely by
the transform task — no locks, all mutation `&mut self` (`transforms/aggregate/mod.rs`,
`AggregationState`). The cap:

- Default `aggregate_context_limit = 1_000_000` (`mod.rs:47-49` `default_context_limit`, field at
  `mod.rs:114-115`, stored in `AggregationState.context_limit` at `mod.rs:531`).
- Enforced in `AggregationState::insert` (`mod.rs:566-571`):
  ```rust
  if !self.contexts.contains_key(metric.context()) && self.contexts.len() >= self.context_limit {
      self.context_limit_breached = true;
      return false;   // new context over the cap is DROPPED
  }
  ```
  Critically the guard is gated on `!contains_key`: an **existing** context always proceeds to
  merge (lines 573+), so the cap only ever rejects *new* contexts, never breaks aggregation of
  already-tracked ones.
- The caller (`mod.rs:375-384`) treats `insert == false` as a drop: increments
  `events_dropped` telemetry and logs **one** warning per breach episode (gated on
  `was_breached` so it doesn't spam).
- Recovery: `context_limit_breached` is cleared once `contexts.len() < context_limit` again
  (`mod.rs:714-715`), e.g. after a flush evicts contexts.

So the precise invariant: **live context count never exceeds `aggregate_context_limit`; over-cap
*new* contexts are dropped-and-counted; *existing* contexts always merge.**

## Failure scenario (Antithesis)

Flood DSD with far more than `aggregate_context_limit` distinct contexts (set the limit low,
e.g. 1000, to make the boundary reachable within a run). Assert the map size never exceeds the
cap and that drops are counted. Antithesis adds value over the deterministic correctness harness
by interleaving the flood with **flush timing** and **counter zero-value keep-alive**: zero-value
counters kept alive after flush still count against the limit (`sut-analysis.md` §3), so the
boundary can be hit by keep-alives, not just fresh contexts — a timing-sensitive interaction the
fixed-clock harness won't explore. Also tests the recovery edge: does `len()` correctly dip below
the cap after a flush and re-admit new contexts (clearing `context_limit_breached`)?

## Suggested assertions (NET-NEW — see existing-assertions.md: NO SDK assertions exist)

- `Always(state.contexts.len() <= context_limit)` anchored in `insert`/after-insert in
  `transforms/aggregate/mod.rs`. Safety: must hold on every check. Honest — there is no code path
  that grows the map past the cap, so this is a true `Always`, not an aspirational one.
- `AlwaysOrUnreachable(contains_key(ctx) || len < limit ⇒ insert succeeds)` — i.e. an existing
  context is *never* dropped by the cap. Captures the "existing always merges" half. Use
  AlwaysOrUnreachable because the merge-of-existing path may not be exercised in a given run.
- `Sometimes(context_limit_breached == true)` — proves the workload actually reaches the boundary
  (otherwise the `Always` above is vacuously true). Liveness/reachability of the interesting state.
- `Sometimes(events_dropped incremented due to context limit)` — proves the drop is counted, not
  silent-and-uncounted.

This is a strong candidate for a true SUT-side `Always` because the bound is a local, lock-free,
single-owner invariant — exactly the kind Antithesis `Always` is designed for.

## Configuration dependencies

- `aggregate_context_limit` (default 1,000,000). For a finite-duration run, must be lowered so
  the boundary is reachable.
- `counter_expiry_seconds` (default 300): kept-alive zero-value counters occupy context slots
  until expiry, affecting how easily the cap is reached and when `len()` dips below it.
- `aggregate_window_duration` / primary flush interval (default 15s): flush cadence drives when
  contexts are evicted and the breach flag clears.

## Open questions

- The cap counts *contexts*, not *bytes*. A single context with many distinct timestamped values
  is one map entry but unbounded value memory (cross-ref `rss-bounded-under-cardinality`). So this
  property bounds entry count, NOT aggregator memory. Prose must not overclaim "bounded memory."

## Investigation Log

#### Zero-value keep-alive counters: storage location and flush-time `contexts.len()` behavior
- **Examined**: `lib/saluki-components/src/transforms/aggregate/mod.rs`:
  `AggregatedMetric` struct (522-525), `AggregationState` (529-558), `insert` (566-610),
  `flush` (612-719), and the dedicated test `context_limit_with_zero_value_counters`
  (1104-1157). Also the module doc on zero-value counters (71-75) and `is_empty` (562-564).
- **Found (a) — storage**: There is **NO separate structure** for zero-value/keep-alive
  counters. An idle counter remains as a normal entry in the single
  `contexts: HashMap<Context, AggregatedMetric>` map (529). On flush, closed-bucket values are
  split off and emitted (682-695) leaving `am.values` empty; the entry is only removed if
  `am.values.is_empty() && should_expire_if_empty` (697). For counters,
  `should_expire_if_empty` is **false** until `last_seen + counter_expire_secs < current_time`
  (649-654), so a kept-alive counter is an empty-valued entry that **stays in `contexts`** and
  on each subsequent flush gets a fresh `0.0` bucket merged in (661-672) and re-emitted.
- **Found (a) — cap check counts them**: `insert` rejects a new context when
  `!contexts.contains_key(..) && contexts.len() >= context_limit` (568). Since idle counters
  are live entries in `contexts`, they **count toward `context_limit`** at the cap check. The
  test at 1104-1157 asserts exactly this: with `context_limit = 2`, two counters that have gone
  to zero-value mode still block insertion of a third (`assert!(!state.insert(... metric3 ...))`,
  1138), and the third only succeeds (1152) after the two expire and are dropped (1148).
- **Found (b) — when `len()` drops**: `contexts.len()` drops **during flush**, in the removal
  pass at 703-707 (`for context in self.contexts_remove_buf.drain(..) { self.contexts.remove(...) }`),
  only for entries that were marked at 697-700 (empty values AND eligible to expire). The breach
  flag is cleared right after if `contexts.len() < context_limit` (714-716). So flush DOES remove
  expired/all-closed non-counter contexts and expired counters; it does NOT remove kept-alive
  (not-yet-expired, empty) counters. The recovery edge (re-admitting new contexts) is reachable
  only after kept-alive counters actually expire (`counter_expire_secs`, default 300s) or a
  non-counter context flushes empty.
- **Not found**: No code path tracks zero-value counters outside `contexts`; no separate counter
  toward the limit.
- **Conclusion**: RESOLVED. The true live bound is exactly **`context_limit`** (map entries),
  NOT `context_limit + zero_value_count` — kept-alive zero-value counters are ordinary map
  entries already included in the `len()` cap check. The `Always(contexts.len() <= context_limit)`
  assertion target is correct as-is. Caveat (already noted): this bounds entry *count*, not bytes;
  with `counter_expire_seconds` default 300s a flood of sparse counters keeps `len()` pinned near
  the cap for ~5 min, which delays but does not breach the bound.
