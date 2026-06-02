---
slug: ddsketch-relative-error-bound
title: DDSketch quantiles within relative-error bound; merges associative/commutative
type: Safety
priority: Low
sut_path: /home/ssm-user/src/saluki
commit: fc4bb29728814ddf9321572b954ec28f58faeb53
updated: 2026-05-30
status: assertion MISSING
---

# ddsketch-relative-error-bound — Quantile accuracy + merge associativity/commutativity

## Property (one sentence)
For values within the non-collapsed representable range, quantile queries are within the
configured relative error (eps ≈ 0.78%, gamma = 1 + 2·eps), and merging sketches is
associative and commutative (order of merges does not change the result).

## Origin
- SUT analysis §5 safety #4: "DDSketch relative-error guarantee: eps=1/128 (~0.78%) … merge
  associative/commutative."

## Files / functions / lines (CONFIRMED)
- `lib/ddsketch/build.rs` (3, 14–38): `eps = 1/128`; `eps *= 2; gamma_v = 1 + eps;
  gamma_ln = ln_1p(eps)`; `norm_min`, `norm_bias` derived; `assert!(norm_min <= min_value)`.
- `lib/ddsketch/src/agent/config.rs`
  - `key(v)` (70–87): `log_gamma(v).round_ties_even() + norm_bias`, **clamped to `[1, MAX_KEY]`**
    (86). Values with `|v| < norm_min` map to key 0 (75–77); values above `gamma^MAX_KEY`
    saturate at `MAX_KEY` (i.e. INF bucket). **Accuracy is NOT guaranteed at these extremes** —
    this is the caveat the property must scope around.
  - `bin_lower_bound(k)` (47–62): inverse mapping; `gamma_v.powf(k - norm_bias)`.
- `lib/ddsketch/src/agent/sketch.rs`
  - `quantile(q)` (498–536): rank via `rank(count,q) = round_ties_even(q*(count-1))` (668–671);
    interpolates `v_low*weight + v_high*(1-weight)` with `v_high = v_low * gamma_v` (522–523);
    result `clamp(self.min, self.max)` (535); empty → `None`; `q<=0 → min`, `q>=1 → max`.
  - `merge(other)` (542–582): merges basic stats (count/min/max/sum/avg) then bins, then
    `trim_left`. Bin merge is order-independent on keys; `avg` uses an incremental formula
    (552) that is **not** exactly order-independent in floating point.
- Canonical impl (`canonical/mapping/logarithmic.rs`): `relative_accuracy() = (gamma-1)/(gamma+1)`
  (114–115); `index = ceil(log(value)/log(gamma))` (9, 97). `with_relative_accuracy` rejects
  accuracy outside `(0,1)` (40–47). Separate from the agent sketch but same family.

## Failure scenario (Antithesis angle)
The diff-test (`stele` 175–182) compares sketches on min/max/avg/sum (ratio 1e-8) + exact
count + exact bin_count, on a deterministic clean run. Antithesis adds:
1. **Merge order under interleaving:** windows flushed/merged in different orders (delayed
   flush, backpressure reordering) could expose non-associativity. The bin merge is exact on
   counts, but `avg` (incremental, 552) and `sum` accumulate floating-point error that depends
   on merge order → a `quantile`/avg drift the diff test (single fixed order) never sees.
2. **Quantile error at the boundary of the representable range:** values near `norm_min` (1e-9)
   or above the max key collapse to key 0 / INF; quantiles there can exceed the 0.78% bound
   (the documented caveat, sketch.rs:48-51 and canonical "extremes"). The property must assert
   the bound only for in-range values and `Sometimes`-observe out-of-range handling.
3. **Collapsed sketch:** once `trim_left`/`CollapsingLowest` collapses low bins, the relative
   error guarantee for low quantiles is intentionally void (`is_collapsed`,
   collapsing_lowest.rs:50-51). Assertion must exclude collapsed-low-quantile queries.

## Observations
- Two checkable sub-properties:
  - (a) **Accuracy:** for an inserted value `v` within range, `quantile(q)` for the rank of `v`
    is within `gamma`-relative error of `v`: `|q_est - v| <= eps_rel * |v|` where
    `eps_rel = (gamma_v - 1)/(gamma_v + 1) ≈ 1/128`.
  - (b) **Merge invariance:** `merge(A, merge(B,C)) ≈ merge(merge(A,B), C)` and
    `merge(A,B) ≈ merge(B,A)` within ratio, on bins exactly and on sum/avg within a small
    floating tolerance.
- Best validated SUT-side with a known input set so the expected rank value is computable;
  workload-side cannot reconstruct per-sample ground truth from emitted aggregates.

## Config dependencies
- eps/gamma/bin_limit are compile-time (build.rs). `aggregate_window_duration` controls how
  many samples land in one sketch before flush/merge (affects collapse likelihood).

