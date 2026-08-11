# A property asserting intake for Datadog Agent-like artifacts

This intake asserts structural properties on payloads from Datadog Agent-like
programs and is intended to mimic the constraints set by Datadog Intake
API. This implementation is forked from the other intake in this project and may
be later merged back up, although the goals to make aggregation assertions here
are invasive.

## How this project works

This document is the specification for an abstract DogStatsD Agent. We assert
that for any given input stream ADP emits to the intake data that is correctly
shaped and, in a future update, that the aggregation model of ADP is accurate to
the reference implementation of Datadog Agent DogStatsD.

The differential scenario adds one narrower oracle. For the same generated
configuration and workload, ADP and the Datadog Agent must eventually report the
same metric contexts.

## On Decoding and Correctness

The intake API presented in this rig attempts to mimic the observed behavior of
Datadog's own API. The full set of constraints are described below as
properties. The API is permissive in ways that may be surprising, with regard
to:

* encoding:
  * Tags: non-utf8 bytes are accepted and coerced into U+FFFD
    * for v2 this is only true if User-Agent is 'datadog-agent'
    * for v3 this is true universally

To be clear, non-utf8 coercion is tags-only. Metric names, units, resources,
source-types etc are _not_ coerced. To state this generally, what this rig does
is as follows:

1. Datadog intake is normative but its observed behavior is lax, Postel's Law in
   action.
2. This rig insists on non-lax behavior from a Datadog Agent (ADP-on, ADP-off).
3. That is, _except_ where lax behavior is venerable, for instance Datadog Agent
   (ADP-off) forwards non-utf8 tags as discussed above.
4. Excepting when that lax behavior causes customer data to be mangled, for
   instance, if tag sizes are greater than a fixed size intake will truncate
   them (REF Pyld23).
5. To that end, we state that Datadog Agent (ADP-off) is normative in
   _consequence_ not shape. We do not require that ADP-on send User-Agent
   'datadog-agent' nor that it forward non-utf8 bytes in tags.

Finally, the goal of this rig is not to demonstrate good behavior -- although
that will happen -- the goal is to _find faults_. Many payloads below, for
example Pyld26, are vacuous on a properly functional Datadog Agent and will only
fire in the prescence of a misbehaving Agent.

## Properties

### Payloads

The Agent emits outputs to Datadog intake endpoints as payloads. The `Pyld`
properties below are asserted on both v2 and v3 intake endpoints. In this
section we define properties that hold for both endpoints, irrespective of load
generation profile.

Some properties reference rig-controlled parameters. `MaxTags(orgID)` and
`MaxResources(orgID)` are per-org caps with defaults 100 and 500 respectively. A
host is not a top-level field but a `resource` whose `type` is `host`, which the
intake folds into a `host:<name>` tag. Pyld60 reads the timeline's sampled
`datadog.yaml`. The following table lists the properties that hold no matter the
intake endpoint:

