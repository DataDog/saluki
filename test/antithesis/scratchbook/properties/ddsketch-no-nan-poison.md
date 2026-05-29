---
slug: ddsketch-no-nan-poison
title: A NaN sample never silently poisons a sketch's sum/avg
type: Safety / Reachability
priority: High
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
status: assertion MISSING; CONFIRMED sketch boundary does NOT guard finiteness
---

# ddsketch-no-nan-poison — A NaN sample never silently poisons a sketch's sum/avg

## Property (one sentence)
A single NaN (or other non-finite) sample must never silently poison a sketch's `sum`/`avg`;
for any finite input stream the sketch's `sum`/`avg` stay finite, and the sketch boundary
rejects or sanitizes non-finite values rather than absorbing them.

## Origin
- SUT analysis §7 #10 (Wildcard): "NaN poisons a DDSketch (`agent/sketch.rs:188-206`):
  `sum`/`avg` go NaN permanently; finiteness is guarded per-source (DSD codec), not at the
  sketch boundary — fragile if a new producer is added."

## Files / functions / lines (CONFIRMED)
- `lib/ddsketch/src/agent/sketch.rs`
  - `adjust_basic_stats(v, n)` (188–206): **NO finiteness check.**
    `self.sum += v * n as f64;` (198) → if `v` is NaN, `sum` becomes NaN permanently (NaN is
    sticky under `+`). `self.avg += (v - self.avg)/count` (201) / the `n>1` branch (205) →
    `avg` also goes NaN. `min`/`max` comparisons (189, 193) are all false for NaN, so a NaN
    leaves min/max unchanged but silently corrupts sum/avg.
  - Entry points that call `adjust_basic_stats` with caller-supplied `v` and **no NaN reject**:
    `insert(v)` (327–330), `insert_n(v,n)` (374–384, calls `adjust_basic_stats` at 380 for n>1
    and `insert` for n==1), `insert_many` (362–371), `insert_interpolate_bucket` (426, 440),
    `insert_raw_bin` (493).
  - `Config::key(NaN)` (config.rs 70–87): every comparison with NaN is false, so it falls to
    `log_gamma(NaN).round_ties_even() as i32` = 0, `key = norm_bias`, clamped to `[1, MAX_KEY]`
    → NaN gets a **valid bin** (count incremented) while sum/avg are poisoned: the sketch looks
    populated but its sum/avg are NaN. `count` still increments (197), so `is_empty()` is false
    and `sum()`/`avg()` return `Some(NaN)`.
  - `quantile` (535): `.or(Some(f64::NAN))` can itself yield NaN as a fallback — distinct from
    poisoning, but means a NaN out of `quantile` is not by itself proof of poisoning.
- ADP call site (the boundary in question): `transform_and_push_metric` (744–745):
  `sketch.insert_n(sample.value.into_inner(), sample.weight.0 as u64)` — calls `insert_n`
  **directly**, with no finiteness guard at this boundary.
