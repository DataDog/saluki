---
slug: ddsketch-bin-count-bounded
title: DDSketch bin count never exceeds bin_limit
type: Safety
priority: Medium
sut_path: /home/ssm-user/src/saluki
commit: fc4bb29728814ddf9321572b954ec28f58faeb53
updated: 2026-05-30
status: assertion MISSING (as Antithesis); strong unit + proptest coverage exists
---

# ddsketch-bin-count-bounded — DDSketch bin count never exceeds bin_limit

## Property (one sentence)
After any sequence of inserts, multi-weight inserts, interpolations, and merges, an agent
`DDSketch`'s bin count never exceeds `bin_limit` (4096).

## Origin
- SUT analysis §5 safety #4: "bin count must never exceed 4096".
- Doc/comment in `agent/sketch.rs` collapse logic and `trim_left` ("leaving exactly bin_limit bins").

## Files / functions / lines (CONFIRMED)
- `lib/ddsketch/build.rs` (2–4): `AGENT_DEFAULT_BIN_LIMIT = 4096`, `AGENT_DEFAULT_EPS = 1/128`,
  `AGENT_DEFAULT_MIN_VALUE = 1e-9`; emitted as `DDSKETCH_CONF_BIN_LIMIT`.
- `lib/ddsketch/src/agent/config.rs` (8, 14–15): `Config.bin_limit` set from `DDSKETCH_CONF_BIN_LIMIT`.
- `lib/ddsketch/src/agent/sketch.rs`
  - `trim_left` (689–714): enforces the cap. `if bin_limit == 0 || bins.len() <= bin_limit { return; }`
    else drains the lowest `bins.len() - bin_limit` bins, folding their mass into the first kept
    bin → "leaving exactly bin_limit bins" (712–713).
  - Every mutation path calls `trim_left(.., SKETCH_CONFIG.bin_limit)`:
    `insert` (358), `insert_keys` (319), `insert_key_counts` (255), `merge` (579).
  - `generate_bins` (716–735): a single `(k, n)` with `n >= u32::MAX` could emit multiple bins
    for one key (overflow split, 731–733); but `trim_left` runs immediately after every caller,
    so the post-operation bin count is still capped. (Historic regression fixed: with old u16
    bins a large weight exploded bin count — test `trim_left_respects_bin_limit_with_large_weights`
    824–846.)
  - `bin_count()` (90–92): `self.bins.len()` — the value to assert on.
- Existing tests already assert this: unit tests (760–891) and **proptests**
  `prop_bin_count_never_exceeds_limit` (924–936), `prop_output_bins_are_sorted_and_distinct`,
  `prop_output_keys_are_highest_from_input`, etc. (919–1023).
- ADP entry into the agent sketch: `aggregate/mod.rs:7` `use ddsketch::DDSketch` (re-export of
  `agent::DDSketch`, `lib.rs:56`); built in `transform_and_push_metric` (743–745) via
  `DDSketch::default()` + `insert_n` per histogram sample, and distributions flow as `SketchPoints`.
- Separate impl: the **canonical** `DDSketch` (`canonical/sketch.rs`) uses
  `CollapsingLowestDenseStore` with `max_num_bins` (default 2048) and an `assert!(max_num_bins >= 1)`
  (collapsing_lowest.rs:37), collapsing on growth (67–87). Not on the aggregate hot path, but the
  same invariant applies if/where it is used.

## Failure scenario (Antithesis angle)
The unit/proptests run only under `cargo test` on isolated inputs. Antithesis adds value by
checking the invariant **live, on real production sketches**, after arbitrary interleavings:
- Histogram→distribution conversion inserting thousands of distinct sample values per flush
  (743–745) feeding `insert_n` with large weights, then `merge`d across windows.
- Merge of many incoming agent sketches (future "take sketches shipped by the agent, merge them"
  use case, sketch.rs:33–35) where each `merge` (542–582) extends then `trim_left`s.
- A code path that mutates bins **without** calling `trim_left` (e.g. a new insert helper, or
  `insert_raw_bin` 491–495 which intentionally does NOT trim) escaping the cap — exactly the
  kind of regression a live `Always` assertion would catch that the targeted tests would not.

