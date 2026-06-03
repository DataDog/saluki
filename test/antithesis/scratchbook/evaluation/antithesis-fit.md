---
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: ADP Confluence space.
---

# Antithesis Fit Evaluation — ADP Property Catalog

Lens: for each of the 27 properties, does *verifying* it require exploring a state space that
deterministic tests cannot reach (timing / concurrency / partial-failure / combinatorial), and does
the chosen assertion TYPE match that mode? Bias: surface problems, not admire the catalog.

Scope key: `catalog-wide` = affects the whole catalog or a family; `property-specific` = one slug.

---

## Findings

### F1. Two "guaranteed-crash" properties are deterministic config/clock checks, not search problems
- **Property/slugs:** `aggregate-no-panic-any-window`, `aggregate-clock-skew-stable` (and the
  catalog-wide note that bills them as "cheap, high-value first targets").
- **Concern:** Both crashes are *deterministic given an input you already know the shape of*. They are
  not states Antithesis must *discover* through interleaving; they are reproducible with a one-line
  unit test. Antithesis's distinctive value (exploring an intractable timing/concurrency space) is
  near-zero here. The real value Antithesis adds is narrower than the catalog implies: (a) *config-space
  exploration* reaching the `{"secs":0,"nanos":N}` Duration shape the registry advertises, and (b) for
  the clock property, a *clock-fault* the deterministic harness cannot inject. Those are genuine but
  modest; the crash itself is trivially provable without a fuzzer.
- **`aggregate-no-panic-any-window` specifically:** _Update 2026-05-30 — this defect is now **fixed
  upstream** (window typed `NonZeroU64`, PR #1772); the analysis below is the historical record of
  the original bug. The property survives only as a regression tripwire._ Confirmed deterministic at
  the time — `mod.rs:818`
  `timestamp % bucket_width_secs` panics on `bucket_width_secs == 0`, and `mod.rs:630`
  `step_by(bucket_width_secs as usize)` panics on 0; `bucket_width_secs = bucket_width.as_secs()`
  (`mod.rs:553`) truncates any sub-1s window to 0. There is no validation (grep confirms only
  definition/default/use sites). This is a single bad config value → guaranteed panic on the first
  metric. A `#[test]` constructing `AggregationState::new(Duration::from_millis(500))` and inserting
  one metric proves it in milliseconds with zero search budget. **It does not need Antithesis to find
  it; it needs a config validator and a unit test.** Verified further: the aggregate transform reads
  `window_duration` only at construction (`mod.rs:92-93`, `:213`) — no `ConfigChangeEvent`/subscribe
  wiring — so this is a *startup-config-only* vector. The catalog's open question "can the gRPC stream
  push this at runtime?" resolves to **no** for this transform (it is not hot-reloaded), which removes
  the one angle that would have made it a live runtime-exploration target.
- **Evidence:** `mod.rs:818,630,553,92-93,213`; registry `aggregate.rs:9`; grep showing no
  `ConfigChangeEvent` subscription in the aggregate transform.
- **Suggested action:** Keep both as **config-exploration / fault-injection targets** but explicitly
  *demote the framing from "headline crash-loop finding" to "cheap config/clock tripwire."* The
  primary fix and primary regression guard is a load-time config validator (`window >= 1s`) plus a
  unit test — that belongs in the SUT, not in search budget. In Antithesis, an
  `assert_unreachable` at the `% 0` site is fine to keep (it costs nothing once instrumented and
  catches a future runtime-push regression), but do not bill these as where Antithesis "earns its
  keep." The clock property retains more Antithesis value than the window property (clock-fault
  injection + the *forward-jump flood bound*, which IS a search-worthy quantitative invariant —
  see F2).

### F2. `aggregate-clock-skew-stable` is partly Antithesis-worthy, partly a deterministic check — the assertion bundles both
- **Property/slug:** `aggregate-clock-skew-stable` (property-specific).
- **Concern:** The property folds two very different things under one slug. The *forward-jump
  zero-value flood* (`Always(zero_value_buckets.len() <= ceil(flush_interval/window)+slack)`) is a
  genuine quantitative invariant that benefits from clock-fault injection during a *live flush race* —
  good Antithesis fit. But the *backward-jump silent gap* and the *pre-epoch `unwrap_or_default()==0`*
  cases are deterministic: step the clock, observe an empty range — reproducible without search. The
  `Always(current_time >= self.last_flush)` monotonicity assertion is essentially a unit-testable
  invariant about a single function. Bundling them means the high-value flood-bound assertion shares a
  slug with deterministic checks, muddying budget attribution.