## Suggested assertion
- `assert_always(quantile_within_relative_error, "DDSketch quantile within eps for in-range value")`
  evaluated in a SUT-side test harness over in-range inputs (exclude key-0 / INF / collapsed-low).
- `assert_always(merge_result_equal_within_ratio, "DDSketch merge is order-independent")` over a
  set of sketches merged in two different orders.
- `Sometimes(value_out_of_representable_range)` to confirm the extreme-value carve-out is exercised
  (and to document that accuracy is not claimed there).

## Open questions
- What floating tolerance for `avg`/`sum` under reordered merges is acceptable vs the 1e-8 the
  diff test uses? The incremental `avg` (552) and `sum +=` (551) are order-sensitive in f64;
  need a principled epsilon to avoid false positives.
(All prior open questions resolved — see Investigation Log below.)

## Investigation Log

#### Which sketch on the live path, and does ADP call `DDSketch::quantile` at runtime?
- **Examined**: `lib/ddsketch/src/lib.rs:56` (crate-root = agent sketch);
  `lib/ddsketch/src/agent/sketch.rs:498` (`pub fn quantile`); `lib/ddsketch/build.rs:2-4`
  (eps = 1/128, build.rs doubles it: `eps *= 2.0` then `gamma_v = 1+eps`, lines 19-20);
  `lib/ddsketch/src/canonical/mapping/fixed.rs:38` (`RELATIVE_ACCURACY = 0.01`);
  `transforms/aggregate/mod.rs:735-799` (histogram statistics emit) and `config.rs:58-69`
  (`value_from_histogram`); `lib/saluki-core/src/data_model/event/metric/value/histogram.rs:166-197`
  (`HistogramSummary::quantile`); grep of `.quantile(` across `lib`+`bin` excluding ddsketch internals;
  `bin/agent-data-plane/src/cli/run.rs:491-498` (live metrics pipeline -> `dd_metrics_encode`);
  `encoders/datadog/metrics/mod.rs:1006-1150` (`encode_sketch_metric` / sketch serialization);
  `destinations/prometheus/mod.rs:343-348` (the one `sketch.quantile(q)` runtime call site).
- **Found (a) — sketch type**: Live aggregation uses the **agent** sketch (`ddsketch::DDSketch`,
  eps 1/128). The canonical sketch (`fixed.rs` RELATIVE_ACCURACY 0.01, 2048 bins) is not wired into
  any ADP component (confirmed in companion `ddsketch-bin-count-bounded` log). So the accuracy
  target is the agent sketch's fixed relative accuracy, not the canonical 0.01.
- **Found (b) — quantile NOT queried on the live path**: ADP does **not** call
  `DDSketch::quantile` at runtime on the production metrics path. Two distinct percentile paths
  exist, neither using `DDSketch::quantile` live:
  (1) Histogram-mode percentiles in aggregate go through `HistogramStatistic::value_from_histogram`
  (`config.rs:67`) -> `summary.quantile(q)`, which is `HistogramSummary::quantile`
  (`histogram.rs:172-197`) operating on **raw sorted samples** — a separate structure, NOT a
  DDSketch. (2) Distribution-mode builds a `DDSketch` (`mod.rs:743-750`) and the DD metrics encoder
  serializes it via `encode_sketch_metric`, writing `sketch.bins()` keys/counts plus
  count/min/max/avg/sum (`metrics/mod.rs:1135-1150`) — it ships the raw bins over the wire and never
  queries a quantile. Quantile estimation happens **server-side at Datadog** after ingestion.
  The only runtime `DDSketch::quantile` caller in the codebase is the **prometheus** destination
  (`prometheus/mod.rs:346`), which is internal-telemetry/scrape only and not in the primary
  metrics topology.
- **Not found**: Any live ADP metrics-path call to `DDSketch::quantile`; any production use of the
  canonical sketch.
- **Conclusion**: RESOLVED (with a framing consequence). The accuracy assertion's "live" target is
  the **agent sketch (eps 1/128)**, and ADP **does not query DDSketch quantiles at runtime on the
  customer metrics path** — it ships raw bins to Datadog. Therefore an `Always(quantile within eps)`
  assertion cannot be anchored to a production runtime call; it must be an **SUT-side test-harness**
  assertion over the agent sketch in isolation (the existing unit/proptest level), OR retargeted to
  the property that actually matters in production: *bins/summary are serialized faithfully and bin
  count stays capped* (covered by `ddsketch-bin-count-bounded`). The histogram-percentile path that
  IS computed in-process uses raw-sample `HistogramSummary::quantile`, which is exact (no DDSketch
  relative-error bound applies). Property framing should be narrowed: the DDSketch relative-error
  guarantee is a library invariant, not a live ADP runtime invariant.
