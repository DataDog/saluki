# interner-full-bounded

**Family:** Resource Boundaries — memory / exhaustion
**Status:** Verified against code at commit 042f41db3b. Two-mode property:
- `allow_heap_allocs = false`: bounded + deterministic drop — **expected to HOLD**.
- `allow_heap_allocs = true` (the DEFAULT): memory no longer bounded — **expected to FAIL** the
  bounded-memory reading.

## What led to the property

Interner determinism is the foundation of the context memory bound (`sut-analysis.md` §3). The
fixed-size interner has a hard byte capacity; the question is what happens when it fills. The
behavior pivots entirely on one config flag, and the **default flips it to the unbounded
branch** — making this the single highest-leverage config knob in the bounded-memory story.

## Behavior in code

Resolution path `ContextResolver::intern` (`lib/saluki-context/src/resolver.rs:339-353`):
```rust
s.try_cheap_clone()                                  // inlineable/cheap strings escape entirely
 .or_else(|| self.interner.try_intern(s.as_ref())..) // fixed buffer; None when full
 .or_else(|| self.allow_heap_allocations.then(|| {   // HEAP FALLBACK
     self.telemetry.intern_fallback_total().increment(1);
     MetaString::from(s.as_ref())                     // unbounded heap alloc
 }))
```

- **Interner-full is deterministic.** `FixedSizeInterner` shard `try_intern`
  (`lib/stringtheory/src/interning/fixed_size.rs:462-494`) returns `None` when neither a reclaimed
  entry nor remaining buffer capacity can fit the string (`required_cap <= self.available()` else
  `None`, lines 489-493). Also `None` if the string exceeds the packed length/capacity max
  (lines 465-467). No allocation, no panic — just `None`.
- **Heap-disallowed => metric dropped.** When `allow_heap_allocations == false`, the final
  `or_else` yields `None`, so `intern` returns `None`, `create_context` returns `None`
  (`resolver.rs:373` `let context_name = self.intern(name)?;`), `resolve` returns `None`, and
  `handle_metric_packet` returns `None` (`sources/dogstatsd/mod.rs:1565-1580`): "We failed to
  resolve the context, likely due to not having enough interner capacity." The metric is dropped
  deterministically; no memory grows. This is exactly what the unit test
  `no_metrics_when_interner_full_allocations_disallowed`
  (`sources/dogstatsd/mod.rs:1808-1834`) asserts (using a noop/zero-size interner + a name longer
  than the 31-byte inline limit so it can be neither inlined nor interned).
- **Heap-allowed (DEFAULT) => unbounded.** `allow_heap_allocations` builder default is `true`
  (`resolver.rs:258` `unwrap_or(true)`, doc `:179-190` calls it "effectively unlimited"), and DSD
  config `dogstatsd_allow_context_heap_allocs` defaults `true`
  (`sources/dogstatsd/mod.rs:149-151, 402-406`; wired at `sources/dogstatsd/resolver.rs:38,56,64`
  for primary, no_agg, and tags resolvers, all sharing one interner at `resolver.rs:40`). On a
  full interner every over-cap context spills to heap, bumping `intern_fallback_total`, and RSS
  grows without bound.

## Failure scenario (Antithesis)

Set a small `dogstatsd_string_interner_size_bytes` and flood high-cardinality contexts so the
interner fills.
- Mode A (`dogstatsd_allow_context_heap_allocs: false`): assert that once the interner is full,
  metrics with un-internable names/tags are dropped and **no heap fallback occurs** — memory
  bounded. Antithesis timing exploration probes the interner reclamation/tombstoning path
  (loom-tested per existing-assertions.md, raw-pointer `'static &str` keys) under concurrent
  intern-vs-drop, where the worst documented case is a duplicate entry (more pressure), never
  corruption.
- Mode B (default `true`): assert `intern_fallback_total` climbs and RSS escapes the interner
  budget — the bounded-memory guarantee is void. This is the more important finding because it is
  the **default** posture.

## Suggested assertions (NET-NEW — see existing-assertions.md: NO SDK assertions exist)

- Heap-disallowed branch: `AlwaysOrUnreachable(interner_full ⇒ metric dropped, no heap alloc)`.
  AlwaysOrUnreachable because "interner full" is a rare/optional path that may not occur in every
  run; when it does occur the drop-not-allocate behavior must hold.
- `Sometimes(try_intern returned None)` / `Sometimes(interner reported full)` — proves the
  workload actually exhausts the interner; otherwise the above is vacuous.
