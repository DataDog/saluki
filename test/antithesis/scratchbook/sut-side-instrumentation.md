---
sut_path: /home/ssm-user/src/saluki
commit: a31a487f1b8f676391dae88064c7d3f64f202245
updated: 2026-06-01
external_references:
  - path: https://antithesis.com/docs/properties_assertions/assertions.md
    why: SDK assertion macro surface and semantics — grounds the macro-choice guidance below.
  - path: https://antithesis.com/docs/best_practices/sometimes_assertions/
    why: Sometimes-assertion / anti-vacuity doctrine applied to the reachability anchors.
---

# SUT-side instrumentation plan

This file pins every "SUT-side needed" callout in `property-catalog.md` to a **verified insertion
point at the current HEAD**, names the exact macro, condition, and message, and records the
cargo-feature plumbing each site costs. It is the bridge between the conceptual catalog and the
`antithesis-workload` skill — the catalog says *what* invariant; this says *where in saluki* the
assertion goes and *what it costs to wire*.

Scope of this pass: **in-SUT assertions only** — predicates that live in saluki production code behind
a cargo feature. Workload-side anchors, the external liveness commands, and the differential diff
harness stay out (see "What stays out of saluki" below).

Two existing in-SUT sites are the precedent and are NOT re-proposed: `main.rs:100`
(`assert_reachable!` bootstrap probe) and `io.rs:555` (`assert_sometimes!` forwarder 2xx). See
`existing-assertions.md`.

## The macro surface — and what saluki can and cannot express

The Rust SDK (`antithesis_sdk`, rev `6829a94`) exposes exactly these assertion macros:

- **Point-in-time predicates:** `assert_always!`, `assert_always_or_unreachable!`,
  `assert_sometimes!`, `assert_reachable!`, `assert_unreachable!`.
- **Numeric guidance:** `assert_always_greater_than!`, `assert_always_greater_than_or_equal_to!`,
  `assert_always_less_than!`, `assert_always_less_than_or_equal_to!`, and the four
  `assert_sometimes_*` counterparts.
- **Boolean-dict guidance:** `assert_always_some!`, `assert_sometimes_all!`.

> [!IMPORTANT]
> **There is no `assert_eventually!`.** "Eventually / liveness" is not an in-SUT macro — it is
> expressed by the `eventually_` / `finally_` **test-command prefixes** that evaluate in a
> faults-paused quiet period (`adp-stays-alive` is the example). So inside saluki we can only place
> *point-in-time* predicates. Every liveness property in the catalog
> (`forwarder-eventual-delivery`, `disk-persisted-retry-survives-restart`, `shutdown-drains-no-loss`,
> `config-stall-no-deadlock`) reduces, in-SUT, to a `Reachable`/`Sometimes` **anchor** on the
> good-function path plus an external command that owns the actual "eventually." Do not reach for an
> in-SUT `Always` to express progress.

> [!TIP]
> **Use the numeric-guidance macros for every bounded-count / bounded-byte invariant.** The catalog
> writes these as `Always(x <= limit)`. Prefer `assert_always_less_than_or_equal_to!(x, limit, …)`
> over `assert_always!(x <= limit, …)`: it hands Antithesis the *margin* `limit - x` as an
> optimization gradient, so the search actively pushes toward the boundary instead of stumbling on
> it. This applies to `contexts.len() <= context_limit`, `bins.len() <= 4096`,
> `total_in_memory_bytes <= max`, and the zero-value-bucket bound. This is a refinement over the
> catalog's plain-`Always` prose, not a new property.

## Feature-gating map — the real cost driver

Wiring cost, not difficulty, is what tiers this work. Adding the `antithesis` feature to a crate is a
small but real edit (optional `antithesis_sdk` dep + `antithesis = ["dep:antithesis_sdk", …]` +
`#[cfg(feature = "antithesis")]` at each site + propagating the feature up to
`agent-data-plane/antithesis`).

| Crate | `antithesis` feature today | Sites that land here |
|---|---|---|
| `saluki-components` | **Present** (`Cargo.toml:15`) | aggregate, forwarder/retry-queue (in-crate), DSD source dispatch, replay reader, metrics encoder |
| `ddsketch` | Missing — add | NaN finiteness, bin-count bound |
| `saluki-context` | Missing — add | resolver heap-fallback anchors |
| `stringtheory` | Missing — add (a `loom` feature already exists) | interner reclamation overlap + full/reuse anchors |
| `saluki-core` | Missing — add | interconnect silent-drop anchor, pool `unreachable!` guards |
| `saluki-io` | Missing — add | retry-queue byte cap (internal), DSD codec `from_utf8_unchecked` guards, retry classifier |
| `saluki-config` | Missing — add | `ready()` reachability, watcher `Lagged` guard |

