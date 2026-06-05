# Scraping internal telemetry

Agent Data Plane exposes internal telemetry as OpenMetrics text on the unprivileged API endpoint.
You can scrape this endpoint to collect ADP process, pipeline, and component metrics.

## Endpoints

ADP registers two internal telemetry routes:

| Route             | Description                                                                                 |
| ----------------- | ------------------------------------------------------------------------------------------- |
| `/metrics`        | Full ADP internal telemetry, rendered as Prometheus/OpenMetrics text exposition.             |
| `/compat/metrics` | Compatibility view with metric names remapped to match equivalent Datadog Agent telemetry.  |

The routes are served from `data_plane.api_listen_address`, also called the "unprivileged API
address." The default address uses port `5100`, so a default local scrape target is:

```text
http://127.0.0.1:5100/metrics
```

Use `/metrics` when you want the complete ADP telemetry surface. Use `/compat/metrics` when you
need dashboards, monitors, or comparisons that expect Datadog Agent-compatible metric names.

> [!NOTE]
> `telemetry.enabled` does not control these ADP endpoints. ADP ignores that core Agent
> configuration key, and the internal telemetry routes are registered by ADP itself.

## Configure a scrape

Configure your scraper to target the unprivileged API address.

### Datadog Agent OpenMetrics check

To scrape ADP telemetry with the Datadog Agent, configure the
[OpenMetrics check](https://docs.datadoghq.com/integrations/openmetrics/) with ADP's telemetry
endpoint. The OpenMetrics check is usually the best fit when the Datadog Agent runs on the same host
as ADP or can reach ADP's unprivileged API address.

```yaml
instances:
  - openmetrics_endpoint: http://127.0.0.1:5100/metrics
    namespace: agent_data_plane
    metrics:
      - ".*"
```

To scrape the compatibility view, change `openmetrics_endpoint` to
`http://127.0.0.1:5100/compat/metrics`.

> [!NOTE]
> The OpenMetrics check submits collected series as custom metrics. Use explicit metric names or
> narrower patterns when you do not need the full ADP telemetry surface.

### Prometheus

For Prometheus-style scrape configuration, use a static target when the address is fixed:

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

If you change `data_plane.api_listen_address`, update the scrape target to match the configured
host and port.

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
in [Configuring DogStatsD](configuration/dogstatsd.md).
