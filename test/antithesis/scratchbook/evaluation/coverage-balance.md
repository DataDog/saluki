---
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: ADP Confluence space.
---

# Coverage Balance Evaluation — ADP Property Catalog (27 properties)

Lens: evaluate the catalog as a **portfolio**. Walk `sut-analysis.md` section by section; for
each risk area check whether a property covers it, whether low-risk areas are over-invested, and
whether the assertion-type mix (safety / liveness / reachability) is appropriate. Cite slugs and
sut-analysis sections. Evidence is grounded in the SUT tree at the pinned commit.

## Method / what I checked against source

- Read all four scratchbook artifacts and all 27 property slug files (present in
  `test/antithesis/scratchbook/properties/`).
- Re-derived the live topology from `bin/agent-data-plane/src/cli/run.rs` (`create_topology`,
  `add_*_pipeline_to_blueprint`, lines 414-758) and `bin/agent-data-plane/src/config.rs:100-156`
  (`*_pipeline_required`).
- Spot-confirmed: events/service_checks are always-wired when DogStatsD is on
  (`config.rs:135-145`); checks_ipc Histogram has no finiteness guard
  (`sources/checks_ipc/mod.rs:195`); host_tags lives in `bin/agent-data-plane/src/components/host_tags/`;
  OTLP/traces/logs/APM pipelines are all wired in `run.rs`.

## Type distribution (portfolio shape)

Counting primary type tags across the 27 properties (compound tags counted by their lead type):

- **Safety-dominant:** ~19 properties are Safety or Safety+X.
- **Liveness-dominant:** 6 (`forwarder-eventual-delivery`, `disk-persisted-retry-survives-restart`,
  `shutdown-drains-no-loss`, `topology-ready-before-intake`, `config-stall-no-deadlock`,
  `graceful-shutdown-within-30s`).
- **Reachability** appears only as a *secondary* clause (paired with Safety/Liveness); there is no
  standalone reachability property.

Assessment: the safety/liveness split is reasonable and matches the headline guarantee (no-crash =
safety, no-loss = both). The portfolio is appropriately weighted toward the two headline families.
The **reachability** dimension is structurally thin — it exists only as `Sometimes`/`Reachable`
anti-vacuity riders. That is acceptable for most properties, but see Finding 7: several
*event/service-check/enrichment* paths have **no reachability anchor at all**, meaning a workload
could pass the whole catalog without ever exercising them.

---

## FINDINGS

### Finding 1 — Events & service-checks intake→enrich→encode→forward path is essentially uncovered, despite being an always-on production path

- **Property/slugs:** catalog-wide (gap). Tangentially touched by `source-dispatch-no-misroute`
  (Medium) and `shutdown-drains-no-loss`, but neither asserts events/SC delivery correctness.
- **Concern:** The catalog is metrics-heavy. When DogStatsD is enabled (the production config),
  `events_pipeline_required()` and `service_checks_pipeline_required()` are **both true**
  (`config.rs:135-145`), so `dsd_in.events → events_enrich → dd_events_encode → dd_out` and
  `dsd_in.service_checks → service_checks_enrich → dd_service_checks_encode → dd_out` are live,
  always-wired edges (`run.rs:681-684`). These are separate codecs
  (`lib/saluki-io/src/deser/codec/dogstatsd/event.rs` 394 LOC, `service_check.rs` 312 LOC, each with
  only 7-8 tests) and separate encoders. No property asserts:
  (a) events/SC parse robustness (malformed event/SC packets — `malformed-dsd-no-crash` is scoped to
  the codec generally but its angle and open questions are metric-framing-centric),
  (b) events/SC eventual delivery or no-silent-loss (the Cat B liveness/safety properties name only
  metrics/forwarder transactions),
  (c) the events/SC encoders' own panic/backpressure behavior.
- **Scope:** Two always-on production sub-pipelines.
- **Evidence brief:** `run.rs:681-684`, `config.rs:135-145`; codec test counts above; sut-analysis §2
  (DSD pipeline diagram explicitly shows the events/service_checks branches) — the analysis names
  them but no property family adopts them.
