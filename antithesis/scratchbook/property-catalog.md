---
sut_path: /home/ssm-user/src/saluki
commit: 93b9f37baf07004a9235f308305d78ef3d7fc226
updated: 2026-07-15
external_references:
  - path: https://datadoghq.atlassian.net/browse/SMPTNG-737
    why: Antithesis triage umbrella; ticket set 738-767
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/pages/6497671050
    why: load-independence liveness doctrine
  - path: git log (full history)
    why: fix-commit triggers for each property
---

# Property catalog — liveness and aggregation

Priority: P0 = cheapest to demonstrate and highest signal, P2 = valuable but needs richer topology or faults. Each property names its Antithesis assertion type, its trigger, its home (SUT-side vs intake-side), and the tickets/commits it guards.

## Liveness

### `boot-liveness-under-partition` (P0)
ADP eventually becomes reachable and binds the DogStatsD socket even when the Core-Agent gRPC stream is partitioned during boot. **Always** (green-by-default; a fatal `exit(1)` trips no platform crash property, so an `always` anchor is the only catch). Trigger: network partition ADP↔Core-Agent across the boot window. Home: SUT-side boot anchors (`internal/env/host.rs:45`, `env/mod.rs:75-100`, `remote_agent.rs`) already exist; harness `eventually_adp_alive` catches the exit. Guards SMPTNG-766 and the current branch. **Needs a split topology** (ADP and a Core-Agent gRPC stub in separate containers) to be partitionable — the converged image cannot partition ADP from its Core Agent.
Open questions: is a lightweight Core-Agent gRPC stub cheaper to stand up than reusing the full Agent? does the partition need to straddle a specific boot sub-step (hostname vs registration vs health check) to be a strong test?

### `flush-liveness-marker` (P0, green-by-default anchor)
Once ADP is up and a uniquely-marked metric is sent, that exact context eventually appears at intake within a bounded window. **Always**, in an `eventually_` command so fault-quiet timing applies. Trigger: driver emits `saluki.antithesis.livemark.<run_nonce>:1|c`; a harness command polls `GET /antithesis/metrics/adp` and asserts the marker context is present. Home: intake-side — only intake proves bytes left ADP. Sound because it reads ONE context's presence (immune to dedup-count false-RED at saturation) and the nonce makes it per-run (immune to global-counter false-GREEN on re-flush). No current-code bug reproduces via it — the forwarder sheds rather than wedges (see sut-analysis counter-fact) — so this is a **regression anchor** against a future wedge (e.g. reintroducing a blocking downstream), not a live-bug hunt. Refines `series_observation.rs:61`.
Open questions: what window bound separates "healthy flush" from "wedge" given the 15 s flush interval plus cpu_mod/clock_jitter skew?

### `component-death-wedge` (P2)
If a single pipeline component task dies, ingest must not silently wedge forever with the UDS still bound. **Sometimes** the pipeline recovers, or **Always** ADP tears down (root supervisor is `OneForOne` with 0 restarts, so death SHOULD bring the process down, not hang). Trigger: node-hang fault on the SUT at a moment that kills one component. Home: SUT-side + the flush marker. Higher cost — needs precise fault timing.

## Aggregation correctness

### `counter-nan-poisoning` (P0)
A single `|g NaN` (or `|c NaN`) sample must not poison a window's aggregate sum. `merge_scalar_sum` (`value/mod.rs:493`) has no NaN guard. **Always** value-not-NaN, already present intake-side as Pyld20 (`point.rs:25`). Trigger: one non-finite DogStatsD sample. No faults. Guards `e955c5a44e`, SMPTNG-753, SMPTNG-764.

### `counter-sum-oracle` (P0)
A per-run-unique counter fed N deterministic increments in one window flushes a point whose value equals the known sum. **Always**, intake-side. Trigger: driver sends N increments to `saluki.antithesis.sumcheck.<nonce>`. Home: **new** assertion in `series_observation.rs evaluate_series` keyed on the marker name — reads the decoded `MetricSeries` point value (the one place the post-aggregation wire value is visible). Composes with `flush-liveness-marker`. This is the missing aggregation-VALUE oracle. No faults.
Open questions: how to bound "one window" from the driver so the sum is deterministic across the 10 s window / 15 s flush?

### `flush-clock-monotonic` (P1)
Flushed payload timestamps never move backward, even under a backward clock step. **Always_ge**, already present SUT-side (`aggregate/mod.rs:646`). Trigger: global clock_jitter fault (near-free). Guards SMPTNG-767 (in progress). Also visible intake-side as Pyld21 timestamp bounds.

### `sketch-bin-bounded` (P1)
DDSketch bin count stays within `bin_limit` and mass is conserved under a bin-collapse sweep. **Always_le** already present (`ddsketch/agent/sketch.rs:785`) + `reachable` collapse. Trigger: `sketchburst` driver (exists) sweeps past `bin_limit` via `|d`. Guards `bc3cc747e5`. No faults.

### `v2-points-per-payload` (P1)
The v2 `/api/v2/series` encoder honors `serializer_max_series_points_per_payload`. **Always_le** point count, intake-side Pyld08 (`metric_payload.rs:27`). Trigger: high points-per-series volume. Guards SMPTNG-765 (OPEN — v2 encoder ignored the limit). No faults.

### `subsecond-window-safe` (P2)
A sub-second aggregate window must not divide-by-zero panic. **Always_or_unreachable**. Trigger: `aggregate` window config < 1 s. Guards `738b6874a9`. Needs a config knob the generator can set.

### `host-tag-identity` (P2)
A `host:` tag is treated as a host dimension, not a normal tag, so context identity matches the Agent. Best as a **differential** property (needs the A/B lane). Guards `380ebef07d`. Not cheap — belongs in `differential`.

## Assumptions
- clock_jitter and cpu_mod are global, symmetric, and near-free; keep them in every cheap scenario.
- Node termination/hang/throttle are the expensive branching axes; a cheap scenario omits them unless the property requires a kill/hang.

## Open questions (catalog-level)
- Should the P0 cheap scenario bundle `flush-liveness-marker` + `counter-nan-poisoning` + `counter-sum-oracle` (no faults, one lean topology) or should boot-liveness-under-partition be built first as the freshest-bug guard? This is the pivotal build decision — see deployment-topology.md.
