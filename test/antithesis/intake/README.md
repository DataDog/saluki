# A property asserting intake for Datadog Agent-like artifacts

This intake asserts structural properties on payloads from Datadog Agent-like
programs and is intended to mimic the constraints set by Datadog Intake
API. This implementation is forked from the other intake in this project and may
be later merged back up, although the goals to make aggregation assertions here
are invasive.

## How this project works

This document is the specification for an abstract DogStatsD Agent. We assert
that for any given input stream ADP emits to the intake data that is correctly
shaped and that the aggregation model of ADP is accurate to the reference
implementation of Datadog Agent DogStatsD.

The differential scenario adds two narrower oracles. For the same generated
configuration and workload, ADP and the Datadog Agent must eventually report the
same metric contexts, and each shared context must carry the same aggregation
curve on both lanes.

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

## Endpoints

This intake supports many endpoints. The following table lists them, their
supported methods and, briefly, their purpose.

| Method | Path                                   | Purpose                                                                    |
|--------|----------------------------------------|----------------------------------------------------------------------------|
| POST   | `/api/v2/series`                       | v2 metric series                                                           |
| POST   | `/api/intake/metrics/v3/series`        | v3 native series                                                           |
| POST   | `/api/beta/sketches`                   | Distribution sketches                                                      |
| POST   | `/api/v1/events_batch`                 | event batches, currently catch and discard                                 |
| POST   | `/api/v1/events`                       | JSON events, currently catch and discard                                   |
| POST   | `/intake/`                             | events and metadata, currently catch and discard                           |
| POST   | `/api/v1/check_run`                    | service checks, currently catch and discard                                |
| GET    | `/api/v1/validate`                     | Datadog Agent connectivity probe                                           |
| POST   | `/antithesis/metrics/contexts`         | Contexts oracle: computes symmetric difference of lane contexts, see below |
| POST   | `/antithesis/metrics/frechet_distance` | Series oracle: computes Frechet distance of lane time series, see below    |
| GET    | `/contexts?n=N`                        | Serves the load generator bounded `N` contexts                             |

All other paths respond with a 404 for every method.

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

### Differential Equivalence

The differential scenario compares ADP and Datadog Agent on the same input
stream, confirming that they are "roughly equivalent". What this means varies by
the precise check, discussed below. The differential scenario uses the same
intake for both lanes. Each check POSTs its parameters and the intake makes the
Antithesis SDK calls, see below.

Both lanes take the series API the timeline sampled, so a lane that ships the
other's encoding is a finding rather than a configured difference.

#### Contexts

For context equivalence, a metric context is:

- metric name
- canonical tag list
- metric type

