---
slug: non-finite-values-handled-consistently
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
type: Safety
priority: Medium
assertion_status: MISSING (net-new instrumentation)
---

# Property: Non-finite metric values are handled consistently, never crash, no ghost metric

## Origin
SUT analysis §7 #7 ("non-finite metric values silently dropped"), #10 ("NaN poisons a
DDSketch … finiteness guarded per-source, fragile if a new producer is added"), #11
("All-non-finite packet → ghost metric with a valid context but zero data points"). No
Antithesis assertion exists (existing-assertions.md).

## What the code does
`lib/saluki-io/src/deser/codec/dogstatsd/metric.rs`:
- `FloatIter::next` (286-307): parses `:`-delimited values; `Ok(value) if value.is_finite()`
  → yields the value; `Ok(_)` (non-finite NaN/±Inf) → `debug!("Dropping non-finite …")` and
  **loops to the next value** (does not yield, does not error); a true parse failure → `Err`.
- `metric_values_from_raw` (250-271): `num_points` is incremented via
  `FloatIter::new(input).inspect(|_| num_points += 1)` (253-254) — `inspect` fires only for
  *yielded* items, so **non-finite values do not increment `num_points`**. An all-non-finite
  packet → `num_points == 0` and the metric-values constructor (e.g. `gauge_fallible(floats)`,
  258) receives an empty iterator, producing an empty `MetricValues` ("ghost" shape: valid
  name/type, zero points).
- Verified by `metric.rs::non_finite_metric_values_are_dropped` (743-759): asserts
  `packet.num_points == 0` for `NaN|g`, `inf|g`, `-inf|g`.

`lib/saluki-components/src/sources/dogstatsd/mod.rs::handle_frame` (1458-1542):
- **1478-1480**: `if metric_packet.num_points == 0 { return Ok(None); }` — the zero-point
  packet is dropped **before** context resolution (`handle_metric_packet`, 1490). So at the
  source level, an all-non-finite packet consumes **no** interner/context-cache resources and
  produces no downstream event. Confirmed by `non_finite_metric_values_are_silently_dropped`
  (mod.rs:2470-2485): "handle_frame then returns Ok(None) for zero-point packets."
- A *partial* packet (`x:NaN:5|g`) → `num_points == 1`, the finite value flows normally and the
  non-finite one is dropped — consistent with the Agent.

