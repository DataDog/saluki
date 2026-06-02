# prefix-filter-ordering-matches-agent

## Origin

Bug-history-sensitive coverage gap. A past correctness fix **"moved DSD prefix/filter in front of
enrich"** (SUT analysis §8, churn hotspots), and the DSD transform chain now applies four
name-rewriting/filtering stages in a *specific order* that must match the Datadog Agent's
listener-side vs. time-sampler split. The order determines which name each downstream stage sees, so
an ordering regression silently changes filtering outcomes. The `dogstatsd_prefix_filter`
(listener-side: prefixing + blocklist/filterlist) and `dogstatsd_post_aggregate_filter`
(time-sampler-side: histogram-aggregate-series filtering) deliberately split responsibility for the
**same four config keys** — a split with subtle correctness rules and zero end-to-end property.

## Code paths

- Pipeline order (`bin/agent-data-plane/src/cli/run.rs:674-679`):
  `dsd_in.metrics → dsd_enrich` (chained: `dogstatsd_mapper`, `run.rs:640-641`)
  `→ dsd_prefix_filter → dsd_tag_filterlist → dsd_agg → dsd_post_agg_filter → metrics_enrich`.
  - Mapper rewrites the name **first**, so prefix/blocklist see the *mapped* name.
  - `dsd_prefix_filter` then prefixes (`statsd_metric_namespace`) and drops blocklisted names
    **before** aggregation.
  - `dsd_post_agg_filter` runs **after** `dsd_agg`, filtering only the generated histogram-aggregate
    *series* names the aggregator produced (e.g. `foo.max`, `foo.95percentile`).
- `dogstatsd_prefix_filter/mod.rs`
  - `process_metric` (`mod.rs:234-267`): if a prefix is configured, prefixes the name **unless** the
    name already starts with a `metric_prefix_blocklist` entry (`has_excluded_prefix`, `mod.rs:269-275`);
    then checks the effective blocklist matcher and drops on match
    (`events.remove_if(...)`, `mod.rs:298-303`).
  - Default `metric_prefix_blocklist` is a fixed list of integration prefixes (`datadog.agent`, `jvm`,
    `kafka`, …) (`mod.rs:67-91`).
- `dogstatsd_post_aggregate_filter/mod.rs`
  - `HistogramSuffixes::contains_filter_entry` (`mod.rs:178-186`): a filterlist entry "owns"
    post-aggregate filtering **only** if it is shaped `<metric>.<aggregate-suffix>` (suffixes derived
    from `histogram_aggregates` + `histogram_percentiles`, `mod.rs:150-172`). Other entries stay the
    listener filter's responsibility — the explicit split.
  - `should_filter_metric` (`mod.rs:238-240`): filters **only scalar series**
    (`Counter|Rate|Gauge|Set`) — **sketches are kept** (test `sketch_metrics_are_not_filtered`,
    `mod.rs:528`), matching the Agent time-sampler.
- Both filters share the same four config keys (`METRIC_FILTERLIST_CONFIG_KEY`,
  `METRIC_FILTERLIST_MATCH_PREFIX_CONFIG_KEY`, `STATSD_METRIC_BLOCKLIST_CONFIG_KEY`,
  `STATSD_METRIC_BLOCKLIST_MATCH_PREFIX_CONFIG_KEY`, `dogstatsd_filterlist.rs`) and both reload live
  (see `filter-config-reload-correct`).

## Failure scenario

- **Ordering regression:** if a refactor moves `dsd_prefix_filter` back behind `dsd_enrich`/mapper or
  past `dsd_agg`, the prefix/blocklist would see a different name (pre-map, or post-aggregate-expanded)
  than the Agent's listener filter does → metrics blocklisted-or-not differently, or double-prefixed.
  The diff suite's happy path may not catch a name that only diverges through this specific stage
  order.
- **Split divergence:** an entry like `foo.max` (looks like a histogram aggregate) must be filtered
  **post-aggregate** (it targets a generated series), while `foo` must be filtered **listener-side**.
  If `contains_filter_entry`'s suffix detection (`mod.rs:178-186`) disagrees with the Agent's, an
  entry is filtered at the wrong stage (or both, or neither) → a metric the operator blocklisted is
  still forwarded, or a raw metric is dropped that should survive to aggregation.
