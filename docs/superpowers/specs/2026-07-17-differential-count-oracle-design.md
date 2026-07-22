
# Differential COUNT oracle — design spec

## Summary

The differential scenario (`test/antithesis/scenarios/differential/`) today compares only the *context set* per lane — `(name, tagset, kind)` membership — via the symmetric-difference oracle in `test/antithesis/scenarios/differential/src/contexts.rs` (`Difference::between`). Point *values* and *multiplicity* are discarded in `test/antithesis/intake/src/capture.rs`: `context_of` (line 219) collapses each `Metric` to a bare `Context`, and `Lanes.seen` (line 103) keeps only a `first_seen` timestamp per `(Target, Context)`.

This adds a second, independent oracle: per-context **emission multiplicity** (cumulative point count) per lane. The intake accumulates a point count alongside the existing `first_seen`. A new long-running `anytime_` check binary polls the two lanes live, ages skew client-side, and reds when a *shared* context's count skew has exceeded `K` for longer than `ACCEPTABLE_FLUSH_DELAY`. The existing set oracle is byte-for-byte behavior-preserved.

Note on paths: the intake is the workspace-shared `antithesis-intake` crate at `test/antithesis/intake/`, not a differential-scoped module. It is also built by the `general` scenario (`test/antithesis/scenarios/general/Dockerfile`), but `general` runs a single `--listen-addr` and never exercises the dual-`Target` record path, so this change is additive and inert for it. All `capture.rs`/`http/antithesis.rs` edits below are in `test/antithesis/intake/`.

## Fault model (soundness precondition)

Cumulative counts cannot self-heal: one dropped or duplicated flush is a permanent skew. The oracle is sound only when the two lanes are driven symmetrically. Decision 6 requires disabling every asymmetric fault. Ground-truthing each against the real config and the Antithesis fault docs: node faults are already off, clock jitter and cpu modulation are global and symmetric so they stay on, and the one genuinely asymmetric surface — SUT→intake network faults — is closed with a single documented `custom.exclude_from_network_faults` param. The whole fault-model change is one scenario-scoped config line.

Fault config lives in `test/antithesis/bin/launch.sh` (the `FAULTS` array, lines 102–114, shared by `general` and `differential`) and `test/antithesis/scenarios/differential/launch.env`.

- **Per-lane node kill / hang / throttle — already off, no change.** `launch.env:5` sets `SCENARIO_FAULT_NODES=""`; `launch.sh:107` gates all three node-fault params on a non-empty list, so none are submitted. This already satisfies the "per-lane process kill" exclusion (`launch.env:3-4` documents the reason).

- **Clock skew — keep `clock_jitter` on, no change.** The draft's premise that Antithesis clock faults are "effectively one-sided between the two SUTs" is **factually wrong**. Fault docs (`/docs/concepts/fault_injection/fault_types/`): clock jitter "affects all nodes equally" and "affects every isolation unit simultaneously because Antithesis simulates a shared wall clock, called virtual time, for the whole system." Both lanes' flush intervals shift in lockstep, even for permanent jumps, so cumulative flush counts stay aligned. It is therefore **not** the "one-sided clock skew" decision 6 names, and disabling it would only throw away the SMPTNG-767-class ADP-vs-Agent clock divergence coverage this oracle exists to surface. Do not touch `custom.clock_jitter` in `launch.sh`.

- **cpu_mod / memory pressure — keep on.** Global, transient, self-healing; a throttled lane catches up on its next flush. Absorbed by `K` + aging. No change.