| Number | Category      | Name                     | Description                                                                                               |
|--------|---------------|--------------------------|-----------------------------------------------------------------------------------------------------------|
| Pyld01 | Envelope      | Content-Type             | `Content-Type` in `{application/x-protobuf, application/json}`                                            |
| Pyld02 | Envelope      | Content-Encoding         | `Content-Encoding` in `{deflate, gzip, zstd, identity}`                                                   |
| Pyld03 | Envelope      | API Key                  | `DD-Api-Key` header present and non-empty                                                                 |
| Pyld05 | Bytes         | Compressed Size          | body < 500 KiB compressed                                                                                 |
| Pyld06 | Bytes         | Uncompressed Size        | body <= 5 MiB uncompressed                                                                                |
| Pyld08 | MetricPayload | Point Count              | total points <= configured `serializer_max_series_points_per_payload`                                     |
| Pyld09 | MetricSeries  | Metric Non-Empty         | `MetricSeries.metric` is non-empty                                                                        |
| Pyld10 | MetricSeries  | Metric Length            | `len(metric) <= 350` bytes                                                                                |
| Pyld11 | MetricSeries  | Metric Alphabetic        | `metric` contains at least one ASCII alphabetic char                                                      |
| Pyld13 | MetricSeries  | Tag Count                | `len(tags) <= MaxTags(orgID)`                                                                             |
| Pyld14 | MetricSeries  | Tag Prefix Reserved      | no tag starts with `device:` or `dd.internal.resource:`                                                   |
| Pyld15 | MetricSeries  | Per-Series Point Count   | `len(points) <=` configured `serializer_max_series_points_per_payload`                                    |
| Pyld16 | MetricSeries  | Origin Valid             | `origin.{product, category, service}` enum-valid when present                                             |
| Pyld17 | Resource      | Host Resource Resolved   | every series resolves a non-empty `(type="host")` resource and every series on a lane shares one host     |
| Pyld18 | Resource      | Resource Count           | `len(resources) <= MaxResources(orgID)`                                                                   |
| Pyld19 | Resource      | Host Name Length         | host `name <= 255` bytes                                                                                  |
| Pyld20 | MetricPoint   | Value Not-NaN            | `value` is not NaN                                                                                        |
| Pyld21 | MetricPoint   | Timestamp Future Bound   | `timestamp <= intake_now + 600s`                                                                          |
| Pyld22 | Bytes         | Content-Length           | `Content-Length` absent or value equals body byte count                                                   |
| Pyld23 | MetricSeries  | Tag Length               | each tag `<= 200` bytes (`MaxTagLength`) -- the intake clips a longer tag                                 |
| Pyld24 | MetricSeries  | Tag Set Size             | total tag bytes per series `<= 100 KiB` (`MaxTagSetSize`)                                                 |
| Pyld60 | Envelope      | Series API As Configured | payload arrives on the series API configured                                                              |

These properties hold exclusively for v2. Note that some property names are
shared with the v3 variant table below, for instance Pyld07-v2 is the equivalent
for Pyld07-v3:

| Number    | Category      | Name             | Description                                                                 |
|-----------|---------------|------------------|-----------------------------------------------------------------------------|
| Pyld07-v2 | MetricPayload | Decode           | body decodes as v2 `MetricPayload` via UTF-8 tolerant, source-gated decoder |
| Pyld12-v2 | MetricSeries  | Type Enum        | `type` in `{COUNT, RATE, GAUGE}`                                            |
| Pyld25-v2 | MetricSeries  | Points Non-Empty | a flushed `COUNT`, `GAUGE`, or `RATE` series carries at least one point     |

These properties hold exclusively for v3:

