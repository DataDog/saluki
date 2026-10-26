# Scraping internal telemetry

Agent Data Plane exposes internal telemetry as OpenMetrics text on the unprivileged API endpoint.
You can scrape this endpoint to collect ADP process, pipeline, and component metrics.

## Endpoints

ADP registers two internal telemetry routes:

| Route             | Description                                                                                 |
| ----------------- | ------------------------------------------------------------------------------------------- |
| `/metrics`        | Full ADP internal telemetry, rendered as Prometheus/OpenMetrics text exposition.             |
| `/compat/metrics` | ADP-only compatibility view with selected metrics under legacy-compatible aliases.          |

The routes are served from `data_plane.api_listen_address`, also called the "unprivileged API
address." The default address uses port `5100`, so a default local scrape target is:

```text
http://127.0.0.1:5100/metrics
```

Use `/metrics` when you want the complete ADP telemetry surface. Use `/compat/metrics` when you
need the ADP-local compatibility aliases. `/compat/metrics` has its own naming scheme; it is not the
same as the RAR metric names on the Core Agent's `/telemetry` endpoint or the names consumed by the
`go_expvar` bridge.

These are ADP-local endpoints. When ADP registers with the Core Agent, the Core Agent also exposes
ADP series on its own `/telemetry` endpoint alongside Core Agent series. Prefer the Core Agent
endpoint when you want a combined view; use the ADP-local routes when you want ADP telemetry only.
ADP series on the Core Agent endpoint carry an `emitter` label so you can distinguish them from
Core Agent series.

> [!NOTE]
> `telemetry.enabled` does not control these ADP endpoints. ADP ignores that Core Agent
> configuration key, and the internal telemetry routes are registered by ADP itself.

To use the Core Agent's combined `/telemetry` endpoint, set `telemetry.enabled: true` in the
**Core Agent** configuration. This key defaults to `false`; without it, the Core Agent does not
serve `/telemetry`. This prerequisite applies to both combined-scrape examples below and to
queries against `/telemetry`, but not to ADP's local `/metrics` or `/compat/metrics` routes.

## Configure a scrape

Configure your scraper to target the unprivileged API address.

### Datadog Agent OpenMetrics check