- **Evidence:** `mod.rs:627-635` (zero-value loop), `time.rs:21-26` (`unwrap_or_default`), property
  file lines 37-55.
- **Suggested action:** Keep the property, but in the workload prioritize the **forward-jump flood
  bound** as the search-worthy assertion (it interacts with flush timing and memory). The
  monotonicity/backward-gap facets are worth an `assert_always`/`Sometimes` but recognize they would
  be caught by a deterministic clock-step test too; they ride along cheaply once clock-fault is wired.

### F3. `ddsketch-relative-error-bound` is pure library/proptest territory — it is not an Antithesis property
- **Property/slug:** `ddsketch-relative-error-bound` (property-specific).
- **Concern:** The property file's own Investigation Log resolves that **ADP never calls
  `DDSketch::quantile` on the live customer path** — histogram percentiles use raw-sample
  `HistogramSummary::quantile` (exact, not a DDSketch bound) and distribution sketches ship raw bins to
  Datadog, quantiled server-side. The only runtime `quantile` caller is the prometheus *internal-
  telemetry* destination. So `Always(|q_est - v| <= eps_rel*|v|)` cannot be anchored to any production
  runtime call. The accuracy half is a *mathematical invariant of a pure function* over arbitrary
  inputs — the textbook definition of a proptest, not a state-space-exploration target. There is no
  timing, concurrency, partial-failure, or live-state dimension to it.
- The merge-associativity half ("merge order under interleaving") *sounds* Antithesis-flavored, but
  the merges happen inside the single-task aggregate transform; the f64 reordering it worries about is
  deterministic for a given merge order and is again a proptest over `merge(A,merge(B,C))` vs
  `merge(merge(A,B),C)`. Antithesis would have to construct the same input sets a proptest constructs,
  with worse shrinking.
- **Evidence:** property file Investigation Log lines 89-128 (no live quantile call); `lib.rs:56`
  (agent sketch re-export); `prometheus/mod.rs:346` (the lone non-production caller).
- **Suggested action:** **Remove from the Antithesis catalog; reassign to proptest/unit territory** (a
  Hegel/proptest test over the agent `DDSketch` accuracy + merge associativity). If anything from this
  property survives into Antithesis, it is already covered by `ddsketch-bin-count-bounded` (bin
  serialization fidelity) and `ddsketch-no-nan-poison` (sum/avg sanity). Demoting to Medium (as the
  catalog did) understates the problem — its assertion type does not match the testing mode at all.
  Keeping it as a live `Always` is misleading because there is no live call to assert against.

### F4. `ddsketch-bin-count-bounded` substantially duplicates existing proptests; Antithesis value is a thin regression tripwire
- **Property/slug:** `ddsketch-bin-count-bounded` (property-specific).
- **Concern:** Verified that strong proptest coverage already exists:
  `prop_bin_count_never_exceeds_limit` at `sketch.rs:925`, plus `prop_output_bins_are_sorted_and_distinct`,
  `prop_output_keys_are_highest_from_input`, and unit tests including
  `trim_left_respects_bin_limit_with_large_weights`. The invariant is *structurally enforced* —
  `trim_left` runs after every mutating method. Antithesis cannot explore an input space the proptest
  doesn't already cover for the *math*; it generates `i16` keys × `u32::MAX` weights already. The
  genuinely non-duplicative value is narrow and real: a **live regression tripwire** that fires if a
  *future mutator* (a new insert helper, a new merge path, the future "merge agent-shipped sketches"
  use case) forgets to call `trim_left` — something a proptest on the *existing* methods cannot catch
  because it tests the methods that exist today.
- **Evidence:** `sketch.rs:919-1023` (proptests), `sketch.rs:255,319,358,579` (trim_left at every
  mutation site), property file lines 38-40, 60-63.
