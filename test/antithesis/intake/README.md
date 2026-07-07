# A property asserting intake for Datadog Agent-like artifacts

This intake asserts structural properties on payloads from Datadog Agent-like programs and is intended
to mimic the constraints set by Datadog Intake API. This implementation is forked from the other intake
in this project and may be later merged back up, although the goals to make aggregation assertions here
are invasive.

## How this project works

This document is the specification for an abstract DogStatsD Agent. We assert that for any given input
stream ADP emits to the intake data that is correctly shaped and, in a future update, that the
aggregation model of ADP is accurate to the reference implementation of Datadog Agent DogStatsD.

The differential scenario adds one narrower oracle. For the same generated configuration and workload,
ADP and the Datadog Agent must eventually report the same metric contexts.

## Properties

### Payloads

The Agent emits outputs to Datadog intake endpoints as payloads. The current specification covers
`/api/v2/series` only. The Agent also emits to `/api/v3/series`, which a future revision will add.

In this section we define properties that hold for `/api/v2/series` payloads irrespective of load
generation profile. Precisely, a payload is an HTTP envelope around the compressed bytes of a
[`MetricPayload`](https://github.com/DataDog/agent-payload/blob/0a5f9ebbbe9c2a1f1e671467511f6189d3a3b443/proto/metrics/agent_payload.proto#L30-L72).

Some properties reference rig-controlled parameters. `MaxTags(orgID)` and `MaxResources(orgID)` are
per-org caps with defaults 100 and 500 respectively.

| Number | Category | Name | Description |
| --- | --- | --- | --- |
| Pyld01 | Envelope | Content-Type | `Content-Type` in `{application/x-protobuf, application/json}` |
| Pyld02 | Envelope | Content-Encoding | `Content-Encoding` in `{deflate, gzip, zstd, identity}` |
| Pyld03 | Envelope | API Key | `DD-Api-Key` header present and non-empty |
| Pyld05 | Bytes | Compressed Size | body < 500 KiB compressed |
| Pyld06 | Bytes | Uncompressed Size | body <= 5 MiB uncompressed |
| Pyld07 | MetricPayload | Decode | body decodes as v2 `MetricPayload` via `rust-protobuf` |
| Pyld08 | MetricPayload | Point Count | total points <= configured `serializer_max_series_points_per_payload` |
| Pyld09 | MetricSeries | Metric Non-Empty | `MetricSeries.metric` is non-empty |
| Pyld10 | MetricSeries | Metric Length | `len(metric) <= 350` bytes |
| Pyld11 | MetricSeries | Metric Alphabetic | `metric` contains at least one ASCII alphabetic char |
| Pyld12 | MetricSeries | Type Enum | `type` in `{COUNT, RATE, GAUGE}` |
| Pyld13 | MetricSeries | Tag Count | `len(tags) <= MaxTags(orgID)` |
| Pyld14 | MetricSeries | Tag Prefix Reserved | no tag starts with `device:` or `dd.internal.resource:` |
| Pyld15 | MetricSeries | Per-Series Point Count | `len(points) <=` configured `serializer_max_series_points_per_payload` |
| Pyld16 | MetricSeries | Origin Populated | `origin.{product, category, service}` enum-valid |
| Pyld17 | Resource | Host Resource Resolved | every series resolves a non-empty `(type="host")` resource and all series in a payload share one host |
| Pyld18 | Resource | Resource Count | `len(resources) <= MaxResources(orgID)` |
| Pyld19 | Resource | Host Name Length | host `name <= 255` bytes |
| Pyld20 | MetricPoint | Value Not-NaN | `value` is not NaN |
| Pyld21 | MetricPoint | Timestamp Future Bound | `timestamp <= intake_now + 600s` |
| Pyld22 | Bytes | Content-Length | `Content-Length` absent or value equals body byte count |
| Pyld23 | MetricSeries | Tag Length | each tag `<= 200` bytes (`MaxTagLength`) |
| Pyld24 | MetricSeries | Tag Set Size | total tag bytes per series `<= 100 KiB` (`MaxTagSetSize`) |

### Differential context capture

The differential scenario uses the same intake binary for both lanes:

- Datadog Agent lane: `POST /api/v2/series`
- ADP lane: `POST /api/v2/series`
- Private control API: `GET /antithesis/metrics/agent`
- Private control API: `GET /antithesis/metrics/adp`

For context equivalence, a metric context is:

- metric name
- canonical tag list
- metric type

The intake folds each captured metric down to its canonical context and stores the
deduplicated set per lane, but it does not compare them. The control API returns those context
sets. The differential workload command fetches both sets and owns the Antithesis assertion for
eventual equivalence.

The context oracle intentionally does not assert aggregate values, sketch values, event payloads, or
service-check payloads. Those remain covered by the normal workload generation and payload structural
assertions rather than by the context-equivalence check.
