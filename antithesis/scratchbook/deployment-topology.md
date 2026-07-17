---
sut_path: /home/ssm-user/src/saluki
commit: 93b9f37baf07004a9235f308305d78ef3d7fc226
updated: 2026-07-15
external_references:
  - path: https://datadoghq.atlassian.net/browse/SMPTNG-737
    why: Antithesis triage umbrella
  - path: test/antithesis/bin/launch.sh
    why: authoritative fault profile and cost knobs
---

# Deployment topology — cheap demonstrative scenarios

Every container and every fault axis is a cost. This document proposes minimal topologies for the target defect families and states the cost delta versus the existing `general` scenario.

## Cost baseline

- `general`: 3 containers (intake, agent [ADP-on SUT], workload) + node term/hang/throttle on `agent` + global cpu_mod + clock_jitter + network faults. 30 m default.
- `differential`: 4 containers, no node faults. Expensive by container count.

The dominant cost knob is the **node-fault branching factor** on `FAULT_NODES`. Dropping it collapses the timeline tree. cpu_mod and clock_jitter are global and cheap and one of them (clock_jitter) is a real aggregation trigger, so keep both.

## Proposal A — `aggregation` (P0, recommended first build)

The cheapest genuinely demonstrative scenario. Reuses the converged `general` topology, drops node faults.

- Containers: 3 — intake, agent (converged, ADP-on), workload. Same as `general`.
- `SCENARIO_FAULT_NODES=""` — **no node faults**. Keep global cpu_mod + clock_jitter.
- Duration: short (10 m is plenty; single-packet triggers converge fast).
- Drivers: existing `send_dogstatsd` + `sketchburst`, plus new marker drivers for `flush-liveness-marker`, `counter-nan-poisoning`, `counter-sum-oracle`.
- New instrumentation: one `eventually_flush_marker` harness command; one value-oracle `assert_always` in `series_observation.rs`.
- Demonstrates: the aggregation family the user named (NaN, sum, sketch, points-per-payload, clock-monotonic under clock_jitter) AND the green-by-default flush-liveness anchor.
- Cost delta vs `general`: removes the entire node-fault branching factor and 2/3 of the duration. Substantially cheaper per run, higher aggregation signal.

Why this over a "slow intake stall" scenario: the forwarder sheds rather than wedges (sut-analysis counter-fact), so backpressure cannot demonstrate a stall. The honest cheap liveness contribution here is a regression anchor, not a live-bug hunt.

## Proposal B — `boot-partition` (P0, freshest-bug guard)

Directly demonstrates the recurring boot-gate stall family (SMPTNG-766, current branch). Not reusable from `general` because the converged image cannot partition ADP from its own Core Agent.

- Containers: 3 — a standalone-ish ADP, a Core-Agent gRPC endpoint (full Agent, or a minimal gRPC stub serving hostname/registration/health), and intake.
- Faults: network faults on the ADP↔Core-Agent link, healed before judging. cpu_mod + clock_jitter global. No node term/hang.
- Duration: boot-length; short.
- Demonstrates: `boot-liveness-under-partition` — the exact family just fixed on this branch. Strong regression guard.
- Cost: cheap in compute (3 containers, short, network-fault-only). More build effort — needs the split topology and possibly a gRPC stub.

## Proposal C — leave `differential` alone
Host-tag identity and cross-lane divergence stay in `differential`. Not cheap; out of scope for this pass.

## Blocker found while de-risking Proposal B

Proposal B as first scoped — split ADP and Core Agent into two containers and network-partition the boot gRPC link — is **infeasible**. ADP dials the Core Agent at a hardcoded `https://127.0.0.1:{cmd_port}` (`lib/datadog-agent/commons/src/ipc/config.rs:209`); the only alternative transport is vsock (host/hypervisor CID). No config points ADP at a Core Agent in another container over TCP. The link is always loopback inside one container, where Antithesis inter-container network faults do not reach. The SMPTNG-766 boot-gate family is therefore not provoked by an inter-container partition — it is provoked by timing faults (cpu_mod, clock_jitter, node-hang) starving the co-located Core Agent past ADP's bounded ~10-attempt retry budget, ingredients that already live in `general`.

The genuinely partition-able cross-container link is ADP → **intake** (the HTTP forwarder). Partitioning it does not wedge ADP (forwarder sheds), but it does let us assert recovery liveness: ADP survives, and load resumes flowing out once the partition heals.

## Decision — built the `liveness` scenario

Goal per the user: demonstrate ADP's liveness is not harmed, cheaply. Built a lean `liveness` scenario:

- Containers: 3 — intake, agent (converged ADP-on SUT), workload. Mirrors `general`.
- `SCENARIO_FAULT_NODES=""` — no node faults. The launcher's always-on network faults partition the ADP→intake link and heal; global cpu_mod + clock_jitter stay on. This is the cheap adversary.
- Duration: short.
- Liveness claims, both green-by-default `always`: (1) `eventually_adp_alive` — ADP boots and stays reachable (reused unchanged); (2) `eventually_flush_marker` — a marker counter sent on the DogStatsD socket eventually reaches intake at `GET /antithesis/metrics/adp`, proving load in ⇒ load out end-to-end after faults quiesce.

The `general` intake records the ADP-on agent's POSTs under the `adp` lane (single-listener `else` branch, `intake/src/bin/intake.rs:96`), so the marker-presence probe reads `/antithesis/metrics/adp` directly.

## Open questions
- Confirm the short duration gives Antithesis enough timelines for a network partition to inject and heal before the `eventually_` fault-quiet judging window.
- Adding node termination on `agent` would additionally demonstrate restart liveness, at the cost of the node-fault branching factor. Deferred to keep this scenario cheap.
