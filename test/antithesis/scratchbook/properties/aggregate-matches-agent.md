---
slug: aggregate-matches-agent
title: Aggregated output matches the Datadog Agent
type: Safety
priority: High
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
status: assertion MISSING (no Antithesis SDK in tree)
---

# aggregate-matches-agent — Aggregated output matches the Datadog Agent

## Property (one sentence)
For the same input metric stream, ADP's aggregated output (counter→rate conversion,
half-open `[start, start+width)` buckets, histogram/distribution statistics) equals the
Datadog Agent's output — and that equivalence is preserved under fault injection
(delayed/skipped flush, restart, backpressure, clock perturbation).

## Origin
- SUT analysis §5 safety #3 ("Aggregation output matches the Datadog Agent … explicitly
  'to match the Datadog Agent'").
- Existing correctness suite is a **diff test** (`bin/correctness/`) that already checks
  happy-path equivalence deterministically. The Antithesis angle is preserving equivalence
  under faults the diff harness cannot inject (§6 gaps 1, 5).

## Files / functions / lines
- `lib/saluki-components/src/transforms/aggregate/mod.rs`
  - `counter_values_to_rate` (810–815): `MetricValues::Counter(points) => MetricValues::rate(points, interval)`.
  - Passthrough conversion (451–459): counters→rate using **bucket width** as the rate
    interval, with the in-code comment "we have to match the behavior of the Datadog Agent. ¯\_(ツ)_/¯".
  - `transform_and_push_metric` (728–808): histogram → per-statistic metrics (count as rate
    with bucket width, others as gauge); copy-to-distribution builds a `DDSketch` via
    `insert_n` per sample (740–750); rate statistics use `MetricValues::rate(.., bucket_width)`.
  - `is_bucket_closed` (821–843) + doc: half-open `[start, start+width)`, closed iff
    `(bucket_start + width - 1) < current_time`.
  - `align_to_bucket_start` (817–819): `timestamp - (timestamp % bucket_width_secs)`.
  - `flush` split at `split_timestamp = align_to_bucket_start(current_time, w).saturating_sub(1)` (620).
- Diff harness:
  - `bin/correctness/stele/src/metrics.rs` `PartialEq for MetricValue` (153–186):
    Count/Rate/Gauge compared with `approx_eq_ratio(RATIO_ERROR=1e-8)`; **Rate also requires
    `interval_a == interval_b`** (171); Sketch compared on min/max/avg/sum (ratio) + exact
    `count()` + exact `bin_count()` (175–182).
  - `bin/correctness/panoramic` (drives identical workload into Agent + ADP), `millstone`
    (load gen), `datadog-intake` (mock intake). Fixed `FLUSH_WAIT = 32s` (per SUT analysis §6).

## Failure scenario (Antithesis angle)
Diff equivalence is established only under a healthy, deterministic run. Faults that can
break it without the existing suite ever noticing:
1. **Delayed / skipped flush:** if the monotonic `primary_flush` interval is delayed (CPU
   starvation, scheduler pause), a bucket that should have closed flushes one interval late.
   Combined with the wall-clock bucketing (see `aggregate-clock-skew-stable`), the emitted
   timestamps/rate intervals can diverge from the Agent.
2. **Restart mid-window:** `flush_open_windows=false` default drops open buckets on shutdown
   (SUT analysis §3). The Agent baseline and ADP may shed different partial windows on a kill,
   producing a one-window data delta.
3. **Backpressure:** a slow downstream blocks the aggregate's dispatcher (`dispatcher.flush().await`,
   330); if input continues to arrive during the stall, late-arriving updates may land in a
   different wall-clock bucket than the Agent assigns them.
4. **Counter→rate interval:** the `interval` carried on a rate is the **bucket width**, not the
   flush interval. If window vs flush interval are misconfigured relative to the Agent, the
   `interval_a == interval_b` check (stele 171) fails even when values match.

## Observations
- This is fundamentally a **differential** property: it requires running both ADP and a
  Datadog Agent baseline against an identical stream and diffing normalized output. It is not
  expressible as a single in-process SDK assertion the way the others in this catalog are.
- Best realized in Antithesis by extending the existing `panoramic` diff harness to run
  *inside* the Antithesis environment and assert equivalence (`assert_always` the diff is
  empty / within ratio) **while Antithesis injects faults** (network, process kill+restart,
  clock). The diff result is the natural assertion anchor.
- OTLP metrics deliberately **skip aggregation** (SUT analysis §2, `run.rs:751-753`) to avoid
  counter→rate; equivalence claims apply to the DSD path.

## Config dependencies
- `aggregate_window_duration` (default 10s) — drives bucket width AND the rate interval.
- `aggregate_flush_interval` (default 15s) — drives flush cadence (monotonic).
- `aggregate_flush_open_windows` (default false) — governs restart-window deltas.
- `counter_expiry_seconds` (default 300) — zero-value counter continuity.
- `histogram_aggregates` / `histogram_copy_to_distribution[_prefix]` — histogram output shape.
- Agent baseline must be configured with matching window/flush/expiry for a fair diff.

## Suggested assertion
- Workload-side (harness): `assert_always(diff_within_ratio, "ADP aggregation matches Agent")`
  evaluated on the normalized stele diff after each flush window, **with faults active**.
- `Sometimes(fault_was_injected_during_window)` to confirm the equivalence check actually ran
  under a perturbed condition (not just clean windows).

## Open questions
- Does the existing `panoramic` harness tolerate a process restart of ADP mid-run, or does it
  assume a single long-lived process? (Determines how restart-equivalence is asserted.)
- What FLUSH_WAIT is needed once faults delay flushes? The fixed 32s may be too short under
  injected scheduler pauses, causing false diffs that are timing artifacts not correctness bugs.
- Is the Agent baseline's bucket width guaranteed identical to ADP's `aggregate_window_duration`
  in the harness config? If not, the `interval` equality check is a false-positive source.
- Are zero-value counters (continuity) emitted identically by both sides across a skipped flush?