- **Suggested action:** A targeted discovery pass on the event/service_check codecs + encoders:
  malformed event/SC framing (`_e{...}` and `_sc|...` shapes), no-silent-loss on the events/SC edges
  under backpressure, and an eventual-delivery facet. At minimum add an event/SC `Sometimes(parsed)` /
  `Sometimes(delivered)` reachability anchor so a metrics-only workload doesn't pass vacuously.

### Finding 2 — Entire trace/APM and logs pipelines have zero properties (component blind spot vs. topology)

- **Property/slugs:** catalog-wide (gap).
- **Concern:** `run.rs` wires a full **traces/APM pipeline** (`traces_enrich` with `ottl_filter`,
  `ottl_transform`, `apm_onboarding`, `trace_obfuscation`, `trace_sampler`; `dd_apm_stats`,
  `dd_stats_encode`, `dd_traces_encode`; `run.rs:551-591`) and a **logs pipeline**
  (`dd_logs_encode`; `run.rs:506-521`). The catalog has **no property** touching traces, APM stats,
  OTTL transforms, trace obfuscation/sampling, or logs encoding. The deployment-topology doc also
  never mentions them. This is the largest component blind spot relative to the actual topology.
- **Scope:** Multiple transforms + encoders + a forwarder fan-in to `dd_out`.
- **Evidence brief:** `run.rs:441-457, 506-521, 551-591`; sut-analysis §2 mentions only the DSD/OTLP
  shape and does not enumerate traces/APM/logs — so this is a gap that exists in *both* the analysis
  and the catalog.
- **Suggested action:** Decide explicitly whether traces/APM/logs are **in scope** for ADP's first
  customers (they may be gated off by default — `traces_pipeline_required` only fires for OTLP-native;
  logs only for checks/OTLP-native). If out of scope, document the exclusion in the catalog's
  catalog-wide notes so it's a *deliberate* boundary, not a silent omission. If in scope, at least
  one no-crash/no-silent-loss property is warranted for the APM-stats and obfuscation transforms
  (string-heavy, SQL-parsing obfuscation in `trace_obfuscation/sql.rs` is a classic untrusted-input
  hazard).

### Finding 3 — OTLP pipeline (native + proxy/relay) is named but uncovered; only referenced as a "closed path" in NaN reasoning

- **Property/slugs:** `ddsketch-no-nan-poison`, `non-finite-values-handled-consistently` (mention OTLP
  only to argue it is *closed*); no property *asserts* OTLP behavior.
- **Concern:** `add_otlp_pipeline_to_blueprint` (`run.rs:700-758`) has two distinct modes — **native**
  (`otlp_in` source → metrics_enrich/dd_logs_encode/traces_enrich) and **proxy/relay**
  (`otlp_relay_in` relay + `local_agent_otlp_out` forwarder to the Core Agent, with a separate
  `otlp_traces_decode` decoder path). OTLP is an *untrusted-input gRPC/protobuf parse surface*
  analogous to the replay reader, and it deliberately bypasses aggregation (`run.rs:751-753`). The
  catalog treats OTLP only as a finiteness-closed branch; there is no malformed-OTLP-no-crash, no
  OTLP-relay-forwarder-delivery, and no property on the relay→forwarder edge.
- **Scope:** A second untrusted-input source + a second forwarder (to Core Agent, not Datadog intake).
- **Evidence brief:** `run.rs:700-758`; catalog mentions "OTLP is closed" in
  `ddsketch-no-nan-poison` resolved-question and `non-finite-values-handled-consistently` §Resolved.
- **Suggested action:** Targeted pass: is OTLP enabled for the design partner? If yes, an
  OTLP-equivalent of `malformed-dsd-no-crash` (malformed protobuf / oversized OTLP request) and a
  relay-forwarder delivery property are warranted. The `local_agent_otlp_out` forwarder is a
  *second* egress path the entire Cat B data-loss family ignores.

### Finding 4 — DSD transform chain (mapper / prefix-filter / tag-filterlist / post-agg-filter) has no correctness property despite a documented ordering-bug history