Implication: **everything in Tier 1 below costs zero new feature plumbing** — it lands in
`saluki-components`, which is already gated. Tier 2/3 each pay one crate's feature setup.

---

## Tier 1 — land first: zero new plumbing, confirmed bugs or cheap always-on invariants

> **Status (2026-06-01): LANDED.** All Tier-1 sites below are implemented in `saluki-components`
> behind the existing `antithesis` feature — 12 new call sites. Verified: compiles with and without
> the feature, all relevant unit tests pass. Numeric-guidance macros used for the bounded invariants
> per the user's direction. See `existing-assertions.md` for the call-site table.

All in `saluki-components` (feature already wired). These give Antithesis an immediate falsification
target for three already-reproduced bugs, plus two near-free standing invariants.

### aggregate-clock-skew-stable — forward/backward wall-clock jump (CONFIRMED bug)
`lib/saluki-components/src/transforms/aggregate/mod.rs`, `flush` (`:617`). `current_time: u64` param,
`self.last_flush`, `bucket_width_secs` (`:620`), zero-value loop at `:635`.

- **Backward-jump guard** — at `:632`, before the loop:
  `assert_always!(self.last_flush == 0 || current_time >= self.last_flush, "aggregate flush wall-clock did not move backward", &json!({ "current_time": current_time, "last_flush": self.last_flush }))`.
- **Forward-jump flood guard — place BEFORE the loop, not after.** The flood does O(jump)
  allocation *inside* `:635`; asserting after the loop reports the bug only once the damage is done.
  Bound the span first:
  `assert_always_less_than_or_equal_to!((current_time.saturating_sub(self.last_flush)) / bucket_width_secs.get(), MAX_FLUSH_BUCKETS, "aggregate zero-value bucket span bounded by flush interval", …)` where `MAX_FLUSH_BUCKETS` is `ceil(flush_interval/window) + slack`.
- **Anchor:** `assert_sometimes!(zero_value_buckets.len() > 1, "aggregate generated multiple zero-value buckets in one flush", …)` after `:639` — proves the idle-counter path is exercised, else the guard is vacuous.

### aggregate-context-limit-enforced — the one non-advisory runtime memory bound
Same file, `insert` (`:571`); cap check at `:573`, `context_limit_breached` set at `:574`.

- **Invariant** — at `:572`, entry to `insert`:
  `assert_always_less_than_or_equal_to!(self.contexts.len(), self.context_limit, "aggregate context map within limit", &json!({ "len": self.contexts.len(), "limit": self.context_limit }))`.
- **Anchor** — at `:574`: `assert_sometimes!(true, "aggregate context limit breached", …)` so the bound is proven load-bearing, not vacuously true on a small corpus.

### ddsketch-no-nan-poison — single NaN permanently poisons sum/avg (CONFIRMED bug)
The robust home is the sketch boundary in `ddsketch` (Tier 2, catches every producer). **Zero-plumbing
stopgap that lands in Tier 1:** assert at the only live bypass, the metrics encoder
`insert_n` call site in `saluki-components`
(`encoders/datadog/metrics/mod.rs` ~`:1054`, per catalog `ddsketch-no-nan-poison`):
`assert_always!(value.is_finite(), "non-finite value reaching sketch insert at encoder", …)`.
Prefer the Tier-2 sketch-boundary version once `ddsketch` is gated; keep this as the reachable-today
guard.

> Note: the catalog also closes a *fixed* hazard here — `aggregate-no-panic-any-window` is now a
> regression tripwire only (`% 0` vector closed upstream, window is `NonZeroU64`). No live in-SUT
> assertion needed beyond an optional `assert_unreachable!` at `align_to_bucket_start` if a future
> sub-second divisor returns.

### replay-corruption-not-silent-eof — corrupt length prefix read as clean EOF (CONFIRMED bug)
`lib/saluki-components/src/sources/dogstatsd/replay/reader.rs`, `read_next`. Two `Ok(None)` returns:
real-EOF at `:86`, corrupt/oversized-length-or-zero-separator at `:96` (indistinguishable today).

- At `:96`: `assert_always_or_unreachable!(self.offset + LENGTH_PREFIX_SIZE > self.contents.len(), "replay read_next returned None only at the real trailer, not on a mid-stream corrupt length", &json!({ "offset": self.offset, "len": self.contents.len() }))`.
- Anchor: `assert_sometimes!(corruption_detected, "replay corruption distinguished from clean EOF", …)`.
- Runs in the separate `agent-data-plane dogstatsd replay` CLI process, not the data-plane — the
  feature is still `saluki-components`, so the catalog repro and the assertion share a crate.

