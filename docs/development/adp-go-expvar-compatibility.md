# ADP `go_expvar` compatibility

This document is an implementation note for preserving legacy Core Agent telemetry collected through `go_expvar` when the Agent Data Plane (ADP) owns DogStatsD processing.

## System model

ADP processes DogStatsD traffic and exposes its internal counters and gauges as OpenMetrics telemetry. The Core Agent retrieves remote ADP telemetry through the Remote Agent Registry (RAR).

The compatibility path is:

```text
ADP telemetry → RAR → Core Agent mapping and filtering → /debug/vars → go_expvar check
```

The Core Agent does not push values into `/debug/vars`. When `/debug/vars` is read, the bridge combines the local expvar value with the selected ADP value.

`/telemetry` is a combined telemetry view. ADP series carry an `emitter` label so consumers can distinguish ADP from Core Agent series. The expvar bridge should use the remote-agent identity or an equivalent ADP-specific selection and must not double-count the combined view.

## Ownership

### Core Agent

The Core Agent owns the compatibility layer:

- Define the mapping from ADP metric families and labels to legacy expvar metrics.
- Query ADP telemetry through RAR.
- Select only the labels belonging to each expvar metric.
- Combine local and ADP values when the semantics are equivalent.
- Preserve counter monotonicity across ADP restarts and removal from RAR.
- Pass gauges through without counter-reset handling.
- Bound remote query time and cache values within a `/debug/vars` read.
- Validate mapping resolution and keep `agent status` internally consistent.

The implementation should extend the existing remote-agent expvar functionality rather than modify the Core Agent DogStatsD pipeline.

### Saluki / ADP

Saluki owns the telemetry supply:

- Emit the underlying ADP counters and gauges.
- Remap ADP internal metric names to the Core Agent telemetry contract.
- Add instrumentation where no equivalent exists.

The current checkout contains ADP compatibility remapping in `bin/agent-data-plane/src/state/metrics/rules/compat.rs`. This does not replace the need for Core Agent bridge and integration tests.

## DogStatsD mapping matrix

All eleven legacy DogStatsD expvar metrics have an underlying ADP metric in the current implementation. The selectors are part of the contract; summing an entire metric family is incorrect.

| Legacy expvar | ADP source and selector | ADP emits | ADP compatibility rule | Classification |
|---|---|---:|---:|---|
| `dogstatsd_udp/Packets` | `component_packets_received_total`, `component_id=dsd_in`, `listener_type=udp` | Yes | Yes | Exact; Core Agent must consume UDP series |
| `dogstatsd_udp/PacketReadingErrors` | Same source plus `state=error` | Yes | Yes | Exact label-selected mapping |
| `dogstatsd_uds/Packets` | Same source plus `listener_type=unix` or `unixgram` | Yes | Yes | Exact; combine both UDS transports |
| `dogstatsd_uds/PacketReadingErrors` | Same source plus UDS listener and `state=error` | Yes | Yes | Exact; combine both UDS transports |
| `dogstatsd_uds/OriginDetectionErrors` | `component_errors_total`, `component_id=dsd_in`, `error_type=origin_detection` | Yes | Yes in current checkout | Exact if error semantics match; reconcile with the audit document |
| `dogstatsd/MetricPackets` | `component_events_received_total`, `component_id=dsd_in`, `message_type=metrics` | Yes | Yes | Exact; metrics only |
| `dogstatsd/MetricParseErrors` | `component_errors_total`, `error_type=decode`, `message_type=metrics` | Yes | Yes | Exact; metrics only |
| `dogstatsd/EventPackets` | `component_events_received_total`, `component_id=dsd_in`, `message_type=events` | Yes | Yes | Exact; events only |
| `dogstatsd/EventParseErrors` | `component_errors_total`, `error_type=decode`, `message_type=events` | Yes | Yes | Exact; events only |
| `dogstatsd/ServiceCheckPackets` | `component_events_received_total`, `component_id=dsd_in`, `message_type=service_checks` | Yes | Yes | Exact; service checks only |
| `dogstatsd/ServiceCheckParseErrors` | `component_errors_total`, `error_type=decode`, `message_type=service_checks` | Yes | Yes | Exact; service checks only |

The six processed-message metrics are one ADP family split by labels:

```text
message_type=metrics        + state=ok    → MetricPackets
message_type=metrics        + state=error → MetricParseErrors
message_type=events         + state=ok    → EventPackets
message_type=events         + state=error → EventParseErrors
message_type=service_checks + state=ok    → ServiceCheckPackets
message_type=service_checks + state=error → ServiceCheckParseErrors
```

## Core Agent deliverable for DogStatsD

The Core Agent work is one shared compatibility bridge, not one implementation per DogStatsD metric:

1. Define the mapping table and fidelity for all eleven expvar metrics.
2. Retrieve ADP telemetry through RAR.
3. Apply the exact label selectors above.
4. Combine the selected ADP value with the local expvar.
5. Preserve counter monotonicity across ADP restarts.
6. Keep gauges, such as queue sizes, as current values.
7. Fall back to local values when ADP is unavailable or times out.
8. Test mapping resolution, label filtering, restart behavior, and `/debug/vars` availability.

No changes are required to the `go_expvar` check itself or to the Core Agent DogStatsD server for this compatibility work.

## Scope boundaries

- **Aggregator:** separate effort; remaining instrumentation gaps and counter-versus-gauge semantics require design decisions.
- **Forwarder:** mostly existing ADP telemetry with a small number of remaps and one semantic mismatch.
- **Serializer/splitter:** structurally different in ADP; document replacement split-reason metrics rather than forcing expvar mappings.
- **Scheduler and logs:** Core Agent-owned and not ADP parity work.
- **`dogstatsd_contexts_by_origin`:** related operationally but not an expvar bridge concern; treat as a separate emission project.

## Recommended PR split

1. **Core Agent compatibility PR:** shared bridge, label-aware mappings, counter state, timeouts, and tests.
2. **Saluki DogStatsD/Forwarder PR:** reconcile or add the small set of ADP remapping rules.
3. **Saluki aggregator PR:** new counters, verbosity, and flush semantics.
4. **Scope/documentation PR:** splitter, scheduler, logs, and migration guidance.

The mapping matrix should be agreed upon before expanding beyond DogStatsD.
