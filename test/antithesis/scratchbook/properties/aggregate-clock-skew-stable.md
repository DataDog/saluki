---
slug: aggregate-clock-skew-stable
title: Aggregation stays sane across wall-clock skew
type: Safety
priority: High
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
status: assertion MISSING; CONFIRMED two-clock hazard, no monotonicity guard
---

# aggregate-clock-skew-stable — Aggregation stays sane across wall-clock skew

## Property (one sentence)
A wall-clock jump (backward or forward) during aggregation never produces a flood of
zero-value counter points nor a silent gap in counter continuity, and bucketing stays
bounded and well-formed.

## Origin
- SUT analysis §7 #9 (Wildcard): "Two-clock hazard: bucketing uses wall clock
  (`get_unix_timestamp`), flush cadence uses monotonic `tokio::interval`. A backward
  wall-clock jump makes the zero-value range empty (silent counter gap); a forward jump
  floods zero-value points and allocates a large `SmallVec`. No monotonicity guard."

## Files / functions / lines (CONFIRMED)
- `lib/saluki-common/src/time.rs`
  - `get_unix_timestamp` (21–26): `SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()`
    — **wall clock**, non-monotonic; on a backward step it returns a smaller value (and on
    pre-epoch it `unwrap_or_default()`s to 0).
- `lib/saluki-components/src/transforms/aggregate/mod.rs`
  - Flush cadence: `interval_at(Instant::now() + flush_interval, flush_interval)` (290–293) —
    **monotonic** tokio timer. So *when* a flush fires is monotonic, but *what timestamp* it
    stamps is wall-clock.
  - `insert` reads `current_time = get_unix_timestamp()` (347) per input batch; buckets via
    `align_to_bucket_start(current_time, bucket_width_secs)` (579).
  - `flush(get_unix_timestamp(), ...)` (319) — flush timestamp is wall clock.
  - Zero-value bucket generation (627–635):
    ```
    let start = align_to_bucket_start(self.last_flush, bucket_width_secs);
    for bucket_start in (start..current_time).step_by(bucket_width_secs as usize) { ... }
    ```
    `self.last_flush` is the wall-clock time of the previous flush (set at 718).
    - **Backward jump:** `current_time < start` → range `start..current_time` is **empty** →
      no zero-value buckets emitted for the gap → silent break in counter continuity (and the
      `should_expire_if_empty` math `am.last_seen + counter_expire_secs < current_time` (651)
      can flip, prematurely expiring or never expiring counters).
    - **Forward jump:** `current_time >> start` → the loop emits one zero-value bucket per
      `bucket_width_secs` across the entire jump, each pushed into
      `SmallVec<[(u64, MetricValues); 4]>` (626) → large heap allocation + a flood of
      zero-value points merged into every counter (661–671) and flushed downstream.
  - `split_timestamp = align_to_bucket_start(current_time, w).saturating_sub(1)` (620) — a
    backward jump moves the split earlier, so values already flushed in earlier (now "future")
    buckets are retained, possibly re-evaluated against a smaller `current_time`.
  - No comparison/guard between `current_time` and `last_flush` for monotonicity anywhere in
    `flush` or `insert`.

## Failure scenario (Antithesis angle — clock fault injection)
1. **Forward jump (e.g. NTP step +1h, width 10s):** next flush generates ~360 zero-value
   buckets, allocating a large `SmallVec` and emitting hundreds of zero-value rate points per
   live counter downstream — a metric flood and memory spike (tension with bounded-memory and
   "match the Agent" — the Agent does not behave this way).
2. **Backward jump:** the zero-value range goes empty; counters that should have emitted
   continuity zeros emit nothing for the skipped interval → downstream sees a gap. On a large
   backward jump, `am.last_seen + counter_expire_secs < current_time` can become false for
   counters that should expire (they linger, consuming context budget) or true for ones that
   shouldn't.
3. **Pre-epoch / clock reset to 0:** `unwrap_or_default()` yields `current_time = 0`, making
   `align_to_bucket_start(0, w) = 0` and most ranges empty — effectively freezes bucketing.
4. **Replay divergence (noted in §7 #9):** because non-timestamped metrics are bucketed by the
   aggregator's *current* wall clock (not per-record capture time), a replayed capture buckets
   differently than at capture — relevant to the replay feature.

## Observations
- Antithesis can drive this directly via **clock fault injection** (step the container clock
  backward/forward) while a steady counter stream flows.
- Natural bounded invariant: the number of zero-value buckets generated in a single flush must
  be bounded by a sane multiple of `flush_interval / window_duration` (a couple), NOT by an
  arbitrary wall-clock delta. `zero_value_buckets.len()` is the in-process anchor.
- Counter-continuity invariant: across a flush, a live counter's emitted timestamps should be
  contiguous in bucket-width steps with no missing closed bucket and no duplicate.
- SUT-side instrumentation wins: `zero_value_buckets.len()` and the `last_flush`/`current_time`
  pair live inside `flush`; workload-side can only observe the downstream flood/gap indirectly.

## Config dependencies
- `aggregate_window_duration` (bucket width) and `aggregate_flush_interval` (cadence) set the
  expected per-flush zero-value bucket count.
- `counter_expiry_seconds` interacts with the skewed `last_seen` expiry comparison (651, 662).

## Suggested assertion
- `assert_always(zero_value_buckets.len() <= max_expected, "zero-value bucket count bounded")`
  inside `flush`, where `max_expected` ≈ `ceil(flush_interval / window_duration) + small_slack`.
  Flags the forward-jump flood.
- `assert_always(current_time >= self.last_flush, "aggregate flush time is monotonic")` OR, if a
  monotonicity guard is added, `AlwaysOrUnreachable` that the backward branch is handled (clamp
  `current_time` to `>= last_flush`).
- `Sometimes(clock_jumped_during_flush)` to confirm the skew fault actually coincided with a flush.

## Open questions
- Intended fix: switch bucketing to a monotonic source, or guard
  `current_time = max(current_time, last_flush)` and cap the zero-value loop iterations? This
  decides whether the assertion is `Unreachable(flood)` vs `Always(bounded)`.
- Is there an upstream protection against `get_unix_timestamp()` returning 0 (pre-epoch)?
  None found — worth confirming the container clock can't be stepped below epoch in the harness.
- What is the Agent's behavior under the same clock step? Needed to know whether "bounded and
  no flood" also means "still matches Agent" (ties into `aggregate-matches-agent`).
- Does the coarse-time path (`get_coarse_unix_timestamp`, 41–59) feed any aggregate code? (It
  does not appear to; aggregate uses the accurate `get_unix_timestamp`.) Confirm no second path.
