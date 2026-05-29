---
sut_path: /home/ssm-user/src/saluki
commit: fc4bb29728814ddf9321572b954ec28f58faeb53
updated: 2026-05-30
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: ADP Confluence space — design partner focus on the tag-filter RC relay shaped the bias findings.
  - path: https://datadoghq.atlassian.net/wiki/spaces/AMCC/pages/6640602441/Tag+Filter+RC+Relay+Stress+Test+agent+ADP
    why: Confirms runtime filter config-reload is the design-partner's documented test focus.
  - path: https://github.com/DataDog/saluki/pull/1768
    why: PR review #4393897611 (Copilot) — the G2 filter-deletion wording and three priority alignments reconciled here.
---

# Property Evaluation Synthesis

Four evaluation lenses (Antithesis-fit, coverage-balance, implementability, wildcard) stress-tested
the 27-property catalog as a portfolio. Findings categorized below as **Refinement** (applied
directly), **Gap** (filled via targeted discovery), or **Bias** (escalated to the user). Lens
evidence: `evaluation/{antithesis-fit,coverage-balance,implementability,wildcard}.md`.

Outcome: 8 properties added (catalog 27 → 35), 9 refinements applied, 1 scope bias escalated.

## Gaps (filled)

### G1 — Events & service-checks paths uncovered (always-on) → 3 properties
Coverage-balance F1, wildcard. When DogStatsD is enabled, `dsd_in.events`/`dsd_in.service_checks`
are always-wired production paths (`run.rs:681-684`) with their own ~394/~312-LOC codecs, yet the
27-property catalog was metrics-only. **Filled** with `events-sc-no-silent-loss`,
`malformed-event-sc-no-crash`, `events-sc-pipeline-reachable` (the last is an anti-vacuity anchor so
a metrics-dominated workload can't pass the first two trivially).

### G2 — ADP-as-transformer correctness + runtime filter config-reload → 5 properties
Coverage-balance F4/F5/F10, wildcard W1/W2/W6, the design-partner focus. The catalog covered ADP as
a *transport* but not the mapping/filtering/enrichment *correctness* layer, and treated runtime
config only as a crash gate — never as a data-correctness event, despite the watcher hazards
(`broadcast::Lagged` silent drop → stale filtering, `watcher.rs:36-74`; partial-deserialize
half-apply; key-deletion leaves filtering silently **stale** because the additive `diff_recursive`
emits no change event, `diff.rs:12-48`, while only an explicit empty/null value **clears** it,
`tag_filterlist/mod.rs:274-276`) and the design partner's documented "Tag Filter RC Relay Stress
Test." (Deletion-is-stale vs. explicit-empty-clears is the distinction detailed in
`filter-config-reload-correct.md` Hazard 3.) **Filled** with `mapper-output-matches-agent`, `mapper-interner-bounded`
(a *second* bounded interner with its own silent drop), `filter-config-reload-correct` (the watcher
hazards on live data), `tag-filterlist-applied-consistently`, `prefix-filter-ordering-matches-agent`
(bug-history-sensitive stage ordering). These need the config-stream add-on topology (not standalone)
and the diff-test add-on; noted in each evidence file.

### Gaps NOT filled (folded or escalated)
- **API-key rotation mid-run** (coverage F9): folded as a fault dimension into
  `disk-persisted-retry-survives-restart` / `forwarder-eventual-delivery` rather than a new property.
- **Internal-supervisor restartability** (coverage F8): noted as a minor gap; low priority relative
  to the data path. Left for a future pass.
- **Traces/APM, logs, OTLP pipelines** (coverage F2/F3, wildcard): escalated as the scope **Bias**
  below rather than filled — they are the "broader topology, lower priority" the user deferred.

## Biases (escalated to user)

### B1 — Catalog (and SUT analysis) is framed around metrics-DogStatsD transport
Wildcard W4/W6, coverage F2/F3, multiple uncertainties. Even after gap-filling, the catalog is
DogStatsD-metrics-centric. The **traces/APM, logs, and OTLP pipelines** (`run.rs:506-591,700-758`) —
including a SQL-parsing trace-obfuscation untrusted-input surface and a *second* OTLP forwarder — have
**zero properties** and are absent from the SUT analysis. Whether they are in scope for first-customer
(Agent 7.80.0) delivery is a product judgment the evaluation can't make. The primary topology
also uses **standalone mode**, which structurally hides the entire runtime-config surface (the
watcher never fires) — so the G2 config-reload properties pass vacuously unless the config-stream
topology is promoted to primary. **Escalated** — see the questions posed to the user. This does not
block the catalog; it scopes which add-ons and pipelines get instrumented first.

## Refinements (applied)

- **R1 (catalog-wide, important):** the container's s6 supervisor auto-restarts ADP on exit, so
  "process up" workload assertions are vacuously green even during a crash-loop. **Every no-crash
  property must assert SUT-side `Unreachable` at panic sites (or on restart-count), never container
  liveness.** Applied as a catalog-wide note and reflected in `malformed-dsd-no-crash`,
  `malformed-event-sc-no-crash`, `data-component-failure-triggers-process-shutdown`.