- **Property/slugs:** catalog-wide (gap). `aggregate-matches-agent` (Safety, High) would catch a
  gross divergence end-to-end but is anchored on the `panoramic` diff harness on happy-path load and
  is explicitly a *separate, optional* run (deployment-topology Add-on 2); it is not a targeted
  transform-ordering check.
- **Concern:** sut-analysis §8 calls out **"moved DSD prefix/filter in front of enrich (pipeline
  ordering bug)"** as a notable correctness fix in churn history. The live order is
  `dsd_enrich(mapper) → dsd_prefix_filter → dsd_tag_filterlist → dsd_agg → dsd_post_agg_filter`
  (`run.rs:674-679`). Tag filtering happens both pre- and post-aggregation. None of the 27 properties
  asserts transform-ordering invariants or mapper/filter correctness (e.g. a metric that should be
  prefix-dropped is never aggregated; a mapped name is mapped before filtering). This is a
  bug-history item (the lens explicitly flags it) that did not map to a property.
- **Scope:** Four chained transforms on the hottest metrics path, with regression history.
- **Evidence brief:** `run.rs:638-679`; sut-analysis §8 "moved DSD prefix/filter in front of enrich".
- **Suggested action:** Either (a) explicitly fold transform-ordering correctness into
  `aggregate-matches-agent`'s scope (the diff harness *would* catch a reordering regression if the
  workload includes prefix-filtered / mapped / tag-filtered metrics — confirm the `millstone` corpus
  does), or (b) add a focused ordering property. Currently it relies on an optional, happy-path,
  separate-run diff test — disproportionately weak for a known regression hotspot.

### Finding 5 — Origin detection / workload-tagger enrichment correctness is uncovered

- **Property/slugs:** catalog-wide (gap). `source-dispatch-no-misroute` touches the source but not
  enrichment/tagging.
- **Concern:** sut-analysis §2 and §9 highlight origin detection via **UDS peer credentials**
  (credential errors counted but *do not drop the packet*) and the workload-tagger/workloadmeta
  enrichment. `host_enrichment` and `host_tags` transforms run on every metric/event/SC
  (`run.rs:482-489, 648-655`); `origin.rs` resolves origin tags. No property asserts enrichment
  *correctness* (right tags attached, no cross-contamination of origin between concurrent packets on
  a shared socket, behavior when peer-cred lookup fails). Given multi-tenant origin detection is a
  correctness-critical and concurrency-sensitive area (per-packet credential lookup under a shared
  UDS listener), the zero-property coverage is a disproportionate gap.
- **Scope:** Origin resolver + two enrichment transforms, all on the hot path.
- **Evidence brief:** `sources/dogstatsd/origin.rs`, `transforms/host_enrichment/mod.rs`,
  `bin/.../components/host_tags/`; sut-analysis §2 "Origin detection uses UDS peer credentials;
  credential errors are counted but do not drop the packet", §9.
- **Suggested action:** Discovery pass on origin/tagger enrichment: (a) does a peer-cred failure
  ever attach *another* connection's origin tags (cross-tenant tag leak — a silent data-corruption
  hazard)? (b) is host_tags' gRPC dependency (it is built `from_configuration().await` and only in
  non-standalone mode, `run.rs:486-489`) able to hang/deadlock enrichment if the tagger stream
  stalls — analogous to `config-stall-no-deadlock`? Note: the **primary topology uses UDP/TCP, not
  UDS** (deployment-topology), so origin detection is *structurally unexercised* by the primary run —
  a second blind spot the topology choice creates.

### Finding 6 — dsd_stats statistics tap and dsd_debug_log destination have no property

- **Property/slugs:** catalog-wide (gap). `no-silent-interconnect-drop`'s open question asks whether
  `dsd_stats_out`/`dsd_debug_log_out` can have zero senders — i.e. the catalog *noticed* these
  destinations but did not adopt them.