### source-dispatch-no-misroute — latent `.expect()` crash + silent dispatch loss
`lib/saluki-components/src/sources/dogstatsd/mod.rs`, `dispatch_events` (`:1667`–`:1716`).

- Panic guards mirroring the latent crashes — at `:1684` and `:1698`:
  `assert_unreachable!("dsd dispatch output 'events' missing", …)` / `…'service_checks' missing`.
  These `.expect("… output should always exist")` calls crash a fail-stop component if the invariant
  breaks.
- Silent-loss anchor — near the swallowed `error!` sites (`:1688/:1702/:1713`):
  `assert_sometimes!(true, "dsd dispatch failed mid-buffer", …)`. **Finding reconfirmed: dispatch
  failure increments no counter** — the `error!` log is the only signal, so this anchor is the only
  in-SUT visibility into the loss.

---

> **Status (2026-06-01): Tiers 2 and 3 LANDED.** The `antithesis` feature was added to `ddsketch`,
> `stringtheory`, `saluki-context`, `saluki-io`, `saluki-core`, and `saluki-config`, each chaining
> into its dependents so the enable signal flows down from `saluki-components/antithesis` (no new
> crate). All assertions below are implemented and verified: ADP compiles with the feature on, the
> workspace compiles with it off, all touched-crate unit tests pass. Bounded invariants use the
> numeric-guidance macros. One deferral: the interner corruption `Unreachable` (overlap check) is NOT
> landed — see its open question on a cheap per-lookup formulation; the `Sometimes(full)` and
> `Sometimes(slot reused)` anchors around it are in.

## Tier 2 — one crate's feature setup each, high value

### ddsketch-no-nan-poison (robust boundary) + ddsketch-bin-count-bounded
`lib/ddsketch/src/agent/sketch.rs`. **Add the `antithesis` feature to `ddsketch`.**

- **NaN, robust:** at `adjust_basic_stats` entry (`:188`):
  `assert_always!(v.is_finite(), "finite value at sketch basic-stats boundary", …)`; backstop after
  the `self.sum += v * n as f64` update (`:198`): `assert_always!(self.sum.is_finite(), …)`. Mirror at
  `insert_n` entry (`:374`). This catches all producers, not just the encoder path.
- **Bin count:** after each `trim_left` call — `insert` (`:358`), `merge` (`:579`), and the
  `insert_key_counts`/`insert_interpolate_buckets` paths (`:383`, `:447`):
  `assert_always_less_than_or_equal_to!(self.bins.len(), DDSKETCH_CONF_BIN_LIMIT as usize, "sketch bin count within limit", …)` (limit 4096, build-generated). **Essential anti-vacuity anchor**, else the
  `Always` never approaches the bound on real corpora:
  `assert_reachable!("trim_left collapsed bins")` at the collapse guard (`:689`).

### interner-full-bounded / rss-bounded-under-cardinality — heap-fallback anchors (CONFIRMED bug)
`lib/saluki-context/src/resolver.rs`, `intern` (`:334`-ish). **Add the `antithesis` feature to
`saluki-context`.** A counter `intern_fallback_total` is already incremented at `:349`.

- At `:347`, the `try_intern == None` branch: `assert_sometimes!(true, "interner full — try_intern returned None", …)` proves exhaustion is reached.
- At `:349`, the heap-fallback branch: `assert_sometimes!(true, "context interner spilled to heap (unbounded under default config)", …)` — this is the seed of the unbounded-memory bug; the RSS `Always`
  itself stays workload/container-side (OOM is observed externally), but this anchor localizes *why*.

### retry-queue-bounded-under-outage — byte caps + drop anchors
Split across two crates. The `PendingTransactions` wrapper is in `saluki-components`
(`common/datadog/io.rs`, `push_high_priority` `:692`, `push_low_priority` `:720`); the byte
accounting and eviction live in `saluki-io` (`net/util/retry/queue/mod.rs`, `push` `:194`, eviction to
`:213`; `total_in_memory_bytes` `:87`, `max_in_memory_bytes` `:88`).

- **In-memory invariant** — after the eviction loop (`mod.rs:213`, `saluki-io`):
  `assert_always_less_than_or_equal_to!(self.total_in_memory_bytes, self.max_in_memory_bytes, …)`. **Needs the feature on `saluki-io`.**
- **Drop anchor** — reachable from `saluki-components` today via `PushResult.items_dropped` at
  `io.rs:706`: `assert_sometimes!(push_result.items_dropped > 0, "retry queue shed oldest on overflow", …)`. Lands in the already-gated crate.