- **Prefix double-apply / blocklist bypass:** `has_excluded_prefix` logic interacting with a mapped
  name could prefix a name that already carries an integration prefix, or fail to block a name that
  only matches after prefixing — silently wrong egress identity.

## Property

- **Type:** Safety (ordering + differential).
- **Invariant:**
  - `Always(end-to-end keep/drop + final name within ratio of the Datadog Agent)` for the same
    `statsd_metric_namespace`, `metric_filterlist`, `statsd_metric_blocklist`, and match-prefix flags
    — the strongest check, anchored on Add-on 2's differential harness with a corpus that exercises
    prefixing, blocklisting, and histogram-aggregate-series names.
  - `AlwaysOrUnreachable(a non-histogram-shaped filterlist entry is NOT applied at the post-aggregate
    stage)` and conversely `AlwaysOrUnreachable(a histogram-aggregate-series name is NOT dropped at
    the listener stage by that entry)` — SUT-side, pins the prefix/post-agg responsibility split.
  - `AlwaysOrUnreachable(post_agg_filter never drops a sketch metric)` (`mod.rs:238-240`).
  - `Sometimes(prefix added)`, `Sometimes(listener blocklist dropped a metric)`,
    `Sometimes(post-aggregate filter dropped a generated series)` for non-vacuity.
  - Optionally a topology-shape assertion (`Always(dsd_prefix_filter is wired between dsd_enrich and
    dsd_tag_filterlist; dsd_post_agg_filter after dsd_agg)`) read from the built blueprint, to catch
    an ordering regression structurally.
- **Antithesis angle:** corpus crafted so the *same* metric name's keep/drop decision depends on the
  stage order (a name that is blocklisted only pre-prefix, or an entry that is ambiguous between
  listener and post-aggregate ownership), plus fault-induced flush-timing skew on the differential
  run. Compose with `mapper-output-matches-agent` (mapper feeds the prefix filter) and
  `filter-config-reload-correct` (these filters reload live).
- **Priority:** Medium (High if run as the primary regression tripwire for the prefix/filter-ordering
  bug class).

## Config dependencies

- `statsd_metric_namespace`, `statsd_metric_namespace_blocklist`, `metric_filterlist`,
  `metric_filterlist_match_prefix`, `statsd_metric_blocklist`,
  `statsd_metric_blocklist_match_prefix`, `histogram_aggregates`, `histogram_percentiles` — set
  identically on both sides for the differential facet.
- The differential facet needs Add-on 2 (Agent baseline + ADP, identical workload). The split/sketch/
  ordering invariants run SUT-side on the primary topology.

## Open Questions

- Does the Agent split listener-side vs. time-sampler filtering on exactly the
  `<metric>.<aggregate-suffix>` shape that `contains_filter_entry` (`mod.rs:178-186`) uses? An
  off-by-one in suffix detection silently routes an entry to the wrong stage.
  `(needs Agent-source confirmation)`
- Is the `dsd_prefix_filter`-before-`dsd_enrich`/after-mapper ordering load-bearing for Agent
  equivalence (the historical fix moved it), and is there a regression guard today other than this
  proposed property? The fix suggests ordering is fragile.
- `has_excluded_prefix` only consults `metric_prefix_blocklist` when a prefix is configured
  (`mod.rs:269-275`); does the Agent skip prefixing for the same default integration-prefix set
  (`mod.rs:67-91`), and does mapping a name change whether it carries such a prefix?
- Both filters read the same four keys via separate watchers — a reload that updates one but lags the
  other (compose with `filter-config-reload-correct`) could transiently filter at one stage but not
  the other for the same logical rule. Confirm reachability.

### Investigation Log

- Examined: `run.rs:638-679` (chain wiring + order), full `dogstatsd_prefix_filter/mod.rs`
  (process_metric, has_excluded_prefix, default blocklist, reload arms), full
  `dogstatsd_post_aggregate_filter/mod.rs` (HistogramSuffixes, scalar-series gate, sketch exclusion,
  reload arms).
- Found: a deliberate listener-vs-time-sampler split over four shared keys, an ordering the codebase
  history shows is correctness-fragile, and only self-consistent unit tests — no end-to-end ordering
  or differential property. Distinct from `mapper-output-matches-agent` (name rewrite) and
  `tag-filterlist-applied-consistently` (per-metric tag stripping); this owns the **prefix/blocklist +
  post-aggregate split and the stage ordering**.