- **Concern:** `dsd_stats_out` is wired off `dsd_in.metrics` unconditionally (`run.rs:686`);
  `dsd_debug_log_out` conditionally (`run.rs:688-693`). These are extra fan-out consumers on the
  busiest output (`dsd_in.metrics` fans to dsd_enrich, dsd_stats_out, and optionally
  dsd_debug_log_out). Per sut-analysis §4, fan-out delivers *sequentially* and a slow consumer stalls
  the others — so a slow/blocked `dsd_stats_out` or `dsd_debug_log_out` could backpressure the entire
  metrics intake. No property tests this fan-out-stall hazard on these taps.
- **Scope:** Two destinations on the primary metrics output's fan-out.
- **Evidence brief:** `run.rs:672, 686, 688-693`; sut-analysis §4 "an output with N senders … one
  slow consumer stalls delivery to the others"; `no-silent-interconnect-drop` open question.
- **Suggested action:** Either fold the dsd_stats/debug-log fan-out into
  `no-silent-interconnect-drop`'s scope (assert a blocked tap backpressures rather than drops, and
  resolve its own open question about zero-sender cases), or add a fan-out-stall reachability anchor.
  Low-cost since it rides the primary topology.

### Finding 7 — SO_REUSEPORT UDP autoscaling has no property

- **Property/slugs:** catalog-wide (gap).
- **Concern:** sut-analysis §2 calls out `SO_REUSEPORT` UDP autoscaling on Linux
  (`sources/dogstatsd/mod.rs:667-686`, also in `lib/saluki-io/src/net/listener.rs` and
  `net/unix/linux.rs`). Multiple worker sockets bound to the same port is a concurrency/sharding
  surface: packet distribution across workers, per-worker buffer-clear-and-continue interacting with
  shared codec state, and worker count scaling under load. No property addresses multi-listener
  behavior; `malformed-dsd-no-crash` implicitly assumes a single listener.
- **Scope:** UDP listener sharding (Linux production default for high-throughput DSD).
- **Evidence brief:** `sources/dogstatsd/mod.rs` REUSEPORT refs; sut-analysis §2.
- **Suggested action:** Confirm whether REUSEPORT autoscaling is on by default and at what worker
  count; if multi-worker, a no-crash / no-loss property under concurrent multi-socket load is
  warranted (also strengthens `malformed-dsd-no-crash` and `interner-reclamation-no-corruption`,
  which assume the real scheduler but a single read loop).

### Finding 8 — Internal supervisor / control-plane restartability is not a property (only the fail-stop data side is)

- **Property/slugs:** `data-component-failure-triggers-process-shutdown` covers the *data* side;
  no property covers the *supervised internal* side.
- **Concern:** sut-analysis §2 stresses the **crucial split**: the internal supervisor (control
  plane, internal telemetry, env/workload) *is* restartable (OneForOne/OneForAll bounded by
  intensity/period), but the data topology is fail-stop. The catalog has a strong property for the
  fail-stop side but **nothing** asserting the supervised side actually restarts correctly within
  its intensity/period bound, or that exceeding intensity escalates (does a crash-looping internal
  child eventually take down the process, or spin forever?). `graceful-shutdown-within-30s`'s open
  question even notes "the internal supervisor shutdown has **no timeout**" — an unguarded path with
  no property.
- **Scope:** The entire restartable half of the supervision model.
- **Evidence brief:** `bin/agent-data-plane/src/internal/mod.rs`, `internal/control_plane.rs`,
  `runtime/supervisor.rs`; sut-analysis §2 "Erlang/OTP-style Supervisor … OneForOne/OneForAll …
  bounded by intensity/period"; `graceful-shutdown-within-30s` open question (no internal-supervisor
  timeout).
- **Suggested action:** Add a property: induce an internal-supervisor child failure (telemetry /
  workload / control-plane) and assert (a) it restarts within the intensity/period bound
  (`Sometimes(child restarted)`), (b) exceeding intensity escalates to a bounded outcome (not an
  infinite restart spin), and (c) the no-timeout internal shutdown does not let total shutdown exceed
  the operational expectation. This is the complement to the fail-stop property and is currently the
  most under-covered architectural half.

### Finding 9 — API-key rotation mid-run surviving in the retry queue is not a property