## Failure scenario (Antithesis)
Send packets that are entirely non-finite (`m:NaN|g`, `m:inf:nan:-inf|h`, multi-value all-NaN
distributions) and mixed finite/non-finite, across all metric types (c/g/ms/h/d/s). Expectations:
(1) no panic; (2) all-non-finite packets produce **no** downstream metric and consume no
interner/context-cache slot (no ghost metric reaches aggregation); (3) finite values in mixed
packets are preserved exactly; (4) no NaN ever reaches a DDSketch (the §7 #10 poisoning hazard).

## Key observations
- **The ghost-metric risk is gated at the source today** by the `num_points == 0` short-circuit
  (mod.rs:1478). Frame honestly: the property asserts the gate *holds* — an all-non-finite packet
  must not create a context/sketch — rather than claiming a ghost metric exists on the DSD path.
- The §7 #10 NaN-poisons-DDSketch hazard is real but currently *prevented* by the per-source
  finiteness filter in `FloatIter`. The fragility is structural: finiteness is enforced in the
  **codec**, not at the **sketch boundary** (`agent/sketch.rs`). A new producer (e.g. OTLP, or a
  replay path that bypasses the codec) could feed NaN to a sketch. The property should assert the
  invariant *at the sketch boundary* so it's robust to new producers.
- Set metrics (`s`) take a different path (metric.rs:259-265, `num_points = 1` unconditionally, value
  is the raw string) — non-finite-ness doesn't apply; exclude sets from the value-finiteness check.

## Config deps
- `permissive` mode and value parsing don't change finiteness handling.
- The sketch-boundary check matters only for histogram/distribution/timer types (which build sketches);
  gauge/counter store raw f64 values.

## Suggested assertion (MISSING — net-new)
- **Always**(no NaN in a sketch): `assert_always(value.is_finite())` at the DDSketch insert boundary
  (`agent/sketch.rs` insert, ~188-206) — generalizes the per-source guard to the sketch itself, robust
  to new producers. Catches §7 #10 directly.
- **AlwaysOrUnreachable**(no zero-point metric reaches aggregation): an all-non-finite packet must not
  produce a downstream `Event::Metric` with empty values — anchor at handle_frame (mod.rs:1478) /
  aggregation insert. If the gate ever lets a zero-point metric through, that's the ghost metric.
- **Sometimes(non_finite dropped)**: at least once, `FloatIter` drops a non-finite value (proves the
  adversarial all-NaN input actually exercised the drop path). Meaningful state, not `Sometimes(true)`.
- **Sometimes(ghost-metric path reachable)** — *only if* a producer that bypasses the `num_points==0`
  gate is found (e.g. replay/OTLP). On the pure DSD path this is expected **Unreachable**; do not assert
  `Sometimes` for it on DSD-only without first confirming a bypass exists (see Open Questions).

## SUT-side instrumentation needs
- The sketch-boundary `Always(is_finite)` and the zero-point-gate check need SDK assertions inside the
  SUT; a workload-only checker sees aggregated output and cannot attribute a NaN sum to a sketch insert.
- A `non_finite_dropped` counter (or assertion at metric.rs:301) gives the `Sometimes` reachability anchor.
  No such counter exists today — currently only a `debug!` log.

## Open questions
- **Does `gauge_fallible([])` / the empty-iterator constructors ever return an `Err`** (the `_fallible`
  suffix) rather than an empty value? If they error on empty input, the path is even safer
  (handle_frame returns `Err`, counted) — confirm the empty-iter behavior. Changes whether num_points==0
  is the only gate.
- **Is `avg`/`sum` on an empty/NaN sketch ever surfaced as a metric** (the §7 #10 "permanently NaN")? Even
  with the source guard, confirm a sketch can't reach a NaN aggregate via merge of pre-timestamped/
  passthrough points. Affects whether the sketch-boundary `Always` is sufficient or a merge-time check is
  also needed.

### Investigation Log

#### Is there any path that builds a metric/sketch from values WITHOUT going through FloatIter?
- **Examined:** all metric/sketch producers and their topology wiring — DSD codec FloatIter
  (`lib/saluki-io/src/deser/codec/dogstatsd/metric.rs:254,299`); OTLP source
  (`lib/saluki-components/src/sources/otlp/metrics/translator.rs`, incl. `get_number_data_point_value`
  :1366, `is_skippable` :1374, `map_number_metrics` :726, the histogram→`Dogsketch::try_from` path
  :314-351 and the explicit-bounds `insert_interpolate_buckets` path :889); self-telemetry
  (`lib/saluki-core/src/observability/metrics/mod.rs:299-310`); checks_ipc
  (`lib/saluki-components/src/sources/checks_ipc/mod.rs:185-204`); the aggregate
  histogram→distribution `insert_n` (`lib/saluki-components/src/transforms/aggregate/mod.rs:745`);
  the Datadog metrics encoder Histogram→sketch conversion
  (`lib/saluki-components/src/encoders/datadog/metrics/mod.rs:1049-1058`); and the ADP topology
  (`bin/agent-data-plane/src/cli/run.rs:462-499, 664-686, 745-755`). Full detail captured in
  ddsketch-no-nan-poison.md Investigation Log.
- **Found:** YES — multiple producers build metrics/sketches without FloatIter, but they fall into
  three categories:
  - **OTLP — guarded by its OWN finiteness filter.** Number/gauge/counter values pass `is_skippable`
    (NaN/Inf skipped, translator.rs:726/754); histogram sketches are built via `Dogsketch::try_from`
    (no raw insert) or `insert_interpolate_buckets` (which reconstructs finite `bin_lower_bound`
    values before `adjust_basic_stats`). OTLP does not poison sum/avg with NaN. So OTLP does NOT make
    the ghost/poison path live.
  - **Aggregate transform `insert_n` (mod.rs:745) — DSD-ONLY.** `dsd_agg` is wired only in the DSD
    pipeline (run.rs:664-679); checks_ipc and OTLP join at `metrics_enrich`, downstream of `dsd_agg`.
    So the aggregate sketch path receives only FloatIter-filtered (finite) values. Not a bypass.
  - **checks_ipc Histogram → Datadog metrics encoder — A REAL BYPASS (live).** checks_ipc
    (mod.rs:195) builds `Metric::histogram(context, (timestamp, value))` from an external Python
    check's raw f64 with **no finiteness check**, and routes `checks_ipc_in.metrics → metrics_enrich
    → dd_metrics_encode` (run.rs:469/499) — skipping both FloatIter and the aggregate transform. The
    encoder converts the Histogram to a sketch via `ddsketch.insert_n(sample.value...)`
    (encoders/datadog/metrics/mod.rs:1054), so a NaN check value poisons the emitted sketch's
    sum/avg. (Note: this poisons sum/avg but does not create the *zero-point ghost* shape — the
    ghost-metric/`num_points==0` concern is specific to the DSD `FloatIter`+`num_points` interaction
    and remains gated on the DSD path at mod.rs:1478.)
- **Not found:** No finiteness guard on the checks_ipc value path; no guard at the sketch boundary;
  no third metric ingress that bypasses both FloatIter and a per-source filter.
- **Conclusion:** RESOLVED. The NaN-poison path (#10) is **LIVE** via checks_ipc → Datadog metrics
  encoder, independent of the DSD codec. The ghost-metric (#11, zero-point) shape is NOT reproduced
  by this bypass (it is DSD-`FloatIter`-specific and gated at handle_frame mod.rs:1478). Because the
  finiteness invariant is enforced per-producer (DSD FloatIter, OTLP is_skippable) and NOT at the
  sketch boundary, the suggested sketch-boundary `assert_always(value.is_finite())` is the
  robust, producer-independent assertion and is justified by a concrete live bypass. The
  `Sometimes(ghost-metric)` assertion should remain Unreachable-style on the DSD path; the live NaN
  exposure is a *poisoned sum/avg* at the encoder, not a zero-point ghost.
