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

The Agent emits outputs to Datadog intake endpoints as payloads. The `Pyld` properties below are
asserted on `/api/v2/series`. The Agent also posts native v3 series to `/api/intake/metrics/v3/series`,
which the intake now captures for the differential oracles with a production-faithful decoder. The
[v3 series](#v3-series-apiintakemetricsv3series) section describes that decode.

In this section we define properties that hold for `/api/v2/series` payloads irrespective of load
generation profile. Precisely, a payload is an HTTP envelope around the compressed bytes of a
[`MetricPayload`](https://github.com/DataDog/agent-payload/blob/0a5f9ebbbe9c2a1f1e671467511f6189d3a3b443/proto/metrics/agent_payload.proto#L30-L72).

Some properties reference rig-controlled parameters. `MaxTags(orgID)` and `MaxResources(orgID)` are
per-org caps with defaults 100 and 500 respectively.

The `v2` and `v3` columns say whether the property holds for each series wire format. `v2` is asserted
today. `v3` records how the property maps to the native v3 wire — `yes` where it is identical, or the
difference — and is not yet asserted (v3 is capture-only, see [v3 series](#v3-series-apiintakemetricsv3series)).

| Number | Category      | Name                   | Description                                                                                           | v2  | v3                             |
| ------ | ------------- | ---------------------- | ----------------------------------------------------------------------------------------------------- | --- | ------------------------------ |
| Pyld01 | Envelope      | Content-Type           | `Content-Type` in `{application/x-protobuf, application/json}`                                        | yes | yes                            |
| Pyld02 | Envelope      | Content-Encoding       | `Content-Encoding` in `{deflate, gzip, zstd, identity}`                                               | yes | yes                            |
| Pyld03 | Envelope      | API Key                | `DD-Api-Key` header present and non-empty                                                             | yes | yes                            |
| Pyld05 | Bytes         | Compressed Size        | body < 500 KiB compressed                                                                             | yes | yes                            |
| Pyld06 | Bytes         | Uncompressed Size      | body <= 5 MiB uncompressed                                                                            | yes | yes                            |
| Pyld07 | MetricPayload | Decode                 | body decodes as v2 `MetricPayload` via `rust-protobuf`                                                | yes | no — decodes as v3 `Payload`   |
| Pyld08 | MetricPayload | Point Count            | total points <= configured `serializer_max_series_points_per_payload`                                 | yes | yes — sum `numPoints[]`        |
| Pyld09 | MetricSeries  | Metric Non-Empty       | `MetricSeries.metric` is non-empty                                                                    | yes | yes                            |
| Pyld10 | MetricSeries  | Metric Length          | `len(metric) <= 350` bytes                                                                            | yes | yes                            |
| Pyld11 | MetricSeries  | Metric Alphabetic      | `metric` contains at least one ASCII alphabetic char                                                  | yes | yes                            |
| Pyld12 | MetricSeries  | Type Enum              | `type` in `{COUNT, RATE, GAUGE}`                                                                      | yes | no — v3 also carries `SKETCH`  |
| Pyld13 | MetricSeries  | Tag Count              | `len(tags) <= MaxTags(orgID)`                                                                         | yes | yes — pre-fold tags            |
| Pyld14 | MetricSeries  | Tag Prefix Reserved    | no tag starts with `device:` or `dd.internal.resource:`                                               | yes | yes — pre-fold tags            |
| Pyld15 | MetricSeries  | Per-Series Point Count | `len(points) <=` configured `serializer_max_series_points_per_payload`                                | yes | yes — `numPoints[i]`           |
| Pyld16 | MetricSeries  | Origin Populated       | `origin.{product, category, service}` enum-valid                                                      | yes | yes — via a dictionary tuple   |
| Pyld17 | Resource      | Host Resource Resolved | every series resolves a non-empty `(type="host")` resource and all series in a payload share one host | yes | yes                            |
| Pyld18 | Resource      | Resource Count         | `len(resources) <= MaxResources(orgID)`                                                               | yes | yes — from `dictResourceLen`   |
| Pyld19 | Resource      | Host Name Length       | host `name <= 255` bytes                                                                              | yes | yes                            |
| Pyld20 | MetricPoint   | Value Not-NaN          | `value` is not NaN                                                                                    | yes | yes — floats + sketch summary  |
| Pyld21 | MetricPoint   | Timestamp Future Bound | `timestamp <= intake_now + 600s`                                                                      | yes | yes                            |
| Pyld22 | Bytes         | Content-Length         | `Content-Length` absent or value equals body byte count                                               | yes | yes                            |
| Pyld23 | MetricSeries  | Tag Length             | each tag `<= 200` bytes (`MaxTagLength`)                                                              | yes | yes                            |
| Pyld24 | MetricSeries  | Tag Set Size           | total tag bytes per series `<= 100 KiB` (`MaxTagSetSize`)                                             | yes | yes — `host` fold shifts total |
| Pyld25 | MetricSeries  | Points Non-Empty       | a flushed `COUNT`, `GAUGE`, or `RATE` series carries at least one point                               | yes | yes — plus `SKETCH`            |

### v3 series (`/api/intake/metrics/v3/series`)

The Agent posts native v3 series here, a dictionary + delta encoded columnar protobuf
([`intake_v3.proto`](../../../lib/protos/datadog/proto/agent-payload/intake_v3.proto)). The intake
decodes it with an independent reimplementation of the production intake's decode and per-series
validation, not the Agent's own decoder, so it catches divergences rather than masking them. Decode is
a two-tier failure model matching the production intake: a dictionary, structural, reference, or
non-tag-string-UTF-8 error aborts the whole payload, while a per-series validation failure drops only
that series. The same drop rules as v2 apply per series — an invalid metric name, the tag-count and
resource-count caps, and the host name length — and a `host` resource is folded into a `host:<name>`
tag so v3 and v2 contexts compare equal. This capture feeds the differential context and count oracles.

New for v3, with no v2 analog — wire-shape invariants a future revision could assert: dictionary
reference bounds, no orphan dictionary entries, parallel-array length consistency, tagset
back-reference well-formedness, payload-level `Metadata` emptiness, sketch bin and summary
consistency, and unit flag and column agreement.

### Differential context capture

The differential scenario uses the same intake binary for both lanes:

- Datadog Agent lane: `POST /api/intake/metrics/v3/series` and `POST /api/beta/sketches`
- ADP lane: `POST /api/v2/series` and `POST /api/beta/sketches`
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