## Observations
- This invariant is structurally enforced today; the Antithesis assertion is a **regression
  tripwire** placed at the sketch boundary, valuable because the cap is re-established by a
  separate `trim_left` call at each mutation site (easy to miss when adding a new mutator).
- SUT-side instrumentation strongly preferred: `bin_count()` is internal and per-sketch; a
  workload-side checker cannot see individual sketches mid-pipeline.

## Config dependencies
- `bin_limit` is compile-time (build.rs), not runtime-configurable for the agent sketch.

## Suggested assertion
- `assert_always(self.bins.len() <= SKETCH_CONFIG.bin_limit as usize,
  "DDSketch bin count within bin_limit")` placed at the **end of every mutating method**
  (`insert`, `insert_n`/`insert_keys`/`insert_key_counts`, `merge`, `insert_interpolate_buckets`)
  — i.e. one shared check after `trim_left`.
- `Reachable("DDSketch trim_left collapsed bins")` to confirm the workload actually drives a
  sketch past the limit (otherwise the `Always` is vacuously true and proves nothing).

## Open questions
- `insert_raw_bin` is `#[cfg(test)]`/`pub(crate)` test-only (490) and bypasses `trim_left` — confirm
  it can never be reached in a release build (otherwise it is a hole in the invariant).
- `generate_bins` overflow split for `n >= u32::MAX`: under truly extreme single-key weights, does
  the transient (pre-`trim_left`) bin vector allocation matter for memory? Probably not, but worth
  a `Sometimes` if huge weights are in scope.

## Investigation Log

#### Which DDSketch (agent 4096 vs canonical 2048) is on ADP's live aggregation path
- **Examined**: `lib/ddsketch/src/lib.rs:46-56` (module layout + crate-root re-export);
  `lib/ddsketch/build.rs:2-4,45` (`AGENT_DEFAULT_BIN_LIMIT = 4096`, `AGENT_DEFAULT_EPS = 1/128`);
  `lib/ddsketch/src/agent/config.rs` (`Config`, generated `DDSKETCH_CONF_BIN_LIMIT`),
  `agent/sketch.rs:255,319,358,829` (`trim_left(..., SKETCH_CONFIG.bin_limit)`);
  `transforms/aggregate/mod.rs:7` (`use ddsketch::DDSketch`), `:740-750` (distribution build via
  `DDSketch::default()` + `insert_n`); `bin/agent-data-plane/src/cli/run.rs:491-498`
  (metrics pipeline => `dd_metrics_encode`), `encoders/datadog/metrics/mod.rs:5,467,841,1006`;
  grep of `ddsketch::canonical` / `canonical::DDSketch` usage across `lib` and `bin`.
- **Found**: The crate root re-exports the **agent** implementation:
  `pub use agent::{Bin, Bucket, DDSketch};` (`lib.rs:56`) — so `ddsketch::DDSketch` *is* the agent
  sketch (bin_limit **4096**, eps 1/128). The aggregate transform imports `ddsketch::DDSketch`
  (`mod.rs:7`) and uses it in the histogram->distribution conversion path (`mod.rs:743-750`). The
  DD metrics encoder also uses `ddsketch::DDSketch` (`metrics/mod.rs:5`) for Histogram/Distribution
  values. The **canonical** sketch (`ddsketch::canonical`, `max_num_bins`-based,
  `CollapsingLowestDenseStore`, relative_accuracy 0.01) has **no non-test usage** in `lib` or
  `bin` — it is library-only / not wired into any ADP topology component.
- **Not found**: Any live ADP component constructing `ddsketch::canonical::DDSketch`.
- **Conclusion**: RESOLVED. Only the **agent sketch (bin_limit 4096)** is on the live ADP path
  (aggregate distribution build + DD metrics sketch encoding). The canonical sketch (2048) is not
  reachable in production. The bin-count assertion should target the agent sketch's
  `SKETCH_CONFIG.bin_limit == 4096` exclusively; canonical can be dropped from scope.