- **Suggested action:** **Keep, but reframe and down-weight.** It is NOT a state-space-exploration win;
  it is a cheap *always-on guard against forgetting `trim_left`*. The assertion (`Always(bins.len() <=
  4096)` at the end of every mutator) is correct and basically free once the SDK is in. But the catalog
  lists it **High** priority — that is too high for a property that mostly re-states an existing
  proptest. Recommend **Medium**: the marginal value over proptests is the live-mutator-regression
  tripwire only, and the `Reachable("trim_left collapsed bins")` anchor is essential or the `Always` is
  vacuous on real production sketches (which rarely exceed 4096 bins under normal cardinality).

### F5. `config-incompatible-refuses-start` is a deterministic gate check that the existing integration suite already exercises
- **Property/slug:** `config-incompatible-refuses-start` (property-specific).
- **Concern:** The gate is a single, *deterministic, ordered control-flow check*: `check_and_warn_config`
  at `run.rs:157` returns `Err` → `exit(1)` *before* `create_topology`/`build`/`spawn`. There is no
  timing, concurrency, or partial-failure dimension — given the config, the outcome is fixed. The
  property file itself notes the existing **integration suite already has "config-check exit codes"
  cases** (sut-analysis §6). The proposed `assert_unreachable("pipeline spawned with high-severity key")`
  after `spawn()` is *statically unreachable* (the `?` already returned) — it is a belt-and-suspenders
  guard against a future reordering regression, not a search target. Antithesis adds essentially nothing
  over a parameterized integration test that feeds N high-severity keys and asserts exit 1.
- **Evidence:** `run.rs:157,331-381`, `main.rs:136-146`; integration "config-check exit codes" cases
  (sut-analysis §6); property file lines 20-49.
- **Suggested action:** Keep as a **cheap config-exploration target** (the SDK markers cost nothing once
  added, and config-space exploration can mutate which key/value is injected), but **demote from High**.
  The deterministic gate is already covered by integration tests; Antithesis's only marginal add is
  fuzzing *which* key triggers it. Most of the catalog's listed value here is duplicative.

### F6. `config-stall-no-deadlock`: the high-value falsification target was retracted; remaining content is a quiescent-hang check with a weak in-process assertion
- **Property/slug:** `config-stall-no-deadlock` (property-specific).
- **Concern:** The property's own Investigation Log **retracts the busy-loop hazard** ("downgrade it
  from highest-value falsification target to a non-issue") because tonic terminates the stream after one
  error → 5s backoff. What remains is: drop the snapshot → ADP hangs *quiescently forever* at
  `ready().await` (no timeout, `lib.rs:694-704`). That is a real and interesting liveness finding, and
  detecting "indefinite quiescent hang vs progress" is reasonable Antithesis fit (timing of the config
  stream is the explored dimension). BUT the assertion is weak: there is no clean in-process assertion
  for "stalled forever" — the file admits the busy-loop guard is "best caught workload-side (monitor
  CPU/log rate)" and that `Always(no panic)` is "implicit." So the property reduces to two reachability
  markers (`Sometimes(config received)`, `Reachable(wait entered)`) plus an out-of-band CPU monitor.
  The `Sometimes(config received)` is trivially satisfiable in the happy path and proves little; the
  load-bearing "hang is quiescent" check is not an SDK assertion at all.
