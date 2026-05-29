---
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: ADP Confluence space.
  - path: https://datadoghq.atlassian.net/wiki/spaces/AMCC/pages/6640602441/Tag+Filter+RC+Relay+Stress+Test+agent+ADP
    why: Team-authored stress-test spec for the runtime tag-filter RC relay — the exact surface the catalog under-covers.
---

# WILDCARD evaluation — ADP property catalog

Bias: find what Lens 1 (Antithesis-Fit), Lens 2 (Coverage-Balance), Lens 3 (Implementability)
miss. The three lenses accept the SUT analysis's framing, in which ADP is essentially a
*transport* — its job is to not crash and not lose bytes. That framing is the catalog's blind
spot. ADP is also a *transformer*: it maps, filters (twice), tag-filters, enriches, and
aggregates customer data, and most of that transformation is driven by **runtime config that
mutates while data flows**. The catalog has 27 properties; ~24 are about crash/memory/loss and
3 (`aggregate-matches-agent`, `aggregate-clock-skew-stable`, the two ddsketch math props) are
about correctness of values. **Zero** properties cover the map/filter/tag-filter/enrichment
layer's correctness, and zero cover runtime config-reload as a *data-correctness* event rather
than a crash/incompatibility event. That is the headline wildcard finding.

---

## FINDING W1 — The runtime config-reload-while-data-flows surface is almost entirely uncovered (catalog-wide / framing miss)

**Concern.** Five production components subscribe to live config updates over the Core Agent RC
stream and **rebuild correctness-affecting state in place while metrics are flowing through them**:

- `dogstatsd_prefix_filter` — watches 4 keys (`metric_filterlist`, `metric_filterlist_match_prefix`,
  `statsd_metric_blocklist`, `statsd_metric_blocklist_match_prefix`) and rebuilds the allow/block
  filter live (`bin/agent-data-plane/src/components/dogstatsd_prefix_filter/mod.rs:285-330`).
- `dogstatsd_post_aggregate_filter` — same 4 keys
  (`.../dogstatsd_post_aggregate_filter/mod.rs:268-308`).
- `tag_filterlist` — watches `metric_tag_filterlist`, and on change does
  `self.filters = compile_filters(...); self.context_cache = build_context_cache();`
  (`.../tag_filterlist/mod.rs:222, 274-278`).
- `dsd_debug_log` (stats enable) and `internal/logging` (`log_level`) — lower stakes.

The catalog's only two runtime-config properties — `config-incompatible-refuses-start` and
`config-runtime-update-not-revalidated` — treat config purely as a **crash / incompatibility-gate
safety** concern (does an incompatible *key* get applied?). Neither asks the question that the
team's own Confluence page ("Tag Filter RC Relay Stress Test — agent + ADP") is built around:
**after a filter/tag config update lands, is the data that gets through actually filtered
correctly, and is that true under load + interleaving + fault?** This is the single biggest
framing gap. It is squarely in Antithesis's sweet spot (timing of the config-stream event vs the
data-flow event is exactly what a deterministic diff-test cannot explore) and it is a
*data-correctness* failure, not a crash — invisible to every existing process-level assertion.

**Concrete hazards inside this surface that no property names:**