- Heap-allowed branch: `Sometimes(intern_fallback_total > 0)` — proves the unbounded spill path is
  reachable under default config (the finding). Pair with the RSS check from
  `rss-bounded-under-cardinality`.
- A counter on `intern_fallback_total` already exists (`resolver.rs:349`) — a natural anchor for
  the `Sometimes`, but it is telemetry, not an assertion, so the SDK assertion is still net-new.

**SUT-side instrumentation strongly preferred:** distinguishing "interned" vs "inlined" vs
"heap-fallback" vs "dropped" requires reading internal resolver state. A workload-only checker
sees dropped metrics (missing at intake) but cannot tell a heap-fallback (memory bug) from a
clean intern (correct), nor an interner-full drop from a parse drop.

## Configuration dependencies

- `dogstatsd_allow_context_heap_allocs` (default **true** — the unbounded branch).
- `dogstatsd_string_interner_size_bytes` / `dogstatsd_string_interner_size` (interner capacity;
  effective default 2 MiB, `sources/dogstatsd/mod.rs:1888-1890`). Both resolvers share one
  interner of this size (`sources/dogstatsd/resolver.rs:40`), so two resolvers draw from the same
  buffer.
- Inlining: strings <= 31 bytes are inlined by `MetaString` (`try_cheap_clone`) and bypass the
  interner entirely — the workload must use long names/tags to actually pressure the interner.

## Open questions

- The fixed-size interner reclaims/tombstones entries when interned strings are dropped. Under
  steady high-cardinality churn, does reclamation keep pace, or does fragmentation make
  `try_intern` return `None` even below nominal byte capacity? Matters: if fragmentation causes
  premature "full," the heap-disallowed mode drops metrics earlier than the configured budget
  implies (more data loss), and the heap-allowed mode spills sooner.
- The tags resolver also has `with_heap_allocations` (`sources/dogstatsd/resolver.rs:45`) and
  shares the interner. Is the bound the sum across name+tag interning of both resolvers? Affects
  the byte budget the assertion measures against.

## Investigation Log

#### Default of `dogstatsd_allow_context_heap_allocs` and whether bounded mode is ever shipped
- **Examined**: `lib/saluki-components/src/sources/dogstatsd/mod.rs:149-151`
  (`default_allow_context_heap_allocations`), `:403-406` (serde field + rename), `:438`;
  `sources/dogstatsd/resolver.rs:38,45,56,64` (resolver wiring); `lib/saluki-context/src/resolver.rs`
  `with_heap_allocations` (187-188) and the default fallback `.unwrap_or(true)` (258, 663);
  `config_registry/datadog/dogstatsd.rs:8,392`; grep of all `with_heap_allocations(false)` in non-test code;
  searched shipped configs (`dist/`, `config/`, all `*.yaml`/`*.toml`) for `heap_alloc`.
- **Found (a) — default**: `const fn default_allow_context_heap_allocations() -> bool { true }`
  (`mod.rs:149-151`), applied via `#[serde(rename = "dogstatsd_allow_context_heap_allocs",
  default = "default_allow_context_heap_allocations")]` (`mod.rs:403-406`). The resolver builder
  default also independently falls back to `true` (`resolver.rs:258` `unwrap_or(true)`). So default
  is **true = unbounded heap-allocation (spill) mode**.
- **Found (b) — bounded mode is test-only**: The only call sites of `with_heap_allocations(false)`
  are inside `#[cfg(test)] mod tests` in `sources/dogstatsd/mod.rs` (lines 1820 and 1841, module
  begins `#[cfg(test)]` at 1736 — tests `no_metrics_when_interner_full_allocations_disallowed`
  and `metric_with_additional_tags`). Production wiring (`resolver.rs:38,45,56,64`) passes
  `config.allow_context_heap_allocations` straight through. No shipped YAML/TOML config sets it to
  false. There is no default/code path that forces bounded mode; it is **opt-in via config only**.
- **Not found**: Any release/default config or non-test code path that sets
  `dogstatsd_allow_context_heap_allocs = false`.
- **Conclusion**: RESOLVED. Default is **true** (unbounded spill). Bounded mode (heap-disallowed,
  interner-is-a-hard-cap) is **opt-in / test-only** — never shipped by default. The
  realistic default-config property is "interner spills to heap (memory unbounded by the interner)";
  the hard-bounded property only holds when an operator explicitly sets the flag false. Property
  framing should remain explicitly two-mode and label the bounded branch as opt-in.