- **Property/slugs:** `forwarder-eventual-delivery`, `retry-queue-bounded-under-outage`,
  `disk-persisted-retry-survives-restart` all touch the retry queue but none exercise API-key
  rotation.
- **Concern:** sut-analysis §2 and §8 both flag that retry-queue IDs were *stabilized to survive
  API-key rotation* (now load-bearing). This is a churn-history correctness fix with no property.
  A key rotation mid-outage could (regression) re-key queued transactions such that a persisted entry
  no longer matches its endpoint, dropping or duplicating data on recovery — exactly the durability
  surface `disk-persisted-retry-survives-restart` cares about, but rotation is never injected.
- **Scope:** Retry-queue identity stability across credential change.
- **Evidence brief:** sut-analysis §2 "Retry-queue IDs are built to survive API-key rotation", §8
  "stabilize additional-endpoint retry-queue IDs (now load-bearing for API-key rotation)";
  `common/datadog/io.rs`.
- **Suggested action:** Add an API-key-rotation fault dimension to the existing forwarder/retry
  properties (rotate the API key via config-stream update during an intake outage, then heal, and
  assert no-loss/no-dup recovery). Cheap to fold into `disk-persisted-retry-survives-restart` or
  `forwarder-eventual-delivery` as an additional fault rather than a new property.

### Finding 10 — Two bug-history items mapped; two did not

- **Property/slugs:** `aggregate-matches-agent`, catalog-wide.
- **Concern:** Lens asks which sut-analysis §8 fixes map to properties. Of the four named
  correctness fixes — *histogram compensated summation*, *unitless histogram counts*, *timestamped
  count sampling*, *prefix/filter ordering* — only the latter two are even *implicitly* reachable via
  `aggregate-matches-agent`'s diff harness, and prefix/filter ordering is weakly covered (Finding 4).
  **Histogram compensated summation** and **unitless histogram counts** have no dedicated property;
  they would only surface in the diff test if the `millstone` corpus happens to include the
  triggering histogram shapes and the 1e-8 ratio catches the drift. Compensated-summation regressions
  are precisely the kind of small-magnitude error a 1e-8 ratio might mask under reordered merges
  (related to `ddsketch-relative-error-bound`, demoted to Medium/library-only).
- **Scope:** Two histogram-accuracy regression classes.
- **Evidence brief:** sut-analysis §8 "compensated summation for histograms; unitless histogram
  counts".
- **Suggested action:** Confirm the diff-test corpus exercises histogram metrics with values chosen
  to expose summation error (large + small magnitude mixed), and that the ratio is tight enough.
  Otherwise these regressions are unguarded. Could be a sub-clause of `aggregate-matches-agent` or a
  histogram-specific accuracy property.

### Finding 11 — Possible over-investment: DDSketch library internals carry 3 properties, 2 demoted to library-only

- **Property/slugs:** `ddsketch-bin-count-bounded` (High), `ddsketch-relative-error-bound`
  (Medium, demoted), `ddsketch-no-nan-poison` (High), and `non-finite-values-handled-consistently`
  (Medium) which overlaps the NaN facet.
- **Concern:** Four properties cluster on DDSketch/non-finite correctness. The catalog itself demotes
  `ddsketch-relative-error-bound` to "a library property, not a live ADP runtime invariant"
  (its own §Resolved: ADP does not call `quantile` on the live path) and notes
  `non-finite-values-handled-consistently` overlaps `ddsketch-no-nan-poison`. So ~2 of the 4 are
  partially redundant / not live-path. This is mild over-investment relative to, say, the
  zero-coverage events/SC/traces/OTLP areas (Findings 1-3).
- **Scope:** Sketch correctness cluster.
- **Evidence brief:** `ddsketch-relative-error-bound` §Resolved (quantile not on live path);
  `non-finite-values-handled-consistently` Priority Medium with overlap note;
  `ddsketch-no-nan-poison` is the one genuinely live, High-value member.
- **Suggested action:** Keep `ddsketch-no-nan-poison` (confirmed-live, High) and
  `ddsketch-bin-count-bounded` (live regression tripwire). Consider merging
  `ddsketch-relative-error-bound` and the NaN facet of `non-finite-values-handled-consistently` into
  harness-side library tests rather than Antithesis runtime properties, freeing portfolio attention
  for the uncovered pipelines. Not a correctness error in the catalog — a *weighting* observation.