- **Disk byte cap + poison drop** — `saluki-io` `net/util/retry/queue/persisted.rs`: `Always(total_on_disk_bytes <= max_on_disk_bytes)` after add (`:194`)/evict (`:327`); at the corrupt-entry
  drop arm (`:238`) `assert_always_or_unreachable!(…, "corrupt on-disk entry dropped, recovery continues")` + `assert_sometimes!(self.entries_dropped > 0, …)`.

### forwarder-eventual-delivery — circuit-breaker + permanent-drop anchors
`saluki-components` `io.rs` (gated) and `saluki-io` classifier.

- **Reachable anchor**, `io.rs` `Error::Open` re-enqueue arm (`:468`):
  `assert_reachable!("circuit breaker opened, retryable re-enqueued to low-priority queue")` — proves
  the recovery path engaged, keeping the external eventual-delivery check non-vacuous. Zero plumbing.
- **Permanent-drop anchor**, `saluki-io` `net/util/retry/classifier/http.rs` `default_should_retry`
  (`:18`-`:21`, the 400/401/403/413 → `false` arm):
  `assert_sometimes!(true, "transaction permanently dropped — non-retryable status", …)`. Needs the
  feature on `saluki-io`.

---

## Tier 3 — guards and concurrency anchors, lower marginal value

### interner-reclamation-no-corruption — loom-tested unsafe path
`lib/stringtheory/src/interning/{fixed_size.rs,map.rs}`. **Add the `antithesis` feature to
`stringtheory`** (a `loom` feature already exists, so the cfg pattern is established).

- **Corruption guard — TABLED (decision 2026-06-01, revisit later).** Not landed; left out by choice,
  not oversight. The cheap formulation IS known: assert `!header.is_active()` (refcount zero, one
  atomic load) at the reclaim/sentinel-fill site, which guards the exact corruption mechanism
  (overwriting a still-referenced buffer) in O(1) and is implementation-independent — no live-range
  overlap scan, no sentinel-value dependency. Deferred pending a decision to wire it into the
  `unsafe`, loom-tested reclamation path. The two sentinels (`fixed_size.rs:458` → `0xAA`,
  `map.rs:392` → `0x21`) are why the direct refcount check beats a sentinel scan.
- **Full anchor:** at the `try_intern` `None` returns (`fixed_size.rs:492`, `map.rs:502`):
  `assert_sometimes!(true, "interner full", …)`.
- **Reuse anchor:** at the reclaimed-slot reuse arm (`fixed_size.rs:484`, `map.rs:496`):
  `assert_sometimes!(true, "reclaimed interner slot reused", …)` — exercises the race window Antithesis
  explores beyond loom's bounded model.

### no-silent-interconnect-drop — wired-edge discard + backpressure anchors
`lib/saluki-core/src/topology/interconnect/dispatcher.rs`, `send` (`:86`). **Add the `antithesis`
feature to `saluki-core`.**

- Zero-sender discard branch (`:87`-`:91`, increments `events_discarded_total`):
  `assert_sometimes!(true, "events discarded on a zero-sender output", …)`. **Confirmed: discard fires
  only when `senders.is_empty()`** — a wired edge cannot reach it — so this is a `Sometimes` anchor on
  the legitimate disconnected-output path, NOT a blanket `Unreachable`. The "no silent loss on a wired
  edge" safety claim is enforced by the *absence* of a discard on wired edges plus the backpressure
  anchor below; do not assert `Unreachable` at the discard site.
- Backpressure anchor at the full-channel `.await` (`:99`-`:111`):
  `assert_sometimes!(true, "interconnect backpressure engaged on full channel", …)`.

### pool semaphore guards — NEW, not previously cataloged
`lib/saluki-core/src/pooling/{fixed.rs:188, elastic.rs:273}` — both hold
`unreachable!("semaphore should never be closed")` on the `poll_acquire` `None` arm. Replace/mirror
with `assert_unreachable!("object pool semaphore closed", …)`. Cheap panic-guard once `saluki-core` is
gated; ties to `data-component-failure-triggers-process-shutdown` (a pool panic is exactly an
"unexpected component finish"). **Add this as a small property or fold into the fail-stop property.**

### malformed-dsd-no-crash / malformed-event-sc-no-crash — codec panic guards
`lib/saluki-io/src/deser/codec/dogstatsd/`. **Add the `antithesis` feature to `saluki-io`.**

- `helpers.rs` `from_utf8_unchecked` sites (`:93`, `:141`, `:163`) — guard with
  `assert_unreachable!("non-UTF8 reached from_utf8_unchecked in dsd helper", …)` (or a debug-only
  `is_ascii` precondition) so a broken upstream filter surfaces instead of producing UB.