To scrape ADP-only telemetry with the Datadog Agent, configure the
[OpenMetrics check](https://docs.datadoghq.com/integrations/openmetrics/) with ADP's telemetry
endpoint. To collect both Core Agent and ADP series in one scrape, target the Core Agent's
`/telemetry` endpoint instead; use the telemetry listener configured for your Core Agent deployment.

```yaml
instances:
  - openmetrics_endpoint: http://127.0.0.1:5100/metrics
    namespace: agent_data_plane
    metrics:
      - ".*"
```

To scrape the compatibility view, change `openmetrics_endpoint` to
`http://127.0.0.1:5100/compat/metrics`.

For a combined Core Agent and ADP scrape, use the Core Agent's telemetry listener rather than
`data_plane.api_listen_address`:

```yaml
instances:
  - openmetrics_endpoint: "http://<core-agent-host>:<core-agent-telemetry-port>/telemetry"
    namespace: datadog_agent
    metrics:
      - "dogstatsd__processed"
      - "transactions__.*"
      - "aggregator__flush"
```

Replace the host and port placeholders with the telemetry listener configured for your deployment.
Use narrower metric patterns when you do not need the full Core Agent telemetry surface. The metric
names in this guide are exposition names; the OpenMetrics check may prefix submitted Datadog metric
names with the configured namespace.

> [!NOTE]
> The OpenMetrics check submits collected series as custom metrics. Use explicit metric names or
> narrower patterns when you do not need the full ADP telemetry surface.

### Prometheus

For Prometheus-style scraping, use ADP's unprivileged API address for ADP-only telemetry. To
collect the combined Core Agent and ADP view, use the Core Agent telemetry listener and set
`metrics_path` to `/telemetry` instead.

```yaml
scrape_configs:
  - job_name: agent-data-plane
    metrics_path: /metrics
    static_configs:
      - targets:
          - 127.0.0.1:5100
```

To scrape the compatibility view, change `metrics_path` to `/compat/metrics`:

```yaml
scrape_configs:
  - job_name: agent-data-plane-compat
    metrics_path: /compat/metrics
    static_configs:
      - targets:
          - 127.0.0.1:5100
```

To scrape the combined Core Agent and ADP view instead, target the Core Agent telemetry listener:

```yaml
scrape_configs:
  - job_name: core-agent-telemetry
    metrics_path: /telemetry
    static_configs:
      - targets:
          - <core-agent-host>:<core-agent-telemetry-port>
```

If you change `data_plane.api_listen_address`, update the ADP-local scrape target to match the
configured host and port.

## Migrate legacy expvar consumers

1. Identify the expvar path your dashboard or `go_expvar` check reads from the Core Agent's
   `/debug/vars` endpoint. Do not substitute an entire metric family for a path that selects labels.
2. Use the table below to find the corresponding ADP series on the Core Agent's `/telemetry`
   endpoint. Select the listed labels and keep `emitter` separate when comparing ADP and local
   traffic. Use counter rates for dashboards rather than adding raw counter values across restarts.
3. If you rely on the existing expvar path, deploy a Core Agent version with the
   [ADP expvar bridge](https://github.com/DataDog/datadog-agent/pull/58438). The bridge is a
   separate Core Agent change; Saluki's RAR remappings alone do not update `/debug/vars`.
   Until that change is available, query `/telemetry` directly for ADP values.

The Core Agent's `/telemetry` endpoint includes ADP series alongside local Agent series. The
bridge combines ADP and local values only for its explicit mappings; a metric on `/telemetry` does
not automatically become an expvar. The table shows the legacy paths and RAR metric families
used by the proposed bridge, including known gaps. These names are *not* the names from ADP's
`/compat/metrics` endpoint. Paths below are relative to `/debug/vars`.

| Legacy expvar paths | ADP series on `/telemetry` | Fidelity and bridge status |
| --- | --- | --- |
| `dogstatsd-udp/Packets`, `dogstatsd-udp/PacketReadingErrors` | `dogstatsd__udp_packets`; packet-reading errors select `state="error"` | Exact. The bridge maps both paths; the packet total includes the relevant packet states. |
| `dogstatsd-uds/Packets`, `dogstatsd-uds/PacketReadingErrors` | `dogstatsd__uds_packets`; select `transport="unix"` or `"unixgram"`, and `state="error"` for errors | Exact. The bridge maps both paths and combines the two UDS transports. |
| `dogstatsd/MetricPackets`, `MetricParseErrors`, `EventPackets`, `EventParseErrors`, `ServiceCheckPackets`, `ServiceCheckParseErrors` | `dogstatsd__processed{message_type=...,state=...}` | Exact only with both labels selected. The bridge maps each path separately: `metrics`, `events`, or `service_checks`, crossed with `ok` or `error`. |
| `dogstatsd-uds/OriginDetectionErrors` | `dogstatsd__uds_origin_detection_error` | Known gap: the bridge expects `dogstatsd__uds_origin_detection_errors` (plural), so the current RAR family name does not match. Do not assume this expvar receives the ADP value yet. |
| `forwarder/Transactions/Success` | `transactions__success` | Approximate: ADP counts `/api/v1/validate` as a transaction, while the legacy value does not. The bridge maps the series; filter out that endpoint in OpenMetrics queries for like-for-like comparison. |
| `forwarder/Transactions/Errors`, `HTTPErrors`, and `ErrorsByType/*` | `transactions__errors{error_type=...}` and `transactions__http_errors` | The bridge maps `Errors` and `HTTPErrors`. **Known gap:** its `ErrorsByType/*` selectors do not match ADP's error-type labels, so those paths do not receive the corresponding ADP counts. For ADP queries, select `dns_error`, `tls_error`, `connection_error`, or `wrote_request_error` on `transactions__errors`; there is no ADP `invalid_request` label for `SentRequestErrors`. |
| `forwarder/Transactions/RetryQueueSize` | `transactions__retry_queue_size{domain=...}` | Exact gauge. The bridge combines the per-domain queue sizes into the legacy value. |
| `forwarder/Transactions/DroppedOnInput` | `transactions__dropped` | Not equivalent and **not bridged**: ADP counts a broader set of failed requests, not only retry-queue drops. Query the ADP series separately. |
| `aggregator/DogstatsdContexts`, `DogstatsdMetricSample`, `SeriesFlushed` | `aggregator__dogstatsd_contexts`, `aggregator__processed{data_type="dogstatsd_metrics"}`, `aggregator__flush{data_type="series"}` | Mapped by the bridge. The contexts value comes from an INFO-level context-resolver gauge (not the DEBUG-only aggregate-transform gauge); the sample and flushed-series values are counters. |
| `aggregator/Event`, `ServiceCheck`, `EventsFlushed`, `ServiceCheckFlushed` | `aggregator__processed` and `aggregator__flush`, selecting `data_type="events"` or `"service_checks"` | Mapped by the bridge with a distinct `data_type` selector for each path. These include encoder-proxy telemetry; compare semantics before replacing existing dashboards. |
| `aggregator/FlushCount/*/LastFlush`, `Flush/*FlushTime/LastFlush` | `aggregator__flush_count{flush_type=...}`, `aggregator__flush_time{flush_type=...}` | Available counts and timings are mapped by the bridge. These are per-process snapshots, not meaningful combined totals in an OpenMetrics query; keep them separate by `emitter`. Not every legacy timing has an ADP equivalent. |
| `aggregator/NumberOfFlush` | `aggregator__number_of_flush` | Mapped. The ADP counter is INFO-level and available at default verbosity, but it counts non-empty flushes and is not an exact substitute. |
| Splitter and serializer | V3 metrics such as `serializer__v3_payload_split_reason{reason="item_too_big"}` | Not bridged. The Core Agent no longer has the legacy splitter expvar subtree. ADP splits proactively rather than using a split-and-retry loop; the V3 payload-split give-up counter is not emitted. |
| Scheduler and Logs Agent | No ADP series | These paths are unaffected and remain local to the Agent. |

The bridge does not cover every aggregator field (for example, Core Agent-owned check telemetry or
flush timings with no ADP analogue). Query `/telemetry` for ADP-specific metrics that have no
legacy expvar equivalent.

### Contexts grouped by origin

Set `telemetry.dogstatsd_origin: true` to emit `aggregator__dogstatsd_contexts_by_origin` on
`/telemetry`. The default is `false`. This gauge groups active DogStatsD contexts by their complete
resolved origin-tag set; contexts without resolved origin tags are omitted. It is not a legacy
expvar and does not replace `aggregator/DogstatsdContexts`.

The gauge exports up to 199 distinct origin groups and one `origin_overflow="true"` group for
additional sets or sets exceeding 32 tags or 4 KiB of tag text. Groups keep their assignment
until their contexts expire. Origin label names are made safe for Prometheus; repeated tag keys
get deterministic numbered labels (for example, `kube_service_0` and `kube_service_1`). Bare tags
and tags with empty values use distinct marker labels. ADP escapes special characters in label
values according to Prometheus text exposition rules. Check the overflow group when comparing
per-origin counts against the total.

The six processed-message expvar metrics share `dogstatsd__processed`, so select both labels when
querying `/telemetry`. For example, metric packets use `message_type="metrics"` and `state="ok"`:

```promql
sum(
  rate(dogstatsd__processed{emitter="Agent Data Plane",message_type="metrics",state="ok"}[5m])
)
```

Change `state` to `error` to query metric parse errors. The emitter value is the registered display
name; replace `Agent Data Plane` if your deployment uses another name.

To combine the compatible series-flush counters, calculate the rate for each emitter before
aggregating so counter resets remain visible:

```promql
sum without (emitter) (rate(aggregator__flush{data_type="series"}[5m]))
```

## Troubleshooting

If scraping fails, verify that ADP is listening on the unprivileged API address:

```shell
curl http://127.0.0.1:5100/metrics
```

If you configured a non-default `data_plane.api_listen_address`, use that address instead of
`127.0.0.1:5100`.

If the endpoint returns data but dashboards do not populate, check whether the dashboard expects
ADP-native names from `/metrics` or compatibility names from `/compat/metrics`.

If you are investigating DogStatsD-specific counters, the internal telemetry endpoint includes
aggregate DogStatsD counters such as processed message counts, packet and byte counts, packet pool
usage, and channel latency. This is separate from the on-demand DogStatsD statistics API documented
in [Agent Data Plane configuration](configuration/configuration.md).