---

## PASSES (areas where coverage balance is appropriate)

- **Memory & resource bounds (Cat A):** Well-proportioned to its risk. Five properties cover the
  RSS bound, the context cap, interner spill, RSS-read-failure, and the retry-queue byte cap — each
  mapping to a distinct sut-analysis §7 attack surface (§7.1-5, 7). The "fails-by-design under
  defaults" framing is correct and the highest-risk area gets the most attention.
- **Egress data-loss (Cat B forwarder cluster):** `forwarder-eventual-delivery`,
  `retry-queue-bounded-under-outage`, `disk-persisted-retry-survives-restart` together cover the
  circuit-breaker, byte cap, and crash-durability surfaces (sut-analysis §2 egress, §6 gaps 1-3).
  Strong, non-redundant, correctly liveness-typed. (Gap: API-key rotation, Finding 9.)
- **Guaranteed-crash config/clock hazards:** `aggregate-no-panic-any-window` and
  `aggregate-clock-skew-stable` captured the two zero-fault-injection deterministic crashes
  (sut-analysis §7.8, §7.9). _Update 2026-05-30 — §7.8 (sub-second window divide-by-zero) is now
  **fixed upstream** (window typed `NonZeroU64`, PR #1772) and demoted to a regression tripwire; the
  clock-skew forward-jump crash remains live, high value, cheap._
- **Untrusted DSD + replay (Cat E):** `malformed-dsd-no-crash`, `replay-no-panic-on-malformed-capture`,
  `replay-corruption-not-silent-eof` cover the codec and the newest/largest replay feature
  (sut-analysis §6 gap 6, §8). Replay is correctly weighted as the top regression-prone area.
- **Lifecycle/config (Cat D):** `config-stall-no-deadlock`, `config-incompatible-refuses-start`,
  `config-runtime-update-not-revalidated`, plus the two shutdown properties and the fail-stop
  property cover the §7.13 no-timeout wait, the startup gate, and the §2 fail-stop model coherently.
- **Type mix:** Safety-heavy is correct for a "no crash / no corruption" SUT; liveness is present
  exactly where progress (delivery, drain, startup) is the contract.

---

## UNCERTAINTIES (need human/team input or a targeted pass to resolve)

- **Are traces/APM/logs/OTLP in scope for the first-customer delivery?** This single answer
  decides whether Findings 2 and 3 are real gaps or deliberate exclusions. ADP targets Agent 7.80.0
  with `data_plane.enabled` routing DogStatsD; the catalog and topology both implicitly assume
  DogStatsD-only. If that assumption is correct, document the exclusion; if not, these are the
  largest gaps in the portfolio. (Needs team input.)
- **Does the `millstone` correctness corpus exercise events, service_checks, mapped/prefix-filtered
  metrics, and adversarial histogram values?** Determines whether `aggregate-matches-agent`
  implicitly covers Findings 1 (events/SC delivery), 4 (transform ordering), and 10 (histogram
  summation), or whether those are truly unguarded. (Needs a corpus read — `bin/correctness/`,
  `millstone.yaml`.)
- **Is SO_REUSEPORT UDP autoscaling on by default, and at what worker count?** Determines whether
  Finding 7 is a live multi-listener concurrency surface or a single-worker no-op. (Needs a config
  default read.)
- **Is origin detection reachable at all in the planned topology?** The primary topology uses UDP/TCP
  (no UDS), so peer-cred origin detection (Finding 5) is structurally unexercised. Confirm whether
  the listener-coverage UDS variant is actually planned to run, else origin enrichment correctness is
  untested by construction. (Needs topology decision.)
- **Can a high-severity-incompatible key actually arrive over the config stream?** Open in
  `config-runtime-update-not-revalidated`; also gates whether the API-key-rotation-via-config-stream
  fault (Finding 9) is reachable. (Needs Core Agent protocol knowledge / team input.)
