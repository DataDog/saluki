# mapper-output-matches-agent

## Origin

Coverage-gap analysis: the existing 27-property catalog frames ADP as a *transport* and a single
aggregation *transformer* (`aggregate-matches-agent`), but the DogStatsD transform chain has four
additional correctness-affecting transforms that all claim Datadog-Agent equivalence and none has a
property. The `dogstatsd_mapper` is the most complex: it rewrites the metric **name** and injects new
**tags** by expanding regex capture groups, with its own result cache and its own string interner.
A divergence from the Agent's mapper is silent, customer-visible data corruption (wrong metric name,
wrong/missing tags) that the happy-path `panoramic` diff suite does not target as a mapper-specific
case.

## Code paths

- `lib/saluki-components/src/transforms/dogstatsd_mapper/mod.rs`
  - `MetricMapper::try_map` (`mod.rs:259-342`) — slow path iterates profiles, runs each `Regex`,
    on a match clears `new_name` and calls `captures.expand(&mapping.name, &mut new_name)`
    (`mod.rs:298-299`), then for each configured tag does
    `captures.expand(tag_value_expr, &mut expanded_tag_value)` (`mod.rs:302-309`).
  - Profile selection: `metric_name.starts_with(&profile.prefix) || profile.prefix == "*"`
    (`mod.rs:292`); **first matching profile + first matching mapping wins** (tests
    `test_wildcard_prefix_order` `mod.rs:781`, `test_multiple_profiles_order` `mod.rs:821`).
  - Wildcard→regex compilation: `build_regex` (`mod.rs:186-215`) escapes `.` → `\.`, turns `*` →
    `([^.]*)`, anchors `^…$`; rejects chars outside `[a-zA-Z0-9\-_*.]` and consecutive `**`.
  - Existing tags are preserved and merged with expanded tags (`merge_shared`, `mod.rs:314-315`;
    test `test_retain_existing_tags` `mod.rs:888`).
- Pipeline placement (`bin/agent-data-plane/src/cli/run.rs:640-641,674-675`): the mapper is the
  *first* transform in the `dsd_enrich` chained transform, ahead of `dsd_prefix_filter`,
  `dsd_tag_filterlist`, `dsd_agg`, `dsd_post_agg_filter`. So a mapper rename changes which
  prefix/blocklist/filterlist rules subsequently apply — a mapper bug cascades into every downstream
  filter decision.
- Agent reference: the Datadog Agent mapper
  (`pkg/dogstatsd/mapper/mapper.go`) is the equivalence target; the wildcard `([^.]*)` translation
  and `$1`/`${1}` expansion syntax mirror Agent behavior (tests `test_use_regex_expansion_alternative_syntax`,
  `test_expand_name`).

## Failure scenario

A mapping profile is configured (statically or pushed at runtime via the config stream). For an input
metric name, ADP's mapper produces a different `(name, tags)` than the Datadog Agent mapper would —
e.g. different capture-group expansion for overlapping wildcards, different first-match selection
across profiles, different handling of a name that matches the wildcard char class but not the
Agent's, or a tag injected/dropped where the Agent would do the opposite. The metric is then
aggregated and forwarded under the wrong identity. This is silent: no error, no drop counter; the
customer sees a metric that does not match the Agent's output for the same workload + mapper config.

## Property

- **Type:** Safety (differential).
- **Invariant:** Harness/diff-side `Always(mapped (name, tags) within ratio of Agent mapper output)`
  per flush window, anchored on the existing `panoramic`/`stele` diff harness but with a
  **mapper-exercising corpus** (millstone names crafted to hit the configured profiles) and an
  identical `dogstatsd_mapper_profiles` config on both the Agent baseline and ADP. Pair with
  `Sometimes(mapper remapped a metric)` so the diff is not vacuous (the corpus actually triggers
  remapping). A SUT-side `Sometimes(cache hit returned same result as a fresh miss)` localizes the
  result-cache correctness facet.
- **Antithesis angle:** Differential equivalence under (a) overlapping/ambiguous profiles that probe
  first-match ordering, (b) names at the wildcard char-class boundary, (c) the same config delivered
  at runtime over the config stream vs. statically, and (d) fault-induced flush-timing skew (the
  `panoramic` harness alone runs one deterministic order; faults explore reordering). Run as the
  Add-on 2 diff topology with a `dogstatsd_mapper_profiles` config on both sides.
- **Priority:** High.

## Config dependencies

- `dogstatsd_mapper_profiles` (JSON array of `{name, prefix, mappings:[{match, match_type, name, tags}]}`)
  must be set identically on the Agent baseline and ADP.
- `dogstatsd_mapper_cache_size` (default 1000) — exercise both cache-on and cache-off (`0`) to cover
  the cache path vs. the slow path returning the same result.
- `dogstatsd_mapper_string_interner_size` (default 64 KiB) — interacts with
  `mapper-interner-bounded`; keep generous here so interner exhaustion does not confound the diff.

## Open Questions

- Does the Datadog Agent mapper apply **all** matching mappings within a profile, or only the first?
  ADP returns on the first matching mapping (`mod.rs:332`). If the Agent differs, this is itself a
  bug, not just a test-setup detail. `(needs Agent-source confirmation)`
- Does the Agent restrict wildcard match characters to the same `[a-zA-Z0-9\-_*.]` class
  (`ALLOWED_WILDCARD_MATCH_PATTERN`, `mod.rs:31-32`)? A name the Agent maps but ADP's class rejects
  at *config-load* (build error) vs. *match-miss* changes the observable divergence.
- Is `FLUSH_WAIT`≈32s on both sides enough once faults delay flushes (timing-artifact false diffs)?
  Same caveat as `aggregate-matches-agent`.
- Can the config stream actually push `dogstatsd_mapper_profiles` at runtime, and does the mapper
  rebuild on that key? (The mapper has **no `watch_for_updates`** — see Open Questions of
  `filter-config-reload-correct`; the mapper appears static-only, unlike the filters.) Determines
  whether the runtime-config facet of this property is reachable.

### Investigation Log

- Examined: full `dogstatsd_mapper/mod.rs` incl. all unit tests; `run.rs:638-679` pipeline wiring;
  `resolver.rs:436-483` for the `Option<Context>` resolution semantics.
- Found: mapper is a `SynchronousTransform` (`mod.rs:388-398`) with first-match-wins selection and
  capture-group expansion; equivalence to the Agent is claimed via mirrored expansion syntax and the
  wildcard translation, but there is **no differential test** against the Agent — only self-consistent
  Rust unit tests.
- Note: the mapper has no config-stream watcher, so unlike the filters it is configured once at build;
  the runtime-config facet is likely Unreachable for the mapper specifically (flag for the team).