1. **`broadcast::Lagged` drops config updates silently → stale filtering, no recovery until the
   next update.** The watcher reads config over a `tokio::broadcast` channel; on `Lagged` it
   logs a warn and `continue`s without re-reading the missed value
   (`lib/saluki-config/src/dynamic/watcher.rs:60-67`). A transform whose `select!` loop is busy
   draining a full event channel under load (backpressure) can lag the broadcast and **miss a
   filter update entirely**, then run with stale filters indefinitely. This is a
   backpressure × config-reload interaction (a directive-#2 combination) that produces wrong
   filtering output, not a crash. No lens models it.
2. **Partial-deserialize skip → half-applied multi-key config.** A malformed/wrong-typed value
   for one watched key is skipped with a warn while the other keys apply
   (`watcher.rs:43-56`). The prefix/post-agg filters watch 4 interdependent keys; a new
   `metric_filterlist` can apply while its companion `metric_filterlist_match_prefix` is rejected,
   leaving the filter in an inconsistent (new-list / old-match-mode) state — silently wrong
   filtering semantics.
3. **Key-deletion silently clears all filtering.** `tag_filterlist` does
   `new_entries.as_deref().unwrap_or(&[])` (`mod.rs:275`): an RC update that *removes* the key
   yields `None` → empty filter set → all tag filtering silently turned off. Correctness loss with
   no signal.
4. **Cache coherency across swap.** `tag_filterlist` rebuilds `context_cache` on swap (good), but
   the swap and the in-flight event batch are processed in the same `select!` loop — a property
   should pin down that no metric in the post-swap batch is filtered against a stale cache entry.

**Scope.** Production DSD hot path (prefix filter, tag filterlist, post-agg filter all sit
between `dsd_in.metrics` and `dd_metrics_encode`, `run.rs:674-679`). Requires Add-on 1 (config
stream) — these are *not* exercised at all in standalone-mode primary topology (see W4).

**Evidence brief.** `dogstatsd_prefix_filter/mod.rs:285-330`,
`dogstatsd_post_aggregate_filter/mod.rs:268-308`, `tag_filterlist/mod.rs:222,274-278`,
`saluki-config/src/dynamic/watcher.rs:43-67`. Confluence "Tag Filter RC Relay Stress Test"
(referenced in `deployment-topology.md:8`) — the team has already scoped a stress test here that
the catalog does not mirror.

**Suggested action.** Add a property family `config-filter-reload-correct` (suggest 1-2
properties): (a) `Always`(after a filter-config update is observed, every subsequently-emitted
metric reflects the *latest* applied filter — no metric retains a tag the new denylist removes /
no blocked metric passes); (b) `Sometimes`(config update applied under concurrent load) +
`Reachable`(broadcast Lagged path) to prove the stale-config branch is reachable. Differential
formulation (Agent + ADP both receive the same RC update mid-stream, diff output after a quiet
window) reuses the `aggregate-matches-agent` harness and is the most feasible check. At minimum,
elevate the watcher's `Lagged`/partial-deserialize/key-deletion behaviors to first-class hazards
in `config-runtime-update-not-revalidated`, which today scopes them out.

---

## FINDING W2 — The DogStatsD mapper is a self-contained correctness + resource surface with no property (slug: none; nearest `aggregate-matches-agent`)

**Concern.** `dogstatsd_mapper` (`lib/saluki-components/src/transforms/dogstatsd_mapper/mod.rs`)
sits first on the DSD metrics path (`dsd_enrich` chains it, `run.rs:639-641,674`). It:
- compiles user-supplied regexes and does capture-group expansion into new metric **names** and
  new **tag values** (`try_map`, `mod.rs:297-314`) — a rich correctness surface (wrong capture
  expansion = silently renamed metric / wrong tag, customer-visible data corruption);
- has its **own result cache** keyed by metric name (`mod.rs:269-285`) and its **own string
  interner** separate from the aggregate interner (`context_string_interner_size`, default 64KiB);
- silently drops the metric when its interner is full: `resolve_with_origin_tags(...)?` on both the
  cache-hit path (`mod.rs:277-282`) and slow path (`mod.rs:318`) returns `None`, and `try_map`'s
  `None` means the original (pre-map) context is used — but a `None` from the cache-hit branch
  returns `None` from `try_map` with no telemetry. This is a *second*, mapper-local interner
  exhaustion path that the catalog's `interner-full-bounded` (scoped to the aggregate/context
  interner) does not cover.

This is exactly the "correctness of the data that gets through" facet directive #1 flagged. Regex
capture expansion and a name-keyed cache are classic silent-wrong-data bug shapes; ADP claims
Agent-equivalence for the mapper too, and nothing tests it under fault or even names it.

**Scope.** Production DSD hot path, first transform. Memory facet overlaps Category A but with a
distinct interner instance.

**Evidence brief.** `dogstatsd_mapper/mod.rs:31-32` (wildcard match regex), `186-211`
(`build_regex`, regex compiled from `*`→`([^.]*)`), `258-345` (`try_map`, cache + capture
expansion + `?`-drop), separate interner at `103-165`.

**Suggested action.** Add `mapper-output-matches-agent-and-bounded` (or fold into a broadened
transform-correctness differential): assert mapped names/tags match the Agent's mapper for the
same input + profiles; `Always`(mapper interner full ⇒ counted drop, not silent / not heap-spill
depending on config); `Sometimes(mapper_interner_full)`. Also worth a cheap regex-DoS angle:
user-supplied `regex`-type mappings are compiled with the `regex` crate (no catastrophic
backtracking, good) — confirm and note as a *pass* so it isn't re-flagged.

---

## FINDING W3 — Replay-then-aggregate timestamp divergence is a correctness hazard mentioned in the SUT analysis but has no property

