---
sut_path: /home/ssm-user/src/saluki
commit: fc4bb29728814ddf9321572b954ec28f58faeb53
updated: 2026-05-30
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: ADP Confluence space — headline guarantees and gap analyses that seed properties.
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/pages/6497671050/What+Comes+After+DogStatsD
    why: Root guarantee for the memory + data-loss clusters.
  - path: https://datadoghq.atlassian.net/browse/DADP
    why: ADP Jira project for tracked gaps/regressions.
  - path: https://github.com/DataDog/saluki/pull/1768
    why: PR review #4393897611 (Copilot) flagged the stale property count reconciled here.
---

# Property Relationships

Lightweight clustering of the 35 catalog properties by shared code paths, failure mechanisms, and
suspected dominance. Slugs match `property-catalog.md`.

## Cluster 1 — Bounded memory (the determinism story)

Properties: `rss-bounded-under-cardinality`, `aggregate-context-limit-enforced`,
`interner-full-bounded`, `memory-limiter-survives-rss-read-failure`,
`retry-queue-bounded-under-outage`, and (memory facet) `aggregate-clock-skew-stable`.

- **Shared mechanism:** every one bears on whether actual RSS stays within the grant. They share the
  `resource-accounting` limiter, the aggregate `HashMap`/`context_limit`, and the `stringtheory`
  interner.
- **Dominance:** `rss-bounded-under-cardinality` is the **roll-up** — it observes the aggregate
  outcome (RSS ≤ grant). The other four explain *why* it does or doesn't hold:
  `aggregate-context-limit-enforced` and `interner-full-bounded` are the two designed bounds;
  `interner-full-bounded` (heap-on default) and `memory-limiter-survives-rss-read-failure` are the
  two leaks that make the roll-up fail. If `rss-bounded` passes, the sub-properties likely hold; if
  it fails, the sub-properties localize the cause. Test the roll-up *and* the components.
- **Config coupling:** all are sensitive to the same three default-off protective settings
  (memory_mode, allow_context_heap_allocs, disk persistence). A single workload-config matrix drives
  the whole cluster.

## Cluster 2 — Egress data loss & durability

Properties: `forwarder-eventual-delivery`, `retry-queue-bounded-under-outage`,
`disk-persisted-retry-survives-restart`, plus the egress facet of `shutdown-drains-no-loss`.

- **Shared code:** all live in `common/datadog/io.rs` + `retry/queue/persisted.rs` (PendingTransactions,
  circuit breaker, disk queue).
- **Tension (not dominance):** `forwarder-eventual-delivery` (no loss) and
  `retry-queue-bounded-under-outage` (bounded memory ⇒ eventual drop) are in direct tension — the
  queue cap is the escape valve that *causes* the loss the delivery property forbids. They must be
  tested with coordinated outage durations: short outage → delivery holds; prolonged outage →
  bounded-drop holds. `retry-queue-bounded` is the safety backstop; `forwarder-eventual-delivery` is
  the liveness goal within the backstop's budget.
- **`disk-persisted-retry-survives-restart`** extends `forwarder-eventual-delivery` across a crash;
  it shares the kill+restart fault with `aggregate-matches-agent` (restart facet).

## Cluster 3 — No silent internal loss / routing

Properties: `no-silent-interconnect-drop`, `source-dispatch-no-misroute`,
`data-component-failure-triggers-process-shutdown`.

- **Shared mechanism:** the `topology/interconnect` dispatcher and the DSD source's multi-output
  dispatch. All three are about events going to the *right place or nowhere silently*.
- **Connection:** `no-silent-interconnect-drop` (backpressure, no discard on wired edges) and
  `source-dispatch-no-misroute` (no cross-output leakage) are complementary halves of "events are
  routed correctly under load/failure." `data-component-failure-triggers-process-shutdown` is the
  backstop: when routing/processing *does* fail, the whole process must stop rather than run half-broken.

## Cluster 4 — Aggregation correctness

Properties: `aggregate-matches-agent`, `aggregate-no-panic-any-window`, `aggregate-clock-skew-stable`,
`ddsketch-bin-count-bounded`, `ddsketch-relative-error-bound`, `ddsketch-no-nan-poison`.

