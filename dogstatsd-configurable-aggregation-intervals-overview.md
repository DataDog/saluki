# Configurable DogStatsD aggregation intervals

## Summary

This PR adds support for aggregating different groups of DogStatsD metrics at different time intervals in Agent Data Plane (ADP). Operators select each group by metric-name prefix.

This allows a deployment to have very fine-grained data for metrics where that resolution is valuable while reducing point volume for metrics where coarser data is sufficient.

## What this enables

Without this feature, every DogStatsD metric uses one aggregation interval. Changing that interval forces the same cost and resolution trade-off across the entire workload. The interval is only configurable today in ADP, not the core Agent.

With this feature, operators can define targeted rules. For example:

```yaml
metric_aggregation_intervals:
  - metric_prefix: latency.critical.
    interval_seconds: 1
  - metric_prefix: archival.
    interval_seconds: 60
```

This results in:

- `latency.critical.*` metrics retain one-second resolution.
- `archival.*` metrics emit one point per minute.
- All other metrics continue using the existing ten-second default.

An empty rule list preserves current behavior.

## Rollout consideration

The metrics backend currently treats a metric's interval as static metadata. Changing the interval of an existing metric can therefore produce incorrect query results for functions such as `as_rate`.

The startup-only configuration discourages frequent interval changes, but it does not prevent an operator from changing a rule during a restart. Before enabling this feature widely, Metrics Query needs to investigate interval transitions and define how queries should handle them. Operators should treat a metric's selected interval as stable until that work is complete.

## First-version limitations

- **Intervals must be whole seconds from 1 through 60, inclusive.** This prevents extreme per-prefix settings that could degrade the customer experience. The existing default interval is not subject to this new limit.
- **Changes require an ADP restart.** Rules are fixed for the lifetime of the process. Metrics never move between intervals while ADP is running.
- **Rules cannot overlap.** For example, `requests.` and `requests.api.` cannot both be configured. This avoids precedence rules and ambiguous behavior.
- **Matching uses the final metric name.** Rules apply after DogStatsD mapper rewrites and metric namespace prefixing.
- **Timestamped passthrough metrics are unaffected.** Metrics configured to bypass aggregation continue to do so.
- **Restart behavior follows the existing shutdown policy.** Open windows are either discarded or emitted as partial windows according to `aggregate_flush_open_windows`.