- `mod.rs:56` nom `Incomplete`-in-complete-parser `unreachable!` → `assert_unreachable!`.
- Decode-failure anchors: `assert_sometimes!` at the event/SC parse-error sites (`event.rs`,
  `service_check.rs`) backing `Sometimes(*_decode_failed > 0)`. Reconfirmed: the event/SC parsers use
  **checked** `simdutf8` and nom slice `take` (no length-prefixed pre-allocation), so the live hazard
  is the `from_utf8_unchecked` helpers, not the parsers — scope the no-crash guards there.

### config-stall-no-deadlock / filter-config-reload-correct — config anchors
`lib/saluki-config/src/lib.rs` and `dynamic/watcher.rs`. **Add the `antithesis` feature to
`saluki-config`.**

- `ready()` (`lib.rs:694`-`:704`, no timeout — confirmed hang bug): `assert_reachable!("config wait entered")` before the `ready_rx.await` (`:700`) and a `Sometimes`/`Reachable` after it returns Ok. The
  *hang itself* is a liveness failure owned by an external `eventually_` command — in-SUT we can only
  anchor that the wait was entered and (sometimes) resolved.
- Watcher `broadcast::Lagged` arm (`watcher.rs:61`-`:66`, warn+continue, no reconciliation):
  `assert_unreachable!("filter config update Lagged-dropped with no re-read", &json!({ "key": … }))`.
  This is the strongest single config-correctness assertion — a dropped final update is permanent
  silent staleness on live customer filtering. Requires the config-stream add-on topology to fire.

---

## What stays OUT of saluki (so the boundary is explicit)

These catalog properties are **not** in-SUT assertions and should not be forced into one:

- **adp-stays-alive** — external `eventually_adp_alive` command (a panicking ADP cannot self-assert).
- **rss-bounded-under-cardinality** (the RSS `Always` itself) — observed as container OOM / external
  liveness; only its *mechanism* anchors (interner heap-fallback, context-limit) are in-SUT.
- **aggregate-matches-agent / mapper-output-matches-agent / prefix-filter-ordering-matches-agent** —
  differential, anchored on the `panoramic`/`stele` diff harness, not an in-process predicate. (An
  in-SUT `Sometimes(metric remapped)` / `Sometimes(cache hit == fresh miss)` is a legitimate
  *anti-vacuity* anchor, but the equivalence check is external.)
- **The "eventually delivered" / "drains within 30s" cores** — external `finally_` reconciliation in a
  quiet period; in-SUT only the `Reachable` anchors above.
- **config-incompatible-refuses-start** — deterministic startup gate, exercised by the integration
  suite's exit-code cases; the `Unreachable` is statically unreachable, so the artifact is an external
  `Reachable(refused)` exploration.

## Recommended landing order

1. **Tier 1, all of it** — zero plumbing, three confirmed bugs (`aggregate-clock-skew-stable`,
   `replay-corruption-not-silent-eof`, NaN stopgap) get an in-run falsification target the moment the
   `antithesis`-feature image runs, plus two free standing invariants.
2. **`ddsketch` feature + NaN boundary + bin-count** — one small crate, closes the robust NaN guard
   and adds the bin-count tripwire.
3. **`saluki-context` heap-fallback anchors** — localizes the headline bounded-memory bug.
4. **`saluki-io` retry-queue/classifier + codec guards**, then **`saluki-core`** (interconnect + pool),
   then **`stringtheory`** (interner) and **`saluki-config`** (config) as the config-stream and
   concurrency add-ons come online.

## Open questions

- Should the bounded-count guards use a fixed `slack` constant or derive it from configured
  `flush_interval / aggregate_window`? Affects false-positive risk on a legitimately long flush gap
  for `aggregate-clock-skew-stable`. `(needs human input)`
- For `interner-reclamation-no-corruption`, is a cheap direct overlap check feasible on the hot
  lookup path, or does it cost too much per-resolve to leave compiled in even as a no-op? The macro is
  a no-op outside Antithesis, but the *condition expression* still evaluates — a range-overlap scan
  per lookup may be too hot. `(partial: macro is no-op in prod, but the predicate is not — needs a
  cheap formulation)`
- Does adding the `antithesis` feature to six more crates create a feature-unification or
  compile-time burden worth a single shared `saluki-antithesis` shim crate instead of per-crate
  optional deps? `(needs human input)`
- Encoder NaN stopgap vs sketch-boundary: ship both (defense in depth) or only the boundary once
  `ddsketch` is gated?