The intake exposes `/antithesis/metrics/contexts`. A POST to this endpoint
computes the [symmetric
difference](https://en.wikipedia.org/wiki/Symmetric_difference) of the observed
contexts per-lane to that point. If a context C enters on lane A at time T-0 it
will be emitted for all subsequent times, even if C never enters on lane A
again. _Contexts do not expire and we do not tally how often contexts have
arrived._ Call the symmetric difference `D`. Let `age` be the difference between
the current time -- from intake's frame of reference -- and the timestamp that
context first ingressed with. We claim that:

* _eventually_ for every member `m` in `D` `age <= acceptable_flush_delay`
* _finally_ `D == {}` after waiting for a period of `acceptable_flush_delay` once load is quiescent

The POST body sets calculation parameters, which are:

* `acceptable_flush_delay` -- number of seconds before which both lanes are allowed to diverge
* `phase` -- `eventually` or `finally`, which check posted

The `phase` picks the assertion name, either
`differential.contexts_eventually_equivalent` or
`differential.contexts_finally_converged`, and the predicate. The `eventually_`
check asserts no member of `D` is delayed, so a member inside
`acceptable_flush_delay` is in flight rather than a divergence. The `finally_`
check asserts `D == {}` and ignores the budget, since load has stopped and a
residual has nothing left to wait for. A report that merged them could not tell a
lane that diverges under load from one that never converges.

These are transmitted by eventually/finally checks and are a matter of scenario
configuration, ultimately.

#### Series

The concern of this section is the equivalence of time series of a context,
which we'll call 'series' for shorthand. Our goal is to demonstrate that both
lanes, if given the same input stream, _aggregate_ to an equivalent
aggregation. Implied in this are two concepts, first, the operations by which
aggregation happens per kind and, second, the definition of equivalence.

Points are stored raw, per lane, with a `seq` number to distinguish points that
arrive at the same time interval. Conceptually they are stored as tuples:

`(name, tagset, kind, timestamp, seq, interval, value)`

`timestamp` is the time recorded from the ingress frame of reference. Recording
from intake's frame of reference subjects stored points to network jitter
effects, which we wish to avoid. For convenience we do not store any known self-telemetry, so for
instance Datadog Agent lane's `datadog.*` is not stored.

Queries over the point storage are done in terms of a bucketing width `w`, a
fold operation per `kind` and a 'resubmit' rule to break ties on a timestamp:
keep last, keep first by `seq` or summation. Queries are executed per-lane, that
is, a query must be made over one lane's store and then the other. Queries are
executed like so:

0. Collapse points sharing a timestamp by the resubmit rule.
1. Assign each point to a bucket `k = floor(timestamp / w)`.
2. Fold bucket points by the kind's fold operation, which are:
   * `count`  -- `sum`
   * `rate`   -- `sum(value * interval) / sum(interval)`
   * `gauge`  -- last by timestamp
   * `sketch` -- dd-sketch merge, then projection to scalar series: count, sum, min, max, p75, p95 and p99
   * `other`  -- none, drop
3. Finally, buckets without values are filled like so:
   * `count`  -- 0-valued
   * `rate`   -- 0-valued
   * `gauge`  -- carry forward previous value
   * `sketch` -- count series is 0-valued, quantile series are not emitted

Note, for sketches we require that both lanes maintain the same bin
quantization. We consider this a difference if they do not, that is, a failure
of equivalence.

The equivalence comparison is then done like so. First, truncate both series to
the range both lanes could have contributed to so far:

```
k_start = max(first bucket on A, first bucket on B) + W
k_end   = floor(min(newest_A, newest_B) / w) - F - W
```

`F` is 1 in the `eventually_` check and 0 in the `finally_` check. The
`eventually_` check drops a bucket to avoid reading out a bucket that is still
filling. After load stops nothing is filling. Both ends drop a further `W` buckets because the two lanes can put the
same input in different buckets. At the cut one lane counts a point the other
placed outside the range. Distance `d` is:

`d(a,b) = |b-a| / max(|a|,|b|)`

where `a` is value of lane A for a bucket and `b` is the value of lane B for
that same bucket, with `d(x,x) = 0` by definition. The Fréchet measure is
defined over pairs of buckets, one from each lane. Let `k` index lane A's
buckets and `k'` lane B's, running from `k_start` to `k_end`. Then:

```
F(k_start, k_start) = d(A_k_start, B_k_start)
F(k, k')            = max( d(A_k, B_k'), min(F(k-1,k'), F(k-1,k'-1), F(k,k'-1)) )
```

A pair is admissible only when `|k - k'| <= W` where `W` is the 'leash width' in
buckets. An inadmissible pair is not present in the calculation. Note that `F(k,
k')` is not the distance between buckets `k` and `k'` it is the running
best-so-far result, that is, of the walks that were possible to reach `k` and
`k'` what is the smallest required 'leash'? We say that both lanes are
equivalent if `F(k_end, k_end) < equivalence_threshold`. Both ends are fixed, so
every bucket of both lanes is matched to something.

This means then that `W` and `equivalence_threshold` have outsized influence on
the calculation. As of this writing we hold `W=1` and
`equivalence_threshold=0.02` until such time as empirical results suggest
different values are warranted.

The intake exposes `/antithesis/metrics/frechet_distance`. A POST to this
endpoint runs the query described above for both lanes and makes necessary
Antithesis SDK calls. The POST body sets distance calculation parameters, which
are:

* `bucket_width`          -- `w` from above, the bucketing width in seconds
* `leash_width`           -- `W` from above, the 'leash' width in buckets
* `equivalence_threshold` -- the value `F(k_end, k_end)` is compared with
* `phase` -- `eventually` or `finally`, which check posted

As with contexts, the `phase` picks the assertion name, either
`differential.series_eventually_equivalent` or
`differential.series_finally_converged`.

These are transmitted by eventually/finally checks -- similar to how context
above works -- and are a matter of scenario configuration, ultimately.