**Concern.** SUT analysis §7.9 explicitly states: "a replayed capture buckets differently than
when captured (the aggregator ignores per-record timestamps for non-timestamped metrics)." The
new replay feature (`e88d04b10a`, the most regression-prone area per §8) re-injects captured
DSD records through the live socket; non-timestamped metrics are then bucketed by **current wall
clock at replay time**, not capture time. The catalog has three replay properties
(`replay-no-panic-on-malformed-capture`, `replay-corruption-not-silent-eof`) — all about
*crash / parse fidelity*, none about *aggregation fidelity of replayed data*. Given replay is the
newest, largest, riskiest feature shipping for the first customers, "does replayed data aggregate to the same
result as the original" is a correctness question the catalog skips.

**Scope.** Replay CLI → DSD socket → aggregate. Listener-coverage variant topology.

**Evidence brief.** SUT analysis §7.9; replay re-injection via DSD UDS
(`sources/dogstatsd/replay/`, replay CLI `dogstatsd.rs:394`); aggregate wall-clock bucketing
(`aggregate-clock-skew-stable` evidence).

**Suggested action.** Either (a) add a note/property that replay fidelity for non-timestamped
metrics is *by design* lossy w.r.t. bucketing (document, assert nothing) — needs human input on
intent; or (b) if capture preserves timestamps, assert replayed aggregation matches original
within ratio. Flag for the team; do not over-engineer until intent is confirmed.

---

## FINDING W4 — Standalone-mode primary topology structurally hides the entire control-plane → data-plane config surface (catalog-wide / topology)

