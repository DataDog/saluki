---
sut_path: /home/ssm-user/src/saluki
commit: 93b9f37baf07004a9235f308305d78ef3d7fc226
updated: 2026-07-15
external_references:
  - path: https://datadoghq.atlassian.net/browse/SMPTNG-737
    why: Antithesis triage umbrella
---

# Existing Antithesis SDK assertions

The codebase is already instrumented on both sides. SUT-side assertions use the `saluki_antithesis` facade (`lib/saluki-antithesis/src/lib.rs`) — a thin, default-off macro wrapper over the SDK, with numeric helpers `always_ge!`/`always_le!`. Intake/harness-side use the raw SDK. Only relevant entries for the liveness/aggregation targets are listed.

## SUT-side — boot liveness (covers stall family #1)

| File:line | Type | Message |
|---|---|---|
| `bin/agent-data-plane/src/main.rs:91` | reachable | agent-data-plane completed bootstrap |
| `bin/agent-data-plane/src/main.rs:153,197` | always_or_unreachable | bootstrap-stage guards |
| `internal/env/host.rs:45` | always_or_unreachable | ADP resolved the Core Agent hostname at boot |
| `internal/env/mod.rs:75,91,100` | always | host / workload / autodiscovery providers connected |
| `internal/remote_agent.rs` | (classify) | initial registration permanent-vs-transient split |

These already catch the boot-gate stall. A fatal boot `exit(1)` trips no Antithesis platform crash property, so these `always`/`always_or_unreachable` anchors plus the harness `eventually_adp_alive` are the only things that catch it.

## SUT-side — aggregation state invariants (covers half of the aggregation target)

| File:line | Type | Message |
|---|---|---|
| `transforms/aggregate/mod.rs:612` | always_le | context map ≤ context_limit |
| `transforms/aggregate/mod.rs:646` | always_ge | flush wall-clock never moves backward (SMPTNG-767) |
| `transforms/aggregate/mod.rs:654` | always_le | zero-value bucket span ≤ 10,000 |
| `transforms/aggregate/mod.rs:672` | sometimes | (flush liveness marker) |
| `transforms/aggregate/mod.rs:693` | always_le | counter-expiry add no overflow |
| `transforms/aggregate/config.rs:68,69` | always_ge/le | histogram percentile quantile in [0,1] |
| `lib/ddsketch/src/agent/sketch.rs:231` | always | DDSketch sample is finite at insert |
| `lib/ddsketch/src/agent/sketch.rs:241` | always_le | DDSketch min ≤ max after insert |
| `lib/ddsketch/src/agent/sketch.rs:784,785` | reachable/always_le | bin collapse reached; bin count within bin_limit |

## SUT-side — data path / stall mechanics

| File:line | Type | Message |
|---|---|---|
| `sources/dogstatsd/mod.rs:726,1240` | always_le | receive-buffer / size bounds |
| `sources/dogstatsd/mod.rs:1958-2001` | unreachable | dispatch output missing / failed mid-buffer |
| `topology/interconnect/dispatcher.rs:93` | sometimes | dispatch backpressure observed |
| `common/datadog/io.rs:719,749` | sometimes | forwarder send outcomes |
| `net/util/retry/queue/mod.rs:288,300` | sometimes/always_le | retry queue dropped-oldest / bound |
| `pooling/elastic.rs:282`, `fixed.rs:189` | unreachable | pool semaphore closed |
| `core/runtime/restart.rs:159` | always_or_unreachable | restart-strategy guard |
| `core/health/mod.rs:638` | always_or_unreachable | health component guard |

## Intake / harness-side

| File:line | Type | Message |
|---|---|---|
| `harness/src/bin/eventually_adp_alive.rs:78` | assert_always | ADP booted: API reachable and DogStatsD socket present |
| `intake/src/series_observation.rs:61` | assert_reachable | intake.first_series_observed (coarse boot-vs-flushed signal) |
| `intake/src/properties/payload/point.rs:25` | assert_always | Pyld20 value not NaN |
| `intake/src/properties/payload/point.rs:46` | assert_always | Pyld21 timestamp future bound |
| `intake/src/properties/payload/metric_payload.rs:16,27` | assert_always | Pyld07 decode, Pyld08 point count |
| `intake/src/properties/payload/envelope.rs:19` | assert_always | Pyld01-03 envelope |
| `scenarios/general/src/bin/parallel_driver_send_dogstatsd.rs` | reachable/sometimes | workload ran / sent a batch |
| `scenarios/general/src/bin/parallel_driver_sketchburst.rs:56` | reachable | sketchburst swept the sketch bins |
| `scenarios/general/src/bin/parallel_driver_poll_stats.rs:67,76` | reachable/sometimes | stats poll / overlap |

## What is MISSING for the target properties

- **Flush-stall liveness ("load in, no load out").** No per-run unique marker probe. `series_observation.rs:61` is only a coarse first-series-ever signal. The intake `Context` (`capture.rs:80`) records identity only, keyed in a dedup `BTreeMap` with `first_seen` — the plumbing to check "a specific marker arrived within a window" exists, but no command uses it. This is the sound signal and it is unbuilt.
- **Aggregation-value oracle.** All intake payload asserts are structural (shape/domain). None reads the flushed point VALUE against an expected sum for a known input. `Context` stores no value, so a "counter sum equals N increments" check needs a small addition in `series_observation.rs evaluate_series`, keyed on a per-run marker name.