- **Shared code:** `transforms/aggregate/mod.rs` + `lib/ddsketch`.
- **Dominance:** `aggregate-matches-agent` is the **differential roll-up** — any sketch-accuracy,
  bin-count, NaN, clock-skew, or bucketing deviation that changes output will show up as a diff
  against the Agent (if the Agent doesn't share the same deviation). The sub-properties catch
  deviations that are silent in a single happy-path comparison (merge-order accuracy, NaN poison,
  internal bin explosion).
- **Crash subset:** the forward-jump facet of `aggregate-clock-skew-stable` is the live crash/DoS
  property that feeds `data-component-failure-triggers-process-shutdown` (a panicking aggregate is
  exactly the "component finishes unexpectedly" trigger). `aggregate-no-panic-any-window` shares the
  cluster but its original `% 0` panic vector was **closed upstream** (window is now `NonZeroU64`,
  PR #1772) — it remains as a low-cost `Unreachable` regression tripwire, not an active crash bug.
- **NaN crosscut:** `ddsketch-no-nan-poison` shares its boundary with
  `non-finite-values-handled-consistently` (Cluster 6) — same NaN, two angles (sketch internals vs.
  source gating).

## Cluster 5 — Lifecycle & config gating

Properties: `topology-ready-before-intake`, `config-stall-no-deadlock`,
`config-incompatible-refuses-start`, `config-runtime-update-not-revalidated`,
`graceful-shutdown-within-30s`, `data-component-failure-triggers-process-shutdown`.

- **Shared code:** `bin/agent-data-plane/src/cli/run.rs`, `internal/remote_agent.rs`,
  `saluki-core/health`, `topology/running.rs`, `runtime/supervisor.rs`.
- **Lifecycle ordering:** `topology-ready-before-intake` (startup) and `graceful-shutdown-within-30s`
  (shutdown) bracket the run; `config-incompatible-refuses-start` and `config-stall-no-deadlock`
  gate whether startup proceeds at all.
- **Config-gate pair:** `config-incompatible-refuses-start` (startup gate) and
  `config-runtime-update-not-revalidated` (the runtime hole in that same gate) are two halves of one
  guarantee — incompatible config is refused. The first holds; the second is the gap. They share
  `check_and_warn_config` / `ConfigClassifier` and the `run.rs` callsite.
- **Shutdown pair:** `graceful-shutdown-within-30s` (timing/clean completion) and
  `shutdown-drains-no-loss` (Cluster 2; what data survives) are deliberately split views of the same
  shutdown event — test together, assert on different things. Neither dominates.

## Cluster 6 — Untrusted input parsing

Properties: `malformed-dsd-no-crash`, `replay-no-panic-on-malformed-capture`,
`replay-corruption-not-silent-eof`, `non-finite-values-handled-consistently`.

- **Shared mechanism:** parsing attacker-influenced bytes (`saluki-io` DSD codec; replay reader).
- **No-crash subset:** `malformed-dsd-no-crash` and `replay-no-panic-on-malformed-capture` are the
  same property class (untrusted input never crashes) on two different parsers; both feed the
  fail-stop backstop in Cluster 5.
- **Fidelity vs. crash:** `replay-corruption-not-silent-eof` is about *silent wrong data* rather than
  crash — a distinct failure mode on the same reader as `replay-no-panic-on-malformed-capture`.
- **`non-finite-values-handled-consistently`** bridges to Cluster 4 via the NaN/sketch boundary.

## Cluster 7 — Concurrency interleavings

Properties: `interner-reclamation-no-corruption`, plus the timing facets of
`no-silent-interconnect-drop` (multi-sender partial delivery), `forwarder-eventual-delivery`
(shared circuit-breaker state), and `aggregate-context-limit-enforced` (breach flag vs. flush race).

- **Shared theme:** these are the properties where Antithesis's interleaving search is the *primary*
  value (vs. fault injection). `interner-reclamation-no-corruption` is the purest — a loom-tested
  unsafe path where Antithesis explores beyond the bounded model. The others are concurrency facets
  of properties whose main home is another cluster.

## Cluster 8 — Transform & enrichment correctness (added after evaluation)

Properties: `mapper-output-matches-agent`, `mapper-interner-bounded`, `filter-config-reload-correct`,
`tag-filterlist-applied-consistently`, `prefix-filter-ordering-matches-agent`.

- **Shared code:** the DSD transform chain — `dogstatsd_mapper` → `dogstatsd_prefix_filter` →
  `dsd_tag_filterlist` → `dsd_agg` → `dsd_post_agg_filter` (`run.rs:674-679`) — plus the runtime
  config watcher (`saluki-config/src/dynamic/watcher.rs`).
- **Differential subset:** `mapper-output-matches-agent`, `prefix-filter-ordering-matches-agent`, and
  the optional facet of `tag-filterlist-applied-consistently` all ride the **diff-test add-on** and
  are facets of `aggregate-matches-agent` (Cluster 4) at earlier pipeline stages — that roll-up
  catches them only if its corpus exercises mapped/filtered metrics (an open question).
- **Config-reload subset:** `filter-config-reload-correct` is the hub — `tag-filterlist-applied-
  consistently` (stale cache after a Lagged-dropped reload) and `config-runtime-update-not-revalidated`
  (Cluster 5) compose with it. All require the **config-stream add-on topology**; all pass vacuously
  in standalone mode.
- **Interner crosscut:** `mapper-interner-bounded` is a second, independent instance of
  `interner-full-bounded` (Cluster 1) — a distinct 64 KiB interner with its own silent-drop path.
- **Bug-history crosscut:** `prefix-filter-ordering-matches-agent` targets the "moved prefix/filter in
  front of enrich" fix; it is the ordering-regression tripwire for the whole chain.

## Cluster 9 — Events & service-checks paths (added after evaluation)

Properties: `events-sc-no-silent-loss`, `malformed-event-sc-no-crash`, `events-sc-pipeline-reachable`.

- **Shared code:** the always-on `dsd_in.{events,service_checks}` sub-pipelines and their separate
  codecs/encoders. These are the events/service-checks analogues of Cluster 3's `no-silent-interconnect-drop`,
  Cluster 2's `forwarder-eventual-delivery`, and Cluster 6's `malformed-dsd-no-crash` — same
  mechanisms, different always-on edges.
- **Anti-vacuity dependency:** `events-sc-pipeline-reachable` is the R4 anchor that keeps the other
  two from passing trivially under a metrics-dominated workload — a hard dependency, not just a relation.

## Shared-scenario pairs (R10 — count is not 35 independent test efforts)

These pairs share a fault scenario / assertion site and should be implemented together; treat them as
one test effort each for portfolio-sizing:

- `shutdown-drains-no-loss` ⇄ `graceful-shutdown-within-30s` — same shutdown event, different assertions
  (what data survives vs. clean completion in time).
- `non-finite-values-handled-consistently` ⇄ `ddsketch-no-nan-poison` — same NaN, two angles (source
  gating vs. sketch-boundary poison).
- `rss-bounded-under-cardinality` ⇄ its four Cluster-1 sub-properties — roll-up vs. localizers.
- `aggregate-matches-agent` ⇄ its Cluster-4 sub-properties and the Cluster-8 differential subset —
  roll-up vs. localizers/earlier-stage facets.
- `config-incompatible-refuses-start` ⇄ `config-runtime-update-not-revalidated` — startup gate vs. its
  runtime hole.

## Cross-cutting observations

- **One config matrix drives many clusters.** The memory_mode / heap-allocs / disk-persistence
  settings gate Clusters 1 and 2; the protective-on vs. default-off split is the master test variable.
- **Two roll-up properties** (`rss-bounded-under-cardinality`, `aggregate-matches-agent`) each
  dominate a cluster — they're cheap to assert and catch broad regressions, but the sub-properties
  localize causes and catch silent-but-output-neutral deviations. Keep both levels.
- **Fail-stop is the shared backstop.** `data-component-failure-triggers-process-shutdown` is
  downstream of every crash property (Clusters 4, 6) — when any invariant trips a panic, this is what
  must happen next. It belongs to Cluster 5 but connects to all crash properties.