- **R2 (catalog-wide; updated 2026-05-30):** the Antithesis Rust SDK is now wired into ADP behind the
  `antithesis` cargo feature (`bin/agent-data-plane/Cargo.toml`) with an `antithesis_init()` +
  bootstrap `assert_reachable!` probe, and the harness binaries carry workload-side anchors — see
  `existing-assertions.md`. The **"fork ADP + add SDK + build an instrumented image"** prerequisite is
  therefore largely satisfied (the wiring is proven end-to-end); what remains is implementing the ~17
  in-process SUT-side **property** assertions on top of that scaffold. The ~10 workload-only
  properties can still run first.
- **R3 (catalog-wide):** no-loss properties (`no-silent-interconnect-drop`, `forwarder-eventual-delivery`,
  `disk-persisted-retry-survives-restart`, `shutdown-drains-no-loss`, `events-sc-no-silent-loss`)
  **must use TCP (or UDS), not UDP**, or UDP's inherent loss confounds "accepted == delivered." Noted
  in the topology and those properties.
- **R4 (catalog-wide vacuity):** safety properties gated by hard-to-reach `Sometimes` anchors
  (bin-collapse, interner-resurrection race, events/SC reachability) must have the workload force the
  anchor config/corpus; the run synthesizer must report **unreached `Sometimes` as inconclusive, not
  passing**. Added to catalog-wide notes.
- **R5 `ddsketch-relative-error-bound`:** demoted — it is a **library/proptest invariant, not a live
  ADP runtime assertion** (ADP ships raw bins, never calls `DDSketch::quantile` on the customer path).
  Re-scoped to harness-side; priority Medium→Low. (Applied during open-question sync; reaffirmed.)
- **R6 `ddsketch-bin-count-bounded`:** demoted High→Medium — substantially duplicates existing
  proptests; genuine Antithesis value is only a live regression tripwire for a future mutator that
  forgets `trim_left`. The `Reachable(collapse)` anchor is essential or the `Always` is vacuous.
- **R7 `config-incompatible-refuses-start`:** demoted High→Medium — a deterministic ordered gate
  already covered by the integration suite's config-check-exit-code cases; kept as cheap config
  exploration with the `Reachable(refused)` framing (the `Unreachable` is statically unreachable).
- **R8 `source-dispatch-no-misroute`:** re-centered on the **live silent-loss facet** (dispatch
  failure loses events with no/under-counting) rather than the structurally-vacuous misroute
  `Unreachable`; priority kept Medium, framing corrected.
- **R9 `memory-limiter-survives-rss-read-failure`:** priority noted as **High *conditional* on a
  scriptable `/proc` custom fault + limiter enabled**; otherwise it is unreachable. Framing clarified.
- **R10 (de-dup labelling):** marked shared-scenario pairs in `property-relationships.md`
  (`shutdown-drains-no-loss` ⇄ `graceful-shutdown-within-30s`; `non-finite-values-handled-consistently`
  ⇄ `ddsketch-no-nan-poison`) so the portfolio count isn't read as 35 independent test efforts.

## Passes (lenses confirmed sound)

- Category A memory bounds — well-proportioned to the highest-risk surface; each property maps to a
  distinct mechanism.
- Category B forwarder cluster (eventual-delivery / byte-cap / crash-durability) — strong, non-redundant.
- The zero-fault clock crash finding `aggregate-clock-skew-stable` (forward-jump facet) — correctly
  prioritized, cheap, high-value (F1 note: clock vector, not a runtime-discoverable state). Its sibling
  `aggregate-no-panic-any-window` had its `% 0` panic vector **closed upstream** (window is now
  `NonZeroU64`, PR #1772, fc4bb297); it is demoted from a live crash bug to a cheap `Unreachable`
  regression tripwire — see the catalog status note and the bug ledger.
- `ddsketch-no-nan-poison` checks_ipc bypass — genuine live latent bug, correctly High.
- Type mix (~safety-heavy with 6+ liveness) appropriate for a no-crash/no-corruption SUT; reachability
  used correctly as anti-vacuity riders.
- `host_enrichment` static (correctly no property); mapper uses the backtracking-free `regex` crate
  (regex-DoS is a non-issue — recorded closed); OTTL panics are traces-only and test-gated.

## Open uncertainties carried forward (need team input)

- Are traces/APM, logs, OTLP in scope for first-customer delivery? (B1)
- Does the `millstone` corpus exercise events/SC, mapped/filtered metrics, and adversarial histogram
  values — i.e. does `aggregate-matches-agent` implicitly cover some G1/G2 ground?
- Can a `Severity::High` config key, or a filter-config update, actually traverse the RC stream at
  runtime (vs. Core-Agent pre-filtering)? Gates the reachability of the config-reload properties.
- ~~Which faults are enabled for the tenant~~ **Resolved (user, 2026-05-28):** node termination,
  clock jitter, and custom `/proc` faults are all enabled — the crash-recovery, clock-skew, and
  limiter-RSS-failure properties are realizable.
- Does `datadog-intake` support a runtime failure-mode toggle (reject/5xx/slow/hang)?

## Scope decision (user, 2026-05-28)

The traces/APM, logs, and OTLP pipelines are **deferred** (documented exclusion in the catalog), not
filled. DogStatsD metrics + events/service-checks + the runtime config/transform surface is the
first-customer scope. Bias B1 is thereby resolved-as-accepted: the catalog is intentionally scoped,
not accidentally narrow.
