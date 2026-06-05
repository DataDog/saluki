# W-property intake

The antithesis harness has its own intake. It lives in the `antithesis-intake`
crate at `test/antithesis/intake`. It replaces the shared `datadog-intake` in
the `intake` image.

## Why a dedicated intake

The shared `bin/correctness/datadog-intake` serves integration and correctness
tests. The W-property assertions belong to the antithesis run alone. A separate
binary keeps the shared intake clean and lets the antithesis intake carry the
Antithesis SDK and evolve on its own.

## What it does

It observes every `/api/v2/series` payload ADP delivers and fires an Antithesis
SDK `assert_always!` per W property. The properties come from the `invariant-jig`
checker. See that project's `README.md` §Properties.Payloads for the numbering
and the double-sourced Agent and intake references.

The intake also keeps the `/metrics/dump` contract `finally_verify_delivery`
polls. It merges v1 series, v2 series, and sketches into a `stele::Metric` store
exposed at `/metrics/dump`. The fallback returns `200 OK` so ADP connectivity
probes and unmodelled endpoints succeed.

## Properties

W1-W22, the full set:

- Envelope W1-W3 read raw headers. Content-Type, Content-Encoding, API key.
  W4 is dropped. ADP sends `agent-data-plane/`, not the Go Agent's
  `datadog-agent/`, and intake does not validate User-Agent, so there is no
  cross-cut contract to assert. Run 5ef26a05 confirmed a 100% false positive.
- Bytes W5, W6, W22 read sizes. Compressed cap, uncompressed cap, content-length
  equality.
- Decode W7 parses the body as a `MetricPayload`.
- Payload W8 totals points across the payload.
- Series W9-W19 read each `MetricSeries`. Name, type, tag and resource counts,
  reserved tag prefixes, origin enum domain, host resolution and length.
- Point W20, W21 read each `MetricPoint`. NaN value, future timestamp bound.

## Envelope measurement

W1-W6 and W22 need the compressed body and the raw headers. The series route
runs a `measure_compressed_size` middleware ahead of the decompression layer. It
records the compressed length, the Content-Encoding, and the declared
Content-Length before the layer strips them, then passes the request along with
those riding as extensions. W6 reads the decompressed length in the handler.

## Hostname and W17

W17 resolves each series host against `DD_HOSTNAME`, default `antithesis-adp`.
Keep it in sync with `hostname:` in `deploy/adp/datadog.yaml`. The `intake`
image sets `DD_HOSTNAME=antithesis-adp`.

Source-confirmed contract. The Agent emits a `type=host` resource on every v2
series unconditionally, named `serie.Host` (`datadog-agent`
`pkg/serializer/internal/metrics/iterable_series.go:296`). ADP does the same,
named `metric.metadata().hostname().unwrap_or_default()`
(`encoders/datadog/metrics/mod.rs:919`). So the host resource is never absent on
either side. W17 failures are therefore name mismatches, not missing resources.
The prime suspect is an empty hostname when dogstatsd metrics are not
host-enriched, where the Agent would write the configured hostname.

W17 now records the resolution in the assertion detail. `observed` carries
`resolution` (`missing` or `mismatch`) and `resolved`, the host name on the
wire. An empty name reads as `mismatch` with `resolved: ""`, which distinguishes
an unnamed host resource from a wrong name. Run 5ef26a05 fired W17 781/851 but
the detail did not carry the resolved value, so the cause was undetermined. The
next run will show it.

## Probe fast-path

ADP probes endpoint connectivity with a `{}` body. That is not a metric payload.
The handler returns `202 Accepted` for it and skips the W pipeline so W7 does not
fire a spurious assertion on every probe.

## Proto layer

The checker predicates port from `prost` to the `rust-protobuf` types in
`datadog_protos`. Field semantics match. The agent-payload proto is identical.
Access changes only. `series.metric()`, `r.type_()`, `series.type_.enum_value()`,
`MessageField` for `metadata` and `origin`.