- Per-source guard that exists today (the *only* current protection):
  - DSD codec drops non-finite float values (SUT analysis §8 "drop non-finite floats in codec";
    §7 #7 `non_finite_metric_values_are_silently_dropped`). So in the current DSD pipeline NaN
    is filtered upstream — but this is **not** enforced at the sketch boundary, so any new
    producer (OTLP path, replay, future sources) that reaches `insert_n` bypasses the guard.
- `stele` diff comparison (`metrics.rs` 175–182) compares sketch `sum`/`avg` with
  `approx_eq_ratio` — a NaN sum makes any comparison false, so poisoning would surface as a
  diff-test mismatch *if* the harness happened to feed a NaN; the deterministic happy-path
  workload does not.

## Failure scenario (Antithesis angle)
1. A producer reaches the sketch boundary with a NaN/±Inf sample value (a new source, a replay
   record with a corrupt value, or a regression that removes the codec guard). `insert_n`
   absorbs it; `sum`/`avg` go NaN for the lifetime of that sketch and propagate through
   `merge` (551–552) into every sketch it touches and downstream into the emitted distribution
   → permanently wrong customer data, silently.
2. `weight` non-finite is not possible (`u64`), but `value.into_inner()` is an `f64` with no
   guarantee of finiteness at this call site.

## Observations
- The cheapest robust assertion is **at the sketch boundary** (inside `adjust_basic_stats` or at
  the top of `insert`/`insert_n`/`insert_many`), because that is the single chokepoint and is
  exactly where the missing guard lives. SUT-side instrumentation strongly wins; workload-side
  can only observe a NaN sum after it has already propagated downstream.
- Two framings:
  - **Outcome invariant:** `assert_always(self.sum.is_finite() && self.avg.is_finite(),
    "sketch sum/avg finite")` after each mutation — for a workload that only injects finite
    values, this catches any internal NaN production; with NaN injection it documents the
    poisoning.
  - **Boundary invariant:** if a finiteness guard is added, `assert_unreachable("non-finite
    value reached DDSketch::adjust_basic_stats")` to prove NaN never gets absorbed.

## Config dependencies
- None directly. Reachability depends on which producers feed the aggregate sketch path
  (DSD codec currently filters; OTLP/replay/future sources may not).

## Suggested assertion
- Primary: `assert_always(v.is_finite(), "value reaching DDSketch is finite")` at the top of
  `adjust_basic_stats` (covers all insert/merge-feeding paths) — OR, if the boundary is changed
  to reject, `assert_unreachable` on the absorbed-NaN path.
- Secondary outcome check: `assert_always(self.sum.is_finite(), "DDSketch.sum finite")` after
  mutations, as a backstop for internally produced non-finite (e.g. overflow → Inf).
- `Reachable("non-finite sample offered at sketch boundary")` only if the workload deliberately
  injects NaN past the codec — otherwise keep it `Unreachable`-style to assert the guard holds.

## Open questions
- Should the fix **reject/skip** the NaN at the sketch boundary (matching the codec's drop
  policy) or **clamp**? Rejecting keeps count/sum consistent; the Agent's policy here should be
  confirmed against the diff baseline (ties into `aggregate-matches-agent`).
- `quantile`'s `.or(Some(f64::NAN))` fallback (535): is a NaN quantile result distinguishable
  from a poisoned-sketch NaN downstream? The assertion should target sum/avg, not quantile, to
  avoid conflating the two.
- Is `+Inf`/`-Inf` (e.g. from an overflowing `sum`) in scope? `v.is_finite()` covers both NaN
  and Inf; confirm the desired policy treats them identically.

### Investigation Log

#### Does any non-DSD producer reach the DDSketch insert boundary without the codec FloatIter finiteness filter?
- **Examined:** the finiteness filter (`lib/saluki-io/src/deser/codec/dogstatsd/metric.rs:254,
  273-303` — `FloatIter` skips non-finite with `value.is_finite()` at :299 and a debug log at :301);
  every agent-DDSketch insert caller in the tree (`grep` for `insert`/`insert_n`/`insert_many`/
  `insert_interpolate_buckets`/`add_n`); and the ADP topology wiring in
  `bin/agent-data-plane/src/cli/run.rs:462-499, 593-686, 745-755`.
  Specifically traced: (a) OTLP — `lib/saluki-components/src/sources/otlp/metrics/translator.rs`;
  (b) self-telemetry — `lib/saluki-core/src/observability/metrics/mod.rs:299-310`,
  `processor.rs`; (c) the aggregate histogram→distribution path —
  `lib/saluki-components/src/transforms/aggregate/mod.rs:737-762`; (d) checks_ipc —
  `lib/saluki-components/src/sources/checks_ipc/mod.rs:185-204`; (e) the datadog metrics encoder —
  `lib/saluki-components/src/encoders/datadog/metrics/mod.rs:1043-1061`.
- **Found:**
  - **Confirmed: the sketch boundary itself has no finiteness guard.** `agent/sketch.rs` `insert`
    (:327), `insert_n` (:374), `insert_many` (:362), `insert_interpolate_bucket` (:387) all call
    `adjust_basic_stats` (:188) which does `self.sum += v * n` unconditionally — NaN poisons
    sum/avg permanently. `FloatIter` (codec) is the *only* finiteness filter in the metric path.
  - **Aggregate transform `insert_n` path (the flagged mod.rs:745) is DSD-ONLY → CLOSED.** The
    aggregate transform (`dsd_agg`) is wired **exclusively** into the DSD pipeline:
    `dsd_in → dsd_enrich → dsd_prefix_filter → dsd_tag_filterlist → dsd_agg → dsd_post_agg_filter →
    metrics_enrich` (run.rs:664-679). DSD metrics pass through `FloatIter` at decode time, so the
    `Histogram` samples reaching `aggregate/mod.rs:745` (`sketch.insert_n(sample.value...)`) are
    already finite. **checks_ipc and OTLP metrics join the topology at `metrics_enrich`
    (run.rs:469/499 and run.rs:753), which is DOWNSTREAM of `dsd_agg`** — they never enter the
    aggregate transform. So no non-DSD producer reaches `insert_n` *in the aggregate transform*.
  - **OTLP number path → CLOSED.** `get_number_data_point_value` (translator.rs:1366) feeds
    `is_skippable` (`value.is_nan() || value.is_infinite()`, :1374-1377) in both
    `map_number_metrics` (:726) and `map_number_monotonic_metrics` (:754); non-finite values are
    skipped with a warn/debug log. Gauges/counters never carry NaN downstream.
  - **OTLP histogram/sketch path → effectively CLOSED (no NaN poisoning).** OTLP histograms become
    sketches via two routes, neither of which feeds a raw NaN into `adjust_basic_stats`:
    (i) exponential histograms build a `Dogsketch` proto and use `DDSketch::try_from`
    (`build_agent_sketch_from_key_counts`, :314-351) — never calls `insert`/`adjust_basic_stats`;
    (ii) explicit-bounds histograms call `qa.insert_interpolate_buckets(buckets)` (:889) where bucket
    `upper_limit` comes from the payload's `explicit_bounds`, which is **not** finiteness-checked.
    However, `insert_interpolate_bucket` (sketch.rs:387) only ever passes
    `SKETCH_CONFIG.bin_lower_bound(key)` (a finite reconstructed value) into `adjust_basic_stats`
    (:426, :440), never the raw bound. A NaN bound makes `distance`/`fkn` NaN → `fkn as u64 == 0` →
    no per-key insert; the remainder branch uses a finite `bin_lower_bound`. So a NaN explicit bound
    can distort *bucketing* but does not poison sum/avg with NaN. (`insert_interpolate_buckets` at
    :465-481 handles ±Inf explicitly but not NaN — a latent robustness gap, not a poisoning path.)
  - **LIVE non-DSD NaN→sketch path FOUND: checks_ipc Histogram → datadog metrics encoder.**
    - `checks_ipc/mod.rs:195`: `MetricType::Histogram => Metric::histogram(context, (timestamp,
      value))` where `value` is the raw f64 from an external Python check over IPC. **No `is_finite`
      / `is_nan` check anywhere in this decode** (mod.rs:185-204). A check emitting NaN produces a
      `Histogram` metric carrying NaN.
    - That metric flows `checks_ipc_in.metrics → metrics_enrich → dd_metrics_encode` (run.rs:469,
      499) — i.e. it does NOT pass through the DSD codec FloatIter and does NOT enter the aggregate
      transform.
    - The encoder converts `MetricValues::Histogram` to a sketch by calling **`ddsketch.insert_n(
      sample.value.into_inner(), sample.weight...)`** at `encoders/datadog/metrics/mod.rs:1054`
      (inside `encode_sketch_metric`, the `Histogram` arm at :1049-1058). This is a direct
      `insert_n` of the raw sample value with no finiteness guard → `adjust_basic_stats` →
      `sum += NaN`. The emitted Datadog sketch payload then carries a NaN sum/avg silently.
  - `distribution_sampled_fallible` (value/mod.rs:312, `insert_n`) is called ONLY from the DSD codec
    (metric.rs:267, fed by `FloatIter`) → DSD-only, safe.
- **Not found:** No finiteness filter on the checks_ipc value path; none at the sketch boundary;
  none in the encoder's Histogram→sketch conversion. No code path where the *aggregate transform*
  receives non-DSD input.
- **Conclusion:** RESOLVED, and the hazard is **LIVE** (not closed on all paths). The specifically
  flagged aggregate `insert_n` path (aggregate/mod.rs:745) is closed because that transform is
  DSD-only. OTLP number and histogram paths do not poison sum/avg. **But there is a live non-DSD
  NaN-poisoning path: a Python check emitting a Histogram metric with a NaN value via checks_ipc
  (checks_ipc/mod.rs:195, no finiteness check) reaches `DDSketch::insert_n` in the Datadog metrics
  encoder (encoders/datadog/metrics/mod.rs:1054), bypassing both the DSD FloatIter and the aggregate
  transform.** Therefore ddsketch-no-nan-poison and the ghost-metric hazard remain LIVE. The robust
  fix is a guard at the sketch boundary (`adjust_basic_stats`/`insert*`), since the per-producer
  filter (FloatIter) demonstrably does not cover the checks_ipc→encoder path. The Antithesis angle:
  drive a check (or checks_ipc IPC input) that emits a NaN histogram value and assert sketch sum/avg
  stay finite at the encoder boundary.
