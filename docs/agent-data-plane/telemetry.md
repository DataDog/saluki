# Scraping internal telemetry

Agent Data Plane exposes internal telemetry as OpenMetrics text on the unprivileged API endpoint.
You can scrape this endpoint to collect ADP process, pipeline, and component metrics.

## Endpoints

ADP registers two internal telemetry routes:

| Route             | Description                                                                                 |
| ----------------- | ------------------------------------------------------------------------------------------- |
| `/metrics`        | Full ADP internal telemetry, rendered as Prometheus/OpenMetrics text exposition.             |
| `/compat/metrics` | ADP-only compatibility view with selected metrics under legacy-compatible aliases.    |

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
> `telemetry.enabled` does not control these ADP endpoints. ADP ignores that core Agent
> configuration key, and the internal telemetry routes are registered by ADP itself.

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

## Legacy expvar mappings and fidelity

The Core Agent's `/telemetry` endpoint includes ADP series alongside local Agent series. When the
`go_expvar` bridge is available, it combines ADP values with local values only for explicit mappings;
a metric on `/telemetry` does not automatically become an expvar. The table shows the legacy expvar
paths and RAR metric families used by the bridge, including known gaps. Use `/telemetry` directly for
ADP metrics that are not bridged.

| Legacy expvar paths | ADP series on `/telemetry` | Fidelity and bridge status |
| --- | --- | --- |
| `dogstatsd-udp/Packets`, `dogstatsd-udp/PacketReadingErrors` | `dogstatsd__udp_packets`; packet-reading errors select `state="error"` | Exact. The bridge maps both paths; the packet total includes the relevant packet states. |
| `dogstatsd-uds/Packets`, `dogstatsd-uds/PacketReadingErrors` | `dogstatsd__uds_packets`; select `transport="unix"` or `"unixgram"`, and `state="error"` for errors | Exact. The bridge maps both paths and combines the two UDS transports. |
| `dogstatsd/MetricPackets`, `MetricParseErrors`, `EventPackets`, `EventParseErrors`, `ServiceCheckPackets`, `ServiceCheckParseErrors` | `dogstatsd__processed{message_type=...,state=...}` | Exact only with both labels selected. The bridge maps each path separately: `metrics`, `events`, or `service_checks`, crossed with `ok` or `error`. |
| `dogstatsd-uds/OriginDetectionErrors` | `dogstatsd__uds_origin_detection_error` | Known gap: the bridge expects `dogstatsd__uds_origin_detection_errors` (plural), so the current RAR family name does not match. Do not assume this expvar receives the ADP value yet. |
| `forwarder/Transactions/Success` | `transactions__success` | Approximate: ADP counts `/api/v1/validate` as a transaction, while the legacy value does not. The bridge maps the series; filter out that endpoint in OpenMetrics queries for like-for-like comparison. |
| `forwarder/Transactions/Errors`, `HTTPErrors`, and `ErrorsByType/*` | `transactions__errors{error_type=...}` and `transactions__http_errors` | Exact for the mapped error types. The bridge selects `dns_lookup_failure`, `tls_handshake_failure`, `connection_failure`, `writing_failure`, or `invalid_request` for the corresponding `ErrorsByType` expvar. |
| `forwarder/Transactions/RetryQueueSize` | `transactions__retry_queue_size{domain=...}` | Exact gauge. The bridge combines the per-domain queue sizes into the legacy value. |
| `forwarder/Transactions/DroppedOnInput` | `transactions__dropped` | Approximate: ADP counts a broader set of failed requests. The bridge currently maps it, but it is not a like-for-like replacement. |
| `aggregator/dogstatsd_contexts`, `dogstatsd_metric_sample`, `series_flushed` | `aggregator__dogstatsd_contexts`, `aggregator__processed{data_type="dogstatsd_metrics"}`, `aggregator__flush{data_type="series"}` | Exact ADP analogues. They remain separate on `/telemetry`; the current expvar bridge does not merge them into the legacy aggregator paths, which continue to report local check traffic. |
| Aggregator `last_flush` counts and timings | `aggregator__flush_count{flush_type=...}`, `aggregator__flush_time{flush_type=...}` | Per-process snapshots, not meaningful combined totals. Keep them separate by `emitter`; some legacy flush timings have no ADP equivalent. |
| Other aggregator counts | `aggregator__processed` and `aggregator__flush` include encoder-proxy series for events and service checks; `aggregator__number_of_flush` is DEBUG-level | Related telemetry, not full parity. `number_of_flush` is absent at the default verbosity; several legacy service-check/event counters have no equivalent. No aggregator expvar paths are currently bridged. |
| Serializer and splitter | V3 metrics such as `serializer__v3_payload_split_reason{reason="item_too_big"}` | Separate telemetry, not a one-to-one legacy expvar replacement. ADP splits proactively rather than using a split-and-retry loop; the V3 payload-split give-up counter is not emitted. |
| Scheduler and Logs Agent | No ADP series | These paths are unaffected and remain local to the Agent. |

ADP-specific metrics without a legacy expvar also remain separate. For example,
`aggregator__dogstatsd_contexts_by_origin` is opt-in telemetry and has no legacy expvar equivalent.

The six processed-message expvar metrics share `dogstatsd__processed`, so select both labels when querying
`/telemetry`. For example, metric packets use `message_type="metrics"` and `state="ok"`:

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
