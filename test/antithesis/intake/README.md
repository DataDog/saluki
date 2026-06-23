# A property asserting intake for Datadog Agent-like artifacts

This intake asserts structural and aggregation properties on payloads from
Datadog Agent-like programs and is intended to mimic the constraints set by
Datadog Intake API. This implementation is forked from the other intake in this
project and may be later merged back up, although the goals to make aggregation
assertions here is invasive.

### How This Project Works

This document is the 'specification' for an abstract DogStatsD Agent. We assert
that for any given input stream ADP emits to the intake data that is correctly
shaped and, in a future update, that the aggregation model of ADP is accurate to
the reference implementation of Datadog Agent DogStatsD.

# Properties

## Payloads

The Agent emits outputs to Datadog intake endpoints as payloads. The current
specification covers `/api/v2/series` only. The Agent also emits to
`/api/v3/series`, which a future revision will add.

In this section we define properties that hold for `/api/v2/series` payloads
irrespective of load generation profile. Precisely, a 'payload' is an HTTP
envelope wrap around the compressed bytes of a
[`MetricPayload`](https://github.com/DataDog/agent-payload/blob/0a5f9ebbbe9c2a1f1e671467511f6189d3a3b443/proto/metrics/agent_payload.proto#L30-L72).

Some properties reference rig-controlled parameters. `MaxTags(orgID)` and
`MaxResources(orgID)` are per-org caps with defaults 100 and 500 respectively.
`hostname` is the value the rig passes to the Agent via the `DD_HOSTNAME`
environment variable. The Agent's hostname provider chain resolves this first,
short-circuiting cloud-metadata and OS fallbacks.

| Number | Category      | Name                   | Description                                                    |
|--------|---------------|------------------------|----------------------------------------------------------------|
| Pyld01     | Envelope      | Content-Type           | `Content-Type` in `{application/x-protobuf, application/json}` |
| Pyld02     | Envelope      | Content-Encoding       | `Content-Encoding` in `{deflate, gzip, zstd, identity}`        |
| Pyld03     | Envelope      | API Key                | `DD-Api-Key` header present and non-empty                      |
| Pyld05     | Bytes         | Compressed Size        | body < 500 KiB compressed                                      |
| Pyld06     | Bytes         | Uncompressed Size      | body <= 5 MiB uncompressed                                     |
| Pyld07     | MetricPayload | Decode                 | body decodes as v2 `MetricPayload` via `rust-protobuf`.        |
| Pyld08     | MetricPayload | Point Count            | total points <= configured `serializer_max_series_points_per_payload` |
| Pyld09     | MetricSeries  | Metric Non-Empty       | `MetricSeries.metric` is non-empty                             |
| Pyld10    | MetricSeries  | Metric Length          | `len(metric) <= 350` bytes                                     |
| Pyld11    | MetricSeries  | Metric Alphabetic      | `metric` contains at least one ASCII alphabetic char           |
| Pyld12    | MetricSeries  | Type Enum              | `type` in `{COUNT, RATE, GAUGE}`                               |
| Pyld13    | MetricSeries  | Tag Count              | `len(tags) <= MaxTags(orgID)`                                  |
| Pyld14    | MetricSeries  | Tag Prefix Reserved    | no tag starts with `device:` or `dd.internal.resource:`        |
| Pyld15    | MetricSeries  | Per-Series Point Count | `len(points) <=` configured `serializer_max_series_points_per_payload` |
| Pyld16    | MetricSeries  | Origin Populated       | `origin.{product, category, service}` enum-valid               |
| Pyld17    | Resource      | Host Resource Resolved | intake's `Host()` scan resolves a `(type="host", name=hostname)` resource |
| Pyld18    | Resource      | Resource Count         | `len(resources) <= MaxResources(orgID)`                        |
| Pyld19    | Resource      | Host Name Length       | host `name <= 255` bytes                                       |
| Pyld20    | MetricPoint   | Value Not-NaN          | `value` is not NaN                                             |
| Pyld21    | MetricPoint   | Timestamp Future Bound | `timestamp <= intake_now + 600s`                               |
| Pyld22    | Bytes         | Content-Length         | `Content-Length` absent or value equals body byte count        |