- **Network faults (SUT→intake and driver→SUT) — exclude `intake` and `workload`, one config line.** Fault docs: "all network faults may be asymmetric. One direction may be disrupted while the other works normally." A partition or clog on the `datadog-agent → intake:2049` or `adp → intake:2050` TCP link (the `default` compose network, `docker-compose.yaml`) drops or defers points on one lane only → permanent skew. The unified `run_test` endpoint exposes `custom.exclude_from_network_faults`, a space-separated list of containers to exempt from network faults (default `""`). Exemption protects only the *named* container's participation in a fault — it does **not** protect a link whose other end is still faultable. A `network_partition` isolates a non-exempt container, so partitioning `datadog-agent` severs `datadog-agent→intake` even when `intake` is exempt, starving the agent lane's capture (run `5988e4ed172922f5582cb9d413bf576a-58-7` red'd exactly this way — `agent_points=1` vs `adp_points=3` on a sketch under an active partition). Naming *all four* containers — `intake workload datadog-agent adp` — leaves no faultable network link in the scenario, which is what the count oracle needs. `cpu_mod` and `clock_jitter` stay on (global, symmetric); node faults stay off (empty `FAULT_NODES`). This is exactly the parity model: SUT-affecting global faults on, lane-desyncing faults off.

  Change sites:
  - `test/antithesis/scenarios/differential/launch.env`: add `SCENARIO_EXCLUDE_FROM_NETWORK_FAULTS="intake workload datadog-agent adp"` (new var, mirrors `SCENARIO_FAULT_NODES`).
  - `test/antithesis/bin/launch.sh`: declare `SCENARIO_EXCLUDE_FROM_NETWORK_FAULTS=""` beside the other `SCENARIO_*` declarations, resolve an `EXCLUDE_FROM_NETWORK_FAULTS="${EXCLUDE_FROM_NETWORK_FAULTS-$SCENARIO_EXCLUDE_FROM_NETWORK_FAULTS}"` override (matching the `FAULT_NODES` pattern), and add `--param custom.exclude_from_network_faults="$EXCLUDE_FROM_NETWORK_FAULTS"` to the `FAULTS` array. The param is safe to always pass — the `""` default reproduces today's behavior — so the `general` scenario is unchanged. Update the `FAULTS`-block comment that currently claims "Network faults stay on everywhere."

Net fault-model diff: one `launch.env` line, one `SCENARIO_*` declaration, one override resolve, one `--param`, one comment fix — all scenario-scoped. `general` is byte-for-byte unchanged (empty exclusion). Node faults already off (`SCENARIO_FAULT_NODES=""`); `clock_jitter`/`cpu_mod` deliberately kept.

Residual steady-state skew after these preconditions hold: the two lanes flush on independent timers of equal interval sharing one wall clock, differing by at most one interval of phase at any instant. `K=1` plus aging absorbs this.

## Data model change (intake)

File: `test/antithesis/intake/src/capture.rs`.

1. **New value record.** Replace the `EpochSeconds` value in `Lanes.seen` (line 103) with a 2-field named struct (keep it a struct, not a tuple: every other carrier in this file is named, and the field names map 1:1 onto `ContextAt`):
   ```rust
   struct Seen { first_seen: EpochSeconds, points: u64 }
   ```
   `seen: BTreeMap<(Target, Context), Seen>`. `first_seen` is preserved so the set oracle is unchanged (decision 1).

2. **Count points, not contexts.** `observe_series` (line 185) and `observe_sketches` (line 204) return `Vec<(Context, u64)>`. The point count per metric is `metric.values().len()` (one `(u64, MetricValue)` per point). Replace each `contexts.extend(metrics.iter().filter_map(context_of))` (lines 194, 210) with a loop that, for each metric, computes `context_of(metric)` and pushes `(context, metric.values().len() as u64)`. `context_of` (line 219) and `MetricKind::of` (line 52) are **unchanged** — multiplicity is read at the call sites, orthogonal to identity.

3. **Accumulate in `record`** (line 107): change the signature to `&[(Context, u64)]`. Keep the `datadog.` skip (line 110) and the `added` new-context count semantics unchanged. On `Entry::Vacant` insert `Seen { first_seen: now, points }` and `added += 1`; on `Entry::Occupied` do `slot.get_mut().points += points` and leave `first_seen` alone. Call sites `record_series_v2` / `record_sketches` (lines 147–155) pass the `observe_*` output straight through.

4. **Wire shape.** Add `points: u64` to `ContextAt` (line 88), serialized flat as a bare number beside `first_seen`. `Lanes::contexts` (line 121) maps `Seen` → `ContextAt { context, first_seen, points }`. `LaneView` and the `GET /antithesis/metrics/{target}` handler (`test/antithesis/intake/src/http/antithesis.rs:31`) serialize `ContextAt` as-is and need no change.

## Oracle (new check + assertion)

**Check-side deserialize.** File `test/antithesis/scenarios/differential/src/contexts.rs`: add `points: i64` to `Captured` (line 24). Leave `Cumulative` (line 31), `cumulative()` (line 76), `Member`, and all of `Difference` (lines 105–179) untouched — do not fold count-skew into `Difference` (mixing lane-tagged set membership with signed intersection skew would make one type serve two oracles). Add, mirroring the existing **private** `cumulative()`:
```rust
fn counts(&self) -> BTreeMap<Context, i64> {
    self.contexts.iter().map(|c| (c.context.clone(), c.points)).collect()
}
```
Add a pure, unit-testable helper over the **intersection** only (decision 4 — single-lane contexts are the set oracle's job and stay silent here):
```rust
// shared contexts whose |points_adp - points_agent| > k, with the signed skew
pub fn count_skews(agent: &LaneView, adp: &LaneView, k: i64) -> Vec<(Context, i64)>
```

**New binary** `test/antithesis/scenarios/differential/src/bin/anytime_differential_counts.rs`.

Prefix rationale (decisive, from `/docs/product/test_templates/test_composer_reference/`): the check must run **live, concurrently with the load driver and with faults active** (decisions 2, 6). The template already contains `parallel_driver_send_dogstatsd_differential`. Per the composer reference: `anytime` commands "can run at any time … including during singleton_driver, serial_driver, and parallel_driver execution," faults are "Yes (during drivers)," and they run alongside "Any except first." By contrast `singleton_driver` "only run[s] once on a given timeline," and the compatibility table lists it and `parallel_driver` as co-schedulable only with `anytime` — not with each other — so a `singleton_driver` in this template would not be scheduled beside the load driver, silently defeating decisions 2 and 6. `eventually_` is also wrong: "when an eventually command starts, Antithesis kills all other running commands and stops all fault injection." So this is a straight `anytime_` binary; the aging state living in it (long-running, client-side) satisfies decision 5, and it is safely killed when a later `finally_`/`eventually_` starts.

It mirrors `eventually_differential_contexts.rs` for setup (`antithesis_init`, `Config { intake_addr }`, `reqwest::blocking::Client`) then loops for the scenario's lifetime:

1. `LaneView::fetch` both lanes; compute `count_skews(&agent, &adp, ACCEPTABLE_COUNT_SKEW)`.
2. Maintain an in-memory `BTreeMap<Context, Instant>` of skew-exceeded-since. Insert `Instant::now()` for each newly-exceeding context; drop any context that healed or is no longer shared.
3. `defects` = entries whose `elapsed() > ACCEPTABLE_FLUSH_DELAY`.
4. `assert_always!(defects == 0, "differential.counts_eventually_within_skew", &details)` — green by default, evaluated every poll (decision 2, live not finally). `details` carries a capped sample (local `SAMPLE_LIMIT = 25`) of `{name, tagset, kind, agent_points, adp_points, skew, age_secs}` — rich enough to correlate a red against the fault log during triage.
5. `sleep(POLL_INTERVAL)` and repeat.

`POLL_INTERVAL` is a local const set to ~15s. Aging is wall-clock (`Instant::now()` vs `ACCEPTABLE_FLUSH_DELAY`), not tick-based, so a coarser poll trades a little detection latency (still well inside the 30s budget) for far fewer SDK calls than the draft's 5s. Aging uses the binary's own monotonic `Instant` — the check binary is outside the SUT fault domain (decision 5), so no server-side skew timestamp is needed. Factor the "is this context a defect given the since-map and a clock" step into a pure function so it is unit-testable without sleeping.

No `finally_differential_counts` variant is added (decision 2): the live loop's own trailing `ACCEPTABLE_FLUSH_DELAY` of aging covers post-load drain, and the loop is killed cleanly when a `finally_`/`eventually_` starts. Do not "complete the pair" by analogy with the set oracle's `eventually_`/`finally_` split.

**Wiring.** `test/antithesis/scenarios/differential/Dockerfile`: line 61 (`cargo build … --bins`) already builds the new bin. Add a `cp … anytime_differential_counts …` beside line 66, and a `COPY --from=tools-builder --chmod=755 /usr/local/bin/anytime_differential_counts /opt/antithesis/test/v1/main/anytime_differential_counts` beside line 126. No `Cargo.toml` change (bins are autodiscovered; `harness`, `reqwest`, `clap`, `serde_json`, `antithesis_sdk` are already deps of the differential package).

## Constants

`test/antithesis/harness/src/lib.rs`, beside `ACCEPTABLE_FLUSH_DELAY` (line 13):
```rust
/// Per-context point-count skew between the two lanes absorbed as steady-state
/// flush phase and non-atomic double-read before a shared context is a defect.
pub const ACCEPTABLE_COUNT_SKEW: i64 = 1;
```
`POLL_INTERVAL` and `SAMPLE_LIMIT` stay local to the new binary. They are **not** hoisted into `harness`: hoisting `SAMPLE_LIMIT` (or extracting the shared `Config` clap struct) would require editing the two existing check binaries that otherwise need no change, enlarging the diff to dedup a 3-line struct and a triage-cap literal. Minimal diff means not touching files with no functional reason to change; the existing per-binary copies already establish that pattern.

## Scope & non-goals

- No change to `Difference`, `eventually_differential_contexts.rs`, or `finally_differential_contexts.rs`. The set oracle is behavior-preserved (only `Captured` gains an ignored-by-it field; `Seen.first_seen` preserves its input).
- No kind filter — kind is part of context identity (decision 1). One flat `ACCEPTABLE_COUNT_SKEW` across all kinds; no per-kind (e.g. sketch) tolerance.
- Domain is the intersection only; symmetric-difference contexts stay with the set oracle (decision 4).
- No new intake route, no new crate, no `singleton_driver`/`finally_` variant. Extend `capture.rs`, `contexts.rs`, `harness`; add one `anytime_` bin; add one scenario-scoped fault-config line in `launch.env`/`launch.sh`.
- Asserting point multiplicity only, not value correctness (consistent with README caveat 2).
- **Accepted residual risk (document, do not code):** cumulative point counts are not idempotent to at-least-once HTTP delivery. An ordinary forwarder retry-on-timeout (possible even under the kept cpu_mod fault, no injected fault required) can permanently inflate one lane's count. `K=1` + aging absorb small/symmetric cases but not an asymmetric retry burst. Adding request-level dedup would be real scope expansion against the minimal-diff constraint, so it is out of scope; add one sentence to the README Caveats naming this as an accepted, triage-cross-checked risk. See open questions.

## Testing (TDD)

- `test/antithesis/intake/src/capture/tests.rs`: (a) re-recording a context sums `points` while `first_seen` stays at first arrival; (b) `points` reflects `metric.values().len()` across multi-point series and sketches; (c) `datadog.`-prefixed points are never counted. Update `contexts_serialize_to_the_flat_wire_shape` (line 19) to expect `"points": N`. Update the `Op` generator / `lanes_match_a_replay_oracle` / `record_returns_the_count_of_new_contexts` and the `record` call sites for the `&[(Context, u64)]` signature (this churn is required by decision 1 regardless of representation).
- `test/antithesis/scenarios/differential/src/contexts.rs` tests: `count_skews` is silent when only one lane carries a context (intersection), silent at skew ≤ K, reports at skew > K, and returns the correct signed skew. Update `deserializes_the_flat_wire_shape` (line 250) to include `points`.
- Aging: unit-test the pure "defect given since-map + clock" function (enter → persist past budget → defect; heal before budget → no defect) without sleeping.
- `make antithesis-validate-differential` for compose/harness wiring; `make antithesis-build-differential` to confirm the new bin builds and copies.

## Open questions (for human decision)

1. **Cumulative point counts are not idempotent to at-least-once HTTP delivery.** With `intake` excluded from network faults the SUT→intake link no longer times out from a partition, but a `cpu_mod`-induced slow intake response could still trip a forwarder retry-on-timeout, double-POSTing and permanently inflating one lane's count. `K=1` + aging may not absorb an asymmetric retry burst. Accept as a documented, triage-cross-checked residual risk, or does the oracle eventually need request-level dedup (scope expansion)?
2. **Resolved by run `5988e4ed...-58-7`:** excluding only `intake` did *not* keep the capture path clean — a partition on the non-exempt `datadog-agent` still severed `datadog-agent→intake`. The exclusion now names all four containers. Open sub-question: confirm the four-container exclusion leaves the scenario with genuinely zero network faults (a follow-up run should show no `network_partition` in `active_faults` on any `counts_within_skew` evaluation).
3. **Multiple `anytime_` instances.** If Antithesis launches concurrent instances of the `anytime_` count binary, each keeps its own independent aging map (redundant but sound, all green-by-default). Confirm the design need not assume a single instance.