| Number    | Category      | Name                      | Description                                                                                                |
|-----------|---------------|---------------------------|------------------------------------------------------------------------------------------------------------|
| Pyld07-v3 | MetricPayload | Decode                    | body decodes as a v3 `Payload` via UTF-8 tolerant decoder                                                  |
| Pyld12-v3 | MetricSeries  | Type Enum                 | `type` in `{COUNT, RATE, GAUGE, SKETCH}`                                                                   |
| Pyld25-v3 | MetricSeries  | Points Non-Empty          | a flushed `COUNT`, `GAUGE`, `RATE`, or `SKETCH` series carries at least one point                          |
| Pyld26    | Columns       | Metric Count              | `len(types) == N` — types sets the metric count N                                                          |
| Pyld27    | Columns       | nameRefs Length           | `len(nameRefs) == N`                                                                                       |
| Pyld28    | Columns       | tagsetRefs Length         | `len(tagsetRefs) == N`                                                                                     |
| Pyld29    | Columns       | resourcesRefs Length      | `len(resourcesRefs) == N`                                                                                  |
| Pyld30    | Columns       | sourceTypeNameRefs Length | `len(sourceTypeNameRefs) == N`                                                                             |
| Pyld31    | Columns       | originInfoRefs Length     | `len(originInfoRefs) == N`                                                                                 |
| Pyld32    | Columns       | intervals Length          | `len(intervals) == N`                                                                                      |
| Pyld33    | Columns       | numPoints Length          | `len(numPoints) == N`                                                                                      |
| Pyld34    | Columns       | unitRefs Length           | `len(unitRefs)` == count of metrics that carry a unit                                                      |
| Pyld35    | Points        | Timestamps Sized          | `len(timestamps) == sum(numPoints[])`                                                                      |
| Pyld36    | Points        | Float64 Column Sized      | `len(valsFloat64)` == Float64 scalar points + 3 per Float64 sketch point                                   |
| Pyld37    | Points        | Float32 Column Sized      | `len(valsFloat32)` == Float32 scalar points + 3 per Float32 sketch point                                   |
| Pyld38    | Points        | Sint64 Column Sized       | `len(valsSint64)` == Sint64 scalar points + 1 per sketch point + 3 more per Sint64-summary sketch point    |
| Pyld39    | Points        | Value Type Defined        | each `type & 0xF0` is one of `0x00, 0x10, 0x20, 0x30`                                                      |
| Pyld40    | Sketch        | numBins Sized             | `len(sketchNumBins)` == total sketch points                                                                |
| Pyld41    | Sketch        | BinKeys Sized             | `len(sketchBinKeys) == sum(sketchNumBins[])`                                                               |
| Pyld42    | Sketch        | BinCnts Sized             | `len(sketchBinCnts) == sum(sketchNumBins[])`                                                               |
| Pyld43    | References    | Name Ref                  | accumulated `nameRef` in `[0, len(dictNameStr))`                                                           |
| Pyld44    | References    | Tagset Ref                | accumulated `tagsetRef` in `[0, nTagsets)`                                                                 |
| Pyld45    | References    | Resource Ref              | accumulated `resourcesRef` in `[0, len(dictResourceLen)]`                                                  |
| Pyld46    | References    | Source-Type Ref           | accumulated `sourceTypeNameRef` in `[0, len(dictSourceTypeName))`                                          |
| Pyld47    | References    | Origin Ref                | accumulated `originInfoRef` in `[0, len(dictOriginInfo)/3 + 1)`                                            |
| Pyld48    | References    | Unit Ref                  | each `unitRef` in `[0, len(dictUnitStr))`                                                                  |
| Pyld49    | Dictionaries  | Blob Bounds               | each string-dict entry length stays within the blob                                                        |
| Pyld50    | Dictionaries  | Strict UTF-8              | `dictNameStr`, `dictUnitStr`, `dictResourceStr`, `dictSourceTypeName` valid UTF-8 (`dictTagStr` sanitized) |
| Pyld51    | Dictionaries  | No Orphans                | every dictionary entry is referenced, directly or through another entry                                    |
| Pyld52    | Tagsets       | Group Size                | each tagset group size prefix in `[0, remaining]`                                                          |
| Pyld53    | Tagsets       | Tag Index                 | each positive tagset entry in `[0, len(dictTagStr))`; negatives are back-references                        |
| Pyld54    | Tagsets       | Back-Reference            | a negative tagset index resolves to an earlier tagset, never self or forward                               |
| Pyld55    | Resources     | Length Sizing             | `sum(dictResourceLen[]) == len(dictResourceType) == len(dictResourceName)`                                 |
| Pyld56    | Resources     | Group Length Non-Negative | each `dictResourceLen` entry `>= 0`                                                                        |
| Pyld57    | Resources     | Field Refs                | resource type and name refs in `[0, len(dictResourceStr))`                                                 |
| Pyld58    | Origin        | OriginInfo Triples        | `len(dictOriginInfo)` is a multiple of 3                                                                   |
| Pyld59    | Metadata      | Resources Even            | payload `Metadata.resources` has even length                                                               |

### Differential context capture

The differential scenario uses the same intake binary for both lanes. Both take the series API the
timeline sampled, so a lane splitting off the other's encoding is a finding:

- Datadog Agent lane: `POST /api/v2/series` or `POST /api/intake/metrics/v3/series`, plus `POST /api/beta/sketches`
- ADP lane: the same two series routes, plus `POST /api/beta/sketches`
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