- **Evidence:** property file lines 103-118 and Investigation Log 120-187; `lib.rs:694-704` (no timeout).
- **Suggested action:** Keep — the no-timeout hang is a genuine operational finding worth demonstrating
  — but **be honest that the verifiable artifact is a workload-side CPU/log-rate liveness check, not an
  SDK invariant.** Consider whether the real recommendation is a SUT change (add a diagnostic timeout)
  rather than a test. As written, Antithesis "proves" the hang is quiescent, which is a weak property
  (it cannot prove "never makes progress" — only observe it didn't within the run).

### F7. `data-component-failure-triggers-process-shutdown` — the `Always` is a temporal property the SDK cannot express in-process; the `Reachable` is the only clean assertion
- **Property/slug:** `data-component-failure-triggers-process-shutdown` (property-specific).
- **Concern:** The defensible invariant ("component death is *always* followed by process exit") is a
  **temporal** property across the process lifetime. The property file itself concedes "To express the
  Always invariant in-process is awkward (it is enforced by control flow)" and falls back to "a
  workload-side temporal assertion" via Antithesis query-logs. So the in-process artifact is just
  `assert_reachable` on the shutdown arm (`run.rs:280-283`) — which proves the path *exists*, not that
  it *always* fires. Inducing the death is genuinely Antithesis-flavored (panic injection, sub-second
  window, clean early finish), so the property has real fit; the concern is that the catalog's `Always`
  framing oversells what the SDK can check. The actual guarantee is structural (one `JoinSet`, one
  `wait_for_unexpected_finish` arm) and would be better unit-tested at the topology level for the
  control-flow part.
- **Evidence:** property file lines 96-108; `running.rs:40-51`, `run.rs:280-283`.
- **Suggested action:** Keep (fault-induced component death is good Antithesis fit), but split the
  claim: the `Reachable(death→shutdown path)` is the legitimate SDK assertion; the `Always(death⇒exit)`
  is a **query-logs temporal check**, not an `assert_always`. Make that explicit so the synthesizer
  doesn't double-count it as an in-process invariant.

### F8. Source-side panic/divide-by-zero properties depend on whether a data-component panic actually crashes the *process* — verify the fail-stop chain or the no-crash assertions are unfalsifiable
- **Property/slugs:** `aggregate-no-panic-any-window`, `malformed-dsd-no-crash`,
  `data-component-failure-triggers-process-shutdown`, `source-dispatch-no-misroute` (catalog-wide
  interaction).
- **Concern:** Several "no crash" / "crash is caught" properties hinge on a panic in a *data component
  task* propagating to a process exit Antithesis can observe. The fail-stop model says a panicking
  component → `JoinError` → `wait_for_unexpected_finish` → whole-process shutdown → s6 restart. But
  Antithesis observes a *container that never exits* (s6 restarts ADP in-place, per sut-analysis §6 and
  deployment-topology). If the container masks the exit, an `Always(process up)` workload assertion
  (`malformed-dsd-no-crash`) is **trivially satisfied even when ADP is crash-looping** — the container
  stays up. This is a catalog-wide soundness risk for every "no crash" property whose assertion is
  workload-side "process up." The catalog half-acknowledges this (it routes crash detection through
  SUT-side `assert_unreachable` at panic sites) but the deployment topology's "process up" framing for
  `malformed-dsd-no-crash` (`Always(process up)`) is exactly the unfalsifiable shape under s6.
- **Evidence:** sut-analysis §6 ("container s6 supervisor restarts ADP on exit, so the container never
  actually exits"); `malformed-dsd-no-crash` invariant `Always(process up)`.
- **Suggested action:** Catalog-wide: make every "no crash" assertion SUT-side (`assert_unreachable` at
  the panic site) **and/or** assert against a restart-count / uptime telemetry, NOT "container process
  up." Flag for the workload author that s6 auto-restart can make `Always(process up)` vacuously pass.
  This is the single most important cross-cutting fit hazard.

### F9. `replay-corruption-not-silent-eof` is a data-fidelity property with no fault/timing dimension — input-mutation only, marginal over a fuzz/unit test
- **Property/slug:** `replay-corruption-not-silent-eof` (property-specific).
- **Concern:** The "bad thing" is *silent truncation reported as success* — a deterministic function of
  the input bytes (`reader.rs:84-104`). Given a corrupt length prefix, `read_next` returns `Ok(None)`
  every time; there is no timing/concurrency/partial-failure. The property file even notes the current
  tests *assert* this behavior intentionally and that distinguishing truncation from clean EOF "may
  need a format change." So Antithesis here is just an input fuzzer over a pure parser, and the
  assertion (`AlwaysOrUnreachable(faithful completion)`) presupposes a SUT change that doesn't exist.
  This is unit/proptest territory (corrupt-prefix → expected outcome), not state-space exploration.
- **Evidence:** property file lines 21-43, 89-92; `reader.rs:84-104`.
- **Suggested action:** Keep at **Medium or lower**, but recognize it as a **fuzz/proptest target on the
  reader**, riding the same adversarial-capture corpus as `replay-no-panic-on-malformed-capture`. Its
  Antithesis-specific value is near-zero beyond bundling with the panic property; the real deliverable
  is a maintainer decision (is silent truncation acceptable?) plus a format/telemetry change.

### F10. Several "Sometimes" anchors risk vacuity or astronomically-unlikely satisfaction; audit reachability budget
- **Property/slugs:** catalog-wide, sharpest on `interner-reclamation-no-corruption`,
  `ddsketch-bin-count-bounded`, `forwarder-eventual-delivery`, `disk-persisted-retry-survives-restart`.
- **Concern:** Many properties pair a safety `Always`/`Unreachable` with a `Sometimes` that must be hit
  for the safety claim to be non-vacuous. Two distinct hazards:
  (a) **Hard-to-reach `Sometimes` → vacuous `Always`.** `ddsketch-bin-count-bounded`'s
  `Always(bins<=4096)` is vacuous unless `Reachable("trim_left collapsed bins")` actually fires — but
  real production sketches under normal cardinality rarely exceed 4096 distinct keys, so the collapse
  path may never trigger without a deliberately pathological corpus. If the workload doesn't force it,
  the property passes while proving nothing.
  (b) **`Sometimes` requiring a precise race.** `interner-reclamation-no-corruption` needs
  `Sometimes(drop re-check found resurrected entry)` — the `is_active()` re-check at `map.rs:459`
  returning true. That is a narrow decrement→lock window. It is *exactly* what Antithesis is good at
  (so fit is good), but the workload must run a near-full interner with heap-fallback **off** and high
  churn or the contended path is never pressured (heap-fallback default true defuses it — property file
  config-deps line 67-69). If the workload uses defaults, the `Sometimes` never fires and the safety
  `Always` is vacuous.
- **Evidence:** `ddsketch-bin-count-bounded` lines 75-76; `interner-reclamation-no-corruption` lines
  72-80, config-deps 67-71.
- **Suggested action:** Catalog-wide: for every safety property gated by a `Sometimes`, the workload
  MUST include the configuration/corpus that forces the anchor (small interner, heap-fallback off,
  high-cardinality corpus that exceeds 4096 keys). Recommend the synthesizer add a "vacuity guard"
  column: each `Always`/`Unreachable` is only meaningful if its paired `Sometimes` is *demonstrated*
  reached in the run report. Properties whose `Sometimes` is unreached should be reported as
  inconclusive, not passing.

### F11. `memory-limiter-survives-rss-read-failure` is good fit but its priority is underestimated and gated behind a custom fault
- **Property/slug:** `memory-limiter-survives-rss-read-failure` (property-specific) — *underestimated*.
- **Concern:** This is a textbook Antithesis property — a *partial-failure* (mid-run `/proc` read
  failure) producing a *silent* fail-open (frozen backoff at 0) that no deterministic test reaches; the
  damaging case is a *race* (reads fail *before* RSS crosses threshold). The bare `std::thread` death
  doesn't trigger process shutdown, so it is invisible. Yet the catalog rates it **Medium** ("High if
  RSS reads can realistically fail post-startup"). Given that the whole memory family is "fails by
  design under defaults," the *one* runtime protection silently vanishing is arguably the highest-stakes
  partial-failure in Category A. The Medium rating undersells it. The countervailing fact: it requires a
  **custom `/proc` fault** (deployment-topology flags it as "Custom; must script") and the limiter must
  be *explicitly enabled* (default Disabled), so reachability is conditional.
- **Evidence:** `limiter.rs:99-122` (`.expect()` in the steady-state loop), property file lines 36-50;
  deployment-topology fault table (custom `/proc` fault).
- **Suggested action:** Raise to **High conditional on the custom `/proc` fault being scriptable on the
  tenant** and on the limiter being enabled in that workload variant. If the custom fault cannot be
  built, the property is *unreachable* and should be reported as such rather than silently passing
  (ties to F8/F10 vacuity concerns). This is a case where Antithesis value is *underestimated* by the
  catalog's Medium tag.

### F12. `source-dispatch-no-misroute` — the misroute is "structurally improbable," making the `Unreachable` likely vacuous; the real live hazard (silent uncounted loss) is relegated to a sub-clause
- **Property/slug:** `source-dispatch-no-misroute` (property-specific).
- **Concern:** The property file's own analysis (lines 33-45) concludes that with the current
  `extract`-then-`send_all` ordering, **misroute is structurally impossible** — `extract` removes
  matching events by predicate and recomputes `seen_event_types` before any send. So the headline
  `Unreachable(misroute)` is *expected to be vacuously unreachable today*; it only guards a future
  refactor. The genuinely live, Antithesis-reachable hazard is the **sub-clause**: a `send_all` failure
  drops the extracted events and there is likely **no counter** for it ("possibly fully silent — a
  finding," lines 102-104). That silent-loss-under-downstream-error path IS a partial-failure worth
  exploring, but it is buried as clause (B) under a headline that will read as a vacuous pass.
- **Evidence:** property file lines 33-45, 84-87, 102-104; `mod.rs:1667-1716`.
- **Suggested action:** Re-center the property on the **silent-uncounted-loss-on-dispatch-failure**
  facet (which overlaps `no-silent-interconnect-drop`) and treat the misroute `Unreachable` as a
  cheap future-regression guard explicitly expected to be unreached. As written, the High-value reading
  (misroute) is the vacuous one and the live reading (silent loss) is the footnote.

---

## Passes (properties whose Antithesis fit and assertion type are sound)

- **`rss-bounded-under-cardinality`** — Burst-vs-250ms-sample race + cooperative-only backoff is a
  timing space no deterministic test reaches; `Always(rss <= grant*tol)` with SUT-side
  `Sometimes(backoff_applied && rss_still_climbing)` is the right shape. Strong fit. (Caveat: needs the
  limiter enabled and a real RSS reading not masked by the container — relates to F8.)
- **`retry-queue-bounded-under-outage`** — Sustained-outage saturation + shared circuit-breaker +
  per-endpoint queues + disk eviction is genuine partial-failure/timing territory; `Always(bytes<=cap)`
  + `Sometimes(items_dropped>0)` correctly pairs a true invariant with a non-vacuity anchor. Strong fit.
- **`no-silent-interconnect-drop`** — Backpressure-vs-drop under a slow downstream is exactly an
  interleaving/partial-failure exploration; `Always(discarded delta==0)` on a wired edge + `Sometimes
  (backpressure engaged)` is well-matched. Strong fit.
- **`forwarder-eventual-delivery`** — Liveness after a 5xx/timeout/reset storm then recovery; correctly
  typed as `Sometimes(all-delivered-after-recovery)` (liveness → progress, not instantaneous Always).
  Needs a quiet/heal window — fit is good. Strong fit.
- **`disk-persisted-retry-survives-restart`** — SIGKILL mid-outage + restart + reconcile + poison-file
  injection is the canonical Antithesis crash-durability scenario; the at-most-once slack caveat and the
  `Reachable(persistence-active)` non-vacuity guard are correctly identified. Strong fit.
- **`shutdown-drains-no-loss`** / **`graceful-shutdown-within-30s`** — Shutdown under load with a slow
  intake pushing past the 30s boundary is a timing race; correctly typed as `Sometimes(clean-drain)` +
  `AlwaysOrUnreachable(timeout⇒forceful)`. Good fit. (Minor: the two slugs overlap; the catalog already
  carves who-owns-what cleanly.)
- **`malformed-dsd-no-crash`** — Adversarial packet input across 4 listener types exploring codec/
  framing error paths is good fuzz+fault fit; the SUT-side `Unreachable` at codec panic sites is the
  right artifact. Pass *with the F8 caveat* (don't anchor on container "process up").
- **`replay-no-panic-on-malformed-capture`** — Untrusted-file fuzzing of an unfuzzed, zero-coverage
  path with confirmed OOM/zstd-bomb vectors; isolated in the replay CLI process so a panic IS
  observable (exit). Good fit; the SUT-side `Unreachable` at panic sites + `Reachable(parse executed)`
  is correct.
- **`interner-reclamation-no-corruption`** — Real-scheduler exploration beyond the bounded loom model is
  precisely Antithesis's edge over loom; the overlap/sentinel `Unreachable` is the right artifact. Pass,
  *conditional on the F10 workload config* (small interner, heap-fallback off) or the `Sometimes` is
  vacuous.
- **`aggregate-context-limit-enforced`** — Cardinality-flood × flush-timing × zero-value keep-alive
  interleaving to hit/clear the breach flag is timing-sensitive; `Always(len<=limit)` + `Sometimes
  (breached)` + `Sometimes(events_dropped)` is well-formed. Pass.
- **`interner-full-bounded`** — Fill-the-buffer timing + concurrent intern-vs-drop on the reclamation
  path; the heap-on/heap-off branch split with matched `Sometimes` anchors is correct. Pass.
- **`aggregate-matches-agent`** — Differential property under faults (delayed flush, restart mid-window,
  backpressure) the deterministic `panoramic` harness cannot inject; correctly anchored on the existing
  diff harness as a `finally_`/quiet-window check, not an in-process assertion. Good fit (heavy; its own
  topology). Pass.
- **`ddsketch-no-nan-poison`** — Confirmed LIVE bypass via checks_ipc Histogram → encoder `insert_n`;
  the SUT-side `Always(sum/avg finite)` at the sketch boundary is correct. *Note:* the poisoning itself
  is deterministic given a NaN input (so the "find it" value is modest), but routing a NaN through a
  realistic checks_ipc producer and proving it reaches the encoder boundary across the live topology is
  more than a unit test. Pass, leaning toward "needs the checks_ipc feeder or it's a unit assertion"
  (deployment-topology already flags this).
- **`non-finite-values-handled-consistently`** — Adversarial all-NaN/Inf packets across metric types;
  the honest framing (ghost-metric expected Unreachable on DSD path, NaN-poison live via non-DSD) is
  correct and the assertion types match. Pass.
- **`topology-ready-before-intake`** — Stalling a downstream's readiness / failing a supervisor child is
  a timing/partial-init exploration; the honest narrowing to "readiness-milestone ordering" (not
  "no byte read pre-ready") keeps the `Always` falsifiable. Pass.
- **`config-runtime-update-not-revalidated`** — Inject a high-severity key over the runtime stream after
  a clean start; this is a control-plane→data-plane path the diff-test never touches, and the
  Reachable/Unreachable framing matches the open design question. Reasonable fit (Medium is right).

---

## Uncertainties

- **U1 (F8 severity):** I have not confirmed *how* the Antithesis harness observes ADP process exit
  under the container s6 supervisor — whether snouty/Antithesis sees the inner process restart or only
  the container. If Antithesis can see inner-process restarts (restart count), the "no crash" workload
  assertions are salvageable as-is; if it only sees the container, every `Always(process up)` is
  vacuous. This determines whether F8 is a catalog-wide blocker or a workload-wording nit. sut-analysis
  §6 strongly implies the container masks exits ("never actually exits"), so I lean toward blocker, but
  did not verify the harness's process-observation granularity.
- **U2 (F3 scope):** I treated `ddsketch-relative-error-bound` as fully non-Antithesis based on the
  property's own resolved finding that quantile is never called live. If a future ADP change starts
  querying quantiles in-process (e.g. a new local-rollup feature), the property would re-acquire live
  fit. Flagging that the "remove from catalog" recommendation is contingent on the current no-live-call
  fact remaining true.
- **U3 (F1 fix direction):** Whether `aggregate-no-panic-any-window` should be fixed by clamp vs reject
  vs sub-second support changes whether the surviving Antithesis assertion is `Always(window>=1)` or
  `Unreachable(% 0)`. Unresolved in the catalog ("needs human input"); my "demote to config-validator +
  unit test" recommendation holds regardless of fix direction, but the exact SDK assertion depends on it.
- **U4 (custom-fault availability):** F11's priority bump for `memory-limiter-survives-rss-read-failure`
  is conditional on a scriptable `/proc` RSS-read fault. I could not confirm the tenant supports custom
  faults of this kind; deployment-topology marks it "Custom; must script." If unavailable, the property
  is unreachable, not Medium.
- **U5 (Sometimes-reachability of bin collapse):** F10(a) assumes real production sketches rarely exceed
  4096 distinct keys under normal cardinality. I did not measure a realistic millstone corpus's per-
  sketch key count; if the high-cardinality corpus routinely blows past 4096, the `Reachable(collapse)`
  anchor fires naturally and the vacuity concern for `ddsketch-bin-count-bounded` is reduced.
