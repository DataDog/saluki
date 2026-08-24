# Configurable DogStatsD aggregation intervals

## Purpose

Allow operators to select DogStatsD aggregation window durations by metric-name prefix. Longer windows emit fewer points and reduce cost. Shorter windows provide finer granularity at greater point, CPU, and memory cost.

This first version is startup-only. Changing the rules requires restarting Agent Data Plane (ADP).

## Configuration

The top-level, Saluki-only `metric_aggregation_intervals` key configures overrides. The existing `aggregate_window_duration_seconds` remains the default for unmatched metrics.

```yaml
aggregate_window_duration_seconds: 10

metric_aggregation_intervals:
  - metric_prefix: high_resolution.
    interval_seconds: 1
  - metric_prefix: archival.
    interval_seconds: 60
```

Each rule contains:

- `metric_prefix`: A non-empty, case-sensitive metric-name prefix.
- `interval_seconds`: An aggregation window duration from 1 through 60 whole seconds, inclusive.

The default rule list is empty, which preserves current behavior.

ADP rejects the complete configuration at startup if:

- a prefix is empty or has leading or trailing whitespace;
- an interval is outside the inclusive range from 1 through 60 seconds;
- prefixes are duplicated; or
- prefixes overlap, meaning either prefix starts with the other.

Rejecting overlap ensures that each metric matches at most one rule. Rules do not have precedence.

## Metric selection

Rules match the final metric name after DogStatsD mapper rewrites and `statsd_metric_namespace` prefixing. They apply after the existing metric and tag filters.

Each untimestamped metric uses exactly one interval:

1. A matching rule selects its configured interval.
2. An unmatched metric uses `aggregate_window_duration_seconds`.

Rules apply to every metric type handled by the aggregator. Metrics with client-provided timestamps that use the no-aggregation path bypass these rules and retain existing behavior.

## Window and flush semantics

The **aggregation interval** controls point granularity. `aggregate_flush_interval` controls when closed windows are forwarded. Changing the flush interval does not change bucket membership or point timestamps.

For interval `W`, a sample processed at Unix time `T` belongs to the epoch-aligned, half-open window:

```text
start = floor(T / W) * W
window = [start, start + W)
```

A window closes at `start + W`. A flush may emit several closed windows together if the aggregation interval is shorter than the flush cadence.

Untimestamped DogStatsD samples use the time at which the aggregator processes them. Metric type semantics remain unchanged within each window, including counter-to-rate conversion, gauge merging, set cardinality, histogram aggregation, distribution merging, and idle-counter zero emission.

## Architecture

One aggregate component owns all interval state and output. It maintains one aggregation state, or **lane**, per distinct interval rather than one component per prefix.

Rules selecting the same duration share a lane. A rule also shares the default lane when its duration equals `aggregate_window_duration_seconds`.

The aggregate component:

- compiles the prefix rules once at startup;
- routes each metric to exactly one lane;
- applies one global context limit across all lanes; existing contexts continue accepting samples at the limit, while samples that introduce another context are dropped until any lane releases capacity;
- coordinates flush, shutdown, telemetry, and context snapshots; and
- merges lane output into the existing post-aggregation pipeline.

This preserves one owner for routing and aggregation state while reusing the existing single-interval aggregation logic within each lane.

## Shutdown and restart

The existing shutdown policy applies to every lane:

- If `aggregate_flush_open_windows` is `false`, ADP discards open windows during shutdown.
- If `aggregate_flush_open_windows` is `true`, ADP emits open windows as partial windows.

A configuration change takes effect only after restart. This version does not move live contexts between intervals or define an in-process policy transition. Any partial-window emission or discard during restart follows the existing shutdown policy.

## Correctness requirements

For a fixed startup configuration:

- Every accepted untimestamped sample belongs to exactly one lane and one epoch-aligned window.
- No accepted sample is duplicated between lanes.
- Unmatched metrics behave identically to the current single-interval aggregator.
- Flush timing and input batch boundaries do not change normalized aggregated output.
- Emitted counter rates use the interval of the window that produced them.
- Idle-counter zero points use the context's selected interval.
- The total retained context count across all lanes, including idle counters retained for zero emission, does not exceed `aggregate_context_limit`.
- Timestamped no-aggregation metrics are unaffected.

Unit tests should cover exact matching and window examples. A model-based property test should compare randomized insert and flush sequences against a naive reference model and check routing, bucket alignment, value conservation, flush-schedule independence, and the global context limit.

## Resource trade-offs

Short intervals create more points and may retain several closed buckets between flushes. Long intervals retain set values, histogram samples, and distribution state for longer. Both effects can increase memory beyond the assumptions of the current single-interval memory bound.

Memory accounting must include all lanes and the number of buckets retained between flushes. Flush scheduling should avoid scanning every long-interval context at the shortest configured interval.

## Out of scope

This version does not support:

- live rule updates;
- context migration between intervals;
- transition activation boundaries;
- overlapping-prefix precedence; or
- separate topology components for each interval.

Live updates require a separate specification for activation timing, open windows, idle counters, update coalescing, and loss or duplication across policy changes.
