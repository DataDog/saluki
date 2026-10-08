# Anomaly detection telemetry over FIT

Agent Data Plane (ADP) forwards the scalar metrics from its DogStatsD pipeline to an
isolated Agent Anomaly Detection (AAD) process over the FIT shared-memory transport.
This document is the contract for that flow. The payload encoding itself is the Checks
FIT wire contract ([`protocol/checks-fit.md`](../../lib/datadog-agent/checks-protocol/protocol/checks-fit.md)
in the `datadog-checks-protocol` crate), whose semantic source is the existing
`checks/v1/*.proto` definitions; this flow defines no new `.proto` file and reuses the
`DDCHECKS` application protocol, version 1.

## Session direction and ownership

The AAD process is the FIT **consumer**: it owns the shared-memory ring, listens on the
setup endpoint, and learns nothing about ADP beyond the session. ADP is the
**producer**: its `anomalydetection` destination connects to that endpoint and
publishes. Consequences:

- Start the AAD process before ADP. A producer whose setup socket is absent fails the
  destination build, which fails ADP startup with a named connection error.
- The ring capacity is the consumer's choice; ADP learns it from the session offer and
  never configures it.
- FIT has no peer-exit detection and no end-of-stream record. The AAD process runs
  until it is signalled, finishes its analysis on shutdown, and writes its output.

## Configuration

| Key | Default | Meaning |
| --- | --- | --- |
| `anomaly_detection_forwarding_enabled` | `false` | Builds the forwarder destination on the DogStatsD pipeline. |
| `anomaly_detection_ipc_endpoint` | `unix:/tmp/aad-isolated/aad.sock` | FIT setup endpoint the AAD process listens on. |

The endpoint accepts `unix:/absolute/path` or `tcp:127.0.0.1:5102` and is validated at
startup when forwarding is enabled. The Unix socket's parent directory must be owned
by the running user with no group or other access; the AAD process creates
`/tmp/aad-isolated` with mode `0700` before listening.

## What is sent

One `DDCHECKS` metric record (type ID `1`) per scalar data point, drawn from the **raw
DogStatsD source output** — the same tap the statistics destination uses, upstream of
the mapper, aggregation, and the Datadog forwarder. Records are published in pipeline
order.

Field mapping from ADP's internal metric model:

| Checks field | Source | Notes |
| --- | --- | --- |
| `metric_type` | counter, rate, or gauge | Numeric value from `checks/v1/metric.proto`. Sets, histograms, and distributions have no scalar representation and are skipped (counted in logs). |
| `name` | metric context name | |
| `value` | the data point | One record per point; a metric carrying several points becomes several records. |
| `timestamp` | the point's timestamp, or the wall clock | Unix seconds. DogStatsD packets without a client timestamp are stamped at forward time; the AAD engine is data-time driven either way. |
| `tags` | metric context tags | Each rendered as `key:value` or a bare key, in the context's order. |
| `hostname` | metric context host | Empty string when unset; the AAD process resolves empty to its configured default host. |
| `interval_secs` | rate interval, zero otherwise | Rates always carry a nonzero interval here. |

Log, service-check, and event records are never sent in this flow. Logs join in a later
milestone using the same protocol's log record (type ID `2`).

## Delivery semantics

- The forwarder answers liveness probes from startup, including while waiting for the
  FIT session, so a listening AAD process is the only startup ordering requirement.
- A full ring rejects records immediately; they are dropped and logged with counts,
  never retried. The bounded internal channel between the pipeline and the FIT sender
  thread drops under sustained overload the same way. Backpressure is explicit and
  lossy; the AAD process detects gaps through its own counters, not the protocol.
- Changing the payload encoding, record types, or field semantics requires
  incrementing the `DDCHECKS` application protocol version in both peers.

## Future extensions

- **Logs** will reuse the existing log record; no version change needed.
- **Hash-only series identity** (drop name/tags from the wire in favor of ADP-assigned
  hashes) requires a new record type or a version-2 layout, agreed in this contract
  before either side changes.
