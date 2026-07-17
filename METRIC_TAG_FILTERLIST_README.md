# Metric tag filter list

`metric_tag_filterlist` changes selected DogStatsD metric tags before aggregation. It can reduce
metric cardinality by removing tag keys, retaining only selected keys, or aggregating unwanted tag
values under an untagged or sentinel context.

Rules apply to exact metric names and currently affect counters and sketch-backed metrics, including
distributions. They apply to both instrumented and origin tags.

## Mental model

Each entry runs in two stages:

1. `action` and `tags` decide which **tag keys** survive.
2. `tag_value_allowlists` or `tag_value_regexes` optionally constrain the **values** of surviving
   keys.

The resulting metric context is then aggregated. Metrics only combine when all remaining context
dimensions match, including the metric name and other tags.

## Filter tag keys

To remove selected tag keys, use `exclude`. This is the default action:

```yaml
metric_tag_filterlist:
  - metric_name: requests.count
    action: exclude
    tags:
      - customer_id
```

To retain only selected tag keys, use `include`:

```yaml
metric_tag_filterlist:
  - metric_name: requests.count
    action: include
    tags:
      - env
      - service
```

Key filtering compares tag names, not complete `name:value` strings. Metric names must match
exactly; prefixes and wildcards are not supported. With an empty `tags` list, `exclude` preserves
all keys and `include` removes all keys.

## Allow selected tag values

Use `tag_value_allowlists` to retain configured values and aggregate everything else:

```yaml
metric_tag_filterlist:
  - metric_name: requests.count
    action: exclude
    tags: []
    tag_value_allowlists:
      customer_id:
        values:
          - customer-1
          - customer-2
        on_miss: replace
        replacement: other
```

Each allowlist supports:

| Field | Default | Behavior |
| --- | --- | --- |
| `values` | `[]` | Values retained unchanged. An empty list makes every value a mismatch. |
| `on_miss` | `remove` | Either remove the tag or replace its value. |
| `replacement` | `other` | Sentinel used when `on_miss` is `replace`. |

With `on_miss: remove`, rejected values lose the tag and combine with otherwise-identical untagged
metrics. With `on_miss: replace`, they retain an explicit group such as `customer_id:other`.

## Require tag values to match a regular expression

Use `tag_value_regexes` to retain values matching a pattern and replace non-matches:

```yaml
metric_tag_filterlist:
  - metric_name: requests.count
    action: exclude
    tags: []
    tag_value_regexes:
      customer_id:
        pattern: "customer-[0-9]+"
        replacement: invalid
```

The pattern uses Rust regular expression syntax and must match the complete tag value. The
`replacement` defaults to `other`. Invalid patterns are configuration errors. Bare tags have no
value and are replaced.

## Combine key and value filtering

Key filtering always runs first. With `action: include`, add every value-filtered key to `tags` or
the key is removed before its value rule runs:

```yaml
metric_tag_filterlist:
  - metric_name: requests.count
    action: include
    tags:
      - env
      - customer_id
    tag_value_allowlists:
      customer_id:
        values:
          - customer-1
        on_miss: replace
        replacement: other
```

A metric/tag pair can have an allowlist or a regular expression, but not both.

## Duplicate rules and updates

Multiple entries for the same metric merge as follows:

- Entries with the same `action` union their tag-key lists.
- If key-filter actions conflict, `exclude` wins.
- Compatible allowlists union their allowed values.
- Conflicting allowlist mismatch behavior or replacement values are errors.
- Duplicate regular expression rules must have identical patterns and replacements.

Invalid startup configuration prevents the component from starting. Invalid runtime updates are
rejected, logged, and leave the last valid rules active. Valid runtime updates replace the compiled
rules and clear the filtered-context cache.

## Operational considerations

- Choose sentinel values that cannot collide with real values.
- `replace` usually produces clearer grouped dashboards because overflow appears as an explicit
  value. It adds one bounded tag value.
- `remove` minimizes contexts and avoids an overflow group, but it cannot distinguish rejected
  values from metrics that arrived without the tag.
- Queries for specific allowed values behave normally. Unfiltered totals include both allowed and
  aggregated contexts.
- The Core Agent does not interpret the value-rule extensions. When ADP is enabled, keep
  `metric_tag_filterlist_adp_only: true`.
- `data_plane.dogstatsd.aggregator_tag_filter_cache_capacity` controls the filtered-context cache
  and defaults to 100,000. It changes resource usage and cache churn, not filtering semantics.

## Schema coherence notes

The current schema exposes three related concepts in separate fields:

- `action` plus `tags` filters keys.
- `tag_value_allowlists` matches explicit values and can remove or replace mismatches.
- `tag_value_regexes` matches patterns and always replaces mismatches.

This is functional, but it creates two asymmetries: value matching is split across two maps, and
only allowlists expose `on_miss`. Duplicate-entry merging also requires several conflict rules.

A possible future simplification is one value-rule map with a single matcher and mismatch policy:

```yaml
metric_tag_filterlist:
  - metric_name: requests.count
    action: exclude
    tags: []
    tag_value_rules:
      customer_id:
        match:
          values: [customer-1, customer-2]
          # Alternatively: regex: "customer-[0-9]+"
        on_miss: replace
        replacement: other
```

This alternative is not implemented. It is included to make the current schema trade-offs explicit
while evaluating the experimental feature.