**Concern.** `deployment-topology.md` runs ~22/27 properties in **standalone mode**, which
"bypasses the remote-agent config stream" (topology doc, Add-on 1). The doc's own open question —
"Confirm no standalone-only code path masks a production behavior we care about" — is answered by
W1/W2: standalone mode masks the *entire runtime-config-driven filtering/mapping correctness
surface*, because in standalone there is no RC stream to deliver filter/tag updates, so the
`watch_for_updates` branches in all five components **never fire**. Any property that doesn't
force Add-on 1 will *vacuously pass* on this surface. The catalog buries the config stream in an
"Add-on" for 3 properties; in production, RC-driven filtering is a primary, always-on behavior
(the design partner's whole tag-filter relay use case). The topology under-weights it.

**Scope.** Whole config-stream cluster + W1 + W2.

**Evidence brief.** `deployment-topology.md:43,78-101,166-168`; `watcher.rs:29-32`
(`if self.rx.is_none() { pending_forever }` — in standalone the watcher future never resolves).

**Suggested action.** Promote the config-stream add-on to a co-equal primary topology (or make
the stub mandatory), and route W1's new properties through it. Note explicitly in the catalog
that filter/tag/mapper-reload properties are vacuous in standalone mode.

---

## FINDING W5 — Duplicate / over-overlapping properties (catalog hygiene)

Confirmed overlaps the other lenses should reconcile:

1. **`shutdown-drains-no-loss` vs `graceful-shutdown-within-30s`** — the catalog already
   acknowledges the split ("one owns *what data survives*, the other owns *clean completion in
   time*"), and the split is defensible, but both assert against the same 30s-timeout boundary and
   the same forceful-stop path with the same fault setup. Risk: they will be implemented as one
   instrumented run with two assertions, so counting them as two "properties" inflates the
   portfolio. Keep both assertions, but treat as one test scenario.
2. **`data-component-failure-triggers-process-shutdown`** overlaps the forceful-stop clause of
   `graceful-shutdown-within-30s` and the crash trigger of `aggregate-no-panic-any-window` /
   `aggregate-clock-skew-stable` (those panics are *how* you induce the component failure). These
   are three properties sharing one mechanism (component dies → process exits → s6 restarts).
   Fine to keep, but the "induce a panic" half is the same event.
3. **`non-finite-values-handled-consistently` vs `ddsketch-no-nan-poison`** — `non-finite` is
   largely a superset: its invariant *is* `Always(value.is_finite() at DDSketch insert boundary)`,
   which is `ddsketch-no-nan-poison`'s core, plus the ghost-metric clause. The catalog demotes
   `non-finite` to Medium and keeps `ddsketch-no-nan-poison` High with the live `checks_ipc`
   bypass as justification. Reasonable, but the assertion site is *identical*
   (`adjust_basic_stats`/`insert*`); these should be one SUT-side assertion with two reachability
   anchors, not two separately-instrumented properties.
4. **`ddsketch-relative-error-bound` vs `ddsketch-bin-count-bounded`** — `relative-error` is
   already demoted to a library/harness invariant (ADP doesn't call `quantile` live) and
   `bin-count-bounded` owns the live facet. Borderline whether `relative-error` belongs in an
   *Antithesis* catalog at all (it's a pure proptest target with existing proptests) — Lens 1
   territory, flagging for cross-check.

**Suggested action.** Mark 1-3 as "shared-scenario" pairs so the portfolio count isn't read as
independent coverage; let Lens 1 rule on whether `ddsketch-relative-error-bound` is Antithesis-
appropriate at all.

---

## FINDING W6 — Mis-prioritization given the 7.80.0 ship context

**Concern.** The ship context is: first customers, *design partner*, whose documented
interest (Confluence) is the **tag-filter RC relay**. The catalog's two highest-effort, highest-
visibility High items are `aggregate-matches-agent` (heaviest topology, own run) and the
memory-bounds family (much of which is "expected to FAIL by design under default config" — i.e.
known limitations, not regressions). Meanwhile the design partner's actual feature — runtime tag
filtering correctness (W1) — has no property. Relative to ship context, W1 should arguably be a
High before some of the "fails-by-design" memory items, which document known gaps the team
already understands rather than surfacing surprises.

**Suggested action.** Re-rank: W1 (filter-reload correctness) to High; keep the two guaranteed-
crash config/clock findings High (cheap, real, ship-blocking). Consider that several Category-A
"fails by design" properties are really *documentation of a known limitation* and could be Medium
unless the team intends to flip defaults before 7.80.0.

---

## PASSES (things the catalog/lenses got right; do not re-flag)

- The two guaranteed-crash findings (`aggregate-no-panic-any-window` sub-second window divide-by-
  zero; `aggregate-clock-skew-stable` forward-jump flood) were correctly High, cheap, and real —
  verified the code shapes matched the catalog claims. _Update 2026-05-30 — the sub-second
  divide-by-zero is now **fixed upstream** (window typed `NonZeroU64`, PR #1772) and demoted to a
  regression tripwire; the forward-jump flood remains live._
- `ddsketch-no-nan-poison`'s live `checks_ipc` bypass is a genuine, correctly-prioritized latent
  bug.
- The default-config-is-hostile framing for Category A is accurate and well-evidenced.
- `host_enrichment` is static (hostname queried once at build, `host_enrichment/mod.rs:67-75`) —
  **not** a runtime-mutable correctness surface; correctly *absent* from the catalog. Don't add it.
- OTTL filter/transform processors are wired into the **traces** path only
  (`run.rs:561-567`), not the DSD metrics hot path, and their panic sites are `#[cfg(test)]` —
  correctly out of scope for the DSD-focused topology. (Note: if a traces topology is ever added,
  OTTL untrusted-config parsing becomes a parse-safety surface like the replay reader.)
- The mapper compiles user regex via the `regex` crate (no catastrophic backtracking by
  construction) — the regex-DoS angle is a non-issue; record as closed.

## UNCERTAINTIES (report-what's-odd; could not fully resolve)

- **Is RC-stream filter update even reachable from the Core Agent for these keys?** Same open
  question the catalog raises for `config-runtime-update-not-revalidated` ("can a High-severity key
  be delivered, or does the Agent pre-filter?"). If the Agent *does* push `metric_tag_filterlist` /
  `metric_filterlist` updates at runtime (the relay use case implies yes), W1 is High-value
  and live; if these are startup-only in practice, W1 collapses to a code-review note. Needs human/
  team input — pivotal for W1's priority.
- **Does `tag_filterlist` only filtering Counter + sketch metrics (`mod.rs:235-237`) match the
  Agent?** Gauges/rates appear to pass through tag-filtering untouched. Could be intentional
  (sketches/counters are the cardinality drivers) or a correctness gap vs the Agent. Odd enough to
  flag; no property would catch it today.
- **broadcast channel depth for config events** — could not determine the `broadcast::channel`
  capacity that feeds `FieldUpdateWatcher`; how easily `Lagged` triggers under load (W1 hazard 1)
  depends on it. If depth is large, the stale-config window is narrow; if 1-16, it's very
  reachable under backpressure. Worth a one-line code check before sizing the W1 workload.
- **Whether the three "shared-scenario" property pairs (W5) are double-counted in the
  coverage-balance portfolio math** — Lens 2's distribution counts may overstate independent
  coverage by ~3-4 properties.
