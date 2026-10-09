# Anomaly detection telemetry over FIT

Agent Data Plane (ADP) forwards the scalar metrics from its DogStatsD pipeline to an
isolated Agent Anomaly Detection (AAD) process over the FIT shared-memory transport.
The datadog-agent can also be a producer of the same endpoint: its anomaly detection
observer forwards the metrics it derives from logs there directly, with no ADP in the
path. This document is the contract for that flow. The payload encoding itself is the Checks
FIT wire contract ([`protocol/checks-fit.md`](../../lib/datadog-agent/checks-protocol/protocol/checks-fit.md)
in the `datadog-checks-protocol` crate), whose semantic source is the existing
`checks/v1/*.proto` definitions; this flow defines no new `.proto` file and reuses the
`DDCHECKS` application protocol, version 1.

## Session direction and ownership

The AAD process is the FIT **consumer**: it owns the shared-memory ring, listens on the
setup endpoint, and learns nothing about its producer beyond the session. Either one of
two peers can be the **producer**, because FIT is single-producer/single-consumer:

- ADP's `anomalydetection` destination, for the DogStatsD metrics path.
- The datadog-agent's anomaly detection observer, for the log pattern metrics path
  (`anomaly_detection.log_pattern_forwarding`, endpoint
  `anomaly_detection.log_pattern_forwarding.endpoint`). It is the same `DDCHECKS`
  version 1 protocol; the agent holds the codec in
  `comp/anomalydetection/checksfit`.

Consequences:

- Start the AAD process before its producer. A producer whose setup socket is absent
  fails: ADP fails startup with a named connection error, and the agent logs the failure
  and retries until the endpoint appears.
- One AAD session accepts one producer and no reconnection: the consumer removes the
  setup socket once the setup handshake completes, so a second producer needs a new AAD
  process. A producer with both paths enabled must therefore pick one endpoint each.
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

Only metric records are sent in either producer flow, and only scalar values: the agent
forwards the metrics its log extractors derive (counters and gauges) exactly like ADP
forwards DogStatsD points, so the AAD process holds no log model. Log, service-check,
and event records (type IDs `2` to `4`) stay unused, even though the protocol reserves
them; forwarding real log records would need the log model on both sides.

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

- **Sending raw log records** (type ID `2`) would carry the pattern string and example
  log alongside the series, which the current metric-only flow cannot: anomaly titles
  show the hashed series name instead. Both producers would have to agree on the log
  model the AAD process would need to store.
- **Hash-only series identity** (drop name/tags from the wire in favor of
  producer-assigned hashes) requires a new record type or a version-2 layout, agreed in
  this contract before either side changes.
